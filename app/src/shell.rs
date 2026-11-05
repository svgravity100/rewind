//! The "shell" thread: owns everything that needs a Windows message loop —
//! global hotkeys, the tray icon and the crosshair overlay — and keeps them in
//! step with the settings. The terminal UI runs on the main thread.

use crate::controller::{Action, Controller, Hotkeys, QUIT};
use crate::notify::Toast;
use crate::overlay::Overlay;
use crate::tray;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, IsIconic, MsgWaitForMultipleObjects, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, QS_ALLINPUT,
};

/// Hotkeys that could not be registered, with the reason (shown in the UI).
pub type Failed = Arc<Mutex<HashMap<Action, String>>>;

pub fn start(ctrl: Arc<Controller>, console: isize, failed: Failed) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut hotkeys = Hotkeys::new(ctrl.clone());
        tray::install(ctrl.clone(), console);
        let mut overlay = Overlay::new();
        let mut toast = Toast::new();
        let mut last_top = Instant::now();
        while !QUIT.load(Ordering::Relaxed) {
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                // Sleep until a message arrives or 50 ms pass.
                MsgWaitForMultipleObjects(None, false, 50, QS_ALLINPUT);
            }
            let s = ctrl.settings.lock().unwrap().clone();
            hotkeys.sync(&s);
            *failed.lock().unwrap() = hotkeys.failed.clone();
            overlay.update(&s, &ctrl.monitors);
            toast.update(ctrl.monitors.get(s.monitor).or(ctrl.monitors.first()));
            if last_top.elapsed() >= Duration::from_secs(1) {
                last_top = Instant::now();
                overlay.keep_on_top();
            }
            tray::refresh(ctrl.pipeline.lock().unwrap().is_some());
            // Minimising the console hides it to the tray (off the taskbar).
            // Checked here, not in the UI loop, so it works even if the UI is busy.
            if console != 0 && unsafe { IsIconic(HWND(console as *mut _)).as_bool() } {
                tray::hide(console);
            }
        }
        tray::remove();
    })
}
