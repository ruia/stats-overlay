//! Direct2D + DirectWrite into a 32-bit premultiplied DIB, pushed to a layered
//! window with UpdateLayeredWindow, so text blends per pixel with the Mica taskbar.
//! The render target is forced to software: a D3D device would cost several MB of
//! working set for a few hundred pixels of text.
//!
//! Content is a grid: columns of lines, each line a list of colored runs. The strip
//! and the hover panel each own a Renderer.

use windows::core::{w, Result};
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
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
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_TEXT_METRICS,
    DWRITE_TEXT_RANGE,
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
pub struct Seg {
    pub text: String,
    pub color: D2D1_COLOR_F,
    pub bold: bool,
}

pub fn seg(text: impl Into<String>, color: D2D1_COLOR_F, bold: bool) -> Seg {
    Seg {
        text: text.into(),
        color,
        bold,
    }
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
    columns: Vec<Vec<IDWriteTextLayout>>,
    line_h: f32,
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
            Ok(Renderer {
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
        self.line_h = 0.0;
        for (i, col) in cols.iter().enumerate() {
            let mut layouts = Vec::new();
            let mut width = 0f32;
            for segs in col {
                let layout = self.layout(segs, &fmt)?;
                let mut m = DWRITE_TEXT_METRICS::default();
                unsafe { layout.GetMetrics(&mut m)? };
                width = width.max(m.widthIncludingTrailingWhitespace);
                self.line_h = self.line_h.max(m.height);
                layouts.push(layout);
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
        let lines = self.columns.iter().map(Vec::len).max().unwrap_or(0) as f32;
        let w = self.widths.iter().sum::<f32>() + gap * (cols.len().max(1) - 1) as f32 + 2.0 * pad;
        let h = height.unwrap_or((self.line_h * lines + 2.0 * pad).ceil() as i32);
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
            let lines = self.columns.iter().map(Vec::len).max().unwrap_or(0) as f32;
            let top = ((h as f32 - self.line_h * lines) / 2.0).max(0.0).round();
            let mut x = pad;
            for (col, width) in self.columns.iter().zip(&self.widths) {
                for (i, l) in col.iter().enumerate() {
                    let origin = Vector2 {
                        X: x.round(),
                        Y: top + self.line_h * i as f32,
                    };
                    self.rt
                        .DrawTextLayout(origin, l, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
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
