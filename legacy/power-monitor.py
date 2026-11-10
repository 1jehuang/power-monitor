#!/usr/bin/env python3
"""Live power monitor TUI, styled after jcode's info widgets.

Rounded dim borders with labels on the border, ▰▱ meters, jcode's
green/yellow/red thresholds. Reads sysfs once per second; no root needed.
"""
import glob, os, shutil, signal, sys, time, unicodedata
from collections import deque

BAT = (glob.glob('/sys/class/power_supply/BAT*') or [''])[0]
LPI = '/sys/devices/system/cpu/cpuidle/low_power_idle_cpu_residency_us'

def fg(r, g, b): return f'\033[38;2;{r};{g};{b}m'
RST, BOLD = '\033[0m', '\033[1m'
BORDER = fg(70, 70, 80)
LABEL = BOLD + fg(175, 175, 190)
DIM = fg(125, 125, 140)
MUTED = fg(140, 140, 150)
TEXT = fg(200, 200, 210)
EMPTY = fg(50, 50, 60)
GREEN, YELLOW, RED = fg(100, 200, 100), fg(255, 200, 100), fg(255, 100, 100)
BLUE, PINK = fg(140, 180, 255), fg(255, 135, 200)

def rd(p, d=None):
    try:
        with open(p) as f: return f.read().strip()
    except OSError: return d

def num(p):
    v = rd(p)
    try: return int(v)
    except (TypeError, ValueError): return None

def width(s):
    """Display cells, ignoring ANSI escapes."""
    out, i = 0, 0
    while i < len(s):
        if s[i] == '\033':
            j = s.find('m', i)
            i = j + 1 if j >= 0 else len(s)
            continue
        out += 2 if unicodedata.east_asian_width(s[i]) in 'WF' else 1
        i += 1
    return out

def good_high(p):  # higher is better
    return GREEN if p > 50 else YELLOW if p > 20 else RED

def meter(pct, cells, color):
    n = round(max(0, min(100, pct)) / 100 * cells)
    return color + '▰' * n + EMPTY + '▱' * (cells - n) + RST

def hm(h):
    if h is None or h <= 0 or h > 99: return '--'
    return f'{int(h)}h {int(h % 1 * 60):02d}m'

def clip(s, cells):
    """Truncate an ANSI string to `cells` display cells, with an ellipsis."""
    if width(s) <= cells: return s
    out, used, i = '', 0, 0
    while i < len(s) and used < cells - 1:
        if s[i] == '\033':
            j = s.find('m', i); out += s[i:j + 1]; i = j + 1; continue
        cw = 2 if unicodedata.east_asian_width(s[i]) in 'WF' else 1
        if used + cw > cells - 1: break
        out += s[i]; used += cw; i += 1
    return out + '…' + RST

def box(w, title, right, rows, footer='', h=None):
    """Rounded jcode-style card, exactly w cells wide and h rows tall."""
    inner = w - 2
    if h is not None:
        rows = (rows + [''] * h)[:max(0, h - 2)]
    rows = [clip(r, inner - 2) for r in rows]
    def edge(l, r, left, right_txt, a, b):
        left = f' {left} ' if left else ''
        right_txt = f' {right_txt} ' if right_txt else ''
        fill = inner - 1 - width(left) - width(right_txt) - (1 if right_txt else 0)
        if fill < 0: right_txt, fill = '', inner - 1 - width(left)
        return (BORDER + a + '─' + RST + left + BORDER + '─' * max(0, fill) + RST
                + right_txt + BORDER + ('─' if right_txt else '') + b + RST)
    out = [edge('', '', LABEL + title + RST if title else '', right, '╭', '╮')]
    for r in rows:
        pad = inner - 1 - width(r)
        out.append(BORDER + '│' + RST + ' ' + r + ' ' * max(0, pad) + BORDER + '│' + RST)
    out.append(edge('', '', footer, '', '╰', '╯'))
    return out

def chart(hist, cells, rows):
    """Multi-row bar chart of watts, newest on the right."""
    vals = list(hist)[-cells:]
    rows = max(1, rows)
    if not vals: return [''] * rows
    top = max(max(vals), 1.0)
    out = []
    for r in range(rows - 1, -1, -1):
        s = ' ' * (cells - len(vals))
        for v in vals:
            eighths = v / top * rows * 8 - r * 8
            ch = '█' if eighths >= 8 else ' ▁▂▃▄▅▆▇'[max(0, int(eighths))]
            s += draw_color(v) + ch
        out.append(s + RST)
    return out

def draw_color(w): return GREEN if w < 6 else YELLOW if w < 12 else RED

HZ = os.sysconf('SC_CLK_TCK')
NCPU = os.cpu_count() or 1

def proc_ticks():
    """CPU ticks per process name, from /proc (no root needed)."""
    out = {}
    for pid in os.listdir('/proc'):
        if not pid.isdigit(): continue
        s = rd(f'/proc/{pid}/stat')
        if not s: continue
        r = s.rfind(')')
        name = s[s.find('(') + 1:r]
        f = s[r + 2:].split()
        out[name] = out.get(name, 0) + int(f[11]) + int(f[12])
    return out

def profiler_rails():
    """Latest 30 s power split from the battery profiler's RAPL readings."""
    files = sorted(glob.glob('/var/log/battery-profiler/samples-*.jsonl'))
    if not files: return None
    try:
        with open(files[-1], 'rb') as f:
            f.seek(0, 2); size = f.tell()
            f.seek(max(0, size - 400_000))
            line = f.read().rstrip(b'\n').rsplit(b'\n', 1)[-1]
        import json
        d = json.loads(line)
        m = d['metrics']['rapl_w']
        g = lambda k: next((v for n, v in m.items() if n.endswith(k)), None)
        return {'package': g(':package-0'), 'core': g(':core'), 'uncore': g(':uncore'),
                'dram': g(':dram'), 'psys': g(':psys'), 'at': d.get('wall', 0)}
    except Exception:
        return None

def main():
    hist = deque(maxlen=300)
    prev_lpi, prev_t = num(LPI), time.monotonic()
    deep_hist = deque(maxlen=5)
    prev_ticks, rails, rails_at = proc_ticks(), None, 0
    cpu_hist = deque(maxlen=5)  # last 5 s of per-process CPU, for stable ranking
    sys.stdout.write('\033[?1049h\033[?25l')
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    try:
        while True:
            now = time.monotonic()
            st = rd(f'{BAT}/status', '?')
            cap = num(f'{BAT}/capacity') or 0
            i, v = num(f'{BAT}/current_now') or 0, num(f'{BAT}/voltage_now') or 0
            ch, full = num(f'{BAT}/charge_now') or 0, num(f'{BAT}/charge_full') or 0
            design = num(f'{BAT}/charge_full_design') or full
            watts = i * v / 1e12
            hist.append(watts)
            last60 = list(hist)[-60:]
            avg = sum(last60) / len(last60)
            wh_now, wh_full = ch * v / 1e12, full * v / 1e12
            lpi = num(LPI)
            if lpi is not None and prev_lpi is not None and now > prev_t:
                deep_hist.append(min(100.0, (lpi - prev_lpi) / ((now - prev_t) * 1e6) * 100))
            prev_lpi, prev_t = lpi, now
            deep = sum(deep_hist) / len(deep_hist) if deep_hist else None

            ticks = proc_ticks()
            cpu_hist.append({k: (v - prev_ticks.get(k, v)) for k, v in ticks.items() if v > prev_ticks.get(k, v)})
            prev_ticks = ticks
            agg = {}
            for snap in cpu_hist:
                for k, v in snap.items(): agg[k] = agg.get(k, 0) + v
            secs = max(1, len(cpu_hist))
            # percent of one core
            top = sorted(((v / HZ / secs * 100, k) for k, v in agg.items()), reverse=True)
            total_cpu = sum(p for p, _ in top)
            if now - rails_at > 10:
                rails, rails_at = profiler_rails(), now

            cols, lines = shutil.get_terminal_size((80, 24))
            two = cols >= 90 and lines >= 14
            cw = cols // 2 if two else cols
            mc = max(6, cw - 18)

            if st == 'Discharging':
                left = hm(wh_now / avg if avg else None)
                tline = f'{TEXT}{BOLD}{left}{RST}{MUTED} left   {DIM}{hm(wh_full / avg if avg else None)} from full{RST}'
                state = RED + '▼ discharging' + RST
            elif st == 'Charging':
                tline = f'{TEXT}{BOLD}{hm((wh_full - wh_now) / avg if avg else None)}{RST}{MUTED} to full{RST}'
                state = GREEN + '▲ charging' + RST
            else:
                tline, state = MUTED + st + RST, BLUE + st.lower() + RST

            prof = rd('/sys/firmware/acpi/platform_profile', '?')
            epp = rd('/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference', '?')
            gov = rd('/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor', '?')
            pcol = GREEN if prof in ('low-power', 'quiet', 'cool') else YELLOW if prof == 'balanced' else RED
            bright = None
            for b in glob.glob('/sys/class/backlight/*'):
                bv, bm = num(f'{b}/brightness'), num(f'{b}/max_brightness')
                if bv is not None and bm: bright = bv * 100 // bm; break

            # Card heights: power gets the slack (its chart grows).
            if two:
                top_h = max(7, lines - 6)
                hs = [top_h, top_h, lines - top_h, lines - top_h]
            else:
                fixed = [12, 5, 5]
                hs = [max(5, lines - sum(fixed))] + fixed
            chart_rows = hs[0] - 5
            cards = []
            cards.append(box(cw, '⚡ power', f'{draw_color(avg)}{avg:.1f} W{RST}{DIM} 1 min avg{RST}', [
                f'{MUTED}{"now":<9}{RST}{draw_color(watts)}{BOLD}{watts:5.1f} W{RST}   {MUTED}peak{RST} {TEXT}{max(hist):.1f} W{RST}',
                f'{MUTED}{"history":<9}{RST}{DIM}last {min(len(hist), cw - 4)}s{RST}',
            ] + chart(hist, cw - 4, chart_rows), DIM + 'green <6 W   yellow <12 W   red' + RST, hs[0]))
            # ---- why: rails, top processes, hints ----
            why = []
            if rails and rails.get('psys'):
                pk, ps = rails['package'] or 0, rails['psys']
                rest = max(0.0, avg - ps)
                why.append(f'{MUTED}{"split":<9}{RST}{TEXT}cpu {rails["core"] or 0:.1f}{RST}{DIM} · {RST}{TEXT}gpu/uncore {rails["uncore"] or 0:.1f}{RST}{DIM} · {RST}{TEXT}ram {rails["dram"] or 0:.1f}{RST}{DIM} · {RST}{TEXT}soc {max(0, pk - (rails["core"] or 0) - (rails["uncore"] or 0)):.1f}{RST}{DIM} W{RST}')
                why.append(f'{MUTED}{"":<9}{RST}{TEXT}platform {max(0, ps - pk):.1f}{RST}{DIM} · {RST}{TEXT}screen+rest ~{rest:.1f}{RST}{DIM} W{RST}')
            why.append(f'{MUTED}{"cpu":<9}{RST}{TEXT}{total_cpu:.0f}%{RST}{DIM} of one core, {total_cpu / NCPU:.0f}% of all {NCPU}{RST}')
            hints = []
            names = {k for _, k in top[:40] if _ >= 1}
            if {'rustc', 'cargo', 'jr-cargo', 'cc1plus', 'ld', 'mold'} & names:
                hints.append((RED, 'local compile running'))
            if deep is not None and deep < 5:
                hints.append((YELLOW, 'chip never deep-sleeps: constant wakeups'))
            if bright is not None and bright > 40:
                hints.append((YELLOW, f'screen at {bright}%, the OLED is a big share'))
            if total_cpu > 100:
                hints.append((YELLOW, f'busy CPU: {total_cpu:.0f}% of a core'))
            jc = sum(1 for p in os.listdir('/proc') if p.isdigit() and rd(f'/proc/{p}/comm') == 'jcode')
            if jc > 10:
                hints.append((DIM, f'{jc} jcode processes redrawing'))
            room = max(0, hs[1] - 2 - len(why))
            procs = [f'{MUTED}top by cpu{RST}']
            pw = cw - 4 - 2 - 14 - 1 - 6 - 1
            for p, k in top[:max(3, room - 1 - len(hints[:3]))]:
                col = RED if p >= 50 else YELLOW if p >= 10 else MUTED
                procs.append(f'  {TEXT}{k[:14]:<14}{RST} {col}{p:5.1f}%{RST} {meter(min(100, p), max(4, pw), col)}')
            hint_rows = [f'{c}● {h}{RST}' for c, h in hints[:3]]
            # Hints first if they fit beside at least 3 processes, else processes win.
            why += hint_rows[:max(0, room - 4)] + procs
            cards.append(box(cw, '🔍 why', f'{draw_color(avg)}{avg:.1f} W{RST}', why, DIM + 'cpu % of one core, last 5 s' + RST, hs[1]))

            cards.append(box(cw, '🔋 battery', state, [
                f'{MUTED}{"charge":<9}{RST}{meter(cap, mc, good_high(cap))} {TEXT}{cap:3d}%{RST}',
                f'{MUTED}{"time":<9}{RST}{tline}',
                f'{MUTED}{"energy":<9}{RST}{TEXT}{wh_now:.1f}{RST}{DIM} / {wh_full:.1f} Wh   health {full * 100 // max(design, 1)}%{RST}',
            ], '', hs[2]))
            dl = (f'{meter(deep, mc, good_high(deep))} {TEXT}{deep:3.0f}%{RST}'
                  if deep is not None else DIM + 'counter unavailable' + RST)
            prow = [f'{MUTED}{"idle":<9}{RST}{dl}',
                    f'{MUTED}{"platform":<9}{RST}{pcol}{prof}{RST}   {MUTED}epp{RST} {TEXT}{epp}{RST}   {MUTED}gov{RST} {TEXT}{gov}{RST}']
            if bright is not None:
                prow.append(f'{MUTED}{"screen":<9}{RST}{meter(bright, mc, PINK)} {TEXT}{bright:3d}%{RST}')
            cards.append(box(cw, '⚙ profile', DIM + 'deep idle goal >70%' + RST, prow, DIM + 'ctrl-c quit' + RST, hs[3]))
            if two:
                rows_out = [a + b for a, b in zip(cards[0], cards[1])]
                rows_out += [a + b for a, b in zip(cards[2], cards[3])]
            else:
                rows_out = [r for c in cards for r in c]
            rows_out = rows_out[:lines]
            # No trailing newline on the last row, so the screen never scrolls.
            sys.stdout.write('\033[H' + '\033[K\r\n'.join(rows_out) + '\033[K\033[J')
            sys.stdout.flush()
            time.sleep(1)
    except KeyboardInterrupt:
        pass
    finally:
        sys.stdout.write('\033[?25h\033[?1049l')
        sys.stdout.flush()

if __name__ == '__main__':
    main()
