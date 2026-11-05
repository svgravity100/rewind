use std::path::{Path, PathBuf};

fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn default_output_dir() -> PathBuf {
    env_dir("USERPROFILE").join("Videos").join("Rewind")
}

/// Where clips went before the program was renamed from «Повтор» to Rewind.
pub fn old_output_dir() -> PathBuf {
    env_dir("USERPROFILE").join("Videos").join("Повтор")
}

/// Portable copy: a file named `portable` next to the .exe keeps the settings
/// there too, so the folder can be carried around (USB stick, another PC).
pub fn portable_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    dir.join("portable").is_file().then_some(dir)
}

pub fn settings_path() -> PathBuf {
    match portable_dir() {
        Some(d) => d.join("settings.json"),
        None => env_dir("APPDATA").join("ReplayBuffer").join("settings.json"),
    }
}

pub fn temp_root() -> PathBuf {
    std::env::temp_dir().join("ReplayBuffer")
}

/// Local time as "2026-11-04 13-45-10".
pub fn stamp() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

pub fn unique_path(dir: &Path, base: &str, ext: &str) -> PathBuf {
    let mut p = dir.join(format!("{base}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{base} ({n}).{ext}"));
        n += 1;
    }
    p
}

pub fn open_path(p: &Path) {
    let _ = std::process::Command::new("explorer").arg(p).spawn();
}

pub fn reveal(p: &Path) {
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{}", p.display()))
        .spawn();
}

pub fn fmt_clock(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

pub fn fmt_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1e9 {
        format!("{:.2} ГБ", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} МБ", b / 1e6)
    } else {
        format!("{:.0} КБ", b / 1e3)
    }
}
