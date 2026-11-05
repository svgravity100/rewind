//! Aiming crosshair: a small click-through, always-on-top window in the centre
//! of the chosen monitor. It is excluded from screen capture, so it never ends
//! up in a clip. Works over windowed / borderless games; exclusive fullscreen
//! games draw over every other window.
//!
//! Must live on a thread that pumps window messages.

use crate::capture::MonitorInfo;
use crate::settings::{CrosshairKind, Settings};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SetWindowDisplayAffinity, SetWindowPos, ShowWindow,
    UpdateLayeredWindow, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA,
    WDA_EXCLUDEFROMCAPTURE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};

/// Everything the picture and position depend on.
#[derive(PartialEq, Clone)]
struct Look {
    kind: CrosshairKind,
    size: u32,
    thickness: u32,
    color: [u8; 3],
    rect: (i32, i32, u32, u32),
}

pub struct Overlay {
    hwnd: Option<HWND>,
    shown: Option<Look>,
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(h, m, w, l)
}

impl Overlay {
    pub fn new() -> Self {
        let hwnd = unsafe {
            let inst: HINSTANCE = GetModuleHandleW(None).map(Into::into).unwrap_or_default();
            let class = w!("RewindCrosshair");
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: inst,
                lpszClassName: class,
                ..Default::default()
            });
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class,
                w!("Прицел"),
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
                // Keep it out of the recording (and out of other capture apps).
                let _ = SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);
            }
        }
        Self { hwnd, shown: None }
    }

    /// Show, hide or redraw to match the settings. Cheap when nothing changed.
    pub fn update(&mut self, s: &Settings, monitors: &[MonitorInfo]) {
        let Some(h) = self.hwnd else { return };
        let mon = monitors.get(s.monitor).or(monitors.first());
        let want = match (s.crosshair_enabled, mon) {
            (true, Some(m)) => Some(Look {
                kind: s.crosshair_kind,
                size: s.crosshair_size,
                thickness: s.crosshair_thickness,
                color: s.crosshair_color,
                rect: (m.x, m.y, m.width, m.height),
            }),
            _ => None,
        };
        if want == self.shown {
            return;
        }
        match &want {
            Some(look) => {
                let (side, px) = render(look);
                let (mx, my, mw, mh) = look.rect;
                let x = mx + mw as i32 / 2 - side as i32 / 2;
                let y = my + mh as i32 / 2 - side as i32 / 2;
                unsafe { present(h, x, y, side, &px) };
                unsafe {
                    let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
                }
            }
            None => unsafe {
                let _ = ShowWindow(h, SW_HIDE);
            },
        }
        self.shown = want;
    }

    /// Games that grab "topmost" themselves can cover us; call now and then.
    pub fn keep_on_top(&self) {
        if let (Some(h), Some(_)) = (self.hwnd, &self.shown) {
            unsafe {
                let _ = SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        if let Some(h) = self.hwnd.take() {
            unsafe {
                let _ = DestroyWindow(h);
            }
        }
    }
}

/// Push premultiplied BGRA pixels to the layered window.
unsafe fn present(h: HWND, x: i32, y: i32, side: u32, px: &[u8]) {
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(screen);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: side as i32,
            biHeight: -(side as i32), // top-down
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
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = UpdateLayeredWindow(
            h,
            screen,
            Some(&POINT { x, y }),
            Some(&SIZE { cx: side as i32, cy: side as i32 }),
            mem,
            Some(&POINT { x: 0, y: 0 }),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(bmp);
    }
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
}

/// Draw the crosshair; returns the square side and premultiplied BGRA pixels.
/// Same shapes the old in-window preview used, with a thin dark outline so it
/// stays visible on light and dark scenes.
fn render(l: &Look) -> (u32, Vec<u8>) {
    let arm = (l.size as f32 * 0.5).clamp(2.0, 100.0);
    let t = l.thickness.clamp(1, 10) as f32;
    let dot = (t * 1.6).max(2.0);
    let side = ((arm * 2.0 + t * 2.0 + 10.0).ceil() as u32 + 1) & !1;
    let c = side as f32 / 2.0;
    let mut acc = vec![[0.0f32; 4]; (side * side) as usize]; // straight RGBA, 0..1

    // Coverage helpers (signed distance → 0..1 over one pixel).
    let cov = |d: f32| (0.5 - d).clamp(0.0, 1.0);
    let rect = |px: f32, py: f32, cx: f32, cy: f32, w: f32, h: f32, grow: f32| {
        let dx = (px - cx).abs() - (w / 2.0 + grow);
        let dy = (py - cy).abs() - (h / 2.0 + grow);
        cov(dx.max(dy))
    };
    let disc = |px: f32, py: f32, r: f32| cov(((px - c).powi(2) + (py - c).powi(2)).sqrt() - r);
    let ring = |px: f32, py: f32, r: f32, w: f32| cov((((px - c).powi(2) + (py - c).powi(2)).sqrt() - r).abs() - w / 2.0);

    let col = [l.color[0] as f32 / 255.0, l.color[1] as f32 / 255.0, l.color[2] as f32 / 255.0];
    let halo = 0.47f32;
    for y in 0..side {
        for x in 0..side {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            // (outline coverage, colour coverage)
            let (o, f) = match l.kind {
                CrosshairKind::Cross => {
                    let o = rect(px, py, c, c, arm * 2.0, t, 1.0).max(rect(px, py, c, c, t, arm * 2.0, 1.0));
                    let f = rect(px, py, c, c, arm * 2.0, t, 0.0).max(rect(px, py, c, c, t, arm * 2.0, 0.0));
                    (o, f)
                }
                CrosshairKind::Dot => (disc(px, py, dot + 1.0), disc(px, py, dot)),
                CrosshairKind::Circle => (ring(px, py, arm, t + 2.0), ring(px, py, arm, t)),
                CrosshairKind::T => {
                    let (hy, vy) = (c - arm * 0.5, c + arm * 0.5);
                    let o = rect(px, py, c, hy, arm * 2.0, t, 1.0).max(rect(px, py, c, vy, t, arm, 1.0));
                    let f = rect(px, py, c, hy, arm * 2.0, t, 0.0).max(rect(px, py, c, vy, t, arm, 0.0));
                    (o, f)
                }
                CrosshairKind::CrossDot => {
                    let bars = rect(px, py, c, c, arm * 2.0, t, 1.0).max(rect(px, py, c, c, t, arm * 2.0, 1.0));
                    let o = bars.max(disc(px, py, dot + 2.0));
                    let f = rect(px, py, c, c, arm * 2.0, t, 0.0)
                        .max(rect(px, py, c, c, t, arm * 2.0, 0.0))
                        .max(disc(px, py, dot - 0.5));
                    (o, f)
                }
            };
            let p = &mut acc[(y * side + x) as usize];
            // Outline first, then the colour over it ("over" in straight alpha).
            for (rgb, a) in [([0.0, 0.0, 0.0], o * halo), (col, f)] {
                if a <= 0.0 {
                    continue;
                }
                let below = p[3];
                let out = a + below * (1.0 - a);
                for i in 0..3 {
                    p[i] = (rgb[i] * a + p[i] * below * (1.0 - a)) / out;
                }
                p[3] = out;
            }
        }
    }
    // Premultiplied BGRA for UpdateLayeredWindow.
    let mut px = Vec::with_capacity(acc.len() * 4);
    for p in acc {
        let a = p[3];
        px.extend_from_slice(&[
            (p[2] * a * 255.0).round() as u8,
            (p[1] * a * 255.0).round() as u8,
            (p[0] * a * 255.0).round() as u8,
            (a * 255.0).round() as u8,
        ]);
    }
    (side, px)
}
