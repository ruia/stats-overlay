//! `statsoverlay.exe --diag`: writes statsoverlay-diag.txt next to the exe with
//! what v0 needs verified: Windows build, taskbar geometry and children, the
//! Widgets/Start button bounds (via UI Automation), and the vmmem PDH instances.

use crate::sampler::memory_status;
use crate::taskbar;
use crate::util::{reg_dword, reg_string, wide, GIB};
use std::fmt::Write;
use windows::core::w;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;
use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, TreeScope_Descendants};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetClassNameW, SW_SHOWNORMAL};

pub fn run() {
    let _ = unsafe {
        windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
    };
    let mut out = String::new();
    let nt = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let _ = writeln!(out, "StatsOverlay diagnostics\n");
    let _ = writeln!(
        out,
        "Windows: {} {} build {}.{}",
        reg_string(HKEY_LOCAL_MACHINE, nt, "ProductName").unwrap_or_default(),
        reg_string(HKEY_LOCAL_MACHINE, nt, "DisplayVersion").unwrap_or_default(),
        reg_string(HKEY_LOCAL_MACHINE, nt, "CurrentBuild").unwrap_or_default(),
        reg_dword(HKEY_LOCAL_MACHINE, nt, "UBR").unwrap_or(0)
    );
    let _ = writeln!(
        out,
        "(ProductName says Windows 10 on Windows 11 too; the build number is what counts)\n"
    );

    let _ = writeln!(out, "== Taskbar ==");
    let _ = writeln!(out, "centered (TaskbarAl != 0): {}", taskbar::centered());
    let _ = writeln!(
        out,
        "widgets enabled (TaskbarDa != 0): {}",
        taskbar::widgets_enabled()
    );
    let _ = writeln!(
        out,
        "light theme (SystemUsesLightTheme): {}",
        crate::theme::system_light()
    );
    match taskbar::find() {
        None => {
            let _ = writeln!(out, "Shell_TrayWnd NOT FOUND");
        }
        Some(tb) => {
            let _ = writeln!(
                out,
                "Shell_TrayWnd {:?} rect {} dpi {}",
                tb.0,
                fmt_rect(&taskbar::rect(tb)),
                taskbar::dpi(tb)
            );
            let _ = writeln!(out, "\nChild windows (class, screen rect):");
            for (class, r) in child_windows(tb) {
                let _ = writeln!(out, "  {class:<50} {}", fmt_rect(&r));
            }
            let _ = writeln!(
                out,
                "\nUI Automation elements (name | automation id | class, screen rect):"
            );
            match uia_elements(tb) {
                Ok(els) => {
                    for (name, id, class, r) in els {
                        let _ = writeln!(out, "  {name} | {id} | {class}  {}", fmt_rect(&r));
                    }
                }
                Err(e) => {
                    let _ = writeln!(out, "  UI Automation failed: {e}");
                }
            }
        }
    }

    let _ = writeln!(out, "\n== Memory ==");
    let (total, avail) = memory_status();
    let _ = writeln!(
        out,
        "GlobalMemoryStatusEx: total {:.2} GiB, available {:.2} GiB, in use {:.2} GiB",
        total as f64 / GIB,
        avail as f64 / GIB,
        (total - avail) as f64 / GIB
    );
    let g = |b: u64| b as f64 / GIB;
    let special = crate::procs::special().unwrap_or_default();
    let _ = writeln!(
        out,
        "\nNtQuerySystemInformation (GiB, working set / committed private):"
    );
    for (name, m) in [
        ("vmmemWSL", special.wsl),
        ("vmmem", special.vmmem),
        ("Memory Compression", special.compression),
    ] {
        match m {
            Some(m) => {
                let _ = writeln!(
                    out,
                    "  {name}: {:.2} / {:.2}",
                    g(m.working_set),
                    g(m.private)
                );
            }
            None => {
                let _ = writeln!(out, "  {name}: not running");
            }
        }
    }
    let snap = crate::sampler::Snapshot {
        total,
        used: total - avail,
        vm: special.wsl_vm(),
        compressed: special.compression.map_or(0, |c| c.working_set),
        wsl_state: crate::sampler::WslState::Off,
        linux: None,
    };
    let _ = writeln!(
        out,
        "  -> strip shows: WIN {:.2}, WSL {:.2} (VM in RAM {:.2} + compressed share {:.2})",
        g(snap.windows()),
        g(snap.vm_ram()),
        g(snap.vm.map_or(0, |v| v.working_set)),
        g(snap.vm_compressed())
    );

    let mut all: Vec<(String, crate::procs::ProcMem)> = Vec::new();
    crate::procs::for_each(|n, m| all.push((String::from_utf16_lossy(n), m)));
    all.sort_by_key(|(_, m)| std::cmp::Reverse(m.working_set));
    let _ = writeln!(
        out,
        "\nTop 15 processes by working set (GiB, working set / committed private):"
    );
    for (n, m) in all.iter().take(15) {
        let _ = writeln!(
            out,
            "  {n:<32} {:.2} / {:.2}",
            g(m.working_set),
            g(m.private)
        );
    }

    let _ = writeln!(out, "\n== WSL distros ==");
    let vm_up = special.wsl_vm().is_some();
    if !vm_up {
        let _ = writeln!(
            out,
            "VmmemWSL not running: wsl.exe not called (it would boot the VM)"
        );
    } else {
        let distros = crate::wsl::running_distros();
        let _ = writeln!(out, "wsl --list --running --quiet: {distros:?}");
        let _ = writeln!(out, "read order: {:?}", crate::wsl::read_order(&distros));
        for d in crate::wsl::read_order(&distros) {
            match crate::wsl::meminfo(d) {
                None => {
                    let _ = writeln!(out, "meminfo via {d}: failed");
                }
                Some(m) => {
                    let _ = writeln!(
                        out,
                        "meminfo via {d}: total {:.2} GiB, available {:.2} GiB, used {:.2} GiB, buffers+cached {:.2} GiB",
                        m.total as f64 / GIB,
                        m.available as f64 / GIB,
                        m.used() as f64 / GIB,
                        m.cache as f64 / GIB
                    );
                }
            }
        }
    }

    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("statsoverlay-diag.txt")))
        .unwrap_or_else(|| "statsoverlay-diag.txt".into());
    if std::fs::write(&path, out).is_ok() {
        let p = wide(&path.to_string_lossy());
        unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                crate::util::pcwstr(&p),
                None,
                None,
                SW_SHOWNORMAL,
            );
        }
    }
}

fn fmt_rect(r: &RECT) -> String {
    format!(
        "({}, {}) - ({}, {})  {}x{}",
        r.left,
        r.top,
        r.right,
        r.bottom,
        r.right - r.left,
        r.bottom - r.top
    )
}

fn child_windows(parent: HWND) -> Vec<(String, RECT)> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let v = &mut *(lp.0 as *mut Vec<(String, RECT)>);
        let mut buf = [0u16; 256];
        let n = GetClassNameW(h, &mut buf) as usize;
        v.push((String::from_utf16_lossy(&buf[..n]), taskbar::rect(h)));
        BOOL(1)
    }
    let mut v: Vec<(String, RECT)> = Vec::new();
    unsafe {
        let _ = EnumChildWindows(Some(parent), Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

fn uia_elements(tb: HWND) -> windows::core::Result<Vec<(String, String, String, RECT)>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let root = uia.ElementFromHandle(tb)?;
        let all = root.FindAll(TreeScope_Descendants, &uia.CreateTrueCondition()?)?;
        let mut v = Vec::new();
        for i in 0..all.Length()? {
            let e = all.GetElement(i)?;
            v.push((
                e.CurrentName().map(|s| s.to_string()).unwrap_or_default(),
                e.CurrentAutomationId()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                e.CurrentClassName()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                e.CurrentBoundingRectangle().unwrap_or_default(),
            ));
        }
        Ok(v)
    }
}
