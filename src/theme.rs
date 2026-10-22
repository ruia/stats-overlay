use crate::util::reg_dword;
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::System::Registry::HKEY_CURRENT_USER;

/// The taskbar follows the system theme, not the app theme.
pub fn system_light() -> bool {
    reg_dword(
        HKEY_CURRENT_USER,
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "SystemUsesLightTheme",
    ) == Some(1)
}

pub struct Palette {
    pub label: D2D1_COLOR_F,
    pub value: D2D1_COLOR_F,
    pub warn: D2D1_COLOR_F,
    pub crit: D2D1_COLOR_F,
    /// Owner names in the "Windows / WSL rows" layout.
    pub win: D2D1_COLOR_F,
    pub wsl: D2D1_COLOR_F,
    /// Bar background in the "Bars" layout.
    pub track: D2D1_COLOR_F,
    /// Hover panel (fill, border).
    pub panel: (D2D1_COLOR_F, D2D1_COLOR_F),
}

const fn rgb(hex: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

const fn rgba(hex: u32, a: f32) -> D2D1_COLOR_F {
    let c = rgb(hex);
    D2D1_COLOR_F { a, ..c }
}

pub fn palette(light: bool) -> Palette {
    if light {
        Palette {
            label: rgb(0x5f5f5f),
            value: rgb(0x1a1a1a),
            warn: rgb(0x9d5d00),
            crit: rgb(0xc42b1c),
            win: rgb(0x0a5bb5),
            wsl: rgb(0xa14a00),
            track: rgba(0x000000, 0.14),
            panel: (rgb(0xf3f3f3), rgb(0xd5d5d5)),
        }
    } else {
        Palette {
            label: rgb(0xa8a8a8),
            value: rgb(0xf3f3f3),
            warn: rgb(0xfce100),
            crit: rgb(0xff99a4),
            win: rgb(0x7fb8ff),
            wsl: rgb(0xffb070),
            track: rgba(0xffffff, 0.16),
            panel: (rgb(0x2b2b2b), rgb(0x474747)),
        }
    }
}

/// Normal below 80 %, yellow from 80 %, red from 90 %.
pub fn level(p: &Palette, fraction: f64) -> D2D1_COLOR_F {
    if fraction >= 0.9 {
        p.crit
    } else if fraction >= 0.8 {
        p.warn
    } else {
        p.value
    }
}
