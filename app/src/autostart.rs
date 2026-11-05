//! Start with Windows: a value in the user's "Run" registry key that starts
//! this program with `--tray` (straight into the tray, no window).

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::*;

const KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const NAME: PCWSTR = w!("Rewind");
/// The value written before the program was renamed (moved to NAME at start).
const OLD_NAME: PCWSTR = w!("Povtor");

/// The command Windows should run at sign-in.
fn command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" --tray", exe.display()))
}

/// The command currently registered, if start with Windows is on.
fn current() -> Option<String> {
    unsafe {
        let mut size = 0u32;
        if RegGetValueW(HKEY_CURRENT_USER, KEY, NAME, RRF_RT_REG_SZ, None, None, Some(&mut size)).is_err() {
            return None;
        }
        let mut buf = vec![0u16; size as usize / 2 + 1];
        RegGetValueW(HKEY_CURRENT_USER, KEY, NAME, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut size))
            .ok()
            .ok()?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

pub fn enabled() -> bool {
    current().is_some()
}

pub fn set(on: bool) -> Result<(), String> {
    unsafe {
        if on {
            let cmd = command().ok_or("не найден путь к программе")?;
            let wide: Vec<u16> = cmd.encode_utf16().chain(Some(0)).collect();
            RegSetKeyValueW(HKEY_CURRENT_USER, KEY, NAME, REG_SZ.0, Some(wide.as_ptr() as *const _), (wide.len() * 2) as u32)
                .ok()
                .map_err(|e| format!("Автозапуск: {}", e.message()))
        } else {
            let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, KEY, NAME);
            if r.is_ok() || r == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                Err(format!("Автозапуск: {}", r.to_hresult().message()))
            }
        }
    }
}

/// When it is on, point it at this copy of the program (it may have been moved).
pub fn refresh() {
    // From before the rename: carry the setting over to the new name.
    unsafe {
        let mut size = 0u32;
        if RegGetValueW(HKEY_CURRENT_USER, KEY, OLD_NAME, RRF_RT_REG_SZ, None, None, Some(&mut size)).is_ok() {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, KEY, OLD_NAME);
            let _ = set(true);
            return;
        }
    }
    if let Some(c) = current() {
        if Some(c) != command() {
            let _ = set(true);
        }
    }
}
