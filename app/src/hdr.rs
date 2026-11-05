//! HDR screens: Windows composes the desktop in scRGB (linear light, 1.0 =
//! 80 nits) and 8-bit capture of that looks washed out. Instead the screen is
//! captured as 16-bit float and turned into normal (SDR) colours here, on the
//! video card: scaled so that the "SDR content brightness" set in Windows
//! becomes white, highlights above it kept at full brightness in their hue,
//! then gamma-encoded like a normal picture.

use windows::core::{s, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

const SHADER: &str = r#"
cbuffer C : register(b0) { float scale; float3 pad; };
Texture2D<float4> src : register(t0);

float4 vs(uint id : SV_VertexID) : SV_Position {
    // One triangle that covers the whole target.
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}

float3 to_srgb(float3 c) {
    return c <= 0.0031308 ? c * 12.92 : 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

float4 ps(float4 pos : SV_Position) : SV_Target {
    float3 c = max(src.Load(int3(pos.xy, 0)).rgb * scale, 0);
    // Normal content (up to SDR white) stays exactly as it is; HDR
    // highlights brighter than that keep their hue at full brightness.
    float m = max(c.r, max(c.g, c.b));
    if (m > 1) {
        c /= m;
    }
    return float4(to_srgb(saturate(c)), 1);
}
"#;

fn err(what: &str) -> impl Fn(windows::core::Error) -> String + '_ {
    move |e| format!("{what}: {}", e.message())
}

unsafe fn compile(entry: PCSTR, target: PCSTR) -> Result<Vec<u8>, String> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let r = D3DCompile(SHADER.as_ptr() as *const _, SHADER.len(), PCSTR::null(), None, None, entry, target, 0, 0, &mut code, Some(&mut errors));
    if let Err(e) = r {
        let msg = errors
            .map(|b| String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())).into_owned())
            .unwrap_or_else(|| e.message());
        return Err(format!("Шейдер HDR: {msg}"));
    }
    let b = code.ok_or("Шейдер HDR: пусто")?;
    Ok(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec())
}

pub struct Tonemap {
    ctx: ID3D11DeviceContext,
    device: ID3D11Device,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cb: ID3D11Buffer,
    out: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    w: u32,
    h: u32,
    /// Shader view of the last source texture (the capture reuses one).
    srv: Option<(usize, ID3D11ShaderResourceView)>,
}

impl Tonemap {
    /// `white`: the scRGB value Windows uses for SDR white on this screen.
    pub fn new(device: &ID3D11Device, w: u32, h: u32, white: f32) -> Result<Self, String> {
        unsafe {
            let ctx = device.GetImmediateContext().map_err(err("Direct3D"))?;
            let (vsb, psb) = (compile(s!("vs"), s!("vs_5_0"))?, compile(s!("ps"), s!("ps_5_0"))?);
            let mut vs = None;
            device.CreateVertexShader(&vsb, None, Some(&mut vs)).map_err(err("Шейдер HDR"))?;
            let mut ps = None;
            device.CreatePixelShader(&psb, None, Some(&mut ps)).map_err(err("Шейдер HDR"))?;
            let consts = [1.0 / white.max(0.1), 0.0, 0.0, 0.0f32];
            let bd = D3D11_BUFFER_DESC {
                ByteWidth: 16,
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: consts.as_ptr() as *const _, ..Default::default() };
            let mut cb = None;
            device.CreateBuffer(&bd, Some(&init), Some(&mut cb)).map_err(err("Шейдер HDR"))?;
            let td = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut out = None;
            device.CreateTexture2D(&td, None, Some(&mut out)).map_err(err("Текстура"))?;
            let out = out.ok_or("Текстура: нет")?;
            let mut rtv = None;
            device.CreateRenderTargetView(&out, None, Some(&mut rtv)).map_err(err("Текстура"))?;
            Ok(Self {
                ctx,
                device: device.clone(),
                vs: vs.unwrap(),
                ps: ps.unwrap(),
                cb: cb.unwrap(),
                out,
                rtv: rtv.unwrap(),
                w,
                h,
                srv: None,
            })
        }
    }

    /// The HDR picture as a normal 8-bit BGRA one.
    pub fn run(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D, String> {
        unsafe {
            let key = windows::core::Interface::as_raw(src) as usize;
            if self.srv.as_ref().is_none_or(|(k, _)| *k != key) {
                let mut v = None;
                self.device.CreateShaderResourceView(src, None, Some(&mut v)).map_err(err("Текстура"))?;
                self.srv = Some((key, v.unwrap()));
            }
            let srv = self.srv.as_ref().unwrap().1.clone();
            let c = &self.ctx;
            c.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.IASetInputLayout(None);
            c.VSSetShader(&self.vs, None);
            c.PSSetShader(&self.ps, None);
            c.PSSetConstantBuffers(0, Some(&[Some(self.cb.clone())]));
            c.PSSetShaderResources(0, Some(&[Some(srv)]));
            c.OMSetRenderTargets(Some(&[Some(self.rtv.clone())]), None);
            c.RSSetViewports(Some(&[D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: self.w as f32, Height: self.h as f32, MinDepth: 0.0, MaxDepth: 1.0 }]));
            c.Draw(3, 0);
            // Unbind so the next capture can write the source again.
            c.PSSetShaderResources(0, Some(&[None]));
            c.OMSetRenderTargets(None, None);
            Ok(self.out.clone())
        }
    }
}

/// Is this texture a 16-bit float (HDR) capture?
pub fn is_float(t: &ID3D11Texture2D) -> bool {
    let mut d = D3D11_TEXTURE2D_DESC::default();
    unsafe { t.GetDesc(&mut d) };
    d.Format == DXGI_FORMAT_R16G16B16A16_FLOAT
}
