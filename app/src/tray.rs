//! Tray icon: the console window hides here, the buffer keeps recording.
//! Menu: open, save replay, toggle buffer, toggle crosshair, open folder, quit.
//!
//! Lives on the shell thread (see `shell.rs`), which pumps its messages.

use crate::controller::{Action, Controller, QUIT};
use crate::util;
use std::cell::RefCell;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{IsIconic, SetForegroundWindow, ShowWindow, SW_HIDE, SW_RESTORE, SW_SHOW};

thread_local! {
    // TrayIcon is not Send; it stays on the shell thread, where its events arrive too.
    static TRAY: RefCell<Option<(TrayIcon, bool)>> = const { RefCell::new(None) };
}

/// Hide the console window (0 = no window to hide).
pub fn hide(hwnd: isize) {
    if hwnd != 0 {
        unsafe {
            let _ = ShowWindow(HWND(hwnd as *mut _), SW_HIDE);
        }
    }
}

pub fn show(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    let h = HWND(hwnd as *mut _);
    unsafe {
        let _ = ShowWindow(h, SW_SHOW);
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        let _ = SetForegroundWindow(h);
    }
}

/// Create the tray icon on the current (message-pumping) thread.
pub fn install(ctrl: Arc<Controller>, hwnd: isize) {
    let menu = Menu::new();
    let open = MenuItem::new("Открыть", true, None);
    let save = MenuItem::new("Сохранить повтор", true, None);
    let toggle = MenuItem::new("Вкл / выкл буфер", true, None);
    let cross = MenuItem::new("Вкл / выкл прицел", true, None);
    let folder = MenuItem::new("Открыть папку", true, None);
    let quit = MenuItem::new("Выход", true, None);
    let _ = menu.append_items(&[
        &open,
        &PredefinedMenuItem::separator(),
        &save,
        &toggle,
        &cross,
        &folder,
        &PredefinedMenuItem::separator(),
        &quit,
    ]);
    let ids = [open.id().clone(), save.id().clone(), toggle.id().clone(), cross.id().clone(), folder.id().clone(), quit.id().clone()];

    let recording = ctrl.pipeline.lock().unwrap().is_some();
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip(tooltip(recording))
        .with_icon(icon(recording))
        .build();
    let Ok(tray) = tray else { return };
    TRAY.with(|t| *t.borrow_mut() = Some((tray, recording)));

    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            show(hwnd);
        }
    }));

    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let [open, save, toggle, cross, folder, quit] = &ids;
        if e.id == *open {
            show(hwnd);
        } else if e.id == *save {
            ctrl.run(Action::SaveReplay);
        } else if e.id == *toggle {
            ctrl.run(Action::ToggleReplay);
        } else if e.id == *cross {
            ctrl.run(Action::ToggleCrosshair);
        } else if e.id == *folder {
            let dir = ctrl.settings.lock().unwrap().output_dir.clone();
            let _ = std::fs::create_dir_all(&dir);
            util::open_path(&dir);
        } else if e.id == *quit {
            QUIT.store(true, Ordering::SeqCst);
        }
    }));
}

/// Recolour the icon when recording starts or stops.
pub fn refresh(recording: bool) {
    TRAY.with(|t| {
        if let Some((tray, shown)) = t.borrow_mut().as_mut() {
            if *shown != recording {
                *shown = recording;
                let _ = tray.set_icon(Some(icon(recording)));
                let _ = tray.set_tooltip(Some(tooltip(recording)));
            }
        }
    });
}

/// Remove the icon (so no ghost stays in the tray after exit).
pub fn remove() {
    TRAY.with(|t| t.borrow_mut().take());
}

fn tooltip(recording: bool) -> &'static str {
    if recording {
        "Rewind — буфер пишется"
    } else {
        "Rewind — буфер выключен"
    }
}

/// The app icon; the dot is red while recording.
fn icon(recording: bool) -> Icon {
    let dot = if recording { crate::icon::DOT_REC } else { crate::icon::DOT_IDLE };
    Icon::from_rgba(crate::icon::render(32, dot), 32, 32).expect("tray icon")
}
