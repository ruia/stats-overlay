//! CPU, GPU and commit charge, without PDH: loading PDH costs about 4 MB of
//! working set, more than the rest of the app. Runs on the sampler thread only.
//!
//! - CPU: system idle/kernel/user times, scaled by the current clock over the
//!   base clock to approximate Task Manager's "% Processor Utility".
//! - GPU: the kernel graphics statistics (D3DKMTQueryStatistics), the same source
//!   Task Manager reads: per-engine running time and per-segment memory.

use std::time::Instant;
use windows::Wdk::Graphics::Direct3D::{
    D3DKMTCloseAdapter, D3DKMTEnumAdapters2, D3DKMTQueryAdapterInfo, D3DKMTQueryStatistics,
    D3DKMT_ADAPTERINFO, D3DKMT_CLOSEADAPTER, D3DKMT_ENUMADAPTERS2, D3DKMT_QUERYADAPTERINFO,
    D3DKMT_QUERYSTATISTICS, D3DKMT_QUERYSTATISTICS_ADAPTER, D3DKMT_QUERYSTATISTICS_NODE,
    D3DKMT_QUERYSTATISTICS_SEGMENT, KMTQAITYPE_ADAPTERTYPE,
};
use windows::Win32::Foundation::{FILETIME, LUID};
use windows::Win32::System::Power::{
    CallNtPowerInformation, ProcessorInformation, PROCESSOR_POWER_INFORMATION,
};
use windows::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows::Win32::System::Threading::GetSystemTimes;

#[derive(Clone, Copy, Default)]
pub struct Gpu {
    /// Busiest engine of the busiest adapter, 0..=100 (Task Manager's GPU column).
    pub percent: f64,
    /// Dedicated memory in use on the adapter with the most of it.
    pub vram: Option<u64>,
}

fn ft(t: FILETIME) -> u64 {
    (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
}

pub struct Cpu {
    last: Option<(u64, u64)>,
    cpus: usize,
}

impl Cpu {
    pub fn new() -> Cpu {
        let mut c = Cpu {
            last: None,
            cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
        };
        c.sample(); // the first reading only sets the baseline
        c
    }

    /// % since the previous call, None on the first.
    pub fn sample(&mut self) -> Option<f64> {
        let (mut idle, mut kernel, mut user) = Default::default();
        unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).ok()? };
        // Kernel time includes idle time.
        let (idle, total) = (ft(idle), ft(kernel) + ft(user));
        let prev = self.last.replace((idle, total));
        let (pi, pt) = prev?;
        let dt = total.checked_sub(pt).filter(|&d| d > 0)?;
        let busy = dt.saturating_sub(idle.saturating_sub(pi)) as f64 / dt as f64;
        Some((busy * self.clock_ratio() * 100.0).clamp(0.0, 100.0))
    }

    /// Average current clock over base clock; above 1 under turbo, below when
    /// throttled. 1 when unknown.
    fn clock_ratio(&self) -> f64 {
        let mut info = vec![PROCESSOR_POWER_INFORMATION::default(); self.cpus];
        let size = (info.len() * size_of::<PROCESSOR_POWER_INFORMATION>()) as u32;
        let status = unsafe {
            CallNtPowerInformation(
                ProcessorInformation,
                None,
                0,
                Some(info.as_mut_ptr().cast()),
                size,
            )
        };
        if status.is_err() {
            return 1.0;
        }
        let (cur, max) = info.iter().fold((0u64, 0u64), |(c, m), p| {
            (c + u64::from(p.CurrentMhz), m + u64::from(p.MaxMhz))
        });
        if cur == 0 || max == 0 {
            1.0
        } else {
            cur as f64 / max as f64
        }
    }
}

struct Adapter {
    luid: LUID,
    nodes: u32,
    segments: u32,
    /// Per node: last running time (100 ns).
    last: Vec<i64>,
}

pub struct Gpus {
    adapters: Vec<Adapter>,
    last: Option<Instant>,
}

fn stats(q: &mut D3DKMT_QUERYSTATISTICS) -> bool {
    unsafe { D3DKMTQueryStatistics(q).is_ok() }
}

fn query(luid: LUID) -> D3DKMT_QUERYSTATISTICS {
    D3DKMT_QUERYSTATISTICS {
        AdapterLuid: luid,
        ..Default::default()
    }
}

/// Hardware adapters (no Basic Render Driver), as LUIDs.
fn adapters() -> Vec<LUID> {
    let mut e = D3DKMT_ENUMADAPTERS2::default();
    unsafe {
        if D3DKMTEnumAdapters2(&mut e).is_err() || e.NumAdapters == 0 {
            return Vec::new();
        }
        let mut list = vec![D3DKMT_ADAPTERINFO::default(); e.NumAdapters as usize];
        e.pAdapters = list.as_mut_ptr();
        if D3DKMTEnumAdapters2(&mut e).is_err() {
            return Vec::new();
        }
        list.truncate(e.NumAdapters as usize);
        list.iter()
            .filter_map(|a| {
                // D3DKMT_ADAPTERTYPE bit 2: SoftwareDevice.
                let mut kind = 0u32;
                let mut q = D3DKMT_QUERYADAPTERINFO {
                    hAdapter: a.hAdapter,
                    Type: KMTQAITYPE_ADAPTERTYPE,
                    pPrivateDriverData: (&mut kind as *mut u32).cast(),
                    PrivateDriverDataSize: 4,
                };
                let software = D3DKMTQueryAdapterInfo(&mut q).is_ok() && kind & 4 != 0;
                let _ = D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER {
                    hAdapter: a.hAdapter,
                });
                (!software).then_some(a.AdapterLuid)
            })
            .collect()
    }
}

impl Gpus {
    pub fn open() -> Option<Gpus> {
        let adapters: Vec<Adapter> = adapters()
            .into_iter()
            .filter_map(|luid| {
                let mut q = query(luid);
                q.Type = D3DKMT_QUERYSTATISTICS_ADAPTER;
                stats(&mut q).then(|| {
                    let a = unsafe { q.QueryResult.AdapterInformation };
                    Adapter {
                        luid,
                        nodes: a.NodeCount,
                        segments: a.NbSegments,
                        last: Vec::new(),
                    }
                })
            })
            .collect();
        if adapters.is_empty() {
            return None;
        }
        let mut g = Gpus {
            adapters,
            last: None,
        };
        g.sample();
        Some(g)
    }

    fn running_times(a: &Adapter) -> Vec<i64> {
        (0..a.nodes)
            .map(|n| {
                let mut q = query(a.luid);
                q.Type = D3DKMT_QUERYSTATISTICS_NODE;
                q.Anonymous.QueryNode.NodeId = n;
                if stats(&mut q) {
                    unsafe { q.QueryResult.NodeInformation.GlobalInformation.RunningTime }
                } else {
                    0
                }
            })
            .collect()
    }

    /// Dedicated (non-aperture) memory committed, as Task Manager and System
    /// Informer count it.
    fn dedicated(a: &Adapter) -> u64 {
        (0..a.segments)
            .filter_map(|s| {
                let mut q = query(a.luid);
                q.Type = D3DKMT_QUERYSTATISTICS_SEGMENT;
                q.Anonymous.QuerySegment.SegmentId = s;
                stats(&mut q).then_some(unsafe { q.QueryResult.SegmentInformation })
            })
            .filter(|seg| seg.Aperture == 0)
            .map(|seg| seg.BytesCommitted)
            .sum()
    }

    /// Utilization since the previous call (None on the first) and VRAM now.
    pub fn sample(&mut self) -> Option<Gpu> {
        let now = Instant::now();
        let wall = self
            .last
            .replace(now)
            .map(|t| now.duration_since(t).as_nanos() as f64 / 100.0);
        let mut percent = 0f64;
        let mut vram = None::<u64>;
        for a in &mut self.adapters {
            let times = Self::running_times(a);
            if let Some(wall) = wall.filter(|&w| w > 0.0) {
                for (t, p) in times.iter().zip(&a.last) {
                    let busy = (t - p).max(0) as f64 / wall * 100.0;
                    percent = percent.max(busy);
                }
            }
            a.last = times;
            let d = Self::dedicated(a);
            vram = Some(vram.map_or(d, |v| v.max(d)));
        }
        wall.map(|_| Gpu {
            percent: percent.min(100.0),
            vram,
        })
    }
}

/// (committed, limit) in bytes. Running out of commit, not RAM, is what makes
/// allocations fail on Windows.
pub fn commit() -> Option<(u64, u64)> {
    let mut pi = PERFORMANCE_INFORMATION {
        cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
        ..Default::default()
    };
    unsafe { GetPerformanceInfo(&mut pi, pi.cb).ok()? };
    let page = pi.PageSize as u64;
    Some((pi.CommitTotal as u64 * page, pi.CommitLimit as u64 * page))
}
