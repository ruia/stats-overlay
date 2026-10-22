# StatsOverlay

A tiny resource readout embedded in the Windows 11 taskbar, showing how much RAM
(and later disk) Windows and WSL2 are using. Single portable .exe, no runtime.

Status: **v0**, proving taskbar placement with a RAM line:

```
WIN 9.1G   WSL 6.2G
ALL 15.3/31.7G
```

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
| `statsoverlay.exe --diag` | Writes `statsoverlay-diag.txt` next to the exe and opens it. |

Left-click opens Task Manager. Right-click shows the mode (embedded or fallback), what WSL polling sees,
Open Task Manager and Exit. Only one instance runs at a time.

## Layout

| File | Role |
| --- | --- |
| `src/main.rs` | Windows, message loop, placement, menu |
| `src/sampler.rs` | Sampler thread: GlobalMemoryStatusEx, VM and compression accounting |
| `src/procs.rs` | Per-process memory via NtQuerySystemInformation (what Task Manager uses) |
| `src/wsl.rs` | wsl.exe calls, only while the VM and a distro are already running |
| `src/render.rs` | Direct2D/DirectWrite into a premultiplied DIB, UpdateLayeredWindow |
| `src/taskbar.rs` | Finding the taskbar, alignment, Widgets setting, position |
| `src/theme.rs` | Light/dark from `SystemUsesLightTheme`, colors |
| `src/diag.rs` | `--diag` report |
| `build.rs`, `app.manifest` | Embeds the manifest through the MSVC linker |

See `docs/v0-review-and-testing.md` for the plan review and the v0 test checklist.
