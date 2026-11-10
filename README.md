# power-monitor

A one-screen, card-based terminal UI for live laptop power on Linux, styled after jcode.

Cards: ⚡ power (now, 1-minute average, peak, history chart), 🔍 why (RAPL rail split,
top processes by CPU, hints), 🔋 battery (charge, time left, time from full, health),
⚙ profile (package deep idle, platform profile, EPP, governor, screen brightness).
No root needed.

## Efficiency

Written in Rust around a single `poll(2)` loop:

- one 1 s timerfd drives the live energy graph,
- sysfs and `/proc/<pid>/stat` files stay open and are re-read with `pread`,
- process CPU accounting runs every 5 s, matching its displayed interval,
- profile, EPP, governor and brightness values are cached for 10 s,
- AC plug/unplug and battery status changes arrive via kernel uevents,
- live RAPL counters provide the rail split when readable. Otherwise inotify
  watches the battery profiler (`/var/log/battery-profiler`) for new samples,
- under niri, the timer is disarmed while the workspace is hidden.
  Unknown/null tile geometry is not treated as hidden. Compositor events
  still wake the watcher, but no sampling runs on hidden workspaces.
  Power/profiler notifications are not polled while hidden,
- signalfd handles resize and quit; ratatui only redraws changed cells.

## Development checks

```sh
justrust test
justrust clippy --all-targets -- -D warnings
python3 scripts/smoke.py target/release/power-monitor
python3 scripts/measure.py target/release/power-monitor --seconds 20
```

The smoke check uses an isolated mock compositor stream and a real PTY to verify
hidden pause/resume, resize and terminal restoration. It does not move real
windows. Layout tests render populated cards at ten sizes, including small and
short terminals. `scripts/preview.py` converts their `LAYOUT_SPANS` JSON records
from `justrust test responsive_rendered_layouts -- --nocapture` into PNGs (Pillow
required only for this development helper).

In a representative 120×30 PTY run on the development laptop, the 5 s process
scan and cached configuration reduced CPU time from 198.7 ms to 47.5 ms and
read calls from 19,503 to 3,143 over 20 s. These are process measurements, not
whole-laptop watt savings, and exclude the terminal emulator's rendering cost.
The graph still samples each second. Rates vary with process count and activity.

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
`psys` energy counter: interval-average platform watts, including energy used
between reads. This is still a hardware estimate, not a calibrated whole-laptop
meter. Without it the graph falls back to the battery's smoothed sensor. Time-left
estimates always use battery discharge, which includes the screen.

```sh
sudo install -m644 70-rapl-energy-read.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger -s powercap -c add
```
