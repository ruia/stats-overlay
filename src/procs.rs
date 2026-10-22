//! Process memory through NtQuerySystemInformation(SystemProcessInformation), the
//! call Task Manager itself uses. One syscall returns every process, including
//! VmmemWSL and Memory Compression, which OpenProcess can't open. Much lighter than
//! PDH, which loads the performance-counter stack and keeps buffers for every process.

use std::sync::atomic::{AtomicU32, Ordering};
use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};
use windows::Win32::Foundation::STATUS_INFO_LENGTH_MISMATCH;
use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION;

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct ProcMem {
    /// Resident in RAM (Task Manager "Working set").
    pub working_set: u64,
    /// Committed private memory, wherever it is: RAM, compression store or page file.
    pub private: u64,
    /// User + kernel CPU time, in 100 ns units.
    pub cpu_time: u64,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Special {
    /// The WSL2 VM.
    pub wsl: Option<ProcMem>,
    /// Plain "vmmem": the WSL VM on older WSL, otherwise another Hyper-V VM.
    pub vmmem: Option<ProcMem>,
    /// The compression store. Its working set is the RAM holding compressed pages.
    pub compression: Option<ProcMem>,
}

impl Special {
    /// The WSL VM, falling back to plain vmmem only when no VmmemWSL exists.
    pub fn wsl_vm(&self) -> Option<ProcMem> {
        self.wsl.or(self.vmmem)
    }
}

/// Buffer size that worked last time, so the steady state is one syscall.
static BUF_SIZE: AtomicU32 = AtomicU32::new(512 * 1024);

/// Calls `f(image name, memory)` for every process.
pub fn for_each(mut f: impl FnMut(&[u16], ProcMem)) -> bool {
    for _ in 0..4 {
        let size = BUF_SIZE.load(Ordering::Relaxed);
        // Large allocations come straight from VirtualAlloc and go back on drop, so
        // the buffer doesn't stay in our working set between samples.
        let mut buf: Vec<u64> = Vec::with_capacity(size as usize / 8);
        let mut needed = 0u32;
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessInformation,
                buf.as_mut_ptr().cast(),
                size,
                &mut needed,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH {
            // Processes come and go between calls: leave headroom.
            BUF_SIZE.store(needed.max(size) + 64 * 1024, Ordering::Relaxed);
            continue;
        }
        if status.is_err() {
            return false;
        }
        let base = buf.as_ptr() as *const u8;
        let mut offset = 0usize;
        loop {
            let p = unsafe { &*(base.add(offset) as *const SYSTEM_PROCESS_INFORMATION) };
            let name = if p.ImageName.Buffer.is_null() {
                &[][..]
            } else {
                unsafe {
                    std::slice::from_raw_parts(
                        p.ImageName.Buffer.0,
                        p.ImageName.Length as usize / 2,
                    )
                }
            };
            // Reserved1 is WorkingSetPrivateSize, HardFaultCount, NumberOfThreadsHighWatermark,
            // CycleTime, CreateTime, UserTime, KernelTime (stable layout since Windows 7).
            let time = |at: usize| u64::from_le_bytes(p.Reserved1[at..at + 8].try_into().unwrap());
            f(
                name,
                ProcMem {
                    working_set: p.WorkingSetSize as u64,
                    private: p.PrivatePageCount as u64,
                    cpu_time: time(32) + time(40),
                },
            );
            if p.NextEntryOffset == 0 {
                return true;
            }
            offset += p.NextEntryOffset as usize;
        }
    }
    false
}

/// ASCII case-insensitive compare of a UTF-16 name with `s`, without allocating.
fn name_is(name: &[u16], s: &str) -> bool {
    name.len() == s.len()
        && name
            .iter()
            .zip(s.bytes())
            .all(|(&a, b)| a < 128 && (a as u8).eq_ignore_ascii_case(&b))
}

pub fn special() -> Option<Special> {
    let mut s = Special::default();
    let add = |slot: &mut Option<ProcMem>, m: ProcMem| {
        let e = slot.get_or_insert_default();
        e.working_set += m.working_set;
        e.private += m.private;
        e.cpu_time += m.cpu_time;
    };
    let ok = for_each(|name, m| {
        if name_is(name, "vmmemWSL") {
            add(&mut s.wsl, m);
        } else if name_is(name, "vmmem") {
            add(&mut s.vmmem, m);
        } else if name_is(name, "Memory Compression") {
            add(&mut s.compression, m);
        }
    });
    ok.then_some(s)
}
