//! Direct2D + DirectWrite into a 32-bit premultiplied DIB, pushed to the layered
//! window with UpdateLayeredWindow, so text blends per pixel with the Mica taskbar.
//! The render target is forced to software: a D3D device would cost several MB of
//! working set for a few hundred pixels of text.

use windows::core::{w, Result};
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
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

pub struct Renderer {
    dwrite: IDWriteFactory,
    rt: ID2D1DCRenderTarget,
    format: Option<(f32, IDWriteTextFormat)>,
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    size: (i32, i32),
    layouts: Vec<IDWriteTextLayout>,
}

const FONT_DIP: f32 = 12.0;
const PAD_DIP: f32 = 6.0;

impl Renderer {
    pub fn new() -> Result<Renderer> {
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
                layouts: Vec::new(),
            })
        }
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

    /// Lays out the lines for a strip `height` px tall and returns the width in px.
    /// Call `present` afterwards to draw and push the pixels.
    pub fn prepare(&mut self, lines: &[Vec<Seg>], scale: f32, height: i32) -> Result<i32> {
        let fmt = self.format(FONT_DIP * scale)?;
        self.layouts.clear();
        let mut width = 0f32;
        for segs in lines {
            let text: Vec<u16> = segs.iter().flat_map(|s| s.text.encode_utf16()).collect();
            let layout = unsafe {
                self.dwrite
                    .CreateTextLayout(&text, &fmt, 4000.0, height as f32)?
            };
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
            let mut m = DWRITE_TEXT_METRICS::default();
            unsafe { layout.GetMetrics(&mut m)? };
            width = width.max(m.widthIncludingTrailingWhitespace);
            self.layouts.push(layout);
        }
        let w = (width + 2.0 * PAD_DIP * scale).ceil() as i32;
        self.ensure_bitmap(w, height)?;
        Ok(w)
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
            // Alpha 1/255 instead of 0: fully transparent pixels of a layered window
            // are click-through, which would make the gaps between words dead.
            self.rt.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0 / 255.0,
            }));
            let brush = self.rt.CreateSolidColorBrush(&fallback_color, None)?;
            let n = self.layouts.len() as f32;
            let mut line_h = 0f32;
            for l in &self.layouts {
                let mut m = DWRITE_TEXT_METRICS::default();
                l.GetMetrics(&mut m)?;
                line_h = line_h.max(m.height);
            }
            let top = ((h as f32 - line_h * n) / 2.0).max(0.0).round();
            let pad = (w as f32 - self.max_width()).max(0.0) / 2.0;
            for (i, l) in self.layouts.iter().enumerate() {
                let origin = Vector2 {
                    X: pad.round(),
                    Y: top + line_h * i as f32,
                };
                self.rt
                    .DrawTextLayout(origin, l, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
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

    fn max_width(&self) -> f32 {
        self.layouts
            .iter()
            .map(|l| {
                let mut m = DWRITE_TEXT_METRICS::default();
                unsafe {
                    let _ = l.GetMetrics(&mut m);
                }
                m.widthIncludingTrailingWhitespace
            })
            .fold(0.0, f32::max)
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
