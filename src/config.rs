//! Display settings, kept as DWORDs under HKCU\Software\StatsOverlay and changed
//! from the right-click menu.

use crate::util::{pcwstr, reg_dword, wide};
use std::ffi::c_void;
use windows::Win32::System::Registry::{RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD};

const KEY: &str = r"Software\StatsOverlay";

#[derive(Clone, Copy, PartialEq)]
pub enum Layout {
    /// Two lines per metric: WIN/ALL, WSL/CMT, CPU/GPU, C:/VHD.
    Grid,
    /// One line for Windows and one for WSL, with ram, disk and cpu side by side.
    ByOwner,
    /// One column per resource, text over a bar split Windows / WSL.
    Bars,
    /// Just WIN and WSL memory.
    Compact,
}

impl Layout {
    pub const ALL: [Layout; 4] = [Layout::Grid, Layout::ByOwner, Layout::Bars, Layout::Compact];

    pub fn label(self) -> &'static str {
        match self {
            Layout::Grid => "Grid (by metric)",
            Layout::ByOwner => "Windows / WSL rows",
            Layout::Bars => "Bars (by resource)",
            Layout::Compact => "Compact (memory only)",
        }
    }
}

/// Where the readout appears.
#[derive(Clone, Copy, PartialEq)]
pub enum Show {
    Taskbar,
    Tray,
    Both,
}

impl Show {
    pub const ALL: [Show; 3] = [Show::Taskbar, Show::Tray, Show::Both];

    pub fn label(self) -> &'static str {
        match self {
            Show::Taskbar => "Show in taskbar",
            Show::Tray => "Tray icon only",
            Show::Both => "Taskbar and tray icon",
        }
    }

    pub fn strip(self) -> bool {
        self != Show::Tray
    }

    pub fn tray(self) -> bool {
        self != Show::Taskbar
    }
}

/// Hover panel designs, picked from Display > Panel.
#[derive(Clone, Copy, PartialEq)]
pub enum PanelStyle {
    /// Label/value rows grouped into Memory, WSL, Disk, CPU · GPU.
    Sections,
    /// One Windows/WSL bar per resource.
    Bars,
    /// Six tiles: big number and bar each.
    Tiles,
    /// Values next to 60-second history graphs.
    History,
    /// A summary line, then what needs attention.
    Insights,
}

impl PanelStyle {
    pub const ALL: [PanelStyle; 5] = [
        PanelStyle::Sections,
        PanelStyle::Bars,
        PanelStyle::Tiles,
        PanelStyle::History,
        PanelStyle::Insights,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PanelStyle::Sections => "Sections (detailed)",
            PanelStyle::Bars => "Bars",
            PanelStyle::Tiles => "Tiles",
            PanelStyle::History => "History (last minute)",
            PanelStyle::Insights => "Insights (what needs attention)",
        }
    }
}

/// What the tray icon's second bar shows (the first is always RAM).
#[derive(Clone, Copy, PartialEq)]
pub enum TraySecond {
    Commit,
    Disk,
    Cpu,
    Nothing,
}

impl TraySecond {
    pub const ALL: [TraySecond; 4] = [
        TraySecond::Commit,
        TraySecond::Disk,
        TraySecond::Cpu,
        TraySecond::Nothing,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TraySecond::Commit => "Second bar: commit charge",
            TraySecond::Disk => "Second bar: system drive",
            TraySecond::Cpu => "Second bar: CPU",
            TraySecond::Nothing => "RAM bar only",
        }
    }
}

/// The optional items, chosen separately for the strip and for the panel.
#[derive(Clone, Copy, PartialEq)]
pub struct Items {
    pub cpu: bool,
    pub gpu: bool,
    pub commit: bool,
    pub disk: bool,
}

impl Items {
    fn from_bits(b: u32) -> Items {
        Items {
            cpu: b & 1 != 0,
            gpu: b & 2 != 0,
            commit: b & 4 != 0,
            disk: b & 8 != 0,
        }
    }

    fn bits(self) -> u32 {
        self.cpu as u32
            | (self.gpu as u32) << 1
            | (self.commit as u32) << 2
            | (self.disk as u32) << 3
    }

    pub fn shows(&self, t: Toggle) -> bool {
        match t {
            Toggle::Cpu => self.cpu,
            Toggle::Gpu => self.gpu,
            Toggle::Commit => self.commit,
            Toggle::Disk => self.disk,
        }
    }

    fn flip(&mut self, t: Toggle) {
        let on = !self.shows(t);
        match t {
            Toggle::Cpu => self.cpu = on,
            Toggle::Gpu => self.gpu = on,
            Toggle::Commit => self.commit = on,
            Toggle::Disk => self.disk = on,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub struct Config {
    pub show: Show,
    pub layout: Layout,
    pub strip: Items,
    pub style: PanelStyle,
    pub panel: Items,
    pub tray_second: TraySecond,
}

/// The items that can be toggled, in menu order.
#[derive(Clone, Copy)]
pub enum Toggle {
    Cpu,
    Gpu,
    Commit,
    Disk,
}

impl Toggle {
    pub const ALL: [Toggle; 4] = [Toggle::Cpu, Toggle::Gpu, Toggle::Commit, Toggle::Disk];

    pub fn label(self) -> &'static str {
        match self {
            Toggle::Cpu => "CPU",
            Toggle::Gpu => "GPU",
            Toggle::Commit => "Commit charge",
            Toggle::Disk => "Disk",
        }
    }
}

fn index<T: PartialEq>(all: &[T], v: &T) -> u32 {
    all.iter().position(|x| x == v).unwrap_or(0) as u32
}

impl Config {
    pub fn load() -> Config {
        let get = |name: &str| reg_dword(HKEY_CURRENT_USER, KEY, name);
        let pick = |name: &str, n: usize| get(name).map_or(0, |v| v as usize).min(n - 1);
        Config {
            show: Show::ALL[pick("Show", Show::ALL.len())],
            layout: Layout::ALL[pick("Layout", Layout::ALL.len())],
            strip: Items::from_bits(get("StripItems").unwrap_or(0xf)),
            style: PanelStyle::ALL[pick("PanelStyle", PanelStyle::ALL.len())],
            panel: Items::from_bits(get("PanelItems").unwrap_or(0xf)),
            tray_second: TraySecond::ALL[pick("TraySecond", TraySecond::ALL.len())],
        }
    }

    pub fn set_layout(&mut self, layout: Layout) {
        self.layout = layout;
        save("Layout", index(&Layout::ALL, &layout));
    }

    pub fn set_show(&mut self, show: Show) {
        self.show = show;
        save("Show", index(&Show::ALL, &show));
    }

    pub fn set_style(&mut self, style: PanelStyle) {
        self.style = style;
        save("PanelStyle", index(&PanelStyle::ALL, &style));
    }

    pub fn set_tray_second(&mut self, t: TraySecond) {
        self.tray_second = t;
        save("TraySecond", index(&TraySecond::ALL, &t));
    }

    pub fn flip_strip(&mut self, t: Toggle) {
        self.strip.flip(t);
        save("StripItems", self.strip.bits());
    }

    pub fn flip_panel(&mut self, t: Toggle) {
        self.panel.flip(t);
        save("PanelItems", self.panel.bits());
    }
}

fn save(name: &str, value: u32) {
    let (k, n) = (wide(KEY), wide(name));
    unsafe {
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            pcwstr(&k),
            pcwstr(&n),
            REG_DWORD.0,
            Some(&value as *const u32 as *const c_void),
            4,
        );
    }
}
