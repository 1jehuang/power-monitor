# power-monitor

A one-screen, card-based terminal UI for live laptop power on Linux, styled after jcode.

Cards: ⚡ power (now, 1-minute average, peak, history chart), 🔍 why (RAPL rail split,
top processes by CPU, hints), 🔋 battery (charge, time left, time from full, health),
⚙ profile (package deep idle, platform profile, EPP, governor, screen brightness).
No root needed.

## Efficiency

Written in Rust around a single `poll(2)` loop:

- one 1 s timerfd (with 50 ms timer slack) is the only periodic wakeup,
- sysfs and `/proc/<pid>/stat` files stay open and are re-read with `pread`,
- AC plug/unplug and battery status changes arrive via kernel uevents,
- the RAPL split is re-read only when inotify sees the battery profiler
  (`/var/log/battery-profiler`) write a sample,
- under niri, the timer is disarmed while the window's workspace is hidden,
- signalfd handles resize and quit; ratatui only redraws changed cells.

## Install

```sh
cargo build --release
install -m755 target/release/power-monitor ~/.local/bin/power-monitor
install -m644 power-monitor.desktop ~/.local/share/applications/
```

The desktop entry opens it in kitty (`--class power-monitor`, used for the
visibility check). `q`, Esc or Ctrl-C quits. The original Python version is
kept in `legacy/`.

## Live power from RAPL (recommended)

`energy_uj` is root-only by default. `70-rapl-energy-read.rules` makes it
readable by your group (edit the group name), after which the graph uses the
`psys` energy counter: exact average watts over each second, spikes included.
Without it the graph falls back to the battery's smoothed sensor. Time-left
estimates always use battery discharge, which includes the screen.

```sh
sudo install -m644 70-rapl-energy-read.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger -s powercap -c add
```
