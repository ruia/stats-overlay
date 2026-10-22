//! Tray-icon mode: instead of the strip, a notification-area icon drawn as two
//! small bars (RAM split Windows / WSL, and commit), with the summary as tooltip.
//! Hovering shows the same panel, right-click the same menu.

use crate::theme::Palette;
use crate::util::wide;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject, HBITMAP};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER,
    NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, HICON, ICONINFO, SM_CXSMICON, WM_APP,
};

/// Sent to the host window; with NOTIFYICON_VERSION_4 the event is LOWORD(lParam).
pub const WM_APP_TRAY: u32 = WM_APP + 2;
const ID: u32 = 1;

/// Fractions 0..=1 shown by the icon.
#[derive(Clone, Copy, PartialEq)]
pub struct Bars {
    pub win: f32,
    pub wsl: f32,
    pub commit: f32,
}

pub struct Tray {
    host: HWND,
    added: bool,
    icon: Option<HICON>,
    /// What the current icon shows, in pixels, so it's only redrawn on change.
    drawn: Option<(i32, i32, i32, u32)>,
}

fn data(host: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: host,
        uID: ID,
        ..Default::default()
    }
}

impl Tray {
    pub fn new(host: HWND) -> Tray {
        Tray {
            host,
            added: false,
            icon: None,
            drawn: None,
        }
    }

    /// Adds or updates the icon (call `reset` first after TaskbarCreated).
    pub fn update(&mut self, bars: Bars, tip: &str, p: &Palette) {
        let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()) }.max(16);
        let px = |f: f32| (f.clamp(0.0, 1.0) * size as f32).round() as i32;
        let key = (
            px(bars.win),
            px(bars.win + bars.wsl),
            px(bars.commit),
            size as u32,
        );
        if self.drawn != Some(key) || self.icon.is_none() {
            if let Some(icon) = draw(size, bars, p) {
                if let Some(old) = self.icon.replace(icon) {
                    unsafe {
                        let _ = DestroyIcon(old);
                    }
                }
                self.drawn = Some(key);
            }
        }
        let mut d = data(self.host);
        d.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
        d.uCallbackMessage = WM_APP_TRAY;
        d.hIcon = self.icon.unwrap_or_default();
        let t = wide(tip);
        let n = t.len().min(d.szTip.len() - 1);
        d.szTip[..n].copy_from_slice(&t[..n]);
        unsafe {
            if self.added && !Shell_NotifyIconW(NIM_MODIFY, &d).as_bool() {
                self.added = false; // Explorer restarted: add it again below
            }
            if !self.added && Shell_NotifyIconW(NIM_ADD, &d).as_bool() {
                d.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                let _ = Shell_NotifyIconW(NIM_SETVERSION, &d);
                self.added = true;
            }
        }
    }

    /// After TaskbarCreated the old icon is gone.
    pub fn reset(&mut self) {
        self.added = false;
    }

    pub fn remove(&mut self) {
        if self.added {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &data(self.host));
            }
            self.added = false;
        }
        if let Some(icon) = self.icon.take() {
            unsafe {
                let _ = DestroyIcon(icon);
            }
        }
        self.drawn = None;
    }

    /// The icon's screen rectangle, to anchor the panel.
    pub fn rect(&self) -> Option<RECT> {
        if !self.added {
            return None;
        }
        let id = NOTIFYICONIDENTIFIER {
            cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: self.host,
            uID: ID,
            ..Default::default()
        };
        unsafe { Shell_NotifyIconGetRect(&id).ok() }
    }
}

fn bgra(c: D2D1_COLOR_F, alpha: f32) -> u32 {
    let ch = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (ch(c.a * alpha) << 24) | (ch(c.r) << 16) | (ch(c.g) << 8) | ch(c.b)
}

/// Two vertical bars, filled from the bottom: RAM (Windows, then WSL on top) and
/// commit (colored by level).
fn draw(size: i32, bars: Bars, p: &Palette) -> Option<HICON> {
    let s = size as usize;
    let mut px = vec![0u32; s * s];
    let bar_w = (s * 3 / 8).max(4);
    let gap = s - 2 * bar_w;
    let track = bgra(p.value, 0.22);
    let commit = crate::theme::level(p, bars.commit as f64);
    let columns = [
        (0, vec![(bars.win, p.win), (bars.wsl, p.wsl)]),
        (bar_w + gap, vec![(bars.commit, commit)]),
    ];
    for (x0, parts) in &columns {
        let mut filled: Vec<u32> = vec![track; s];
        let mut at = 0usize; // rows from the bottom
        for (frac, color) in parts {
            let h = (frac.clamp(0.0, 1.0) * s as f32).round() as usize;
            let end = (at + h).min(s);
            for slot in filled.iter_mut().take(end).skip(at) {
                *slot = bgra(*color, 1.0);
            }
            at = end;
        }
        for (row_from_bottom, color) in filled.iter().enumerate() {
            let y = s - 1 - row_from_bottom;
            for x in *x0..*x0 + bar_w {
                px[y * s + x] = *color;
            }
        }
    }
    unsafe {
        let color = CreateBitmap(size, size, 1, 32, Some(px.as_ptr().cast()));
        let mask = CreateBitmap(
            size,
            size,
            1,
            1,
            Some(vec![0u8; s * s.div_ceil(16) * 2].as_ptr().cast()),
        );
        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).ok();
        for b in [color, mask] {
            if b != HBITMAP::default() {
                let _ = DeleteObject(b.into());
            }
        }
        icon
    }
}
