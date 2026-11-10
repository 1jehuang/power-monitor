# power-monitor

A one-screen, card-based terminal UI for live laptop power on Linux, styled after jcode.

Shows whole-laptop draw (now, 1-minute average, peak, history chart), battery charge,
time left and time from full, package deep-idle residency, and the power profile
(platform profile, EPP, governor, screen brightness). Reads sysfs once per second and needs no root.

## Install

```sh
install -m755 power-monitor ~/.local/bin/power-monitor
install -m644 power-monitor.desktop ~/.local/share/applications/
```

The desktop entry opens it in kitty. Ctrl-C quits.
