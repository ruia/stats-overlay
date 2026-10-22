//! Disk: the system drive, and the virtual disks (VHDX) that hold WSL distros and
//! Docker's data. All of it is file-system metadata, so it is cheap and never
//! touches WSL; only "Linux used" asks a running distro (`df`), and only every
//! DISK_INTERVAL.

use crate::util::{pcwstr, reg_dword, reg_string, reg_subkeys, wide};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use windows::Win32::Storage::FileSystem::{GetCompressedFileSizeW, GetDiskFreeSpaceExW};
use windows::Win32::System::Registry::HKEY_CURRENT_USER;

const LXSS: &str = r"Software\Microsoft\Windows\CurrentVersion\Lxss";
const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;

#[derive(Clone)]
pub struct Vhd {
    /// Distro name, or "Docker data" for Docker Desktop's data disk.
    pub name: String,
    pub path: PathBuf,
    /// Space the file takes on the drive (less than its length when sparse).
    pub on_disk: u64,
    pub sparse: bool,
    /// Used inside, from `df`; None when the distro isn't running or for Docker data.
    pub linux_used: Option<u64>,
}

impl Vhd {
    /// Space Linux has freed but the VHDX still holds. Only shrinks with compaction
    /// (or by itself with sparseVhd).
    pub fn reclaimable(&self) -> Option<u64> {
        self.linux_used.map(|u| self.on_disk.saturating_sub(u))
    }

    pub fn drive(&self) -> Option<char> {
        drive_of(&self.path)
    }
}

#[derive(Clone, Copy)]
pub struct Drive {
    pub letter: char,
    pub total: u64,
    pub free: u64,
}

impl Drive {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }
}

#[derive(Clone, Default)]
pub struct DiskSnap {
    /// The system drive first, then every other drive holding a VHDX.
    pub drives: Vec<Drive>,
    pub vhds: Vec<Vhd>,
}

impl DiskSnap {
    pub fn system(&self) -> Option<Drive> {
        self.drives.first().copied()
    }

    pub fn vhd_total(&self) -> u64 {
        self.vhds.iter().map(|v| v.on_disk).sum()
    }

    /// Sum of what is known to be reclaimable; None if nothing is known.
    pub fn reclaimable(&self) -> Option<u64> {
        let known: Vec<u64> = self.vhds.iter().filter_map(Vhd::reclaimable).collect();
        (!known.is_empty()).then(|| known.iter().sum())
    }

    /// Used space on `d` that isn't a WSL/Docker virtual disk.
    pub fn windows_used(&self, d: &Drive) -> u64 {
        let vhds: u64 = self
            .vhds
            .iter()
            .filter(|v| v.drive() == Some(d.letter))
            .map(|v| v.on_disk)
            .sum();
        d.used().saturating_sub(vhds)
    }
}

fn drive_of(path: &Path) -> Option<char> {
    let s = path.to_str()?;
    let c = s.chars().next()?.to_ascii_uppercase();
    (c.is_ascii_alphabetic() && s[1..].starts_with(':')).then_some(c)
}

fn drive(letter: char) -> Option<Drive> {
    let root = wide(&format!("{letter}:\\"));
    let (mut total, mut free) = (0u64, 0u64);
    unsafe {
        GetDiskFreeSpaceExW(pcwstr(&root), None, Some(&mut total), Some(&mut free)).ok()?;
    }
    Some(Drive {
        letter,
        total,
        free,
    })
}

fn vhd(name: String, path: PathBuf) -> Option<Vhd> {
    let meta = std::fs::metadata(&path).ok()?;
    let w = wide(path.to_str()?);
    let mut high = 0u32;
    let low = unsafe { GetCompressedFileSizeW(pcwstr(&w), Some(&mut high)) };
    let on_disk = if low == u32::MAX && high == 0 {
        meta.len()
    } else {
        (u64::from(high) << 32) | u64::from(low)
    };
    Some(Vhd {
        name,
        path,
        on_disk,
        sparse: meta.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0,
        linux_used: None,
    })
}

/// Registered WSL 2 distros, from HKCU\...\Lxss.
fn distros() -> Vec<Vhd> {
    let mut out = Vec::new();
    for id in reg_subkeys(HKEY_CURRENT_USER, LXSS) {
        let key = format!(r"{LXSS}\{id}");
        let get = |n: &str| reg_string(HKEY_CURRENT_USER, &key, n);
        if reg_dword(HKEY_CURRENT_USER, &key, "Version") == Some(1) {
            continue; // WSL 1 has no virtual disk.
        }
        let (Some(name), Some(base)) = (get("DistributionName"), get("BasePath")) else {
            continue;
        };
        let base = base.strip_prefix(r"\\?\").unwrap_or(&base).to_string();
        let file = get("VhdFileName").unwrap_or_else(|| "ext4.vhdx".into());
        if let Some(v) = vhd(name, Path::new(&base).join(file)) {
            out.push(v);
        }
    }
    out
}

/// Docker Desktop's data disk lives next to its distro but isn't registered.
fn docker_data(known: &[Vhd]) -> Option<Vhd> {
    let root = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join(r"Docker\wsl");
    [r"disk\docker_data.vhdx", r"data\ext4.vhdx"]
        .iter()
        .map(|p| root.join(p))
        .filter(|p| !known.iter().any(|v| v.path.eq(p)))
        .find_map(|p| vhd("Docker data".into(), p))
}

/// Reads drives and VHDX sizes. `linux_used` maps a running distro to its `df`.
pub fn read(linux_used: impl Fn(&str) -> Option<u64>) -> DiskSnap {
    let mut vhds = distros();
    if let Some(d) = docker_data(&vhds) {
        vhds.push(d);
    }
    for v in &mut vhds {
        v.linux_used = linux_used(&v.name);
    }
    let system = std::env::var("SystemDrive")
        .ok()
        .and_then(|s| s.chars().next())
        .unwrap_or('C')
        .to_ascii_uppercase();
    let mut letters = vec![system];
    for c in vhds.iter().filter_map(Vhd::drive) {
        if !letters.contains(&c) {
            letters.push(c);
        }
    }
    DiskSnap {
        drives: letters.into_iter().filter_map(drive).collect(),
        vhds,
    }
}
