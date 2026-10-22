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

#[derive(Clone, Copy, PartialEq)]
pub struct Config {
    pub show: Show,
    pub layout: Layout,
    pub cpu: bool,
    pub gpu: bool,
    pub commit: bool,
    pub disk: bool,
}

/// The columns that can be toggled, in menu order.
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

    fn name(self) -> &'static str {
        match self {
            Toggle::Cpu => "ShowCpu",
            Toggle::Gpu => "ShowGpu",
            Toggle::Commit => "ShowCommit",
            Toggle::Disk => "ShowDisk",
        }
    }
}

impl Config {
    pub fn load() -> Config {
        let get = |name: &str| reg_dword(HKEY_CURRENT_USER, KEY, name);
        let on = |t: Toggle| get(t.name()) != Some(0);
        Config {
            show: match get("Show") {
                Some(1) => Show::Tray,
                Some(2) => Show::Both,
                _ => Show::Taskbar,
            },
            layout: match get("Layout") {
                Some(1) => Layout::ByOwner,
                Some(2) => Layout::Bars,
                Some(3) => Layout::Compact,
                _ => Layout::Grid,
            },
            cpu: on(Toggle::Cpu),
            gpu: on(Toggle::Gpu),
            commit: on(Toggle::Commit),
            disk: on(Toggle::Disk),
        }
    }

    pub fn shows(&self, t: Toggle) -> bool {
        match t {
            Toggle::Cpu => self.cpu,
            Toggle::Gpu => self.gpu,
            Toggle::Commit => self.commit,
            Toggle::Disk => self.disk,
        }
    }

    pub fn set_layout(&mut self, layout: Layout) {
        self.layout = layout;
        let i = Layout::ALL.iter().position(|&l| l == layout).unwrap_or(0);
        save("Layout", i as u32);
    }

    pub fn set_show(&mut self, show: Show) {
        self.show = show;
        let i = Show::ALL.iter().position(|&s| s == show).unwrap_or(0);
        save("Show", i as u32);
    }

    pub fn flip(&mut self, t: Toggle) {
        let on = !self.shows(t);
        match t {
            Toggle::Cpu => self.cpu = on,
            Toggle::Gpu => self.gpu = on,
            Toggle::Commit => self.commit = on,
            Toggle::Disk => self.disk = on,
        }
        save(t.name(), on as u32);
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
