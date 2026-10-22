//! Finding the taskbar and working out where the strip goes.

use crate::util::{pcwstr, reg_dword, wide};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Registry::HKEY_CURRENT_USER;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW, GetWindowRect};

const ADVANCED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced";

/// Logical px kept free on the left for the Widgets (weather) button. A first
/// guess: `--diag` reports the real button bounds, and `--offset` overrides this.
pub const WIDGETS_RESERVE_DIP: i32 = 180;
const MARGIN_DIP: i32 = 8;

pub fn find() -> Option<HWND> {
    let class = wide("Shell_TrayWnd");
    unsafe { FindWindowW(pcwstr(&class), None).ok() }
}

pub fn rect(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut r);
    }
    r
}

pub fn dpi(hwnd: HWND) -> u32 {
    match unsafe { GetDpiForWindow(hwnd) } {
        0 => 96,
        d => d,
    }
}

/// TaskbarAl: 0 = left, 1 or missing = center (the Windows 11 default).
pub fn centered() -> bool {
    reg_dword(HKEY_CURRENT_USER, ADVANCED, "TaskbarAl") != Some(0)
}

/// TaskbarDa: 0 = Widgets button hidden, 1 or missing = shown.
pub fn widgets_enabled() -> bool {
    reg_dword(HKEY_CURRENT_USER, ADVANCED, "TaskbarDa") != Some(0)
}

/// The notification area (clock, tray), used when the taskbar is left-aligned.
pub fn tray_rect(taskbar: HWND) -> Option<RECT> {
    let class = wide("TrayNotifyWnd");
    let h = unsafe { FindWindowExW(Some(taskbar), None, pcwstr(&class), None).ok()? };
    Some(rect(h))
}

pub struct Placement {
    /// Screen coordinates.
    pub x: i32,
    pub y: i32,
    pub height: i32,
}

/// Where a strip `width` px wide goes. Centered taskbar: on the left, after the
/// Widgets button. Left-aligned taskbar: just left of the tray, since the left
/// side is taken by Start and the app icons.
pub fn place(taskbar: HWND, width: i32, offset_dip: Option<i32>) -> Placement {
    let r = rect(taskbar);
    let scale = dpi(taskbar) as f32 / 96.0;
    let px = |dip: i32| (dip as f32 * scale).round() as i32;
    let x = if centered() {
        let reserve = offset_dip.unwrap_or(if widgets_enabled() {
            WIDGETS_RESERVE_DIP
        } else {
            0
        });
        r.left + px(reserve + MARGIN_DIP)
    } else {
        let right = tray_rect(taskbar).map_or(r.right, |t| t.left);
        right - width - px(MARGIN_DIP)
    };
    Placement {
        x,
        y: r.top,
        height: r.bottom - r.top,
    }
}
