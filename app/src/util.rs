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

/// Where the buffer's files go: a "RewindBuffer" folder in the chosen place,
/// or the Windows temp folder when none is chosen.
pub fn buffer_root(dir: &Option<PathBuf>) -> PathBuf {
    match dir {
        Some(d) => d.join("RewindBuffer"),
        None => std::env::temp_dir().join("ReplayBuffer"),
    }
}

/// Free space (bytes) on the disk holding `path` (or its nearest existing parent).
pub fn free_space(path: &Path) -> Option<u64> {
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let mut free = 0u64;
    let w = windows::core::HSTRING::from(p.as_os_str());
    unsafe { windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(&w, Some(&mut free), None, None).ok()? };
    Some(free)
}

/// Fixed (internal) disks: "C:\", "D:\" …
pub fn fixed_drives() -> Vec<PathBuf> {
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| PathBuf::from(format!("{}:\\", (b'A' + i) as char)))
        .filter(|root| unsafe { GetDriveTypeW(&windows::core::HSTRING::from(root.as_os_str())) } == 3) // DRIVE_FIXED
        .collect()
}

/// "C:" for a path on C: (or the start of the path when it has no drive letter).
pub fn drive_of(path: &Path) -> String {
    let s = path.display().to_string();
    match s.get(..2) {
        Some(d) if d.ends_with(':') => d.to_uppercase(),
        _ => s,
    }
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
