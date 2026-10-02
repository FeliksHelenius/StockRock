//! The window/taskbar icon, rendered at runtime so no resource compiler is needed.
//!
//! A rounded dark tile with a green up-trending line and arrowhead, anti-aliased by 4x4
//! supersampling of simple geometric shapes.

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;

use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

type Point = (f64, f64);

const TILE: [u8; 3] = [0x1C, 0x20, 0x27];
const GREEN: [u8; 3] = [0x34, 0xD3, 0x99];
const LINE: [Point; 4] = [(0.18, 0.68), (0.38, 0.46), (0.54, 0.60), (0.80, 0.30)];
const ARROW: [Point; 3] = [(0.866, 0.224), (0.885, 0.400), (0.689, 0.230)];

pub fn create(size: i32) -> Option<HICON> {
    let size = size.clamp(16, 256);
    let pixels = render(size as usize);
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    unsafe {
        let mut bits: *mut c_void = null_mut();
        let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        std::slice::from_raw_parts_mut(bits as *mut u32, pixels.len()).copy_from_slice(&pixels);
        // With a 32-bit color bitmap the mask is ignored, but the API still requires one.
        let mask = CreateBitmap(size, size, 1, 1, None);
        let icon = CreateIconIndirect(&ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        });
        let _ = DeleteObject(mask.into());
        let _ = DeleteObject(color.into());
        icon.ok()
    }
}

/// Premultiplied BGRA pixels, one `u32` each.
fn render(size: usize) -> Vec<u32> {
    const N: usize = 4;
    let mut out = vec![0u32; size * size];
    for y in 0..size {
        for x in 0..size {
            let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
            for sy in 0..N {
                for sx in 0..N {
                    let p = (
                        (x as f64 + (sx as f64 + 0.5) / N as f64) / size as f64,
                        (y as f64 + (sy as f64 + 0.5) / N as f64) / size as f64,
                    );
                    let color = if in_trend(p) {
                        GREEN
                    } else if in_tile(p) {
                        TILE
                    } else {
                        continue;
                    };
                    r += color[0] as u32;
                    g += color[1] as u32;
                    b += color[2] as u32;
                    a += 255;
                }
            }
            let samples = (N * N) as u32;
            let (r, g, b, a) = (r / samples, g / samples, b / samples, a / samples);
            out[y * size + x] = a << 24 | r << 16 | g << 8 | b;
        }
    }
    out
}

fn in_tile((x, y): Point) -> bool {
    const RADIUS: f64 = 0.22;
    // Distance from the nearest corner-circle center, only relevant in the corner regions.
    let dx = (x - 0.5).abs() - (0.5 - RADIUS);
    let dy = (y - 0.5).abs() - (0.5 - RADIUS);
    dx.max(0.0).hypot(dy.max(0.0)) <= RADIUS
}

fn in_trend(p: Point) -> bool {
    const HALF_WIDTH: f64 = 0.055;
    LINE.windows(2)
        .any(|s| distance_to_segment(p, s[0], s[1]) <= HALF_WIDTH)
        || in_triangle(p, ARROW)
}

fn distance_to_segment(p: Point, a: Point, b: Point) -> f64 {
    let (abx, aby) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * abx + (p.1 - a.1) * aby) / (abx * abx + aby * aby)).clamp(0.0, 1.0);
    (p.0 - (a.0 + t * abx)).hypot(p.1 - (a.1 + t * aby))
}

fn in_triangle(p: Point, t: [Point; 3]) -> bool {
    let side = |a: Point, b: Point| (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
    let (d1, d2, d3) = (side(t[0], t[1]), side(t[1], t[2]), side(t[2], t[0]));
    !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_are_transparent_and_center_is_opaque() {
        let size = 32;
        let pixels = render(size);
        assert_eq!(pixels[0] >> 24, 0, "top-left corner should be transparent");
        assert_eq!(
            pixels[size * size - 1] >> 24,
            0,
            "bottom-right corner should be transparent"
        );
        assert_eq!(
            pixels[size / 2 * size + size / 2] >> 24,
            255,
            "center should be opaque"
        );
    }

    #[test]
    fn trend_line_is_green_on_a_dark_tile() {
        let size = 64;
        let pixels = render(size);
        // A point on the first line segment, and one in an empty part of the tile.
        let on_line = pixels[(0.57 * size as f64) as usize * size + (0.28 * size as f64) as usize];
        let off_line = pixels[(0.90 * size as f64) as usize * size + (0.50 * size as f64) as usize];
        assert_eq!(on_line & 0xFFFFFF, 0x34D399);
        assert_eq!(off_line & 0xFFFFFF, 0x1C2027);
    }
}
