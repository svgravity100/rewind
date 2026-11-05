//! On-screen notice in the top-right corner of the recorded screen ("Клип
//! сохранён · 00:44 · 51 МБ"), like the ShadowPlay overlay. A click-through,
//! always-on-top window, excluded from capture so it never ends up in a clip.
//! It fades out after a few seconds.
//!
//! `post` can be called from any thread; the window lives on the shell thread.

use crate::capture::MonitorInfo;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject, FillRect, GetDC, ReleaseDC,
    SelectObject, SetBkMode, SetTextColor, TextOutW, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    CLEARTYPE_QUALITY, DIB_RGB_COLORS, FW_BOLD, FW_NORMAL, HDC, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SetWindowDisplayAffinity, SetWindowPos, ShowWindow,
    UpdateLayeredWindow, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA,
    WDA_EXCLUDEFROMCAPTURE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};

struct Note {
    title: String,
    detail: String,
    ok: bool,
}

static PENDING: Mutex<Option<Note>> = Mutex::new(None);

/// Show a notice (replaces one still on screen).
pub fn post(title: impl Into<String>, detail: impl Into<String>, ok: bool) {
    *PENDING.lock().unwrap() = Some(Note { title: title.into(), detail: detail.into(), ok });
}

const SHOW_FOR: Duration = Duration::from_millis(3500);
const FADE: Duration = Duration::from_millis(400);

// Theme colours (same as the app window).
const BG: [u8; 3] = [0x0b, 0x13, 0x08];
const EDGE: [u8; 3] = [0x3f, 0x66, 0x00];
const ACCENT: [u8; 3] = [0x76, 0xb9, 0x00];
const REC: [u8; 3] = [0xff, 0x4d, 0x5e];
const TEXT: [u8; 3] = [0xe6, 0xf2, 0xd8];
const MUTED: [u8; 3] = [0x9a, 0xb5, 0x84];

pub struct Toast {
    hwnd: Option<HWND>,
    /// Picture (premultiplied BGRA), size, position, and when it appeared.
    shown: Option<(Vec<u8>, i32, i32, i32, i32, Instant)>,
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(h, m, w, l)
}

impl Toast {
    pub fn new() -> Self {
        let hwnd = unsafe {
            let inst: HINSTANCE = GetModuleHandleW(None).map(Into::into).unwrap_or_default();
            let class = w!("RewindToast");
            RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: inst, lpszClassName: class, ..Default::default() });
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class,
                w!("Rewind"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                inst,
                None,
            )
            .ok()
        };
        if let Some(h) = hwnd {
            unsafe {
                let _ = SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);
            }
        }
        Self { hwnd, shown: None }
    }

    /// Call often (every ~50 ms): shows new notices, fades and hides old ones.
    pub fn update(&mut self, monitor: Option<&MonitorInfo>) {
        let Some(h) = self.hwnd else { return };
        if let Some(note) = PENDING.lock().unwrap().take() {
            let scale = unsafe { GetDpiForSystem() } as f32 / 96.0;
            let (px, w, ht) = unsafe { render(&note, scale) };
            let (mx, my, mw) = monitor.map_or((0, 0, 1920), |m| (m.x, m.y, m.width as i32));
            let margin = (24.0 * scale) as i32;
            let (x, y) = (mx + mw - w - margin, my + margin);
            unsafe {
                present(h, x, y, w, ht, &px, 255);
                let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
                let _ = SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            self.shown = Some((px, x, y, w, ht, Instant::now()));
            return;
        }
        let Some((px, x, y, w, ht, at)) = &self.shown else { return };
        let age = at.elapsed();
        if age >= SHOW_FOR + FADE {
            unsafe {
                let _ = ShowWindow(h, SW_HIDE);
            }
            self.shown = None;
        } else if age > SHOW_FOR {
            let left = 1.0 - (age - SHOW_FOR).as_secs_f32() / FADE.as_secs_f32();
            unsafe { present(h, *x, *y, *w, *ht, px, (left.clamp(0.0, 1.0) * 255.0) as u8) };
        }
    }
}

impl Drop for Toast {
    fn drop(&mut self) {
        if let Some(h) = self.hwnd.take() {
            unsafe {
                let _ = DestroyWindow(h);
            }
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn rgb(c: [u8; 3]) -> COLORREF {
    COLORREF(c[0] as u32 | (c[1] as u32) << 8 | (c[2] as u32) << 16)
}

/// Draw the notice with GDI; returns premultiplied BGRA pixels, width, height.
unsafe fn render(n: &Note, scale: f32) -> (Vec<u8>, i32, i32) {
    let s = |v: f32| (v * scale).round() as i32;
    let (w, h) = (s(340.0), s(70.0));
    let screen = GetDC(None);
    let dc = CreateCompatibleDC(screen);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut out = vec![0u8; (w * h * 4) as usize];
    if let Ok(bmp) = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
        let old = SelectObject(dc, bmp);
        let fill = |r: RECT, c: [u8; 3]| {
            let b = CreateSolidBrush(rgb(c));
            FillRect(dc, &r, b);
            let _ = DeleteObject(b);
        };
        let accent = if n.ok { ACCENT } else { REC };
        // Frame, background, and a coloured stripe on the left.
        fill(RECT { left: 0, top: 0, right: w, bottom: h }, EDGE);
        fill(RECT { left: 1, top: 1, right: w - 1, bottom: h - 1 }, BG);
        fill(RECT { left: 1, top: 1, right: 1 + s(5.0), bottom: h - 1 }, accent);
        // A round mark: ● on the left.
        let cx = s(30.0);
        let cy = h / 2;
        let r = s(9.0);
        SetBkMode(dc, TRANSPARENT);
        let font = |px: i32, weight: i32| {
            CreateFontW(-px, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, CLEARTYPE_QUALITY.0 as u32, 0, w!("Segoe UI"))
        };
        let text = |dc: HDC, x: i32, y: i32, t: &str, c: [u8; 3], f| {
            let old = SelectObject(dc, f);
            SetTextColor(dc, rgb(c));
            let _ = TextOutW(dc, x, y, &wide(t));
            SelectObject(dc, old);
        };
        let title = font(s(17.0), FW_BOLD.0 as i32);
        let detail = font(s(14.0), FW_NORMAL.0 as i32);
        text(dc, s(50.0), s(13.0), &n.title, TEXT, title);
        text(dc, s(50.0), s(38.0), &n.detail, MUTED, detail);
        let _ = DeleteObject(title);
        let _ = DeleteObject(detail);
        std::ptr::copy_nonoverlapping(bits as *const u8, out.as_mut_ptr(), out.len());
        SelectObject(dc, old);
        let _ = DeleteObject(bmp);
        // GDI leaves alpha at 0: make the panel opaque, then draw the mark
        // (a disc in the accent colour) with smooth edges.
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                out[i + 3] = 255;
                let d = (((x - cx) as f32 + 0.5).powi(2) + ((y - cy) as f32 + 0.5).powi(2)).sqrt();
                let a = (r as f32 + 0.5 - d).clamp(0.0, 1.0);
                if a > 0.0 {
                    for (k, c) in [accent[2], accent[1], accent[0]].into_iter().enumerate() {
                        out[i + k] = (c as f32 * a + out[i + k] as f32 * (1.0 - a)) as u8;
                    }
                }
            }
        }
    }
    let _ = DeleteDC(dc);
    ReleaseDC(None, screen);
    (out, w, h)
}

/// Push the picture to the layered window with an overall opacity.
unsafe fn present(h: HWND, x: i32, y: i32, w: i32, ht: i32, px: &[u8], alpha: u8) {
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(screen);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -ht,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    if let Ok(bmp) = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len());
        let old = SelectObject(mem, bmp);
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8 };
        let _ = UpdateLayeredWindow(h, screen, Some(&POINT { x, y }), Some(&SIZE { cx: w, cy: ht }), mem, Some(&POINT { x: 0, y: 0 }), COLORREF(0), Some(&blend), ULW_ALPHA);
        SelectObject(mem, old);
        let _ = DeleteObject(bmp);
    }
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
}

/// Testing: draw a notice into a PNG (the real window is hidden from capture).
pub fn render_png(title: &str, detail: &str, ok: bool, path: &std::path::Path) -> Result<(), String> {
    let (px, w, h) = unsafe { render(&Note { title: title.into(), detail: detail.into(), ok }, GetDpiForSystem() as f32 / 96.0) };
    crate::capture::save_png(&px, w as u32, h as u32, path)
}
