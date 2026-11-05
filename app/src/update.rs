//! Updates from GitHub Releases.
//!
//! Checks `releases/latest` of the repository now and then. When a newer
//! version is out, the UI offers it; installing downloads the right package:
//!  - installed copy: the setup .exe, run silently after this program exits
//!    (the installer starts the new version when it is done);
//!  - portable copy: the zip; the new Rewind.exe replaces this one in place
//!    (the running .exe is renamed to *.old.exe and removed on the next start).

use crate::controller::QUIT;
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const REPO: &str = "svgravity100/rewind";
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    setup_url: Option<String>,
    portable_url: Option<String>,
}

#[derive(Clone, Debug)]
pub enum State {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    /// Downloading: percent done.
    Downloading(u8),
    /// Downloaded; the program is closing to finish the update.
    Installing,
    Failed(String),
}

pub static STATE: Mutex<State> = Mutex::new(State::Idle);

fn set(s: State) {
    *STATE.lock().unwrap() = s;
}

pub fn state() -> State {
    STATE.lock().unwrap().clone()
}

/// This program's version (RB_FAKE_VERSION pretends to be older, for testing).
pub fn current() -> String {
    std::env::var("RB_FAKE_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

fn parts(v: &str) -> Vec<u64> {
    v.trim_start_matches('v').split(['.', '-', '+']).map_while(|p| p.parse().ok()).collect()
}

/// Is `a` a newer version than `b`? ("0.10.0" > "0.9.3")
pub fn newer(a: &str, b: &str) -> bool {
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).timeout_read(Duration::from_secs(60)).build()
}

/// Ask GitHub for the latest release. Ok(None) when there is none yet.
fn latest() -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = match agent()
        .get(&url)
        .set("User-Agent", "Rewind-updater")
        .set("Accept", "application/vnd.github+json")
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(ureq::Error::Status(403 | 429, _)) => return Err("GitHub временно ограничил проверки".into()),
        Err(ureq::Error::Status(code, _)) => return Err(format!("GitHub ответил ошибкой {code}")),
        Err(ureq::Error::Transport(t)) => {
            return Err(match t.kind() {
                ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed => "нет интернета".into(),
                ureq::ErrorKind::Io => "связь оборвалась".into(),
                _ => "нет связи с GitHub".into(),
            })
        }
    };
    let v: serde_json::Value = resp.into_json().map_err(|e| format!("ответ GitHub не прочитан ({e})"))?;
    let tag = v["tag_name"].as_str().ok_or("в релизе нет версии")?;
    let mut r = Release { version: tag.trim_start_matches('v').to_string(), setup_url: None, portable_url: None };
    for a in v["assets"].as_array().into_iter().flatten() {
        let (Some(name), Some(url)) = (a["name"].as_str(), a["browser_download_url"].as_str()) else { continue };
        if name.ends_with("-setup.exe") {
            r.setup_url = Some(url.to_string());
        } else if name.ends_with("-portable.zip") {
            r.portable_url = Some(url.to_string());
        }
    }
    Ok(Some(r))
}

/// Testing: what the check sees, as text.
pub fn report() -> String {
    match latest() {
        Ok(Some(r)) => format!(
            "current {}, latest {} (setup: {}, portable: {}), update: {}",
            current(),
            r.version,
            r.setup_url.is_some(),
            r.portable_url.is_some(),
            newer(&r.version, &current())
        ),
        Ok(None) => format!("current {}, no releases yet", current()),
        Err(e) => format!("check failed: {e}"),
    }
}

/// Check once, now (on a background thread). `announce`: show the corner
/// notice when an update is found.
pub fn check_now(announce: bool) {
    if matches!(state(), State::Checking | State::Downloading(_) | State::Installing) {
        return;
    }
    set(State::Checking);
    std::thread::spawn(move || match latest() {
        Ok(Some(r)) if newer(&r.version, &current()) => {
            if announce {
                crate::notify::post(format!("Доступно обновление Rewind {}", r.version), "Откройте окно и нажмите U", true);
            }
            set(State::Available(r));
        }
        Ok(_) => set(State::UpToDate),
        Err(e) => set(State::Failed(e)),
    });
}

/// Check at start-up and then every few hours, while `enabled()` says so.
pub fn start_auto(enabled: impl Fn() -> bool + Send + 'static) {
    std::thread::spawn(move || {
        // Let the program settle first.
        let mut next = Instant::now() + Duration::from_secs(10);
        // After a failed check (e.g. no network yet right after Windows starts):
        // try again sooner, 1, 5, then 15 minutes later.
        let retries = [60u64, 300, 900];
        let mut failed = 0usize;
        let mut was_checking = false;
        while !QUIT.load(Ordering::Relaxed) {
            let st = state();
            if was_checking && !matches!(st, State::Checking) {
                was_checking = false;
                if matches!(st, State::Failed(_)) {
                    next = Instant::now() + Duration::from_secs(retries[failed.min(retries.len() - 1)]);
                    failed += 1;
                } else {
                    failed = 0;
                }
            }
            if Instant::now() >= next {
                if enabled() && !matches!(st, State::Available(_) | State::Downloading(_) | State::Installing) {
                    check_now(true);
                    was_checking = true;
                }
                next = Instant::now() + CHECK_EVERY;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

/// Download and install the available update, then close this program.
pub fn install() {
    let State::Available(r) = state() else { return };
    set(State::Downloading(0));
    std::thread::spawn(move || {
        let result = if crate::util::portable_dir().is_some() { install_portable(&r) } else { install_setup(&r) };
        match result {
            Ok(()) => {
                set(State::Installing);
                // Give the UI a moment to show it, then quit; the helper waits for us.
                std::thread::sleep(Duration::from_millis(800));
                QUIT.store(true, Ordering::SeqCst);
            }
            Err(e) => set(State::Failed(e)),
        }
    });
}

fn download(url: &str, to: &Path) -> Result<(), String> {
    let resp = agent().get(url).set("User-Agent", "Rewind-updater").call().map_err(|e| format!("загрузка не удалась ({e})"))?;
    let total: u64 = resp.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut src = resp.into_reader();
    let mut f = std::fs::File::create(to).map_err(|e| format!("не записать файл ({e})"))?;
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    loop {
        let n = src.read(&mut buf).map_err(|e| format!("загрузка прервалась ({e})"))?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n]).map_err(|e| format!("не записать файл ({e})"))?;
        done += n as u64;
        if total > 0 {
            set(State::Downloading((done * 100 / total).min(100) as u8));
        }
    }
    if total > 0 && done != total {
        return Err("файл скачан не полностью".into());
    }
    Ok(())
}

/// Run `cmd` a few seconds after this program has closed (so the single-copy
/// lock is free), with no console window.
fn run_after_exit(cmd: &str) -> Result<(), String> {
    std::process::Command::new("cmd")
        .raw_arg(format!("/c ping -n 6 127.0.0.1 >nul & {cmd}"))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("не запустить установку ({e})"))
}

fn install_setup(r: &Release) -> Result<(), String> {
    let url = r.setup_url.as_ref().ok_or("в релизе нет установщика")?;
    let path = std::env::temp_dir().join(format!("Rewind-{}-setup.exe", r.version));
    download(url, &path)?;
    // Silent install; the installer starts the new version when it finishes.
    run_after_exit(&format!("\"{}\" /VERYSILENT /SUPPRESSMSGBOXES /NORESTART", path.display()))
}

fn install_portable(r: &Release) -> Result<(), String> {
    let url = r.portable_url.as_ref().ok_or("в релизе нет портативной версии")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("нет папки программы")?.to_path_buf();
    let zip_path = std::env::temp_dir().join(format!("Rewind-{}-portable.zip", r.version));
    download(url, &zip_path)?;
    let new_exe = dir.join("Rewind.new.exe");
    extract_exe(&zip_path, &new_exe)?;
    let _ = std::fs::remove_file(&zip_path);
    // A running .exe can be renamed but not overwritten.
    let old = old_path(&exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("не заменить программу ({e})"))?;
    if let Err(e) = std::fs::rename(&new_exe, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("не заменить программу ({e})"));
    }
    run_after_exit(&format!("start \"\" \"{}\"", exe.display()))
}

fn extract_exe(zip_path: &Path, to: &Path) -> Result<(), String> {
    let f = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| format!("архив повреждён ({e})"))?;
    for i in 0..z.len() {
        let mut entry = z.by_index(i).map_err(|e| e.to_string())?;
        if entry.name().to_ascii_lowercase().ends_with("rewind.exe") {
            let mut out = std::fs::File::create(to).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    Err("в архиве нет Rewind.exe".into())
}

fn old_path(exe: &Path) -> PathBuf {
    exe.with_extension("old.exe")
}

/// After a portable update: remove the previous .exe left next to this one.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_path(&exe));
        if let Some(d) = exe.parent() {
            let _ = std::fs::remove_file(d.join("Rewind.new.exe"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn compares_versions_by_number() {
        assert!(newer("0.2.0", "0.1.0"));
        assert!(newer("v0.10.0", "0.9.9"));
        assert!(newer("1.0", "0.99.99"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
        assert!(!newer("v0.1.0", "0.1"));
    }
}
