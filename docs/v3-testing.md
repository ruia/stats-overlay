# v3 testing: device batteries

Build with `cargo build --release`, exit the running copy (right-click > Exit),
then start `target\release\statsoverlay.exe`.

## 1. Off by default

- On a fresh start nothing changes: no battery column, and Task Manager shows
  the same memory as before (~3.9 MB).
- Right-click > Display > Device batteries has **Show device batteries**
  unticked and a greyed hint under it.

## 2. Turning it on

- Tick **Show device batteries**. Within a few seconds the strip gets a column
  with one line per device ("MX 20%", "G515 86%"), followed by a receiver
  (USB dongle) or Bluetooth glyph. Every hover panel style gets a BATTERIES
  section; Insights shows them on its summary line.
- Compare with G HUB / Logi Options+: the % should match.
- The submenu now lists each device with its connection and %. Untick one: it
  disappears from the strip and panel, and stays hidden after a restart.
  Tick it again to bring it back.
- Untick **Show device batteries**: the column goes away. Memory goes back to
  where it was (the polling thread exits).

## 3. Charging and sleep

- Plug a Logitech mouse or keyboard into its cable: within a minute a bolt
  appears after the %, and the panel says "charging" (or "charged" when full,
  "plugged in" when it's on power but not charging).
- Leave a keyboard alone until it sleeps: its % turns grey and the panel says
  "asleep · last reading". Press a key: within a minute it's live again.
- Below 20 % the number turns yellow, below 10 % red; at 15 % or less (not
  plugged in) Insights lists "<name> battery at N%".

## 4. Bluetooth

- A Logitech device on Bluetooth (e.g. switch an MX mouse to its Bluetooth
  channel): it shows with the Bluetooth glyph and, unlike other Bluetooth
  devices, with the charging bolt. It's listed once, not twice.
- Earbuds, a headset or a controller that show a battery % in Settings >
  Bluetooth & devices: they show with the Bluetooth glyph and no bolt, only
  while connected. Disconnect them: they disappear within a minute.

## 5. Next to G HUB and Options+

Leave both running. Their own battery readings should keep working, and
StatsOverlay's numbers should keep updating.

## 6. Build without the feature

`cargo build --release --no-default-features`: no Device batteries submenu,
nothing else changes.

## Measured on the dev PC (Oct 2026)

| | exe | private working set, idle |
| --- | --- | --- |
| main (before) | 528,384 B | 3.90 MB |
| this branch, `--no-default-features` | 534,016 B | – |
| this branch, feature off | 568,832 B | 3.86 MB |
| this branch, feature on (2 devices) | 568,832 B | 4.25 MB |
