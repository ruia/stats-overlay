//! Sampler thread: collects a RAM snapshot every 2 s and pokes the UI thread.
//! The UI never calls into PDH; it only redraws from the latest snapshot.

use crate::wsl::{self, LinuxMem};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_LARGE, PDH_HCOUNTER, PDH_HQUERY,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

pub const WM_APP_SAMPLE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;
const INTERVAL: Duration = Duration::from_secs(2);
/// Longer than the time a distro waits idle before stopping, so polling never keeps
/// a distro (and with it the VM) alive after you close your terminals.
const WSL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq)]
pub enum WslState {
    /// No VmmemWSL: the VM is stopped. wsl.exe is never called in this state.
    Off,
    /// VM up, but no distro running yet (or not checked yet). It shuts down on its own.
    NoDistro,
    /// At least one distro running; `linux` holds its last meminfo read.
    Running,
}

#[derive(Clone, Copy)]
pub struct Snapshot {
    /// Usable physical memory (GlobalMemoryStatusEx ullTotalPhys).
    pub total: u64,
    /// ullTotalPhys - ullAvailPhys, should match Task Manager "In use".
    pub used: u64,
    /// Working set of VmmemWSL, None when the WSL VM is not running.
    pub wsl: Option<u64>,
    pub wsl_state: WslState,
    /// Linux's own view, refreshed every WSL_INTERVAL while a distro runs.
    pub linux: Option<LinuxMem>,
}

pub static LATEST: Mutex<Option<Snapshot>> = Mutex::new(None);

pub fn spawn(notify: HWND) {
    let notify = notify.0 as usize;
    std::thread::spawn(move || {
        let pdh = Pdh::open();
        let mut wsl_checked: Option<Instant> = None;
        let (mut wsl_state, mut linux) = (WslState::Off, None);
        loop {
            let mut snap = sample(pdh.as_ref());
            if snap.wsl.is_none() {
                // VM stopped: never call wsl.exe, it would boot the VM.
                (wsl_state, linux, wsl_checked) = (WslState::Off, None, None);
            } else if wsl_checked.is_none_or(|t| t.elapsed() >= WSL_INTERVAL) {
                wsl_checked = Some(Instant::now());
                let distros = wsl::running_distros();
                match wsl::pick(&distros) {
                    Some(d) => (wsl_state, linux) = (WslState::Running, wsl::meminfo(d).or(linux)),
                    None => (wsl_state, linux) = (WslState::NoDistro, None),
                }
            } else if wsl_state == WslState::Off {
                wsl_state = WslState::NoDistro;
            }
            (snap.wsl_state, snap.linux) = (wsl_state, linux);
            *LATEST.lock().unwrap() = Some(snap);
            unsafe {
                let _ = PostMessageW(Some(HWND(notify as _)), WM_APP_SAMPLE, WPARAM(0), LPARAM(0));
            }
            std::thread::sleep(INTERVAL);
        }
    });
}

fn sample(pdh: Option<&Pdh>) -> Snapshot {
    let (total, avail) = memory_status();
    let wsl = pdh
        .and_then(|p| p.collect())
        .and_then(|items| vmmem_working_set(&items));
    Snapshot {
        total,
        used: total.saturating_sub(avail),
        wsl,
        wsl_state: WslState::Off,
        linux: None,
    }
}

pub fn memory_status() -> (u64, u64) {
    let mut m = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GlobalMemoryStatusEx(&mut m);
    }
    (m.ullTotalPhys, m.ullAvailPhys)
}

/// VmmemWSL is the WSL2 VM. Plain "vmmem" is only used as a fallback (older WSL),
/// since on current systems it can also be Windows Sandbox or another Hyper-V VM.
pub fn vmmem_working_set(items: &[(String, i64)]) -> Option<u64> {
    let sum = |pred: &dyn Fn(&str) -> bool| {
        let v: Vec<i64> = items
            .iter()
            .filter(|(n, _)| pred(n))
            .map(|(_, v)| *v)
            .collect();
        (!v.is_empty()).then(|| v.iter().sum::<i64>().max(0) as u64)
    };
    // PDH names duplicates "vmmemWSL#1", so match the prefix.
    sum(&|n| n.to_ascii_lowercase().starts_with("vmmemwsl"))
        .or_else(|| sum(&|n| n.eq_ignore_ascii_case("vmmem")))
}

/// PDH query for `\Process(*)\<counter>`. Reading through PDH works even though
/// OpenProcess on vmmem is usually denied.
pub struct Pdh {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
}

impl Pdh {
    pub fn open() -> Option<Pdh> {
        Self::open_path(w!("\\Process(*)\\Working Set"))
    }

    pub fn open_path(path: windows::core::PCWSTR) -> Option<Pdh> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(None, 0, &mut query) != 0 {
                return None;
            }
            let mut counter = PDH_HCOUNTER::default();
            if PdhAddEnglishCounterW(query, path, 0, &mut counter) != 0 {
                return None;
            }
            Some(Pdh { query, counter })
        }
    }

    /// Collects once and returns (instance name, value) for every instance.
    pub fn collect(&self) -> Option<Vec<(String, i64)>> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_LARGE, &mut size, &mut count, None);
            if size == 0 {
                return Some(Vec::new());
            }
            // u64 storage keeps the item array 8-byte aligned.
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_LARGE,
                &mut size,
                &mut count,
                Some(items),
            ) != 0
            {
                return None;
            }
            let items = std::slice::from_raw_parts(items, count as usize);
            Some(
                items
                    .iter()
                    .map(|it| {
                        (
                            it.szName.to_string().unwrap_or_default(),
                            it.FmtValue.Anonymous.largeValue,
                        )
                    })
                    .collect(),
            )
        }
    }
}
