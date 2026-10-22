//! Sampler thread: collects a RAM snapshot every 2 s and pokes the UI thread.
//! The UI never queries anything itself; it only redraws from the latest snapshot.

use crate::procs::{self, ProcMem};
use crate::wsl::{self, LinuxMem};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
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
    /// ullTotalPhys - ullAvailPhys, matches Task Manager "In use".
    pub used: u64,
    /// The WSL VM, None when it is not running.
    pub vm: Option<ProcMem>,
    /// RAM held by the Memory Compression store.
    pub compressed: u64,
    pub wsl_state: WslState,
    /// Linux's own view, refreshed every WSL_INTERVAL while a distro runs.
    pub linux: Option<LinuxMem>,
}

impl Snapshot {
    /// RAM the VM really costs: its resident pages plus its share of the compression
    /// store. Windows compresses the VM's idle pages and moves them into the Memory
    /// Compression process, so its working set alone undercounts WSL. The store also
    /// holds other processes' pages; we attribute to the VM at most the part of its
    /// commit that isn't resident.
    pub fn vm_ram(&self) -> u64 {
        self.vm.map_or(0, |v| v.working_set + self.vm_compressed())
    }

    pub fn vm_compressed(&self) -> u64 {
        self.vm.map_or(0, |v| {
            self.compressed.min(v.private.saturating_sub(v.working_set))
        })
    }

    /// Everything in use that isn't the WSL VM.
    pub fn windows(&self) -> u64 {
        self.used.saturating_sub(self.vm_ram())
    }
}

pub static LATEST: Mutex<Option<Snapshot>> = Mutex::new(None);

pub fn spawn(notify: HWND) {
    let notify = notify.0 as usize;
    let _ = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut wsl_checked: Option<Instant> = None;
            let (mut wsl_state, mut linux) = (WslState::Off, None);
            loop {
                let mut snap = sample();
                if snap.vm.is_none() {
                    // VM stopped: never call wsl.exe, it would boot the VM.
                    (wsl_state, linux, wsl_checked) = (WslState::Off, None, None);
                } else if wsl_checked.is_none_or(|t| t.elapsed() >= WSL_INTERVAL) {
                    wsl_checked = Some(Instant::now());
                    let distros = wsl::running_distros();
                    (wsl_state, linux) = if distros.is_empty() {
                        (WslState::NoDistro, None)
                    } else {
                        (
                            WslState::Running,
                            wsl::meminfo_any(&distros).map(|(_, m)| m).or(linux),
                        )
                    };
                } else if wsl_state == WslState::Off {
                    wsl_state = WslState::NoDistro;
                }
                (snap.wsl_state, snap.linux) = (wsl_state, linux);
                *LATEST.lock().unwrap() = Some(snap);
                unsafe {
                    let _ =
                        PostMessageW(Some(HWND(notify as _)), WM_APP_SAMPLE, WPARAM(0), LPARAM(0));
                }
                std::thread::sleep(INTERVAL);
            }
        });
}

fn sample() -> Snapshot {
    let (total, avail) = memory_status();
    let special = procs::special().unwrap_or_default();
    Snapshot {
        total,
        used: total.saturating_sub(avail),
        vm: special.wsl_vm(),
        compressed: special.compression.map_or(0, |c| c.working_set),
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
