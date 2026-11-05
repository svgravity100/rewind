//! Rewind — replay buffer with a terminal UI.
//!
//! Threads: main = terminal UI; "shell" = hotkeys, tray icon and crosshair
//! overlay (they need a Windows message loop); recorder threads = capture,
//! encoding, audio, writer.

mod audio;
mod autostart;
mod avi;
mod capture;
mod gpu;
mod hdr;
mod icon;
mod controller;
mod mf;
mod notify;
mod overlay;
mod recorder;
mod settings;
mod shell;
mod sound;
mod tray;
mod tui;
mod update;
mod util;

use controller::{Controller, QUIT};
use recorder::AppEvent;
use settings::Settings;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use windows::Win32::Foundation::{BOOL, COLORREF, HWND};
use windows::Win32::System::Console::{
    GetConsoleMode, GetConsoleProcessList, GetConsoleScreenBufferInfoEx, SetConsoleScreenBufferInfoEx, CONSOLE_CHARACTER_ATTRIBUTES, CONSOLE_SCREEN_BUFFER_INFOEX, GetConsoleWindow, GetStdHandle, SetConsoleCP, SetConsoleCtrlHandler, SetConsoleMode,
    SetConsoleOutputCP, CONSOLE_MODE, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT, ENABLE_EXTENDED_FLAGS,
    ENABLE_PROCESSED_OUTPUT, ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL, DeleteMenu, FindWindowW, GetClassNameW, GetSystemMenu, MF_BYCOMMAND, SC_CLOSE};

/// For the console close handler, which runs on its own thread.
static CTRL: OnceLock<Arc<Controller>> = OnceLock::new();

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Hidden self-test: record for N seconds, save the buffer, report, exit.
    if let Some(i) = args.iter().position(|a| a == "--selftest") {
        let secs: u64 = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(5);
        let dir = args.get(i + 2).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("shots"));
        selftest(secs, dir);
        return;
    }
    if args.iter().any(|a| a == "--hotkeys-check") {
        // Try to take the default shortcuts, report which are free.
        let m = global_hotkey::GlobalHotKeyManager::new().expect("hotkey manager");
        let d = Settings::default();
        for (name, h) in [("save", d.hk_save), ("toggle", d.hk_toggle), ("crosshair", d.hk_crosshair)] {
            let hk = controller::to_hotkey(h);
            let r = m.register(hk);
            println!("{name} {}: {:?}", h.label(), r);
            let _ = m.unregister(hk);
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--inspect") {
        let file = PathBuf::from(args.get(i + 1).cloned().unwrap_or_default());
        let dir = args.get(i + 2).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("shots/inspect"));
        match mf::inspect(&file, &dir) {
            Ok(s) => println!("{s}"),
            Err(e) => println!("inspect failed: {e}"),
        }
        return;
    }

    if args.iter().any(|a| a == "--gpu-colors") {
        // Compare colours and the pointer after the GPU and the CPU conversion.
        mf::com_init();
        let m = capture::list_monitors().into_iter().next().expect("monitor");
        match gpu::color_test(&m, &std::env::temp_dir().join("rb-colors")) {
            Ok(s) => print!("{s}"),
            Err(e) => println!("color test failed: {e}"),
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--toast-png") {
        let p = PathBuf::from(args.get(i + 1).cloned().unwrap_or_else(|| "toast.png".into()));
        let _ = notify::render_png("Клип сохранён", "00:44 · 51 МБ · Rewind", true, &p);
        let _ = notify::render_png("Не удалось сохранить", "в буфере ещё нет кадров", false, &p.with_extension("err.png"));
        return;
    }
    if args.iter().any(|a| a == "--hdr-info") {
        for (i, m) in capture::list_monitors().iter().enumerate() {
            println!("{}: {} hdr={}", i + 1, m.label(i), capture::is_hdr(m));
        }
        return;
    }
    if args.iter().any(|a| a == "--update-check") {
        println!("{}", update::report());
        return;
    }
    if args.iter().any(|a| a == "--render") {
        // Print the three pages as text, without recording or a console UI.
        let (tx, rx) = crossbeam_channel::unbounded::<AppEvent>();
        let ctrl = Arc::new(Controller::new(Settings::load(), tx));
        print!("{}", tui::Tui::new(ctrl, rx, Arc::default(), 0).snapshot(110, 28));
        return;
    }

    // Started with Windows (autostart): straight into the tray, no window.
    let to_tray = args.iter().any(|a| a == "--tray");
    let (console, owned, pseudo) = console_info();
    // Not in a console window of our own (Windows Terminal, or a cmd/PowerShell
    // window shared with a shell): reopen in our own classic console window —
    // only that one can be hidden to the tray and taken off the taskbar.
    if !(owned && !pseudo) && std::env::var_os("RB_CONHOST").is_none() {
        if let Ok(exe) = std::env::current_exe() {
            // Started the way Explorer starts programs (ShellExecute): conhost
            // then opens its own window. CreateProcess with inherited or empty
            // std handles made conhost run windowless or not start the app.
            let mut params = format!("\"{}\"", exe.display());
            for a in &args[1..] {
                params.push_str(&format!(" \"{a}\""));
            }
            std::env::set_var("RB_CONHOST", "1");
            let params = windows::core::HSTRING::from(params);
            let r = unsafe { ShellExecuteW(None, w!("open"), w!("conhost.exe"), &params, None, if to_tray { SW_HIDE } else { SW_SHOWNORMAL }) };
            let ok = r.0 as isize > 32;
            if ok {
                return;
            }
            std::env::remove_var("RB_CONHOST");
        }
    }

    unsafe {
        // One copy only: a second one would wipe the first one's buffer files.
        // Instead, bring the running one's console back from the tray.
        let mutex = CreateMutexW(None, false, w!("Local\\RewindReplayBuffer"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if !to_tray {
                if let Ok(h) = FindWindowW(w!("ConsoleWindowClass"), w!("Rewind")) {
                    tray::show(h.0 as isize);
                }
            }
            return;
        }
        // Held for the life of the process.
        std::mem::forget(mutex);
        // Keep "start with Windows" pointing at this copy of the program.
        autostart::refresh();
        // After a portable update: remove the previous .exe.
        update::cleanup();
        // Physical pixels everywhere: the overlay is placed by DXGI coordinates.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = SetConsoleOutputCP(65001);
        // Colour escape codes on.
        if let Ok(out) = GetStdHandle(STD_OUTPUT_HANDLE) {
            let mut mode = CONSOLE_MODE(0);
            if GetConsoleMode(out, &mut mode).is_ok() {
                let _ = SetConsoleMode(out, mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
        let _ = SetConsoleCP(65001);
    }
    // Only a console we own may be hidden, resized and lose its close button.
    let hide_hwnd = if owned && !pseudo { console } else { 0 };
    // The classic console garbles 24-bit colour codes (text ends up white), so
    // there the UI uses the 16 standard colours. In our own console window
    // those 16 are repainted to the green theme; Windows Terminal gets RGB.
    // Our own classic console never gets RGB, whatever COLORTERM says.
    let truecolor = hide_hwnd == 0 && (pseudo || std::env::var("COLORTERM").is_ok_and(|v| v == "truecolor" || v == "24bit"));
    tui::TRUECOLOR.store(truecolor, Ordering::Relaxed);
    if hide_hwnd != 0 {
        unsafe { set_green_palette() };
    }
    unsafe {
        // QuickEdit: a click in the console starts a text selection that freezes
        // all output — the UI would hang until Esc. Not wanted here.
        if let Ok(input) = GetStdHandle(STD_INPUT_HANDLE) {
            let mut mode = CONSOLE_MODE(0);
            if GetConsoleMode(input, &mut mode).is_ok() {
                let _ = SetConsoleMode(input, (mode & !ENABLE_QUICK_EDIT_MODE) | ENABLE_EXTENDED_FLAGS);
            }
        }
    }
    if hide_hwnd != 0 {
        use crossterm::{execute, terminal};
        let _ = execute!(std::io::stdout(), terminal::SetTitle("Rewind"));
        unsafe { set_console_size(110, 28) };
        unsafe {
            // The X would kill the process; leaving is via Q or the tray menu.
            let menu = GetSystemMenu(HWND(console as *mut _), false);
            let _ = DeleteMenu(menu, SC_CLOSE, MF_BYCOMMAND);
            set_window_icon(console);
        }
        if to_tray {
            tray::hide(console);
        }
    }

    mf::com_init();
    let settings = Settings::load();
    let (tx, rx) = crossbeam_channel::unbounded::<AppEvent>();
    let ctrl = Arc::new(Controller::new(settings.clone(), tx));
    let _ = CTRL.set(ctrl.clone());
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(on_console_event), true);
    }
    if settings.replay_enabled {
        ctrl.start_now();
    }
    {
        let c = ctrl.clone();
        update::start_auto(move || c.settings.lock().unwrap().check_updates);
    }
    let failed: shell::Failed = Arc::default();
    let shell = shell::start(ctrl.clone(), hide_hwnd, failed.clone());

    let res = tui::Tui::new(ctrl.clone(), rx, failed, hide_hwnd).run();
    QUIT.store(true, Ordering::SeqCst);
    let _ = shell.join();
    ctrl.shutdown();
    if let Err(e) = res {
        eprintln!("Ошибка интерфейса: {e}");
    }
}

/// Window and buffer exactly `w`×`h` cells (no scrollback, no scroll bar).
/// The window has to shrink first: the buffer may never be smaller than it.
unsafe fn set_console_size(w: i16, h: i16) {
    use windows::Win32::System::Console::{SetConsoleScreenBufferSize, SetConsoleWindowInfo, COORD, SMALL_RECT};
    let Ok(out) = GetStdHandle(STD_OUTPUT_HANDLE) else { return };
    let _ = SetConsoleWindowInfo(out, true, &SMALL_RECT { Left: 0, Top: 0, Right: 0, Bottom: 0 });
    let _ = SetConsoleScreenBufferSize(out, COORD { X: w, Y: h });
    let _ = SetConsoleWindowInfo(out, true, &SMALL_RECT { Left: 0, Top: 0, Right: w - 1, Bottom: h - 1 });
}

/// Our icon (resource 1 in the .exe) on the console window and its taskbar button.
unsafe fn set_window_icon(hwnd: isize) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, WM_SETICON};
    let Ok(inst) = GetModuleHandleW(None) else { return };
    let h = HWND(hwnd as *mut _);
    for (kind, size) in [(ICON_BIG, 32), (ICON_SMALL, 16)] {
        if let Ok(icon) = LoadImageW(inst, windows::core::PCWSTR(1 as *const u16), IMAGE_ICON, size, size, LR_DEFAULTCOLOR) {
            SendMessageW(h, WM_SETICON, WPARAM(kind as usize), LPARAM(icon.0 as isize));
        }
    }
}

/// Repaint our console's 16 colours to the theme (see `tui::to_16`).
unsafe fn set_green_palette() {
    let Ok(out) = GetStdHandle(STD_OUTPUT_HANDLE) else { return };
    let mut info = CONSOLE_SCREEN_BUFFER_INFOEX { cbSize: std::mem::size_of::<CONSOLE_SCREEN_BUFFER_INFOEX>() as u32, ..Default::default() };
    if GetConsoleScreenBufferInfoEx(out, &mut info).is_err() {
        return;
    }
    for (i, (r, g, b)) in tui::PALETTE {
        info.ColorTable[i] = COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16);
    }
    // Default text: "white" (theme text) on "black" (theme background).
    info.wAttributes = CONSOLE_CHARACTER_ATTRIBUTES(15);
    // Known quirk: the call shrinks the window by one cell unless compensated.
    info.srWindow.Right += 1;
    info.srWindow.Bottom += 1;
    let _ = SetConsoleScreenBufferInfoEx(out, &info);
}

/// Console window handle, whether this process is its only user, and whether
/// it is a Windows Terminal pseudo-console (which cannot be hidden).
fn console_info() -> (isize, bool, bool) {
    unsafe {
        let h = GetConsoleWindow();
        let mut list = [0u32; 4];
        let owned = GetConsoleProcessList(&mut list) == 1;
        let mut buf = [0u16; 64];
        let n = GetClassNameW(h, &mut buf) as usize;
        let class = String::from_utf16_lossy(&buf[..n]);
        (h.0 as isize, owned, class == "PseudoConsoleWindow")
    }
}

/// Console closed / logoff / shutdown: stop recording cleanly first.
unsafe extern "system" fn on_console_event(kind: u32) -> BOOL {
    if kind == CTRL_CLOSE_EVENT || kind == CTRL_LOGOFF_EVENT || kind == CTRL_SHUTDOWN_EVENT {
        QUIT.store(true, Ordering::SeqCst);
        if let Some(c) = CTRL.get() {
            c.shutdown();
        }
        tray::remove();
        std::thread::sleep(Duration::from_millis(200));
    }
    // Ctrl+C / Ctrl+Break are handled as keys by the UI.
    BOOL(1)
}

fn selftest(secs: u64, dir: PathBuf) {
    let mut s = Settings::load();
    s.output_dir = std::path::absolute(&dir).unwrap_or(dir);
    s.replay_enabled = true;
    if std::env::var("RB_AVI").is_ok() {
        s.format = settings::Format::Avi;
    }
    if let Some(f) = std::env::var("RB_FPS").ok().and_then(|v| v.parse().ok()) {
        s.fps = f;
    }
    if let Some(m) = std::env::var("RB_MINUTES").ok().and_then(|v| v.parse().ok()) {
        s.buffer_minutes = m;
    }
    if std::env::var("RB_MIC").is_ok() {
        s.microphone = true;
    }
    let save_secs: Option<u64> = std::env::var("RB_SAVE").ok().and_then(|v| v.parse().ok());
    let (tx, rx) = crossbeam_channel::unbounded();
    let ctrl = Controller::new(s, tx);
    ctrl.start_now();
    std::thread::sleep(Duration::from_secs(secs));
    if let Some(p) = ctrl.pipeline.lock().unwrap().as_ref() {
        let sh = &p.shared;
        println!(
            "buffered {} ms, disk {} B, sent {}, dropped {}, err {:?}, audio {:?}, fps {}, gpu {}, tracks {:?}, chat {:?}, note {:?}",
            sh.buffered_ms.load(Ordering::Relaxed),
            sh.disk_bytes.load(Ordering::Relaxed),
            sh.frames_sent.load(Ordering::Relaxed),
            sh.frames_dropped.load(Ordering::Relaxed),
            sh.error.lock().unwrap(),
            sh.audio_note.lock().unwrap(),
            p.fps,
            p.gpu,
            p.tracks,
            sh.chat_found.lock().unwrap(),
            sh.format_note.lock().unwrap()
        );
    }
    // RB_COMPARE: save the same buffer the new and the old way, twice each.
    let compare = std::env::var_os("RB_COMPARE").is_some();
    // RB_SAVES=n: save n times in a row, RB_GAP seconds apart (default 1).
    let rounds = if compare { 4 } else { std::env::var("RB_SAVES").ok().and_then(|v| v.parse().ok()).unwrap_or(1) };
    let gap: u64 = std::env::var("RB_GAP").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    for round in 0..rounds {
        let seq = compare && round % 2 == 1;
        mf::SEQUENTIAL.store(seq, Ordering::Relaxed);
        let t = std::time::Instant::now();
        ctrl.save_replay(save_secs);
        let how = if seq { "old (sequential)" } else { "new (parallel)" };
        match rx.recv_timeout(Duration::from_secs(300)) {
            Ok(AppEvent::Saved { path, secs, bytes, what, .. }) => {
                println!("{how} {what}: {} — {secs:.2} s, {bytes} B, saved in {} ms", path.display(), t.elapsed().as_millis())
            }
            Ok(AppEvent::Error(e)) => println!("error: {e}"),
            Err(_) => println!("timeout"),
        }
        std::thread::sleep(Duration::from_millis(gap * 1000 + 100));
    }
    ctrl.shutdown();
}
