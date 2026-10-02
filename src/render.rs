//! GDI renderer for the ticker tape.
//!
//! The whole tape is laid out and drawn once into an off-screen bitmap whenever the data, window
//! size or DPI changes. Scrolling is then a single `BitBlt` per frame: no text layout, no
//! allocation, no GPU driver. When the tape is wider than the window the bitmap carries one extra
//! window-width of repeated items, so wrapping around is seamless without a second blit.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;

use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::core::{PCWSTR, w};

use crate::worker::Entry;

pub enum Content<'a> {
    Quotes {
        entries: &'a [Entry],
        /// Display names by symbol; a symbol without one is drawn as it is.
        names: &'a BTreeMap<String, String>,
    },
    /// A status line shown in place of quotes (loading, errors).
    Message { text: &'a str, error: bool },
}

pub const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

struct Palette {
    bg: COLORREF,
    symbol: COLORREF,
    price: COLORREF,
    up: COLORREF,
    down: COLORREF,
    flat: COLORREF,
    dim: COLORREF,
    sep: COLORREF,
    warn: COLORREF,
}

const PALETTE: Palette = Palette {
    bg: rgb(0x14, 0x16, 0x1A),
    symbol: rgb(0xF3, 0xF5, 0xF8),
    price: rgb(0xC8, 0xCF, 0xDA),
    up: rgb(0x34, 0xD3, 0x99),
    down: rgb(0xF8, 0x71, 0x71),
    flat: rgb(0x8B, 0x95, 0xA5),
    dim: rgb(0x6B, 0x72, 0x80),
    sep: rgb(0x36, 0x3B, 0x46),
    warn: rgb(0xFB, 0xBF, 0x24),
};

struct Fonts {
    symbol: HFONT,
    price: HFONT,
    delta: HFONT,
    arrow: HFONT,
}

impl Fonts {
    fn new(px: i32) -> Self {
        Self {
            symbol: make_font(w!("Segoe UI"), px, FW_BOLD.0 as i32),
            price: make_font(w!("Segoe UI"), px, FW_SEMIBOLD.0 as i32),
            delta: make_font(w!("Segoe UI"), px, FW_NORMAL.0 as i32),
            arrow: make_font(w!("Segoe UI Symbol"), px * 7 / 10, FW_NORMAL.0 as i32),
        }
    }

    fn delete(&self) {
        for font in [self.symbol, self.price, self.delta, self.arrow] {
            unsafe {
                let _ = DeleteObject(font.into());
            }
        }
    }
}

fn make_font(face: PCWSTR, px: i32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -px,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_TT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            face,
        )
    }
}

/// A piece of text with one font and color, plus spacing to the next run.
struct Run {
    text: Vec<u16>,
    font: HFONT,
    color: COLORREF,
    width: i32,
    gap_after: i32,
    dy: i32,
}

/// One symbol's worth of runs, drawn left to right.
struct Item {
    runs: Vec<Run>,
    width: i32,
}

impl Item {
    fn new(runs: Vec<Run>) -> Self {
        let width = runs.iter().map(|r| r.width + r.gap_after).sum();
        Self { runs, width }
    }
}

struct Strip {
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    width: i32,
}

#[derive(Clone, Copy)]
enum Mode {
    /// Everything fits: draw the strip centered, nothing moves.
    Static,
    /// Wider than the window: scroll through a loop of `period` pixels.
    Cyclic { period: i32 },
}

pub struct Renderer {
    dc: HDC,
    fonts: Fonts,
    bg: HBRUSH,
    dpi: u32,
    px: i32,
    strip: Option<Strip>,
    mode: Mode,
}

impl Renderer {
    pub fn new(dpi: u32, font_pt: f64) -> Self {
        let px = font_px(dpi, font_pt);
        unsafe {
            Self {
                dc: CreateCompatibleDC(None),
                fonts: Fonts::new(px),
                bg: CreateSolidBrush(PALETTE.bg),
                dpi,
                px,
                strip: None,
                mode: Mode::Static,
            }
        }
    }

    /// Recreates fonts for a new DPI or font size. Call `build` afterwards.
    pub fn set_metrics(&mut self, dpi: u32, font_pt: f64) {
        self.release_strip();
        self.release_fonts();
        self.dpi = dpi;
        self.px = font_px(dpi, font_pt);
        self.fonts = Fonts::new(self.px);
    }

    /// Height of the ticker bar in physical pixels.
    pub fn bar_height(&self) -> i32 {
        self.px + 2 * self.scale(11)
    }

    pub fn scrolls(&self) -> bool {
        matches!(self.mode, Mode::Cyclic { .. })
    }

    /// Length of one scroll loop in pixels, or 0 when the tape is static.
    pub fn period(&self) -> i32 {
        match self.mode {
            Mode::Cyclic { period } => period,
            Mode::Static => 0,
        }
    }

    fn scale(&self, dip: i32) -> i32 {
        (dip * self.dpi as i32 + 48) / 96
    }

    pub fn build(&mut self, content: Content, view_w: i32, view_h: i32) {
        self.release_strip();
        self.mode = Mode::Static;
        if view_w <= 0 || view_h <= 0 {
            return;
        }
        let items = self.layout(content);
        if items.is_empty() {
            return;
        }

        let gap = self.scale(36);
        let total = items.iter().map(|i| i.width).sum::<i32>() + gap * (items.len() as i32 - 1);
        let (strip_w, mode) = if total > view_w {
            let period = total + gap;
            (period + view_w, Mode::Cyclic { period })
        } else {
            (total.max(1), Mode::Static)
        };

        if self.create_strip(strip_w, view_h) {
            self.mode = mode;
            self.paint_strip(
                &items,
                strip_w,
                view_h,
                gap,
                matches!(mode, Mode::Cyclic { .. }),
            );
        }
    }

    /// Draws the current frame into `dst`. `offset` is the scroll position in pixels.
    pub fn blit(&self, dst: HDC, view_w: i32, view_h: i32, offset: i32) {
        unsafe {
            let Some(strip) = &self.strip else {
                self.fill(dst, 0, view_w, view_h);
                return;
            };
            match self.mode {
                Mode::Cyclic { period } => {
                    let _ = BitBlt(
                        dst,
                        0,
                        0,
                        view_w,
                        view_h,
                        Some(self.dc),
                        offset.rem_euclid(period),
                        0,
                        SRCCOPY,
                    );
                }
                Mode::Static => {
                    let x = ((view_w - strip.width) / 2).max(0);
                    self.fill(dst, 0, x, view_h);
                    let _ = BitBlt(
                        dst,
                        x,
                        0,
                        strip.width.min(view_w),
                        view_h,
                        Some(self.dc),
                        0,
                        0,
                        SRCCOPY,
                    );
                    self.fill(dst, x + strip.width, view_w, view_h);
                }
            }
        }
    }

    fn fill(&self, dc: HDC, left: i32, right: i32, height: i32) {
        if right > left {
            let rect = RECT {
                left,
                top: 0,
                right,
                bottom: height,
            };
            unsafe {
                FillRect(dc, &rect, self.bg);
            }
        }
    }

    fn layout(&self, content: Content) -> Vec<Item> {
        match content {
            Content::Quotes { entries, names } => entries
                .iter()
                .map(|e| self.quote_item(e, names.get(&e.symbol).unwrap_or(&e.symbol)))
                .collect(),
            Content::Message { text, error } => {
                let color = if error { PALETTE.warn } else { PALETTE.dim };
                vec![Item::new(vec![self.run(
                    text,
                    self.fonts.delta,
                    color,
                    0,
                    0,
                )])]
            }
        }
    }

    fn quote_item(&self, entry: &Entry, label: &str) -> Item {
        let gap = self.scale(10);
        let symbol = self.run(label, self.fonts.symbol, PALETTE.symbol, gap, 0);
        let Some(quote) = &entry.quote else {
            return Item::new(vec![
                symbol,
                self.run("\u{2014}", self.fonts.delta, PALETTE.dim, 0, 0),
            ]);
        };

        let decimals = decimals(quote.price);
        let (Some(change), Some(change_pct)) = (quote.change(), quote.change_pct()) else {
            // No previous close from this source: show the price alone.
            let price_color = if entry.stale {
                PALETTE.dim
            } else {
                PALETTE.price
            };
            return Item::new(vec![
                symbol,
                self.run(
                    &group(quote.price, decimals),
                    self.fonts.price,
                    price_color,
                    0,
                    0,
                ),
            ]);
        };
        let (arrow, mut color) = match (change * 10f64.powi(decimals as i32)).round() {
            r if r > 0.0 => ("\u{25B2}", PALETTE.up),
            r if r < 0.0 => ("\u{25BC}", PALETTE.down),
            _ => ("\u{2013}", PALETTE.flat),
        };
        let mut price_color = PALETTE.price;
        if entry.stale {
            // Last refresh failed: drain the color so old data doesn't pass for live data.
            color = PALETTE.dim;
            price_color = PALETTE.dim;
        }
        let delta = format!(
            "{} ({:.2}%)",
            group(change.abs(), decimals),
            change_pct.abs()
        );

        Item::new(vec![
            symbol,
            self.run(
                &group(quote.price, decimals),
                self.fonts.price,
                price_color,
                gap,
                0,
            ),
            // The triangle glyph sits on the baseline; lift it so it's centered on the digits.
            self.run(
                arrow,
                self.fonts.arrow,
                color,
                self.scale(5),
                -(self.px * 13 + 50) / 100,
            ),
            self.run(&delta, self.fonts.delta, color, 0, 0),
        ])
    }

    fn run(&self, text: &str, font: HFONT, color: COLORREF, gap_after: i32, dy: i32) -> Run {
        let text: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE::default();
        unsafe {
            SelectObject(self.dc, font.into());
            let _ = GetTextExtentPoint32W(self.dc, &text, &mut size);
        }
        Run {
            text,
            font,
            color,
            width: size.cx,
            gap_after,
            dy,
        }
    }

    fn create_strip(&mut self, width: i32, height: i32) -> bool {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = null_mut();
        unsafe {
            match CreateDIBSection(Some(self.dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(bitmap) => {
                    let previous = SelectObject(self.dc, bitmap.into());
                    self.strip = Some(Strip {
                        bitmap,
                        previous,
                        width,
                    });
                    true
                }
                Err(_) => false,
            }
        }
    }

    fn paint_strip(&self, items: &[Item], strip_w: i32, view_h: i32, gap: i32, cyclic: bool) {
        // Digits are cap-height tall (~0.7 em); put their vertical center mid-bar.
        let baseline = (view_h + (self.px as f64 * 0.7).round() as i32) / 2;
        let sep_w = self.scale(1).max(1);
        let (sep_top, sep_bottom) = (view_h * 3 / 10, view_h * 7 / 10);

        unsafe {
            self.fill(self.dc, 0, strip_w, view_h);
            SetBkMode(self.dc, TRANSPARENT);
            SetTextAlign(self.dc, TA_BASELINE | TA_LEFT);
            let sep = CreateSolidBrush(PALETTE.sep);

            let (mut x, mut index) = (0, 0);
            while x < strip_w {
                let item = &items[index];
                let mut run_x = x;
                for run in &item.runs {
                    SelectObject(self.dc, run.font.into());
                    SetTextColor(self.dc, run.color);
                    let _ = TextOutW(self.dc, run_x, baseline + run.dy, &run.text);
                    run_x += run.width + run.gap_after;
                }
                x += item.width;

                let last = index + 1 == items.len();
                if last && !cyclic {
                    break;
                }
                let rect = RECT {
                    left: x + gap / 2,
                    top: sep_top,
                    right: x + gap / 2 + sep_w,
                    bottom: sep_bottom,
                };
                FillRect(self.dc, &rect, sep);
                x += gap;
                index = if last { 0 } else { index + 1 };
            }
            let _ = DeleteObject(sep.into());
        }
    }

    fn release_strip(&mut self) {
        if let Some(strip) = self.strip.take() {
            unsafe {
                SelectObject(self.dc, strip.previous);
                let _ = DeleteObject(strip.bitmap.into());
            }
        }
    }

    fn release_fonts(&mut self) {
        unsafe {
            // A font can't be deleted while selected into a DC.
            SelectObject(self.dc, GetStockObject(SYSTEM_FONT));
        }
        self.fonts.delete();
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.release_strip();
        self.release_fonts();
        unsafe {
            let _ = DeleteObject(self.bg.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

fn font_px(dpi: u32, font_pt: f64) -> i32 {
    (font_pt * dpi as f64 / 72.0).round().max(6.0) as i32
}

/// Decimal places to show: whole-dollar prices get cents, penny stocks and crypto get more.
fn decimals(price: f64) -> usize {
    match price.abs() {
        a if a >= 1.0 => 2,
        a if a >= 0.01 => 4,
        _ => 6,
    }
}

/// Formats with thousands separators: `1234567.891` -> `1,234,567.89`.
fn group(value: f64, decimals: usize) -> String {
    let formatted = format!("{value:.decimals$}");
    let (integer, fraction) = match formatted.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (formatted.as_str(), None),
    };
    let (sign, digits) = match integer.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", integer),
    };
    let mut out = String::from(sign);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if let Some(fraction) = fraction {
        out.push('.');
        out.push_str(fraction);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_thousands() {
        assert_eq!(group(1234.5, 2), "1,234.50");
        assert_eq!(group(999.999, 2), "1,000.00");
        assert_eq!(group(0.5, 4), "0.5000");
        assert_eq!(group(12.0, 2), "12.00");
        assert_eq!(group(-1234567.891, 2), "-1,234,567.89");
        assert_eq!(group(100.0, 0), "100");
    }

    #[test]
    fn picks_decimals_by_magnitude() {
        assert_eq!(decimals(330.32), 2);
        assert_eq!(decimals(1.0), 2);
        assert_eq!(decimals(0.5), 4);
        assert_eq!(decimals(0.00001234), 6);
    }

    #[test]
    fn font_pixels_follow_dpi() {
        assert_eq!(font_px(96, 13.0), 17);
        assert_eq!(font_px(192, 13.0), 35);
    }
}
