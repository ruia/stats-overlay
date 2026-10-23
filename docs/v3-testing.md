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

- Plug a Logitech mouse or keyboard into its cable: within a few seconds a slim green bolt
  appears after the %, about as tall as the connection glyph. The panel adds
  "charged" when full and "plugged in" when on power but not charging; while
  charging the bolt alone says so.
- Leave a keyboard alone until it sleeps: its % turns grey and the panel says
  "asleep · last reading". Press a key: within a few seconds it's live again (the receiver announces it).
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
| this branch, `--no-default-features` | 537,088 B | – |
| this branch, feature off | 572,928 B | 3.86 MB |
| this branch, feature on (2 devices) | 572,928 B | 4.25 MB |

(Memory measured on the first build of the branch; the bolt drawing added
about 4 KB to the exe since.)

## Results on the dev PC (Oct 2026)

G HUB and Logi Options+ running throughout; neither interfered, and G HUB
kept recording its own battery readings.

| Device | Link | Read via | Shown |
| --- | --- | --- | --- |
| MX Master 3S | Bolt receiver (C548), index 1 | HID++ 0x1004 at feature index 0x08 | 20%, then 20% + bolt on the cable (`14 02 01 01`) |
| G515 LS TKL | Lightspeed receiver (C547), index 1 | HID++ 0x1004 at 0x08 | 86%; greyed "asleep · last reading" when it slept; 85% + bolt on the cable (`55 08 01 01`). On the cable it stays on the receiver, no second entry |
| MX Master 3S | Bluetooth LE (B034), index 0xFF on the FF43 long collection | HID++ 0x1004 at 0x08 | 25%, then 30% + bolt, Bluetooth glyph; listed once |
| Nothing Ear (2) | Bluetooth | Windows' battery property (on its hands-free node; "connected" read from the device node) | 90%, Bluetooth glyph, no bolt |

- Show device batteries off → on → off: the polling thread exits and comes
  back, the column goes and returns, the setting is saved.
- Unticking a device hides it from the strip and the panel and is saved
  (`HiddenBatteries`); ticking it brings it back.
- Strip, and all five panel styles, checked in the dark theme.
- Updates come from the devices' own notifications: plugging and unplugging
  either device's cable flips the bolt within a few seconds; switching the MX
  from Bluetooth back to the Bolt dongle shows it there again right away.
  Both receivers already had wireless notifications on (register 0x00:
  `00 09 00` Bolt, `00 01 00` Lightspeed), so nothing was written.
- Idle with the feature on and the collections held open: ~3.0 MB private
  working set, ~0.05% CPU over a minute.
