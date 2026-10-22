//! Sampler thread: collects a snapshot every 2 s and pokes the UI thread.
//! The UI never queries anything itself; it only redraws from the latest snapshot.

use crate::metrics::{self, Gpu};
use crate::procs::{self, ProcMem};
use crate::wsl::{self, LinuxMem, Reclaim};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

pub const WM_APP_SAMPLE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;
const INTERVAL: Duration = Duration::from_secs(2);
/// Longer than the time a distro waits idle before stopping, so polling never keeps
/// a distro (and with it the VM) alive after you close your terminals.
const WSL_INTERVAL: Duration = Duration::from_secs(30);

/// Set after a WSL action (reclaim, shutdown) so the next sample re-reads WSL.
static WSL_RECHECK: AtomicBool = AtomicBool::new(false);
static SAMPLER: OnceLock<std::thread::Thread> = OnceLock::new();

/// Re-reads WSL right away instead of at the next 30 s check.
pub fn request_wsl_recheck() {
    WSL_RECHECK.store(true, Ordering::Relaxed);
    if let Some(t) = SAMPLER.get() {
        t.unpark();
    }
}

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
    pub reclaim: Reclaim,
    /// (committed, limit).
    pub commit: Option<(u64, u64)>,
    /// Task Manager's CPU %, None until the second sample.
    pub cpu: Option<f64>,
    /// CPU used by the WSL VM, % of all logical processors.
    pub wsl_cpu: Option<f64>,
    pub gpu: Option<Gpu>,
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
            let _ = SAMPLER.set(std::thread::current());
            let pdh = metrics::Pdh::open();
            let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
            let mut wsl_checked: Option<Instant> = None;
            let mut reclaim_checked: Option<Instant> = None;
            let (mut wsl_state, mut linux, mut reclaim) = (WslState::Off, None, Reclaim::NotSet);
            let mut last_vm_cpu: Option<(Instant, u64)> = None;
            loop {
                let mut snap = sample();
                if let Some(p) = &pdh {
                    if p.collect() {
                        snap.cpu = p.cpu();
                        snap.gpu = p.gpu();
                    }
                }
                let now = Instant::now();
                snap.wsl_cpu = match (snap.vm, last_vm_cpu) {
                    (Some(vm), Some((t, cpu))) if vm.cpu_time >= cpu => {
                        let wall = now.duration_since(t).as_nanos() as f64 / 100.0;
                        Some(((vm.cpu_time - cpu) as f64 / (wall * cpus) * 100.0).clamp(0.0, 100.0))
                    }
                    _ => None,
                };
                last_vm_cpu = snap.vm.map(|vm| (now, vm.cpu_time));

                let recheck = WSL_RECHECK.swap(false, Ordering::Relaxed);
                if snap.vm.is_none() {
                    // VM stopped: never call wsl.exe, it would boot the VM.
                    (wsl_state, linux, wsl_checked) = (WslState::Off, None, None);
                } else if recheck || wsl_checked.is_none_or(|t| t.elapsed() >= WSL_INTERVAL) {
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
                if recheck || reclaim_checked.is_none_or(|t| t.elapsed() >= WSL_INTERVAL) {
                    // A small file read; .wslconfig changes rarely.
                    reclaim_checked = Some(now);
                    reclaim = wsl::auto_memory_reclaim();
                }
                (snap.wsl_state, snap.linux, snap.reclaim) = (wsl_state, linux, reclaim);
                *LATEST.lock().unwrap() = Some(snap);
                unsafe {
                    let _ =
                        PostMessageW(Some(HWND(notify as _)), WM_APP_SAMPLE, WPARAM(0), LPARAM(0));
                }
                // request_wsl_recheck() unparks us early.
                if !WSL_RECHECK.load(Ordering::Relaxed) {
                    std::thread::park_timeout(INTERVAL);
                }
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
        reclaim: Reclaim::NotSet,
        commit: metrics::commit(),
        cpu: None,
        wsl_cpu: None,
        gpu: None,
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
