# StatsOverlay

A tiny resource readout embedded in the Windows 11 taskbar, showing how much RAM
and disk Windows and WSL2 are using. Single portable .exe, no runtime.

Status: **v2** (disk, display options, WSL settings).

Four layouts, picked from right-click > Display:

```
Grid:      WIN 20.8G   ALL 50.0/63.9G   CPU 12%   C: 412/931G
           WSL 29.2G   CMT 94%          GPU  4%   VHD 86G ↓31G
By owner:  WIN  20.8G ram   326G disk   8% cpu    CMT 94%
           WSL  29.2G ram   86G ↓31G    4% cpu    GPU  4%
Bars:      RAM 45.0/63.9G   C: 692/838G   CMT 92%   CPU 14%
           [win|wsl---]     [win|wsl--]   [####-]   GPU  0%
Compact:   WIN 20.8G
           WSL 29.2G
```

CPU, GPU, commit and disk can each be hidden. Settings live in
`HKCU\Software\StatsOverlay`.

**Disk.** VHD is the space WSL's virtual disks (every WSL 2 distro's `ext4.vhdx`
and Docker Desktop's `docker_data.vhdx`) take on the drive. ↓ is what Linux has
freed inside them but the files still hold, known only for distros that ran since
StatsOverlay started (it asks `df` inside running distros, every 60 s). "Windows"
disk is the drive's used space minus those files.

**WSL settings.** Right-click > WSL settings edits `%USERPROFILE%\.wslconfig`:
`autoMemoryReclaim` (gradual / dropCache / disabled / not set) and the VM memory
limit (`[wsl2] memory`: default, presets, or Custom… for any whole number of GB). Other lines and comments are kept; the original is
copied once to `.wslconfig.statsoverlay-backup`. Changes apply when WSL restarts,
and it offers to restart it.

CMT is commit charge (committed / commit limit). Running out of commit, not RAM,
is what makes allocations fail on Windows, and the WSL VM can push it high.

Hover for the full breakdown: Windows vs WSL VM (in RAM, compressed, committed),
Linux used and cache, `autoMemoryReclaim` from `.wslconfig`, commit charge,
CPU (and WSL's share), GPU and VRAM.

## What the RAM numbers mean

- **ALL**: Task Manager's "In use" (total minus available).
- **WSL**: RAM the WSL VM really costs: VmmemWSL's working set plus its share of
  the Memory Compression store. Windows compresses the VM's idle pages into that
  store, so the VM's working set alone undercounts WSL (on a 64 GB machine we saw
  14 GB resident plus 14.6 GB compressed). The share is capped at the part of the
  VM's commit that isn't resident.
- **WIN**: ALL minus WSL, so the two add up to ALL.

The right-click menu also shows the VM's committed memory (what it holds anywhere,
page file included) and Linux's own used/cache figures.

## Build

Needs Rust with the MSVC toolchain (the default on Windows) and the Visual Studio
Build Tools (C++ workload).

```
cargo build --release
```

Output: `target\release\statsoverlay.exe`.

## Run

| Command | What it does |
| --- | --- |
| `statsoverlay.exe` | Embeds the strip in the taskbar (falls back to a topmost overlay if that fails). |
| `statsoverlay.exe --popup` | Forces the fallback overlay mode. |
| `statsoverlay.exe --offset 120` | Left offset in logical px, instead of the Widgets-button guess. |
| `statsoverlay.exe --no-cpu` / `--no-gpu` | Turns the PDH CPU or GPU counters off (to measure their memory cost). |
| `statsoverlay.exe --diag` | Writes `statsoverlay-diag.txt` next to the exe and opens it. |

Left-click opens Task Manager. Right-click: Reclaim WSL cache (drops Linux's page
cache, only while a distro runs), WSL settings, Shut down WSL (asks first),
Display, Open Task Manager, Start with Windows (HKCU Run key), Exit. Only one instance runs at a time.

## Layout

| File | Role |
| --- | --- |
| `src/main.rs` | Windows, message loop, placement, menu |
| `src/sampler.rs` | Sampler thread: GlobalMemoryStatusEx, VM and compression accounting |
| `src/metrics.rs` | PDH (CPU utility, GPU 3D engines, VRAM) and commit charge |
| `src/startup.rs` | Start with Windows |
| `src/config.rs` | Display settings (layout, shown columns) in the registry |
| `src/disk.rs` | Drives and WSL/Docker VHDX sizes |
| `src/wslconfig.rs` | Reading and editing `.wslconfig` |
| `src/prompt.rs` | The Custom… memory limit dialog (in-memory template) |
| `src/procs.rs` | Per-process memory via NtQuerySystemInformation (what Task Manager uses) |
| `src/wsl.rs` | wsl.exe calls, only while the VM and a distro are already running |
| `src/render.rs` | Direct2D/DirectWrite into a premultiplied DIB, UpdateLayeredWindow |
| `src/taskbar.rs` | Finding the taskbar, alignment, Widgets setting, position, top/bottom |
| `src/theme.rs` | Light/dark from `SystemUsesLightTheme`, colors |
| `src/diag.rs` | `--diag` report |
| `build.rs`, `app.manifest` | Embeds the manifest through the MSVC linker |

See `docs/v0-review-and-testing.md` for the plan review and the v0 test checklist.
