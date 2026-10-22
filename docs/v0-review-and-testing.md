# v0: plan review and test checklist

## Plan review

The plan is sound. These are the changes and risks I found, most important first.
Items marked **(done)** are already handled in the v0 code.

1. **Broadcasts never reach a child window (done).** `TaskbarCreated` and
   `WM_SETTINGCHANGE` are sent only to top-level windows, so the strip, being a
   child of the taskbar, would never hear about an Explorer restart or a theme
   change. v0 has a hidden, unowned top-level "host" window that receives them
   (it must not be a message-only window, which also misses broadcasts).
2. **A stall in our UI thread can freeze the taskbar.** A child window owned by
   another process shares input processing with Explorer. The one-sampler-thread
   rule in the plan becomes a hard rule: nothing slow on the UI thread, including
   waiting on `wsl.exe` in v1.
3. **Layered child windows need the Windows 8+ manifest entry (done).** Without
   the `supportedOS` entry, `WS_EX_LAYERED` on a child silently fails. Two related
   details: fully transparent pixels are click-through, so the background is drawn
   at alpha 1/255 to keep clicks between words working; and ClearType can't be used
   over a transparent background, so text uses grayscale antialiasing.
4. **Polling `wsl.exe` every 5 s would keep WSL running forever (done).** Each call
   starts a process inside a distro, which resets the distro's idle timer, so the
   VM never reaches its own idle shutdown. Each spawn also costs some CPU, which
   works against ~0% idle. Suggestion: call `wsl.exe` only when a distro is
   already running (`wsl --list --running`), target that distro with `-d`, and
   poll less often than the idle timeout (30 s or more) or only while the hover
   panel is open. The same applies to the 60 s `df` poll. I'm confident about the
   mechanism but quoting the timeout values from memory; v1 testing will show
   whether VmmemWSL still disappears when WSL is idle. Now implemented in
   `src/wsl.rs` and the sampler: no VmmemWSL means wsl.exe is never called; with
   the VM up, `wsl --list --running --quiet` runs at most every 30 s, and meminfo
   is read only through a distro that is already running.
5. **The 5 MB working-set target is tight with Direct2D/DirectWrite.** Loading
   those DLLs alone probably puts the full working set at 6 to 10 MB, though most
   of that is shared DLL pages. The meaningful budget number is
   "Memory (private working set)" in Task Manager. v0 uses a software render
   target (no Direct3D device), which saves several MB. Please report both numbers.
   First measurement: 8.9 MB. PDH was then replaced with a single
   NtQuerySystemInformation call (the source Task Manager uses), so pdh.dll and
   the performance-counter stack are no longer loaded.
6. **`vmmem` isn't always WSL.** On current systems a process named `vmmem` can be
   Windows Sandbox or another Hyper-V VM. v0 reads `vmmemWSL*` and falls back to
   plain `vmmem` only when no `vmmemWSL` exists.
7. **wsl.exe encoding (v1).** Only wsl.exe's own output (`--list`, errors) is
   UTF-16LE. Output of a Linux command run with `-e` is that command's bytes
   (UTF-8). Setting `WSL_UTF8=1` in the child's environment makes `--list` UTF-8 too.
8. **Docker reclaimable space (v2).** `df /` in a user distro doesn't see Docker's
   `docker_data.vhdx`. Its "Linux used" needs `wsl -d docker-desktop`, only when
   that distro is running; otherwise show its size only.
9. **Widgets button width.** There is no API for it. `TaskbarDa` in the registry
   says whether it's on. v0 reserves 180 logical px as a guess (`--offset`
   overrides it), and `--diag` dumps the real bounds of the Widgets and Start
   buttons through UI Automation, so we can pick a fixed value or query it at
   layout time in v1.
10. **Embedding is unsupported by Microsoft.** It works today (TrafficMonitor relies
    on it) but any Explorer update can break it. v0 creates the window directly as
    a child of `Shell_TrayWnd`, which is equivalent to `SetParent` and skips the
    popup-to-child style swap. The fallback overlay is there for that case.
11. **Manifest embedding without a build dependency (done).** `build.rs` passes
    `/MANIFEST:EMBED /MANIFESTINPUT:app.manifest` to the MSVC linker, so there are
    zero build dependencies. This needs the MSVC toolchain, which is the default.
    The only runtime dependency is the `windows` crate (plus its `windows-numerics`
    helper for one vector type).
12. **Sparse VHD.** I couldn't verify the "re-enabled as experimental in WSL 3.0.2"
    claim. It doesn't matter for this project, since sparse status is display-only.
13. **Expect "32G" to read 31.x.** `ullTotalPhys` is usable memory, slightly less
    than the installed amount Task Manager shows at the top of the Memory page.

## v0 test checklist

Build with `cargo build --release`. Please report the size of
`target\release\statsoverlay.exe`.

### 1. Diagnostics

Open a WSL terminal first so the VM is running, then run
`statsoverlay.exe --diag`. It writes `statsoverlay-diag.txt` next to the exe and
opens it. **Send me that file.** It answers the three "verify early" questions:
your Windows build, the exact PDH instance name for VmmemWSL and its values, and
the Widgets/Start button bounds.

### 2. Placement and theme

Run `statsoverlay.exe` (no arguments).

- **Screenshot A:** the left half of the taskbar, dark theme.
- **Screenshot B:** the same in light mode (Settings > Personalization > Colors >
  Choose your mode). The strip should switch colors within a couple of seconds.
- **Screenshot C:** the right-click menu. Its first line says whether it's
  "embedded in taskbar" or "overlay (fallback)". Embedded is what we want.
- **Screenshots D, E:** Widgets on and off (Settings > Personalization > Taskbar).
  If the strip overlaps the Widgets button or sits too far right, tell me and try
  `--offset <px>` to find a value that looks right.

### 3. Numbers against Task Manager

- **ALL** (used / total) vs Performance > Memory > "In use". Should match within
  about 0.1 G. Total will be slightly under the installed amount (see item 13).
- **WSL** vs Details tab > `VmmemWSL`. Right-click a column header > Select
  columns > tick "Working set (memory)". The strip should match that column. Tell
  me both that value and the default "Memory (active private working set)" value,
  because they can differ a lot for a VM process.
- **WIN** should equal ALL minus WSL.
- Run `wsl --shutdown`. Within about 2 s the strip should show `WSL off` and
  WIN should equal ALL.
- **Screenshot F:** Task Manager Details tab next to the strip.

### 4. Robustness

- **Explorer restart:** Task Manager > Processes > Windows Explorer > Restart. The
  strip should come back by itself within a few seconds. Tell me if it doesn't, or
  if it comes back in fallback mode (check the right-click menu).
- **Scaling (optional):** change display scaling (for example 100% to 125%). The
  strip should resize and stay readable. **Screenshot G.**
- **Clicks:** left-click opens Task Manager, including when clicking in the gap
  between words.
- **Single instance:** starting the exe a second time does nothing.

### 5. Footprint

After a minute or two of running, from Task Manager's Details tab for
`statsoverlay.exe`: CPU, "Working set (memory)" and "Memory (private working
set)". Also check that the taskbar itself stays responsive.

### 6. Fallback mode

Exit the strip, run `statsoverlay.exe --popup`. **Screenshot H.** It should sit in
the same place, above the taskbar, and hide while a fullscreen app or game runs.

### 7. WSL polling never keeps WSL alive

The strip doesn't show Linux's numbers yet (that's v1's hover panel), but the
right-click menu has a second line with what the sampler sees:
`WSL: off`, `WSL: VM up, no distro running (not polled)`, or
`Linux: used X, cache Y, VM holds Z (other W)`.

- With a WSL terminal open, wait up to 30 s, then **screenshot the menu (I)**.
  Compare "used" with `free -h` in that terminal.
- Close every WSL terminal and leave the strip running. Within a couple of
  minutes the menu should go to "no distro running", then `WSL: off`, and
  VmmemWSL should disappear from Task Manager. **Tell me how long that took**, and
  whether it ever doesn't happen. That is the proof the polling doesn't keep WSL
  alive.
- With WSL off, nothing should start it: VmmemWSL must not reappear on its own.
- `--diag` now also lists running distros and the meminfo read (only when the VM
  is already up).
