# v1 test checklist

Build with `cargo build --release`, run `target\release\statsoverlay.exe`.

## Strip

- **Screenshot A:** the strip. Three columns: WIN/ALL, WSL/CMT, CPU/GPU. Columns
  shouldn't jump sideways as values change.
- **CPU** vs Task Manager > Performance > CPU "Utilization". Same counter, so it
  should match within a few % (they sample at slightly different moments).
- **GPU** vs Task Manager > Performance > GPU, the "3D" graph of your main GPU.
- **CMT** vs Performance > Memory > "Committed" (x / y GB). CMT is x / y as a %.

## Hover panel

- **Screenshot B:** hover the strip. The panel opens above it and closes when the
  mouse leaves. It should follow light/dark mode too.
- Check: Windows + WSL VM = Memory in use. "VM committed" ≈ Details tab > vmmemWSL
  "Commit size" column. "autoMemoryReclaim" matches your `%USERPROFILE%\.wslconfig`.
- "CPU ... WSL n%" ≈ vmmemWSL's CPU in the Details tab.
- VRAM ≈ Performance > GPU > "Dedicated GPU memory".

## Actions (right-click)

- **Reclaim WSL cache:** enabled only while a distro runs. After clicking, within a
  few seconds the hover panel's Linux cache should drop, and over the next
  minutes the WSL VM figure should shrink as the VM hands memory back.
  Tell me the before/after numbers.
- **Shut down WSL…:** asks first (Cancel is the default). OK stops WSL and the strip
  shows `WSL off` within a few seconds.
- **Start with Windows:** toggling adds/removes `StatsOverlay` under
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (check with regedit or
  Task Manager > Startup apps). The checkmark reflects it.

## Footprint

- Task Manager > Processes: statsoverlay.exe memory and CPU after a couple of
  minutes. v0 was 2.8 MB / 0.1%. v1 brings PDH back for CPU and GPU (only those
  counters); tell me how much that costs. If it's a lot, GPU can move to the
  D3DKMT statistics Task Manager uses, at the cost of more code.
