//! Direct2D + DirectWrite into a 32-bit premultiplied DIB, pushed to a layered
//! window with UpdateLayeredWindow, so text blends per pixel with the Mica taskbar.
//! The render target is forced to software: a D3D device would cost several MB of
//! working set for a few hundred pixels of text.
//!
//! Content is a grid: columns of lines, each line a list of colored runs or a bar.
//! The strip
//! and the hover panel each own a Renderer.

use crate::util::wide;
use windows::core::{w, Result, BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED,
    D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_HIT_TEST_METRICS,
    DWRITE_TEXT_METRICS, DWRITE_TEXT_RANGE,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};
use windows_numerics::Vector2;

/// A run of text drawn in one color. `bold` is used for values.
#[derive(Clone)]
pub struct Seg {
    pub text: String,
    pub color: D2D1_COLOR_F,
    pub bold: bool,
    /// Font size relative to the style's, for big numbers.
    pub size: f32,
    /// Set by `bar()` / `spark()`: the line is drawn as a graphic instead of text.
    pub bar: Option<Box<Bar>>,
    pub spark: Option<Box<Spark>>,
    /// Drawn in the icon font (Segoe Fluent Icons, or MDL2 Assets on Windows 10).
    pub icon: bool,
    /// A lightning bolt drawn with Direct2D in the space of this run's text.
    pub bolt: bool,
}

/// A small history graph: one column per sample, series stacked bottom-up,
/// each value a fraction 0..=1 of the graph's height.
#[derive(Clone)]
pub struct Spark {
    pub series: Vec<(Vec<f32>, D2D1_COLOR_F)>,
    pub track: D2D1_COLOR_F,
}

/// A thin horizontal bar as wide as its column: filled parts on a track.
#[derive(Clone)]
pub struct Bar {
    /// (fraction of the track, color), drawn left to right.
    pub parts: Vec<(f32, D2D1_COLOR_F)>,
    pub track: D2D1_COLOR_F,
}

pub fn seg(text: impl Into<String>, color: D2D1_COLOR_F, bold: bool) -> Seg {
    Seg {
        text: text.into(),
        color,
        bold,
        size: 1.0,
        bar: None,
        spark: None,
        icon: false,
        bolt: false,
    }
}

/// A glyph from the icon font, e.g. `'\u{E702}'` (Bluetooth).
#[cfg_attr(not(feature = "batteries"), allow(dead_code))]
pub fn icon(glyph: char, color: D2D1_COLOR_F) -> Seg {
    Seg {
        icon: true,
        ..seg(glyph, color, false)
    }
}

/// A slim filled lightning bolt, about as wide as a digit.
#[cfg_attr(not(feature = "batteries"), allow(dead_code))]
pub fn bolt(color: D2D1_COLOR_F) -> Seg {
    Seg {
        bolt: true,
        // Two en spaces reserve the room; the bolt is drawn over them.
        ..seg("\u{2002}\u{2002}", color, false)
    }
}

/// A bold run at `size` times the normal font size.
pub fn big(text: impl Into<String>, color: D2D1_COLOR_F, size: f32) -> Seg {
    Seg {
        size,
        ..seg(text, color, true)
    }
}

/// A line holding just a history graph.
pub fn spark(series: Vec<(Vec<f32>, D2D1_COLOR_F)>, track: D2D1_COLOR_F) -> Vec<Seg> {
    vec![Seg {
        spark: Some(Box::new(Spark { series, track })),
        ..seg("", track, false)
    }]
}

/// A line holding just a bar.
pub fn bar(parts: Vec<(f32, D2D1_COLOR_F)>, track: D2D1_COLOR_F) -> Vec<Seg> {
    vec![Seg {
        text: String::new(),
        color: track,
        bold: false,
        size: 1.0,
        bar: Some(Box::new(Bar { parts, track })),
        spark: None,
        icon: false,
        bolt: false,
    }]
}

/// Narrowest a bar gets, and its thickness.
const BAR_MIN_DIP: f32 = 64.0;
const BAR_H_DIP: f32 = 4.0;
/// History graph size: width, and height in normal lines.
const SPARK_W_DIP: f32 = 140.0;
const SPARK_LINES: f32 = 2.0;

enum Line {
    /// Layout, its height, and the bolts in it (text position, color).
    Text(IDWriteTextLayout, f32, Vec<(u32, D2D1_COLOR_F)>),
    Bar(Bar),
    Spark(Spark),
}

/// A column of lines.
pub type Column = Vec<Vec<Seg>>;

pub struct Style {
    pub font_dip: f32,
    /// Padding around the grid.
    pub pad_dip: f32,
    /// Space between columns.
    pub gap_dip: f32,
    /// Panel background and border; None draws on a (near) transparent background.
    pub background: Option<(D2D1_COLOR_F, D2D1_COLOR_F)>,
    /// Columns never shrink between frames, so values don't make later columns jump.
    pub sticky_widths: bool,
}

pub struct Renderer {
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    rt: ID2D1DCRenderTarget,
    format: Option<(f32, IDWriteTextFormat)>,
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    size: (i32, i32),
    style: Style,
    scale: f32,
    widths: Vec<f32>,
    columns: Vec<Vec<Line>>,
    /// Height of a normal line of text.
    line_h: f32,
    /// Height of each row (the tallest line at that index across columns).
    rows: Vec<f32>,
    /// Family name for icon runs, NUL-terminated.
    icon_font: Vec<u16>,
}

impl Renderer {
    pub fn new(style: Style) -> Result<Renderer> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let rt = d2d.CreateDCRenderTarget(&props)?;
            // Grayscale: ClearType needs an opaque background.
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let dc = CreateCompatibleDC(None);
            let icon_font = wide(icon_family(&dwrite));
            Ok(Renderer {
                d2d,
                dwrite,
                rt,
                format: None,
                dc,
                bitmap: HBITMAP::default(),
                old: HGDIOBJ::default(),
                size: (0, 0),
                style,
                scale: 1.0,
                widths: Vec::new(),
                columns: Vec::new(),
                line_h: 0.0,
                rows: Vec::new(),
                icon_font,
            })
        }
    }

    /// Forgets the sticky column widths, e.g. after the columns changed meaning.
    pub fn reset_widths(&mut self) {
        self.widths.clear();
    }

    pub fn set_background(&mut self, background: Option<(D2D1_COLOR_F, D2D1_COLOR_F)>) {
        self.style.background = background;
    }

    fn format(&mut self, px: f32) -> Result<IDWriteTextFormat> {
        if let Some((p, f)) = &self.format {
            if *p == px {
                return Ok(f.clone());
            }
        }
        let f = unsafe {
            self.dwrite.CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                px,
                w!("en-us"),
            )?
        };
        self.format = Some((px, f.clone()));
        Ok(f)
    }

    /// Lays out the grid and returns the bitmap size in px. `height` fixes the
    /// height (the strip fills the taskbar); None sizes it to the content.
    /// Call `present` afterwards to draw and push the pixels.
    pub fn prepare(
        &mut self,
        cols: &[Column],
        scale: f32,
        height: Option<i32>,
    ) -> Result<(i32, i32)> {
        if scale != self.scale || cols.len() != self.widths.len() {
            self.widths = vec![0.0; cols.len()];
            self.scale = scale;
        }
        let fmt = self.format(self.style.font_dip * scale)?;
        self.columns.clear();
        self.line_h = {
            let probe = self.layout(&[seg("0", D2D1_COLOR_F::default(), false)], &fmt)?;
            let mut m = DWRITE_TEXT_METRICS::default();
            unsafe { probe.GetMetrics(&mut m)? };
            m.height
        };
        self.rows.clear();
        for (i, col) in cols.iter().enumerate() {
            let mut layouts = Vec::new();
            let mut width = 0f32;
            for (row, segs) in col.iter().enumerate() {
                let first = segs.first();
                let (line, w, h) = if let Some(b) = first.and_then(|s| s.bar.as_deref()) {
                    (Line::Bar(b.clone()), BAR_MIN_DIP * scale, self.line_h)
                } else if let Some(sp) = first.and_then(|s| s.spark.as_deref()) {
                    let h = self.line_h * SPARK_LINES;
                    (Line::Spark(sp.clone()), SPARK_W_DIP * scale, h)
                } else {
                    let layout = self.layout(segs, &fmt)?;
                    let mut m = DWRITE_TEXT_METRICS::default();
                    unsafe { layout.GetMetrics(&mut m)? };
                    let (w, h) = (m.widthIncludingTrailingWhitespace, m.height);
                    let mut at = 0u32;
                    let mut bolts = Vec::new();
                    for s in segs {
                        if s.bolt {
                            bolts.push((at, s.color));
                        }
                        at += s.text.encode_utf16().count() as u32;
                    }
                    (Line::Text(layout, h, bolts), w, h)
                };
                width = width.max(w);
                if self.rows.len() <= row {
                    self.rows.push(0.0);
                }
                self.rows[row] = self.rows[row].max(h);
                layouts.push(line);
            }
            self.widths[i] = if self.style.sticky_widths {
                self.widths[i].max(width)
            } else {
                width
            };
            self.columns.push(layouts);
        }
        let pad = self.style.pad_dip * scale;
        let gap = self.style.gap_dip * scale;
        let content_h: f32 = self.rows.iter().sum();
        let w = self.widths.iter().sum::<f32>() + gap * (cols.len().max(1) - 1) as f32 + 2.0 * pad;
        let h = height.unwrap_or((content_h + 2.0 * pad).ceil() as i32);
        self.ensure_bitmap(w.ceil() as i32, h)?;
        Ok(self.size)
    }

    fn layout(&self, segs: &[Seg], fmt: &IDWriteTextFormat) -> Result<IDWriteTextLayout> {
        let text: Vec<u16> = segs.iter().flat_map(|s| s.text.encode_utf16()).collect();
        let layout = unsafe { self.dwrite.CreateTextLayout(&text, fmt, 4000.0, 1000.0)? };
        let mut start = 0u32;
        for s in segs {
            let len = s.text.encode_utf16().count() as u32;
            let range = DWRITE_TEXT_RANGE {
                startPosition: start,
                length: len,
            };
            unsafe {
                let brush = self.rt.CreateSolidColorBrush(&s.color, None)?;
                layout.SetDrawingEffect(&brush, range)?;
                if s.bold {
                    layout.SetFontWeight(DWRITE_FONT_WEIGHT_SEMI_BOLD, range)?;
                }
                if s.size != 1.0 {
                    layout.SetFontSize(fmt.GetFontSize() * s.size, range)?;
                }
                if s.icon {
                    layout.SetFontFamilyName(PCWSTR(self.icon_font.as_ptr()), range)?;
                }
            }
            start += len;
        }
        Ok(layout)
    }

    fn ensure_bitmap(&mut self, w: i32, h: i32) -> Result<()> {
        if self.size == (w, h) {
            return Ok(());
        }
        unsafe {
            if !self.bitmap.is_invalid() {
                SelectObject(self.dc, self.old);
                let _ = DeleteObject(self.bitmap.into());
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            self.bitmap =
                CreateDIBSection(Some(self.dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
            self.old = SelectObject(self.dc, self.bitmap.into());
        }
        self.size = (w, h);
        Ok(())
    }

    pub fn present(&mut self, hwnd: HWND, fallback_color: D2D1_COLOR_F) -> Result<()> {
        let (w, h) = self.size;
        let pad = self.style.pad_dip * self.scale;
        let gap = self.style.gap_dip * self.scale;
        unsafe {
            self.rt.BindDC(
                self.dc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                },
            )?;
            self.rt.BeginDraw();
            match self.style.background {
                None => {
                    // Alpha 1/255 instead of 0: fully transparent pixels of a layered
                    // window are click-through, which would make the gaps between words dead.
                    self.rt.Clear(Some(&D2D1_COLOR_F {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0 / 255.0,
                    }));
                }
                Some((fill, border)) => {
                    self.rt.Clear(Some(&D2D1_COLOR_F::default()));
                    let r = 8.0 * self.scale;
                    let rect = D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F {
                            left: 0.5,
                            top: 0.5,
                            right: w as f32 - 0.5,
                            bottom: h as f32 - 0.5,
                        },
                        radiusX: r,
                        radiusY: r,
                    };
                    self.rt
                        .FillRoundedRectangle(&rect, &self.rt.CreateSolidColorBrush(&fill, None)?);
                    self.rt.DrawRoundedRectangle(
                        &rect,
                        &self.rt.CreateSolidColorBrush(&border, None)?,
                        1.0,
                        None,
                    );
                }
            }
            let brush = self.rt.CreateSolidColorBrush(&fallback_color, None)?;
            let content_h: f32 = self.rows.iter().sum();
            let top = ((h as f32 - content_h) / 2.0).max(0.0).round();
            let row_tops: Vec<f32> = self
                .rows
                .iter()
                .scan(top, |y, rh| {
                    let t = *y;
                    *y += rh;
                    Some(t)
                })
                .collect();
            let mut x = pad;
            for (col, width) in self.columns.iter().zip(&self.widths) {
                for (i, l) in col.iter().enumerate() {
                    // Each line is centered vertically in its row.
                    let (y, rh) = (row_tops[i], self.rows[i]);
                    match l {
                        Line::Text(l, lh, bolts) => {
                            let origin = Vector2 {
                                X: x.round(),
                                Y: (y + (rh - lh) / 2.0).round(),
                            };
                            self.rt
                                .DrawTextLayout(origin, l, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                            for &(pos, color) in bolts {
                                self.draw_bolt(l, pos, origin, color)?;
                            }
                        }
                        Line::Bar(b) => {
                            let y = y + (rh - self.line_h) / 2.0;
                            self.draw_bar(b, x.round(), y, *width)?
                        }
                        Line::Spark(sp) => {
                            let sh = self.line_h * SPARK_LINES;
                            self.draw_spark(
                                sp,
                                x.round(),
                                (y + (rh - sh) / 2.0).round(),
                                *width,
                                sh,
                            )?
                        }
                    }
                }
                x += width + gap;
            }
            self.rt.EndDraw(None, None)?;

            let size = SIZE { cx: w, cy: h };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            UpdateLayeredWindow(
                hwnd,
                None,
                None,
                Some(&size),
                Some(self.dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )?;
        }
        Ok(())
    }
}

impl Renderer {
    /// Columns of stacked samples, right-aligned so the newest is at the right edge.
    unsafe fn draw_spark(&self, sp: &Spark, x: f32, y: f32, width: f32, h: f32) -> Result<()> {
        let rect = D2D_RECT_F {
            left: x,
            top: y,
            right: x + width,
            bottom: y + h,
        };
        self.rt
            .FillRectangle(&rect, &self.rt.CreateSolidColorBrush(&sp.track, None)?);
        let n = sp.series.iter().map(|(v, _)| v.len()).max().unwrap_or(0);
        if n == 0 {
            return Ok(());
        }
        let col_w = (width / 30.0).max(1.0);
        let brushes: Vec<_> = sp
            .series
            .iter()
            .map(|(_, c)| self.rt.CreateSolidColorBrush(c, None))
            .collect::<Result<_>>()?;
        for i in 0..n {
            let left = x + width - (n - i) as f32 * col_w;
            if left < x {
                continue;
            }
            let mut bottom = y + h;
            for ((values, _), brush) in sp.series.iter().zip(&brushes) {
                let v = values.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0);
                let top = (bottom - v * h).max(y);
                let r = D2D_RECT_F {
                    left,
                    top,
                    right: left + col_w - 1.0,
                    bottom,
                };
                self.rt.FillRectangle(&r, brush);
                bottom = top;
            }
        }
        Ok(())
    }

    /// A bolt over the two en spaces at text position `pos`: as tall as the icon
    /// glyphs, centered in the spaces, sitting on the baseline.
    unsafe fn draw_bolt(
        &self,
        layout: &IDWriteTextLayout,
        pos: u32,
        origin: Vector2,
        color: D2D1_COLOR_F,
    ) -> Result<()> {
        let (mut px, mut py) = (0f32, 0f32);
        let mut hit = DWRITE_HIT_TEST_METRICS::default();
        layout.HitTestTextPosition(pos, false, &mut px, &mut py, &mut hit)?;
        let size = self.style.font_dip * self.scale;
        let h = size * 1.0;
        let w = h * 0.5;
        // The hit box is the first space; the run is two.
        let left = origin.X + hit.left + hit.width - w / 2.0;
        // Baseline of a normal-size line, from the top of its cell.
        let bottom = origin.Y + hit.top + hit.height * 0.84;
        let top = bottom - h;
        // A slim zigzag: top right, down to the middle left, a notch, bottom.
        let pts = [
            (0.70, 0.00),
            (0.12, 0.58),
            (0.46, 0.58),
            (0.30, 1.00),
            (0.88, 0.40),
            (0.54, 0.40),
        ];
        let geom = self.d2d.CreatePathGeometry()?;
        let sink = geom.Open()?;
        let p = |(fx, fy): (f32, f32)| Vector2 {
            X: left + fx * w,
            Y: top + fy * h,
        };
        sink.BeginFigure(p(pts[0]), D2D1_FIGURE_BEGIN_FILLED);
        for &pt in &pts[1..] {
            sink.AddLine(p(pt));
        }
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
        self.rt
            .FillGeometry(&geom, &self.rt.CreateSolidColorBrush(&color, None)?, None);
        Ok(())
    }

    /// A bar centered in the line slot at `y`. Call between BeginDraw and EndDraw.
    unsafe fn draw_bar(&self, b: &Bar, x: f32, y: f32, width: f32) -> Result<()> {
        let h = (BAR_H_DIP * self.scale).round().max(2.0);
        let top = (y + (self.line_h - h) / 2.0).round();
        let rect = |l: f32, r: f32| D2D_RECT_F {
            left: l,
            top,
            right: r,
            bottom: top + h,
        };
        let track = D2D1_ROUNDED_RECT {
            rect: rect(x, x + width),
            radiusX: h / 2.0,
            radiusY: h / 2.0,
        };
        self.rt
            .FillRoundedRectangle(&track, &self.rt.CreateSolidColorBrush(&b.track, None)?);
        let mut at = x;
        for (frac, color) in &b.parts {
            let w = (frac.clamp(0.0, 1.0) * width).min(x + width - at);
            if w <= 0.0 {
                break;
            }
            let part = D2D1_ROUNDED_RECT {
                rect: rect(at, at + w),
                radiusX: (h / 2.0).min(w / 2.0),
                radiusY: h / 2.0,
            };
            self.rt
                .FillRoundedRectangle(&part, &self.rt.CreateSolidColorBrush(color, None)?);
            at += w;
        }
        Ok(())
    }
}

/// Segoe Fluent Icons (Windows 11), else Segoe MDL2 Assets (Windows 10); the
/// glyphs used here have the same code points in both.
fn icon_family(dwrite: &IDWriteFactory) -> &'static str {
    const FLUENT: &str = "Segoe Fluent Icons";
    let mut fonts = None;
    let (mut index, mut exists) = (0u32, BOOL(0));
    let w = wide(FLUENT);
    unsafe {
        if dwrite.GetSystemFontCollection(&mut fonts, false).is_ok() {
            if let Some(f) = fonts {
                let _ = f.FindFamilyName(PCWSTR(w.as_ptr()), &mut index, &mut exists);
            }
        }
    }
    if exists.as_bool() {
        FLUENT
    } else {
        "Segoe MDL2 Assets"
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            if !self.bitmap.is_invalid() {
                SelectObject(self.dc, self.old);
                let _ = DeleteObject(self.bitmap.into());
            }
            let _ = DeleteDC(self.dc);
        }
    }
}
