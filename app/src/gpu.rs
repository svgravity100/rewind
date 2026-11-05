//! MP4 recording without the CPU touching the picture: the captured screen
//! stays on the video card, the video card's own video processor scales it
//! and turns it into NV12, and the hardware H.264 encoder reads it from there.
//!
//! Only the small square under the mouse pointer goes through the CPU (the
//! pointer is not part of the captured picture and has to be drawn in).

use crate::capture::{self, Capturer, Pointer};
use crate::mf::{self, Mf};
use std::mem::ManuallyDrop;
use windows::core::Interface;
use windows::Win32::Foundation::BOOL;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;

fn err(what: &str) -> impl Fn(windows::core::Error) -> String + '_ {
    move |e| format!("{what}: {}", e.message())
}

/// Lets Media Foundation (the encoder) use the capture device's textures.
pub unsafe fn device_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager, String> {
    mf::startup()?;
    // The game comes first on the video card; our copies and conversions after it.
    if let Ok(d) = device.cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>() {
        let _ = d.SetGPUThreadPriority(-4);
    }
    // The encoder works on the device from its own threads.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        let _ = mt.SetMultithreadProtected(true);
    }
    let mut token = 0u32;
    let mut m = None;
    MFCreateDXGIDeviceManager(&mut token, &mut m).map_err(err("Видеокарта для кодировщика"))?;
    let m = m.ok_or("Видеокарта для кодировщика: нет менеджера")?;
    m.ResetDevice(device, token).map_err(err("Видеокарта для кодировщика"))?;
    Ok(m)
}

pub struct Converter {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    en: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
    pub in_w: u32,
    pub in_h: u32,
    out_w: u32,
    out_h: u32,
    out_bind: u32,
    /// Copy of the screen with the pointer drawn in.
    composed: ID3D11Texture2D,
    /// CPU-readable square for the area under the pointer.
    patch: Option<(ID3D11Texture2D, u32, u32)>,
    patch_px: Vec<u8>,
    /// HDR screen: turns the 16-bit float capture into normal colours first.
    tonemap: Option<crate::hdr::Tonemap>,
    /// scRGB value of SDR white on this screen (see `capture::sdr_white`).
    pub white: f32,
}

impl Converter {
    pub fn new(device: &ID3D11Device, in_w: u32, in_h: u32, out_w: u32, out_h: u32, fps: u32) -> Result<Self, String> {
        unsafe {
            let ctx = device.GetImmediateContext().map_err(err("Direct3D"))?;
            let vdev: ID3D11VideoDevice = device.cast().map_err(err("Видеопроцессор"))?;
            let vctx: ID3D11VideoContext = ctx.cast().map_err(err("Видеопроцессор"))?;
            let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: rate,
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let en = vdev.CreateVideoProcessorEnumerator(&desc).map_err(err("Видеопроцессор"))?;
            let support = en.CheckVideoProcessorFormat(DXGI_FORMAT_NV12).map_err(err("Видеопроцессор"))?;
            if support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
                return Err("видеопроцессор не выдаёт NV12".into());
            }
            let vp = vdev.CreateVideoProcessor(&en, 0).map_err(err("Видеопроцессор"))?;
            // Screen: full-range RGB. Video: BT.709, limited range (16–235),
            // the same as the CPU path writes.
            vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 });
            vctx.VideoProcessorSetOutputColorSpace(&vp, &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) });
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);

            let composed = texture(device, in_w, in_h, DXGI_FORMAT_B8G8R8A8_UNORM, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32)?;
            // Encoder input textures: marked for the encoder where the driver allows it.
            let rt = D3D11_BIND_RENDER_TARGET.0 as u32;
            let out_bind = if texture(device, out_w, out_h, DXGI_FORMAT_NV12, rt | D3D11_BIND_VIDEO_ENCODER.0 as u32).is_ok() {
                rt | D3D11_BIND_VIDEO_ENCODER.0 as u32
            } else {
                rt
            };
            Ok(Self {
                device: device.clone(),
                ctx,
                vdev,
                vctx,
                en,
                vp,
                in_w,
                in_h,
                out_w,
                out_h,
                out_bind,
                composed,
                patch: None,
                patch_px: Vec::new(),
                tonemap: None,
                white: 1.0,
            })
        }
    }

    /// The picture to encode: the screen itself, or a copy with the pointer drawn in.
    pub fn compose(&mut self, screen: &ID3D11Texture2D, pointer: Option<&Pointer>) -> ID3D11Texture2D {
        // HDR capture: tone-map to 8-bit first; everything after works on that.
        let sdr;
        let screen = if crate::hdr::is_float(screen) {
            if self.tonemap.is_none() {
                match crate::hdr::Tonemap::new(&self.device, self.in_w, self.in_h, self.white) {
                    Ok(t) => self.tonemap = Some(t),
                    Err(_) => return screen.clone(),
                }
            }
            match self.tonemap.as_mut().unwrap().run(screen) {
                Ok(t) => {
                    sdr = t;
                    &sdr
                }
                Err(_) => return screen.clone(),
            }
        } else {
            screen
        };
        let Some(p) = pointer.filter(|p| p.visible && !p.data.is_empty()) else { return screen.clone() };
        unsafe {
            let shape_h = if p.kind == 1 { p.h / 2 } else { p.h };
            let x0 = p.x.max(0) as u32;
            let y0 = p.y.max(0) as u32;
            let x1 = ((p.x + p.w as i32).max(0) as u32).min(self.in_w);
            let y1 = ((p.y + shape_h as i32).max(0) as u32).min(self.in_h);
            if x0 >= x1 || y0 >= y1 {
                return screen.clone();
            }
            let (pw, ph) = (x1 - x0, y1 - y0);
            if self.patch.as_ref().is_none_or(|(_, w, h)| *w < pw || *h < ph) {
                let (w, h) = (pw.max(64), ph.max(64));
                let mut d = desc(w, h, DXGI_FORMAT_B8G8R8A8_UNORM, 0);
                d.Usage = D3D11_USAGE_STAGING;
                d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                let mut t = None;
                if self.device.CreateTexture2D(&d, None, Some(&mut t)).is_err() {
                    return screen.clone();
                }
                self.patch = Some((t.unwrap(), w, h));
            }
            let staging = self.patch.as_ref().unwrap().0.clone();
            let bx = D3D11_BOX { left: x0, top: y0, front: 0, right: x1, bottom: y1, back: 1 };
            self.ctx.CopyResource(&self.composed, screen);
            self.ctx.CopySubresourceRegion(&staging, 0, 0, 0, 0, screen, 0, Some(&bx));
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            if self.ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).is_err() {
                return screen.clone();
            }
            let row = (pw * 4) as usize;
            self.patch_px.resize(row * ph as usize, 0);
            for y in 0..ph as usize {
                let s = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), row);
                self.patch_px[y * row..(y + 1) * row].copy_from_slice(s);
            }
            self.ctx.Unmap(&staging, 0);
            // Draw the pointer into the square, at its place inside it.
            let local = Pointer { visible: true, x: p.x - x0 as i32, y: p.y - y0 as i32, kind: p.kind, w: p.w, h: p.h, pitch: p.pitch, data: p.data.clone() };
            capture::draw_pointer(&local, pw, ph, &mut self.patch_px, pw, ph);
            self.ctx.UpdateSubresource(&self.composed, 0, Some(&bx), self.patch_px.as_ptr() as *const _, pw * 4, 0);
            self.composed.clone()
        }
    }

    /// Scale and convert to NV12 on the video card; the result is ready for the encoder.
    pub fn convert(&self, src: &ID3D11Texture2D) -> Result<Mf<IMFSample>, String> {
        unsafe {
            let out = texture(&self.device, self.out_w, self.out_h, DXGI_FORMAT_NV12, self.out_bind)?;
            let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut iv = None;
            self.vdev.CreateVideoProcessorInputView(src, &self.en, &ivd, Some(&mut iv)).map_err(err("Видеопроцессор"))?;
            let ovd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut ov = None;
            self.vdev.CreateVideoProcessorOutputView(&out, &self.en, &ovd, Some(&mut ov)).map_err(err("Видеопроцессор"))?;
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: BOOL::from(true),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: ManuallyDrop::new(iv),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };
            let r = self.vctx.VideoProcessorBlt(&self.vp, ov.as_ref(), 0, std::slice::from_ref(&stream));
            ManuallyDrop::drop(&mut stream.pInputSurface);
            r.map_err(err("Видеопроцессор"))?;
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &out, 0, false).map_err(err("Кадр"))?;
            if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                if let Ok(n) = b2.GetContiguousLength() {
                    let _ = buf.SetCurrentLength(n);
                }
            }
            let s = MFCreateSample().map_err(err("Кадр"))?;
            s.AddBuffer(&buf).map_err(err("Кадр"))?;
            Ok(Mf(s))
        }
    }
}

fn desc(w: u32, h: u32, format: DXGI_FORMAT, bind: u32) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    }
}

fn texture(device: &ID3D11Device, w: u32, h: u32, format: DXGI_FORMAT, bind: u32) -> Result<ID3D11Texture2D, String> {
    unsafe {
        let mut t = None;
        device.CreateTexture2D(&desc(w, h, format, bind), None, Some(&mut t)).map_err(err("Текстура"))?;
        t.ok_or_else(|| "Текстура: нет".to_string())
    }
}

/// Testing: encode a known picture (colour blocks and a pointer) both ways,
/// GPU and CPU, decode the files and compare the colours at known places.
pub fn color_test(monitor: &crate::capture::MonitorInfo, dir: &std::path::Path) -> Result<String, String> {
    let (w, h) = (640u32, 360u32);
    let cols: [(&str, [u8; 3]); 6] =
        [("red", [255, 0, 0]), ("green", [0, 200, 0]), ("blue", [0, 0, 255]), ("white", [255, 255, 255]), ("grey", [128, 128, 128]), ("nv-green", [118, 185, 0])];
    // Six blocks in a 3×2 grid, BGRA.
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = (y / (h / 2) * 3 + x / (w / 3)).min(5) as usize;
            let [r, g, b] = cols[i].1;
            let o = ((y * w + x) * 4) as usize;
            px[o..o + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    let center = |i: u32| ((i % 3) * (w / 3) + w / 6, (i / 3) * (h / 2) + h / 4);
    // A 16×16 magenta "colour" pointer, put inside the grey block, away from its centre.
    let pointer = Pointer { visible: true, x: 30, y: 200, kind: 2, w: 16, h: 16, pitch: 64, data: [255u8, 0, 255, 255].repeat(256) };
    let _ = std::fs::create_dir_all(dir);
    let fps = 30;
    let bitrate = 8_000_000;
    let cap = Capturer::new(monitor)?;
    let mgr = unsafe { device_manager(cap.device())? };
    let src = unsafe {
        let t = texture(cap.device(), w, h, DXGI_FORMAT_B8G8R8A8_UNORM, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32)?;
        cap.device().GetImmediateContext().map_err(err("Direct3D"))?.UpdateSubresource(&t, 0, None, px.as_ptr() as *const _, w * 4, 0);
        t
    };
    let mut conv = Converter::new(cap.device(), w, h, w, h, fps)?;
    let gpu_file = dir.join("color-gpu.mp4");
    let mut v = mf::VideoSegWriter::create_gpu(&gpu_file, w, h, fps, bitrate, crate::settings::Codec::H264, &mgr)?;
    for i in 0..30 {
        let composed = conv.compose(&src, Some(&pointer));
        v.write_sample(&conv.convert(&composed)?.0, i)?;
    }
    v.finish()?;
    let mut cpu_px = px.clone();
    capture::draw_pointer(&pointer, w, h, &mut cpu_px, w, h);
    let mut nv12 = Vec::new();
    mf::bgra_to_nv12(&cpu_px, w as usize, h as usize, &mut nv12);
    let cpu_file = dir.join("color-cpu.mp4");
    let mut v = mf::VideoSegWriter::create(&cpu_file, w, h, fps, bitrate, crate::settings::Codec::H264)?;
    for i in 0..30 {
        v.write(&nv12, i)?;
    }
    v.finish()?;
    // The same picture as an HDR screen shows it: linear light, SDR white at
    // 2.5 (200 nits). After tone-mapping it must look like the others.
    let white = 2.5f32;
    let lin = |c: u8| {
        let c = c as f32 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    let mut fp = Vec::with_capacity((w * h * 4) as usize);
    for p in px.chunks_exact(4) {
        for v in [lin(p[2]) * white, lin(p[1]) * white, lin(p[0]) * white, 1.0] {
            fp.push(f16_bits(v));
        }
    }
    let hdr_src = unsafe {
        let t = texture(cap.device(), w, h, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D11_BIND_SHADER_RESOURCE.0 as u32)?;
        cap.device().GetImmediateContext().map_err(err("Direct3D"))?.UpdateSubresource(&t, 0, None, fp.as_ptr() as *const _, w * 8, 0);
        t
    };
    let mut hconv = Converter::new(cap.device(), w, h, w, h, fps)?;
    hconv.white = white;
    let hdr_file = dir.join("color-hdr.mp4");
    let mut v = mf::VideoSegWriter::create_gpu(&hdr_file, w, h, fps, bitrate, crate::settings::Codec::H264, &mgr)?;
    for i in 0..30 {
        let composed = hconv.compose(&hdr_src, Some(&pointer));
        v.write_sample(&hconv.convert(&composed)?.0, i)?;
    }
    v.finish()?;
    let mut out = String::new();
    for (name, file) in [("gpu", &gpu_file), ("cpu", &cpu_file), ("hdr", &hdr_file)] {
        let (fw, _, rgb) = mf::last_frame_bgra(file)?;
        let at = |x: u32, y: u32| {
            let o = ((y * fw + x) * 4) as usize;
            [rgb[o + 2], rgb[o + 1], rgb[o]]
        };
        out.push_str(&format!("{name}:"));
        for (i, (cn, want)) in cols.iter().enumerate() {
            let (x, y) = center(i as u32);
            out.push_str(&format!(" {cn} {want:?}->{:?}", at(x, y)));
        }
        out.push_str(&format!(" pointer [255,0,255]->{:?}\n", at(38, 208)));
    }
    Ok(out)
}

/// f32 → IEEE half float bits (for building test pictures).
fn f16_bits(v: f32) -> u16 {
    let b = v.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32 - 127 + 15;
    let man = b & 0x7f_ffff;
    if exp <= 0 {
        if exp < -10 {
            return sign;
        }
        let m = (man | 0x80_0000) >> (1 - exp);
        return sign | ((m + 0x1000) >> 13) as u16;
    }
    if exp >= 31 {
        return sign | 0x7c00;
    }
    sign | ((exp as u32) << 10 | ((man + 0x1000) >> 13)) as u16
}

/// Check the whole GPU path on this PC: capture device, conversion, and the
/// hardware encoder taking video-card frames (writes a tiny file).
pub fn probe(monitor: &crate::capture::MonitorInfo, dir: &std::path::Path, w: u32, h: u32, fps: u32, bitrate: u32, codec: crate::settings::Codec) -> Result<(), String> {
    let cap = Capturer::new(monitor)?;
    let mgr = unsafe { device_manager(cap.device())? };
    let conv = Converter::new(cap.device(), monitor.width, monitor.height, w, h, fps)?;
    let blank = texture(cap.device(), monitor.width, monitor.height, DXGI_FORMAT_B8G8R8A8_UNORM, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32)?;
    let _ = std::fs::create_dir_all(dir);
    let p = dir.join("probe-gpu.mp4");
    let mut v = mf::VideoSegWriter::create_gpu(&p, w, h, fps, bitrate, codec, &mgr)?;
    for i in 0..3 {
        let s = conv.convert(&blank)?;
        v.write_sample(&s.0, i)?;
    }
    let r = v.finish();
    let ok = std::fs::metadata(&p).map(|m| m.len() > 0).unwrap_or(false);
    let _ = std::fs::remove_file(&p);
    r?;
    if ok {
        Ok(())
    } else {
        Err("пустой файл".into())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn half_floats() {
        assert_eq!(super::f16_bits(0.0), 0x0000);
        assert_eq!(super::f16_bits(1.0), 0x3c00);
        assert_eq!(super::f16_bits(2.5), 0x4100);
        assert_eq!(super::f16_bits(-2.0), 0xc000);
        assert_eq!(super::f16_bits(0.5), 0x3800);
    }
}
