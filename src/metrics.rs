//! CPU, GPU and commit charge. Runs on the sampler thread only.
//!
//! CPU and GPU come from PDH, because that is where Task Manager's own numbers
//! live: "% Processor Utility" (frequency-adjusted, can exceed 100 with turbo) and
//! the per-engine GPU utilization. The query holds only these counters; the
//! per-process memory that used to go through PDH now uses procs.rs.

use windows::core::{w, PCWSTR};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhGetFormattedCounterValue, PdhOpenQueryW, PDH_FMT, PDH_FMT_COUNTERVALUE,
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
};
use windows::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};

/// Double, without PDH's cap at 100 (PDH_FMT_NOCAP100): utility goes above 100
/// under turbo and summed GPU engines can too; we clamp ourselves.
const FMT: PDH_FMT = PDH_FMT(PDH_FMT_DOUBLE.0 | 0x8000);

pub struct Pdh {
    query: PDH_HQUERY,
    cpu: Option<PDH_HCOUNTER>,
    gpu: Option<PDH_HCOUNTER>,
    vram: Option<PDH_HCOUNTER>,
}

#[derive(Clone, Copy, Default)]
pub struct Gpu {
    /// Busiest adapter's 3D engine utilization, 0..=100.
    pub percent: f64,
    /// Busiest adapter's dedicated memory in use.
    pub vram: Option<u64>,
}

impl Pdh {
    pub fn open() -> Option<Pdh> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(None, 0, &mut query) != 0 {
                return None;
            }
            let add = |path: PCWSTR| {
                let mut c = PDH_HCOUNTER::default();
                (PdhAddEnglishCounterW(query, path, 0, &mut c) == 0).then_some(c)
            };
            let p = Pdh {
                query,
                cpu: add(w!("\\Processor Information(_Total)\\% Processor Utility")),
                gpu: add(w!("\\GPU Engine(*engtype_3D)\\Utilization Percentage")),
                vram: add(w!("\\GPU Adapter Memory(*)\\Dedicated Usage")),
            };
            // Rate counters need a first sample to diff against.
            PdhCollectQueryData(query);
            Some(p)
        }
    }

    pub fn collect(&self) -> bool {
        unsafe { PdhCollectQueryData(self.query) == 0 }
    }

    /// Clamped to 100: utility exceeds 100 when the CPU runs above base clock.
    pub fn cpu(&self) -> Option<f64> {
        let mut v = PDH_FMT_COUNTERVALUE::default();
        let ok = unsafe { PdhGetFormattedCounterValue(self.cpu?, FMT, None, &mut v) } == 0;
        (ok && v.CStatus == 0).then(|| unsafe { v.Anonymous.doubleValue }.clamp(0.0, 100.0))
    }

    pub fn gpu(&self) -> Option<Gpu> {
        let engines = array(self.gpu?)?;
        // Instance: pid_1234_luid_0x00000000_0x0000D1F2_phys_0_eng_0_engtype_3D.
        // Sum all processes' 3D usage per adapter, report the busiest adapter.
        let mut per_adapter: Vec<(String, f64)> = Vec::new();
        for (name, v) in engines {
            let key = adapter_key(&name);
            match per_adapter.iter_mut().find(|(k, _)| *k == key) {
                Some((_, sum)) => *sum += v,
                None => per_adapter.push((key, v)),
            }
        }
        let percent = per_adapter
            .iter()
            .map(|(_, v)| *v)
            .fold(0.0, f64::max)
            .min(100.0);
        let vram = self
            .vram
            .and_then(array)
            .and_then(|a| a.iter().map(|(_, v)| *v).reduce(f64::max))
            .map(|v| v as u64);
        Some(Gpu { percent, vram })
    }
}

fn adapter_key(instance: &str) -> String {
    let start = instance.find("luid_").unwrap_or(0);
    let end = instance[start..]
        .find("_phys")
        .map_or(instance.len(), |e| start + e);
    instance[start..end].to_string()
}

fn array(counter: PDH_HCOUNTER) -> Option<Vec<(String, f64)>> {
    unsafe {
        let fmt = FMT;
        let (mut size, mut count) = (0u32, 0u32);
        PdhGetFormattedCounterArrayW(counter, fmt, &mut size, &mut count, None);
        if size == 0 {
            return Some(Vec::new());
        }
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
        if PdhGetFormattedCounterArrayW(counter, fmt, &mut size, &mut count, Some(items)) != 0 {
            return None;
        }
        let items = std::slice::from_raw_parts(items, count as usize);
        Some(
            items
                .iter()
                .filter(|it| it.FmtValue.CStatus == 0)
                .map(|it| {
                    (
                        it.szName.to_string().unwrap_or_default(),
                        it.FmtValue.Anonymous.doubleValue,
                    )
                })
                .collect(),
        )
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
