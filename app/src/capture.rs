//! Screen capture through DXGI Desktop Duplication (works for windowed and
//! borderless games and the normal desktop; uses the GPU copy path).

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;

#[derive(Clone, Debug, PartialEq)]
pub struct MonitorInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Top-left corner on the desktop, in physical pixels.
    pub x: i32,
    pub y: i32,
    pub adapter: u32,
    pub output: u32,
    /// Screen refresh rate in Hz (0 if unknown).
    pub hz: u32,
}

impl MonitorInfo {
    pub fn label(&self, idx: usize) -> String {
        if self.hz > 0 {
            format!("{}: {}×{} · {} Гц", idx + 1, self.width, self.height, self.hz)
        } else {
            format!("{}: {}×{}", idx + 1, self.width, self.height)
        }
    }
}

pub fn list_monitors() -> Vec<MonitorInfo> {
    let mut out = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return out;
        };
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                if let Ok(desc) = output.GetDesc() {
                    if desc.AttachedToDesktop.as_bool() {
                        let r = desc.DesktopCoordinates;
                        let len = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(32);
                        let hz = refresh_rate(&desc.DeviceName[..len]);
                        out.push(MonitorInfo {
                            name: String::from_utf16_lossy(&desc.DeviceName[..len]),
                            width: (r.right - r.left).unsigned_abs(),
                            height: (r.bottom - r.top).unsigned_abs(),
                            x: r.left,
                            y: r.top,
                            adapter: a,
                            output: o,
                            hz,
                        });
                    }
                }
                o += 1;
            }
            a += 1;
        }
    }
    out
}

#[derive(Default)]
pub struct Pointer {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    /// DXGI_OUTDUPL_POINTER_SHAPE_TYPE: 1 monochrome, 2 color, 4 masked color.
    pub kind: u32,
    pub w: u32,
    pub h: u32,
    pub pitch: u32,
    pub data: Vec<u8>,
}

pub struct Capturer {
    pub pointer: Pointer,
    monitor: MonitorInfo,
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dup: Option<IDXGIOutputDuplication>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// Last captured picture, BGRA, tightly packed (CPU mode).
    pub frame: Vec<u8>,
    /// Last captured picture, kept on the video card (GPU mode).
    latest: Option<ID3D11Texture2D>,
    /// GPU mode: pictures stay on the video card, `frame` is not filled.
    pub gpu: bool,
    has_frame: bool,
    pub width: u32,
    pub height: u32,
}

fn err(what: &str, e: windows::core::Error) -> String {
    format!("{what}: {}", e.message())
}

impl Capturer {
    pub fn new(monitor: &MonitorInfo) -> Result<Self, String> {
        unsafe {
            let factory = CreateDXGIFactory1::<IDXGIFactory1>().map_err(|e| err("DXGI", e))?;
            let adapter = factory
                .EnumAdapters1(monitor.adapter)
                .map_err(|e| err("Видеокарта", e))?;
            let mut device = None;
            let mut ctx = None;
            // Video support lets the video card's video processor be used (GPU mode).
            let create = |flags: D3D11_CREATE_DEVICE_FLAG, device: &mut Option<ID3D11Device>, ctx: &mut Option<ID3D11DeviceContext>| {
                D3D11CreateDevice(&adapter, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(), flags, None, D3D11_SDK_VERSION, Some(device), None, Some(ctx))
            };
            if create(D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT, &mut device, &mut ctx).is_err() {
                create(D3D11_CREATE_DEVICE_BGRA_SUPPORT, &mut device, &mut ctx).map_err(|e| err("Direct3D", e))?;
            }
            let mut c = Capturer {
                pointer: Pointer::default(),
                monitor: monitor.clone(),
                device: device.ok_or("Direct3D: нет устройства")?,
                ctx: ctx.ok_or("Direct3D: нет контекста")?,
                dup: None,
                staging: None,
                frame: Vec::new(),
                latest: None,
                gpu: false,
                has_frame: false,
                width: 0,
                height: 0,
            };
            c.reopen()?;
            Ok(c)
        }
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// GPU mode: the last captured picture (without the pointer).
    pub fn latest(&self) -> Option<&ID3D11Texture2D> {
        self.latest.as_ref()
    }

    fn reopen(&mut self) -> Result<(), String> {
        self.dup = None;
        unsafe {
            let factory = CreateDXGIFactory1::<IDXGIFactory1>().map_err(|e| err("DXGI", e))?;
            let adapter = factory
                .EnumAdapters1(self.monitor.adapter)
                .map_err(|e| err("Видеокарта", e))?;
            let output = adapter
                .EnumOutputs(self.monitor.output)
                .map_err(|e| err("Экран", e))?;
            let output1: IDXGIOutput1 = output.cast().map_err(|e| err("Экран", e))?;
            let dup = output1
                .DuplicateOutput(&self.device)
                .map_err(|e| err("Захват экрана", e))?;
            self.dup = Some(dup);
        }
        Ok(())
    }

    /// Try to get a new picture. Ok(true) when `frame` was updated.
    pub fn grab(&mut self, timeout_ms: u32) -> Result<bool, String> {
        if self.dup.is_none() {
            self.reopen()?;
        }
        let dup = self.dup.clone().unwrap();
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            if let Err(e) = dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) {
                if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    return Ok(false);
                }
                if e.code() == DXGI_ERROR_ACCESS_LOST {
                    // Mode change, UAC prompt, fullscreen switch: reopen next time.
                    self.dup = None;
                    return Ok(false);
                }
                self.dup = None;
                return Err(err("Захват экрана", e));
            }
            let mut pointer_changed = false;
            if info.LastMouseUpdateTime != 0 {
                let p = &mut self.pointer;
                p.visible = info.PointerPosition.Visible.as_bool();
                p.x = info.PointerPosition.Position.x;
                p.y = info.PointerPosition.Position.y;
                pointer_changed = true;
            }
            if info.PointerShapeBufferSize > 0 {
                let mut buf = vec![0u8; info.PointerShapeBufferSize as usize];
                let mut need = 0u32;
                let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
                if dup
                    .GetFramePointerShape(buf.len() as u32, buf.as_mut_ptr() as *mut _, &mut need, &mut si)
                    .is_ok()
                {
                    let p = &mut self.pointer;
                    p.kind = si.Type;
                    p.w = si.Width;
                    p.h = si.Height;
                    p.pitch = si.Pitch;
                    p.data = buf;
                    pointer_changed = true;
                }
            }
            let result = self.copy_out(&info, res);
            let _ = dup.ReleaseFrame();
            result.map(|f| f || (pointer_changed && self.has_frame))
        }
    }

    unsafe fn copy_out(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        res: Option<IDXGIResource>,
    ) -> Result<bool, String> {
        // Only the cursor moved: the picture itself is unchanged.
        if info.LastPresentTime == 0 && self.has_frame {
            return Ok(false);
        }
        let Some(res) = res else { return Ok(false) };
        let tex: ID3D11Texture2D = res.cast().map_err(|e| err("Текстура", e))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut desc);
        let (w, h) = (desc.Width, desc.Height);
        if self.gpu {
            // Keep a copy on the video card; the duplication frame goes back to Windows.
            if self.latest.is_none() || self.width != w || self.height != h {
                let mut d = desc;
                d.Usage = D3D11_USAGE_DEFAULT;
                d.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
                d.CPUAccessFlags = 0;
                d.MiscFlags = 0;
                d.MipLevels = 1;
                d.ArraySize = 1;
                d.SampleDesc.Count = 1;
                d.SampleDesc.Quality = 0;
                let mut t = None;
                self.device.CreateTexture2D(&d, None, Some(&mut t)).map_err(|e| err("Текстура", e))?;
                self.latest = t;
            }
            self.ctx.CopyResource(self.latest.as_ref().unwrap(), &tex);
            self.width = w;
            self.height = h;
            self.has_frame = true;
            return Ok(true);
        }
        let need_new = match &self.staging {
            Some((_, sw, sh)) => *sw != w || *sh != h,
            None => true,
        };
        if need_new {
            let mut sd = desc;
            sd.Usage = D3D11_USAGE_STAGING;
            sd.BindFlags = 0;
            sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            sd.MiscFlags = 0;
            sd.MipLevels = 1;
            sd.ArraySize = 1;
            sd.SampleDesc.Count = 1;
            sd.SampleDesc.Quality = 0;
            let mut st = None;
            self.device
                .CreateTexture2D(&sd, None, Some(&mut st))
                .map_err(|e| err("Текстура", e))?;
            self.staging = Some((st.unwrap(), w, h));
        }
        let staging = self.staging.as_ref().unwrap().0.clone();
        self.ctx.CopyResource(&staging, &tex);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        self.ctx
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| err("Чтение кадра", e))?;
        let row = (w * 4) as usize;
        self.frame.resize(row * h as usize, 0);
        let src = mapped.pData as *const u8;
        for y in 0..h as usize {
            let s = std::slice::from_raw_parts(src.add(y * mapped.RowPitch as usize), row);
            self.frame[y * row..(y + 1) * row].copy_from_slice(s);
        }
        self.ctx.Unmap(&staging, 0);
        self.width = w;
        self.height = h;
        self.has_frame = true;
        Ok(true)
    }
}

/// `scale_bgra` split over several threads by bands of output rows.
pub fn scale_bgra_par(src: &[u8], sw: u32, sh: u32, dst: &mut [u8], dw: u32, dh: u32) {
    let (swu, shu, dwu, dhu) = (sw as usize, sh as usize, dw as usize, dh as usize);
    if src.len() < swu * shu * 4 || dst.len() < dwu * dhu * 4 {
        return;
    }
    let bands = 4usize;
    let rows_per = dhu.div_ceil(bands).max(1);
    std::thread::scope(|s| {
        for (i, band) in dst[..dwu * dhu * 4].chunks_mut(rows_per * dwu * 4).enumerate() {
            s.spawn(move || {
                let y0 = i * rows_per;
                let n = band.len() / (dwu * 4);
                if sw == dw && sh == dh {
                    band.copy_from_slice(&src[y0 * swu * 4..(y0 + n) * swu * 4]);
                    return;
                }
                let xs: Vec<(usize, usize)> = (0..dwu)
                    .map(|x| {
                        let fx = ((2 * x + 1) * swu) / (2 * dwu);
                        (fx.min(swu - 1) * 4, (fx + 1).min(swu - 1) * 4)
                    })
                    .collect();
                for yy in 0..n {
                    let y = y0 + yy;
                    let fy = ((2 * y + 1) * shu) / (2 * dhu);
                    let r0 = &src[fy.min(shu - 1) * swu * 4..][..swu * 4];
                    let r1 = &src[(fy + 1).min(shu - 1) * swu * 4..][..swu * 4];
                    let d = &mut band[yy * dwu * 4..][..dwu * 4];
                    for (x, &(a, b)) in xs.iter().enumerate() {
                        for c in 0..3 {
                            let v = r0[a + c] as u16 + r0[b + c] as u16 + r1[a + c] as u16 + r1[b + c] as u16;
                            d[x * 4 + c] = (v / 4) as u8;
                        }
                        d[x * 4 + 3] = 255;
                    }
                }
            });
        }
    });
}

/// Draw the mouse pointer onto a picture that was scaled from `sw`×`sh` to `dw`×`dh`.
pub fn draw_pointer(p: &Pointer, sw: u32, sh: u32, dst: &mut [u8], dw: u32, dh: u32) {
    if !p.visible || p.data.is_empty() || sw == 0 || sh == 0 {
        return;
    }
    let shape_h = if p.kind == 1 { p.h / 2 } else { p.h };
    let (fx, fy) = (dw as f32 / sw as f32, dh as f32 / sh as f32);
    let x0 = (p.x as f32 * fx).floor() as i32;
    let y0 = (p.y as f32 * fy).floor() as i32;
    let ow = ((p.w as f32 * fx).ceil() as i32).max(1);
    let oh = ((shape_h as f32 * fy).ceil() as i32).max(1);
    for oy in 0..oh {
        let ty = y0 + oy;
        if ty < 0 || ty >= dh as i32 {
            continue;
        }
        let sy = ((oy as f32 / fy) as u32).min(shape_h - 1) as usize;
        for ox in 0..ow {
            let tx = x0 + ox;
            if tx < 0 || tx >= dw as i32 {
                continue;
            }
            let sx = ((ox as f32 / fx) as u32).min(p.w - 1) as usize;
            let d = (ty as usize * dw as usize + tx as usize) * 4;
            let px = &mut dst[d..d + 3];
            match p.kind {
                1 => {
                    let pitch = p.pitch as usize;
                    let bit = 0x80u8 >> (sx % 8);
                    let and = p.data[sy * pitch + sx / 8] & bit != 0;
                    let xor = p.data[(sy + shape_h as usize) * pitch + sx / 8] & bit != 0;
                    match (and, xor) {
                        (false, false) => px.copy_from_slice(&[0, 0, 0]),
                        (false, true) => px.copy_from_slice(&[255, 255, 255]),
                        (true, true) => px.iter_mut().for_each(|c| *c = 255 - *c),
                        (true, false) => {}
                    }
                }
                2 => {
                    let s = &p.data[sy * p.pitch as usize + sx * 4..][..4];
                    let a = s[3] as u32;
                    for c in 0..3 {
                        px[c] = ((s[c] as u32 * a + px[c] as u32 * (255 - a)) / 255) as u8;
                    }
                }
                4 => {
                    let s = &p.data[sy * p.pitch as usize + sx * 4..][..4];
                    if s[3] == 0 {
                        px.copy_from_slice(&s[..3]);
                    } else {
                        for c in 0..3 {
                            px[c] ^= s[c];
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

pub fn save_png(bgra: &[u8], w: u32, h: u32, path: &std::path::Path) -> Result<(), String> {
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for px in bgra.chunks_exact(4) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    image::save_buffer(path, &rgb, w, h, image::ExtendedColorType::Rgb8).map_err(|e| e.to_string())
}

fn refresh_rate(device: &[u16]) -> u32 {
    use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
    let mut name: Vec<u16> = device.to_vec();
    name.push(0);
    let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    unsafe {
        if EnumDisplaySettingsW(windows::core::PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm).as_bool() {
            dm.dmDisplayFrequency
        } else {
            0
        }
    }
}
