# v2 testing: disk, display options, WSL settings

Build with `cargo build --release`, exit the running copy (right-click > Exit),
then start `target\release\statsoverlay.exe`.

## 1. Display options (right-click > Display)

- Switch between **Grid**, **Windows / WSL rows**, **Bars** and **Compact**. The strip
  should change straight away and stay that way after Exit and restart.
- Untick CPU, GPU, Commit charge and Disk one by one; each should disappear.
  In Compact they are greyed out (memory only).
- Bars: blue is Windows, orange is WSL. The RAM bar's two parts should match
  WIN and WSL; the C: bar's orange part is the VHDX files.
- Screenshot: the strip in each of the four layouts.

## 2. Disk

Compare with:

- **C:** used/total: Explorer > This PC, or Settings > System > Storage.
- **VHD**: the size on disk of each `ext4.vhdx` (Explorer > right-click >
  Properties > "Size on disk", not "Size") and of
  `%LOCALAPPDATA%\Docker\wsl\disk\docker_data.vhdx`.
- **Linux used**: in a distro, `df -h /`.
- `statsoverlay.exe --diag` now has a Disk section listing every drive and VHDX
  it found. Send it if anything looks off or a distro is missing.

↓ (reclaimable) only appears once a distro has run while StatsOverlay was up,
and only if the gap is at least 1 GB.

Screenshot: the hover panel (it now has MEMORY, WSL, DISK and CPU · GPU
sections).

## 3. WSL settings (right-click > WSL settings)

- The ticks should match your `.wslconfig` (Open .wslconfig shows it in Notepad).
- Pick **Memory reclaim > Gradual** and **Memory limit > 24 GB**. Check that
  `.wslconfig` now has `autoMemoryReclaim=gradual` and `memory=24GB`, everything
  else unchanged, and that `.wslconfig.statsoverlay-backup` holds the original.
- With WSL running it asks to restart WSL. Say No the first time (nothing should
  happen), Yes the second (the WSL part goes to "off"). After you open a distro
  again, the hover panel's WSL VM figure should stay under the limit.
- **Memory limit > Custom…** opens a small box: type e.g. 20, OK, and check
  `memory=20GB` in the file. A value you set elsewhere (e.g. `42701MB` from the
  WSL Settings app) shows ticked as "Custom (41.7G)…".
- Set **Memory limit > Default** to remove the line again.

## 4. Taskbar at the top (only if you use ExplorerPatcher / StartAllBack)

The hover panel and the menu should open below the taskbar.

## 5. Memory and CPU/GPU without PDH

PDH is gone (it cost ~4 MB per counter set). Check:

- Task Manager > Processes > statsoverlay.exe Memory: expected around 3.5-4 MB.
- CPU: compare the strip with Task Manager > Performance > CPU "Utilization"
  while idle and under load (e.g. a build). A few points of difference is
  expected; tell me if it's consistently off by more than ~5.
- GPU: compare with Task Manager > Performance > GPU (the big % is the busiest
  engine) and "Dedicated GPU memory" against the panel's VRAM, e.g. while a
  video or game runs.
## 6. Tray icon (right-click > Display)

- **Tray icon only**: the strip disappears and a two-bar icon appears (blue =
  Windows RAM, orange = WSL RAM, second bar = commit). No "Mode" line in its
  panel.
- **Keep tray icon visible**: the icon moves out of ^ next to the clock
  (maybe after a few seconds; tell me if it needs an Explorer restart).
- **Taskbar and tray icon**: both show; hovering either opens the panel next to
  the one you hovered.
- Hover / left-click / right-click on the icon: panel / Task Manager / menu.
- Restart StatsOverlay: it comes back in the same mode.
- Display > untick Commit: the icon's second bar shows C: instead; untick
  Disk too and it shows just the RAM bar. The same toggles now also hide those
  rows/sections in the hover panel.

## 7. Memory after menus

Open the right-click menu, close it, and the WSL settings Custom… box: memory
should drop back to ~3.5-4 MB within a second instead of staying at ~7 MB.
