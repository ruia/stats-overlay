//! Talking to WSL through wsl.exe, without ever starting it or keeping it alive.
//!
//! Callers must only use this while VmmemWSL exists: `wsl.exe` starts the VM when
//! it is stopped. `--list --running` only asks the WSL service and starts nothing.
//! Commands run inside a distro reset that distro's idle timer, so they are made
//! only for distros that are already running, and rarely (see sampler::WSL_INTERVAL).

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Default)]
pub struct LinuxMem {
    pub total: u64,
    pub available: u64,
    /// Buffers + Cached.
    pub cache: u64,
}

impl LinuxMem {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }
}

/// Runs wsl.exe with a timeout and returns stdout, or None on failure.
fn wsl(args: &[&str]) -> Option<Vec<u8>> {
    let mut child = Command::new("wsl.exe")
        .args(args)
        // Makes wsl.exe's own output (e.g. --list) UTF-8 on WSL versions that support it.
        .env("WSL_UTF8", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = Vec::new();
                child.stdout.take()?.read_to_end(&mut out).ok()?;
                return status.success().then_some(out);
            }
            Ok(None) if start.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// wsl.exe's own messages are UTF-16LE unless WSL_UTF8 took effect.
fn decode(bytes: &[u8]) -> String {
    let utf16 = bytes.len() >= 2
        && bytes.chunks_exact(2).remainder().is_empty()
        && bytes.iter().skip(1).step_by(2).filter(|&&b| b == 0).count() * 2 > bytes.len() / 2;
    if utf16 {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Names of running distros. Empty when none run (wsl.exe then exits non-zero).
pub fn running_distros() -> Vec<String> {
    let Some(out) = wsl(&["--list", "--running", "--quiet"]) else {
        return Vec::new();
    };
    decode(&out)
        .lines()
        .map(|l| {
            l.trim_matches(|c: char| c.is_whitespace() || c == '\0')
                .to_string()
        })
        .filter(|l| !l.is_empty())
        .collect()
}

/// Distros to read meminfo through, best first. Any running one works, since all
/// distros share the VM's kernel. Docker Desktop's distro comes first: it runs for
/// as long as Docker Desktop does, so reading through it never keeps one of your
/// own distros from stopping when idle.
pub fn read_order(distros: &[String]) -> Vec<&str> {
    let mut v: Vec<&str> = distros.iter().map(|s| s.as_str()).collect();
    v.sort_by_key(|d| *d != "docker-desktop");
    v
}

/// meminfo through the first distro in `read_order` that answers.
pub fn meminfo_any(distros: &[String]) -> Option<(String, LinuxMem)> {
    read_order(distros)
        .into_iter()
        .find_map(|d| meminfo(d).map(|m| (d.to_string(), m)))
}

/// /proc/meminfo of the WSL VM, read through an already running distro.
pub fn meminfo(distro: &str) -> Option<LinuxMem> {
    let out = wsl(&["-d", distro, "-e", "cat", "/proc/meminfo"])?;
    parse_meminfo(&String::from_utf8_lossy(&out))
}

fn parse_meminfo(text: &str) -> Option<LinuxMem> {
    let field = |name: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.split(':').next() == Some(name))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    };
    Some(LinuxMem {
        total: field("MemTotal")?,
        available: field("MemAvailable")?,
        cache: field("Buffers").unwrap_or(0) + field("Cached").unwrap_or(0),
    })
}
