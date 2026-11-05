//! The app's own icon, drawn in code (no image files): a dark rounded square,
//! a green "replay" arrow going round, and a dot in the middle.
//!
//! Shared by build.rs (writes the .ico embedded in the .exe) and the app (tray
//! icon, console window icon), so all of them look the same.

/// Dot colours: red = recording / app icon, light = buffer off (tray).
pub const DOT_REC: [u8; 3] = [0xff, 0x4d, 0x5e];
#[allow(dead_code)] // used by the app, not build.rs
pub const DOT_IDLE: [u8; 3] = [0xe8, 0xf2, 0xdc];

const GREEN: [f32; 3] = [118.0, 185.0, 0.0];
const BG_TOP: [f32; 3] = [24.0, 38.0, 14.0];
const BG_BOTTOM: [f32; 3] = [6.0, 10.0, 4.0];

/// Straight-alpha RGBA pixels, `size`×`size`.
pub fn render(size: u32, dot: [u8; 3]) -> Vec<u8> {
    let n = size as f32;
    let c = n / 2.0;
    // Arc: from START going clockwise round to END; the gap between them is
    // at the top right, the arrowhead sits at START pointing into the gap.
    let (start, end) = (-20f32.to_radians(), 280f32.to_radians());
    let ring_r = 0.29 * n;
    let ring_w = 0.10 * n;
    let dot_r = 0.115 * n;
    // Arrowhead: base across the arc at START, tip along the anticlockwise tangent.
    let p = (c + ring_r * start.cos(), c + ring_r * start.sin());
    let dir = (start.sin(), -start.cos());
    let side = (-dir.1, dir.0);
    let (half, len) = (0.135 * n, 0.15 * n);
    let tri = [
        (p.0 + side.0 * half, p.1 + side.1 * half),
        (p.0 - side.0 * half, p.1 - side.1 * half),
        (p.0 + dir.0 * len, p.1 + dir.1 * len),
    ];
    let in_tri = |x: f32, y: f32| {
        let s = |a: (f32, f32), b: (f32, f32)| (b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0);
        let (d1, d2, d3) = (s(tri[0], tri[1]), s(tri[1], tri[2]), s(tri[2], tri[0]));
        !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
    };
    let radius = 0.22 * n;
    let in_bg = |x: f32, y: f32| {
        let qx = (x - c).abs() - (c - radius);
        let qy = (y - c).abs() - (c - radius);
        let out = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
        out + qx.max(qy).min(0.0) <= radius
    };
    let in_arc = |x: f32, y: f32| {
        let (dx, dy) = (x - c, y - c);
        let d = (dx * dx + dy * dy).sqrt();
        if (d - ring_r).abs() > ring_w / 2.0 {
            return false;
        }
        let mut a = dy.atan2(dx);
        if a < start {
            a += std::f32::consts::TAU;
        }
        a <= end
    };
    let in_dot = |x: f32, y: f32| (x - c).powi(2) + (y - c).powi(2) <= dot_r * dot_r;

    // 4×4 supersampling for smooth edges at small sizes.
    const SS: usize = 4;
    let dotc = [dot[0] as f32, dot[1] as f32, dot[2] as f32];
    let mut px = vec![0u8; (size * size * 4) as usize];
    for py in 0..size {
        for pxi in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = pxi as f32 + (sx as f32 + 0.5) / SS as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                    if !in_bg(x, y) {
                        continue;
                    }
                    let t = y / n;
                    let mut col = [0.0; 3];
                    for i in 0..3 {
                        col[i] = BG_TOP[i] + (BG_BOTTOM[i] - BG_TOP[i]) * t;
                    }
                    if in_arc(x, y) || in_tri(x, y) {
                        col = GREEN;
                    }
                    if in_dot(x, y) {
                        col = dotc;
                    }
                    for i in 0..3 {
                        acc[i] += col[i];
                    }
                    acc[3] += 1.0;
                }
            }
            let i = ((py * size + pxi) * 4) as usize;
            if acc[3] > 0.0 {
                for k in 0..3 {
                    px[i + k] = (acc[k] / acc[3]).round() as u8;
                }
                px[i + 3] = (acc[3] / (SS * SS) as f32 * 255.0).round() as u8;
            }
        }
    }
    px
}

/// A complete .ico file with the given sizes (32-bit DIB entries).
#[allow(dead_code)] // used by build.rs
pub fn ico(sizes: &[u32], dot: [u8; 3]) -> Vec<u8> {
    let mut images = Vec::new();
    for &s in sizes {
        let rgba = render(s, dot);
        let mut d = Vec::new();
        // BITMAPINFOHEADER; height counts the colour rows plus the AND mask.
        for v in [40u32, s, s * 2] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&32u16.to_le_bytes());
        d.extend_from_slice(&[0u8; 24]);
        // Pixels bottom-up, BGRA.
        for y in (0..s).rev() {
            for x in 0..s {
                let i = ((y * s + x) * 4) as usize;
                d.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
            }
        }
        // AND mask (unused with alpha, all zero), rows padded to 32 bits.
        let row = (s.div_ceil(32) * 4) as usize;
        d.extend(std::iter::repeat_n(0u8, row * s as usize));
        images.push((s, d));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (s, d) in &images {
        let b = if *s >= 256 { 0 } else { *s as u8 };
        out.extend_from_slice(&[b, b, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(d.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += d.len() as u32;
    }
    for (_, d) in images {
        out.extend_from_slice(&d);
    }
    out
}
