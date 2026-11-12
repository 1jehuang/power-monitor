//! Data collection. Every sysfs/procfs file stays open and is re-read with
//! `pread(…, 0)`, so a tick costs no path lookups.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

/// A sysfs attribute held open across reads.
pub struct Attr {
    path: String,
    file: Option<File>,
    cache_for: Duration,
    cached: Option<(Instant, Option<String>)>,
    retry_at: Instant,
}

impl Attr {
    pub fn new(path: impl Into<String>) -> Self {
        let path = path.into();
        let file = File::open(&path).ok();
        Self {
            path,
            file,
            cache_for: Duration::ZERO,
            cached: None,
            retry_at: Instant::now(),
        }
    }

    fn slow(mut self) -> Self {
        self.cache_for = Duration::from_secs(10);
        self
    }

    pub fn exists(&self) -> bool {
        self.file.is_some()
    }

    pub fn text(&mut self) -> Option<String> {
        let now = Instant::now();
        if let Some((at, value)) = &self.cached {
            if now.duration_since(*at) < self.cache_for {
                return value.clone();
            }
        }
        if self.file.is_none() && now >= self.retry_at {
            self.file = File::open(&self.path).ok();
            self.retry_at = now + Duration::from_secs(10);
        }
        let value = self.read_text();
        self.cached = Some((now, value.clone()));
        value
    }

    fn read_text(&mut self) -> Option<String> {
        let f = self.file.as_ref()?;
        let mut buf = [0u8; 512];
        match f.read_at(&mut buf, 0) {
            Ok(n) => Some(String::from_utf8_lossy(&buf[..n]).trim().to_string()),
            Err(_) => {
                // The device went away (e.g. a driver rebind): try reopening once.
                self.file = None;
                None
            }
        }
    }

    pub fn num(&mut self) -> Option<i64> {
        self.text()?.parse().ok()
    }
}

fn first_dir(class: &str, want_type: Option<&str>) -> Option<String> {
    let mut dirs: Vec<String> = fs::read_dir(class)
        .ok()?
        .flatten()
        .map(|e| e.path().to_string_lossy().into_owned())
        .collect();
    dirs.sort();
    dirs.into_iter().find(|d| match want_type {
        Some(t) => fs::read_to_string(format!("{d}/type")).is_ok_and(|s| s.trim() == t),
        None => true,
    })
}

pub struct Battery {
    status: Attr,
    capacity: Attr,
    power_now: Attr,
    current_now: Attr,
    voltage_now: Attr,
    energy_now: Attr,
    energy_full: Attr,
    energy_design: Attr,
    charge_now: Attr,
    charge_full: Attr,
    charge_design: Attr,
}

impl Battery {
    fn new(dir: &str) -> Self {
        let a = |n: &str| Attr::new(format!("{dir}/{n}"));
        Self {
            status: a("status"),
            capacity: a("capacity"),
            power_now: a("power_now"),
            current_now: a("current_now"),
            voltage_now: a("voltage_now"),
            energy_now: a("energy_now"),
            energy_full: a("energy_full"),
            energy_design: a("energy_full_design"),
            charge_now: a("charge_now"),
            charge_full: a("charge_full"),
            charge_design: a("charge_full_design"),
        }
    }
}

#[derive(Default, Clone)]
pub struct BatState {
    pub status: String,
    pub capacity: i64,
    pub watts: f64,
    pub wh_now: f64,
    pub wh_full: f64,
    pub health: i64,
}

#[derive(Default, Clone, Copy)]
pub struct Rails {
    pub package: f64,
    pub core: f64,
    pub uncore: f64,
    pub dram: f64,
    pub psys: f64,
}

/// Raise the descriptor limit to the hard limit and return how many stat fds
/// the process scan may keep. Half the limit, because fds of processes that
/// exited during a scan are only released at its end, and the rest of the
/// monitor (sysfs attributes, timers, the terminal) needs its share.
fn fd_budget() -> usize {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit/setrlimit with a valid, initialised rlimit struct.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return 256;
        }
        if lim.rlim_cur < lim.rlim_max {
            let raised = libc::rlimit {
                rlim_cur: lim.rlim_max.min(65_536),
                rlim_max: lim.rlim_max,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &raised) == 0 {
                lim = raised;
            }
        }
    }
    usize::try_from(lim.rlim_cur / 2).unwrap_or(256).min(16_384)
}

/// Per-process CPU ticks, from `/proc/<pid>/stat` fds kept open.
///
/// Only `max_open` of them are kept. A machine can run more processes than
/// the descriptor limit allows, and once the table is full `read_dir("/proc")`
/// itself fails, which used to empty the process list entirely. Processes past
/// the budget are opened and closed on each scan instead.
struct ProcScan {
    procs: HashMap<i32, (Option<File>, u64, u64)>,
    max_open: usize,
}

struct StatSample {
    name: String,
    ticks: u64,
    /// Boot-relative start time: tells a reused pid from the same process.
    start: u64,
}

impl ProcScan {
    fn new() -> Self {
        Self {
            procs: HashMap::new(),
            max_open: fd_budget(),
        }
    }

    fn parse(buf: &[u8]) -> Option<StatSample> {
        let s = std::str::from_utf8(buf).ok()?;
        let open = s.find('(')?;
        let close = s.rfind(')')?;
        let name = s[open + 1..close].to_string();
        let mut f = s.get(close + 2..)?.split_ascii_whitespace();
        let utime: u64 = f.nth(11)?.parse().ok()?;
        let stime: u64 = f.next()?.parse().ok()?;
        let start = f.nth(6).and_then(|v| v.parse().ok()).unwrap_or(0);
        Some(StatSample {
            name,
            ticks: utime + stime,
            start,
        })
    }

    fn read(file: &File) -> Option<StatSample> {
        let mut buf = [0u8; 2048];
        let n = file.read_at(&mut buf, 0).ok()?;
        Self::parse(&buf[..n])
    }

    fn open_read(pid: i32) -> Option<(File, StatSample)> {
        let file = File::open(format!("/proc/{pid}/stat")).ok()?;
        let sample = Self::read(&file)?;
        Some((file, sample))
    }

    /// CPU ticks per process name since the previous scan, plus the jcode count.
    fn scan(&mut self) -> (HashMap<String, u64>, usize) {
        let mut deltas: HashMap<String, u64> = HashMap::new();
        let mut seen = HashSet::new();
        let mut jcode = 0;
        let mut open = 0;
        let Ok(dir) = fs::read_dir("/proc") else {
            return (deltas, 0);
        };
        for e in dir.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            seen.insert(pid);
            let mut file = None;
            let mut prev = None;
            let mut sample = None;
            match self.procs.remove(&pid) {
                // A read through the old fd fails once that task has exited,
                // so success means it is still the same process.
                Some((Some(f), ticks, _)) => {
                    if let Some(now) = Self::read(&f) {
                        sample = Some(now);
                        file = Some(f);
                        prev = Some(ticks);
                    }
                }
                Some((None, ticks, start)) => {
                    if let Some((f, now)) = Self::open_read(pid) {
                        if now.start == start {
                            prev = Some(ticks);
                        }
                        sample = Some(now);
                        file = Some(f);
                    }
                }
                None => {}
            }
            if sample.is_none() {
                // New pid, or the old fd belonged to an exited task whose pid was reused.
                if let Some((f, now)) = Self::open_read(pid) {
                    sample = Some(now);
                    file = Some(f);
                }
            }
            let Some(now) = sample else {
                continue;
            };
            if now.name == "jcode" {
                jcode += 1;
            }
            if let Some(prev) = prev.filter(|prev| now.ticks > *prev) {
                *deltas.entry(now.name).or_default() += now.ticks - prev;
            }
            let keep = file.filter(|_| open < self.max_open);
            open += usize::from(keep.is_some());
            self.procs.insert(pid, (keep, now.ticks, now.start));
        }
        self.procs.retain(|pid, _| seen.contains(pid));
        (deltas, jcode)
    }
}

/// Live RAPL energy counters (`intel-rapl:*`). Differencing a monotonic
/// energy counter gives the exact average power over the interval, so short
/// spikes between reads are counted, unlike instantaneous battery readings.
struct RaplZone {
    name: String,
    energy: Attr,
    range: i64,
    last: Option<i64>,
}

fn rapl_zones() -> Vec<RaplZone> {
    let mut v = Vec::new();
    let Ok(dir) = fs::read_dir("/sys/class/powercap") else {
        return v;
    };
    for e in dir.flatten() {
        let d = e.path();
        if !e.file_name().to_string_lossy().starts_with("intel-rapl:") {
            continue;
        }
        let Ok(name) = fs::read_to_string(d.join("name")) else {
            continue;
        };
        let energy = Attr::new(d.join("energy_uj").to_string_lossy().into_owned());
        if energy.file.is_none() {
            continue;
        }
        let range = fs::read_to_string(d.join("max_energy_range_uj"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(i64::MAX);
        v.push(RaplZone {
            name: name.trim().to_string(),
            energy,
            range,
            last: None,
        });
    }
    v
}

pub const PROFILER_DIR: &str = "/var/log/battery-profiler";

pub struct Sampler {
    bat: Option<Battery>,
    lpi: Attr,
    pub profile: Attr,
    pub epp: Attr,
    pub gov: Attr,
    bl: Option<(Attr, Attr)>,
    procs: ProcScan,
    last: Option<(Instant, Option<i64>)>,
    deep: VecDeque<(f64, f64)>,
    cpu: VecDeque<(f64, HashMap<String, u64>)>,
    last_process_scan: Option<Instant>,

    pub hz: f64,
    pub ncpu: usize,
    /// Graph series: live psys watts when RAPL is readable, else battery.
    pub hist: VecDeque<f64>,
    /// Battery discharge watts (whole laptop incl. screen), for time left.
    pub bat_hist: VecDeque<f64>,
    rapl: Vec<RaplZone>,
    pub live_rails: bool,
    pub bat_state: BatState,
    pub rails: Option<Rails>,
    pub jcode: usize,
}

impl Sampler {
    #[cfg(test)]
    pub fn demo_activity(&mut self) {
        let attr = |value: &str| {
            let mut a = Attr::new("/nonexistent/demo-attribute").slow();
            a.cached = Some((Instant::now(), Some(value.into())));
            a
        };
        self.profile = attr("balanced");
        self.epp = attr("balance_power");
        self.gov = attr("powersave");
        self.bl = Some((attr("35"), attr("100")));
        self.lpi = Attr::new("/dev/null");
        self.deep = [(5_000_000.0, 1_500_000.0)].into();
        self.ncpu = 16;
        self.hz = 100.0;
        self.cpu = [(
            5.0,
            HashMap::from([
                ("jcode".into(), 125),
                ("kitty".into(), 20),
                ("niri".into(), 15),
                ("power-monitor".into(), 1),
            ]),
        )]
        .into();
        self.rails = Some(Rails {
            package: 3.2,
            core: 1.2,
            uncore: 0.4,
            dram: 0.3,
            psys: 7.5,
        });
        self.live_rails = true;
    }

    pub fn new() -> Self {
        let cpu = "/sys/devices/system/cpu";
        Self {
            bat: first_dir("/sys/class/power_supply", Some("Battery")).map(|d| Battery::new(&d)),
            lpi: Attr::new(format!("{cpu}/cpuidle/low_power_idle_cpu_residency_us")),
            profile: Attr::new("/sys/firmware/acpi/platform_profile").slow(),
            epp: Attr::new(format!("{cpu}/cpu0/cpufreq/energy_performance_preference")).slow(),
            gov: Attr::new(format!("{cpu}/cpu0/cpufreq/scaling_governor")).slow(),
            bl: first_dir("/sys/class/backlight", None).map(|d| {
                (
                    Attr::new(format!("{d}/brightness")).slow(),
                    Attr::new(format!("{d}/max_brightness")).slow(),
                )
            }),
            procs: ProcScan::new(),
            last: None,
            deep: VecDeque::with_capacity(5),
            cpu: VecDeque::with_capacity(5),
            last_process_scan: None,
            // SAFETY: plain libc queries.
            hz: (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as f64,
            ncpu: (unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }).max(1) as usize,
            hist: VecDeque::with_capacity(300),
            bat_hist: VecDeque::with_capacity(60),
            rapl: rapl_zones(),
            live_rails: false,
            bat_state: BatState::default(),
            rails: None,
            jcode: 0,
        }
    }

    /// Battery fields only. Called on every tick and on power_supply uevents.
    pub fn read_battery(&mut self) {
        let Some(b) = self.bat.as_mut() else { return };
        let volts = b.voltage_now.num().unwrap_or(0) as f64 / 1e6;
        let watts = match b.power_now.num() {
            Some(p) if b.power_now.exists() => p.unsigned_abs() as f64 / 1e6,
            _ => b.current_now.num().unwrap_or(0).unsigned_abs() as f64 / 1e6 * volts,
        };
        let (now, full, design) = if b.energy_now.exists() {
            let e = |a: &mut Attr| a.num().unwrap_or(0) as f64 / 1e6;
            (
                e(&mut b.energy_now),
                e(&mut b.energy_full),
                e(&mut b.energy_design),
            )
        } else {
            let c = |a: &mut Attr| a.num().unwrap_or(0) as f64 / 1e6 * volts;
            (
                c(&mut b.charge_now),
                c(&mut b.charge_full),
                c(&mut b.charge_design),
            )
        };
        self.bat_state = BatState {
            status: b.status.text().unwrap_or_else(|| "?".into()),
            capacity: b.capacity.num().unwrap_or(0),
            watts,
            wh_now: now,
            wh_full: full,
            health: if design > 0.0 {
                (full / design * 100.0) as i64
            } else {
                100
            },
        };
    }

    /// Forget rate baselines, e.g. after being hidden: the next tick starts fresh.
    pub fn rebaseline(&mut self) {
        self.deep.clear();
        self.cpu.clear();
        for z in &mut self.rapl {
            z.last = z.energy.num();
        }
        self.procs.scan();
        self.last_process_scan = Some(Instant::now());
        self.last = Some((Instant::now(), self.lpi.num()));
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        self.read_battery();
        if self.bat_hist.len() == 60 {
            self.bat_hist.pop_front();
        }
        self.bat_hist.push_back(self.bat_state.watts);
        let secs = self.last.map(|(t, _)| now.duration_since(t).as_secs_f64());
        let was_live = self.live_rails;
        let live = self.read_rapl(secs);
        if was_live != self.live_rails {
            // Never mix two different sensors under one graph/source label.
            self.hist.clear();
        }
        let w = live.unwrap_or(self.bat_state.watts);
        if self.hist.len() == 300 {
            self.hist.pop_front();
        }
        self.hist.push_back(w);

        let lpi = self.lpi.num();
        // Process accounting is displayed over five seconds. Reading every
        // /proc task every second merely repeats work without better resolution
        // for that view. Keep the graph/RAPL sampling at one second.
        if self
            .last_process_scan
            .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(5))
        {
            let (deltas, jcode) = self.procs.scan();
            self.jcode = jcode;
            if let Some(then) = self.last_process_scan {
                self.cpu.clear();
                self.cpu
                    .push_back((now.duration_since(then).as_secs_f64(), deltas));
            }
            self.last_process_scan = Some(now);
        }
        if let Some((then, prev_lpi)) = self.last {
            let secs = now.duration_since(then).as_secs_f64();
            if secs > 0.0 {
                if let (Some(a), Some(b)) = (prev_lpi, lpi) {
                    push5(&mut self.deep, (secs * 1e6, (b - a).max(0) as f64));
                }
            }
        }
        self.last = Some((now, lpi));
    }

    /// Read every RAPL counter; returns psys watts once two reads exist.
    fn read_rapl(&mut self, secs: Option<f64>) -> Option<f64> {
        self.live_rails = false;
        let mut w: HashMap<String, f64> = HashMap::new();
        for z in &mut self.rapl {
            let Some(e) = z.energy.num() else {
                z.last = None;
                continue;
            };
            if let (Some(prev), Some(s)) = (z.last, secs.filter(|s| *s > 0.0)) {
                if let Some(watts) = energy_watts(prev, e, z.range, s) {
                    w.insert(z.name.clone(), watts);
                }
            }
            z.last = Some(e);
        }
        let psys = *w.get("psys")?;
        let g = |k: &str| w.get(k).copied().unwrap_or(0.0);
        self.rails = Some(Rails {
            package: g("package-0"),
            core: g("core"),
            uncore: g("uncore"),
            dram: g("dram"),
            psys,
        });
        self.live_rails = true;
        Some(psys)
    }

    /// One-minute mean of the graph series.
    pub fn avg(&self) -> f64 {
        mean(self.hist.iter().rev().take(60))
    }

    /// One-minute mean battery discharge: the whole-laptop figure.
    pub fn bat_avg(&self) -> f64 {
        mean(self.bat_hist.iter())
    }

    pub fn peak(&self) -> f64 {
        self.hist.iter().copied().fold(0.0, f64::max)
    }

    /// Package deep-idle share over the last 5 s.
    pub fn deep_idle(&self) -> Option<f64> {
        if !self.lpi.exists() || self.deep.is_empty() {
            return None;
        }
        let (wall, idle) = self
            .deep
            .iter()
            .fold((0.0, 0.0), |(w, i), (dw, di)| (w + dw, i + di));
        Some((idle / wall * 100.0).min(100.0))
    }

    /// (percent of one core, process name), busiest first, over the last 5 s.
    pub fn top(&self) -> Vec<(f64, String)> {
        let secs: f64 = self.cpu.iter().map(|(s, _)| s).sum();
        if secs <= 0.0 {
            return Vec::new();
        }
        let mut agg: HashMap<&str, u64> = HashMap::new();
        for (_, m) in &self.cpu {
            for (k, v) in m {
                *agg.entry(k).or_default() += v;
            }
        }
        let mut v: Vec<(f64, String)> = agg
            .into_iter()
            .map(|(k, t)| (t as f64 / self.hz / secs * 100.0, k.to_string()))
            .collect();
        v.sort_by(|a, b| b.0.total_cmp(&a.0));
        v
    }

    pub fn brightness(&mut self) -> Option<i64> {
        let (cur, max) = self.bl.as_mut()?;
        let (c, m) = (cur.num()?, max.num()?);
        (m > 0).then(|| c * 100 / m)
    }

    /// Latest 30 s RAPL split from the battery profiler's log, if readable.
    /// Keeps the last good value when the newest line is incomplete.
    pub fn load_rails(&mut self) {
        if self.live_rails {
            return;
        }
        if let Some(r) = read_rails() {
            self.rails = Some(r);
        }
    }
}

fn energy_watts(previous: i64, current: i64, range: i64, seconds: f64) -> Option<f64> {
    if previous < 0 || current < 0 || range <= 0 || !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    let delta = if current >= previous {
        current as i128 - previous as i128
    } else {
        current as i128 + range as i128 - previous as i128
    };
    (delta >= 0).then_some(delta as f64 / 1e6 / seconds)
}

fn mean<'a>(it: impl Iterator<Item = &'a f64>) -> f64 {
    let (n, sum) = it.fold((0, 0.0), |(n, s), v| (n + 1, s + v));
    if n == 0 {
        0.0
    } else {
        sum / n as f64
    }
}

fn push5<T>(q: &mut VecDeque<T>, v: T) {
    if q.len() == 5 {
        q.pop_front();
    }
    q.push_back(v);
}

fn read_rails() -> Option<Rails> {
    let newest = fs::read_dir(PROFILER_DIR)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("samples-") && n.ends_with(".jsonl"))
        .max()?;
    let f = File::open(format!("{PROFILER_DIR}/{newest}")).ok()?;
    let size = f.metadata().ok()?.len();
    let want = size.min(400_000);
    let mut buf = vec![0u8; want as usize];
    let n = f.read_at(&mut buf, size - want).ok()?;
    let buf = &buf[..n];
    let body = buf.strip_suffix(b"\n")?; // an unterminated last line is still being written
    let line = body.rsplit(|&b| b == b'\n').next()?;
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    let m = v.get("metrics")?.get("rapl_w")?.as_object()?;
    let g = |suffix: &str| {
        m.iter()
            .find(|(k, _)| k.ends_with(suffix))
            .and_then(|(_, v)| v.as_f64())
            .unwrap_or(0.0)
    };
    Some(Rails {
        package: g(":package-0"),
        core: g(":core"),
        uncore: g(":uncore"),
        dram: g(":dram"),
        psys: g(":psys"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_delta_handles_wrap_and_invalid_intervals() {
        assert_eq!(
            energy_watts(1_000_000, 5_000_000, 10_000_000, 2.0),
            Some(2.0)
        );
        assert_eq!(
            energy_watts(9_000_000, 1_000_000, 10_000_000, 1.0),
            Some(2.0)
        );
        assert_eq!(energy_watts(0, 1, 10, 0.0), None);
        assert_eq!(energy_watts(0, 1, 10, f64::NAN), None);
        assert_eq!(energy_watts(-1, 1, 10, 1.0), None);
    }

    #[test]
    fn slow_attributes_reuse_cached_value() {
        let mut attr = Attr::new("/definitely-not-a-real-sysfs-file").slow();
        attr.cached = Some((Instant::now(), Some("cached".into())));
        assert_eq!(attr.text().as_deref(), Some("cached"));
        attr.cached = Some((
            Instant::now() - Duration::from_secs(11),
            Some("expired".into()),
        ));
        assert_eq!(attr.text(), None);
    }

    #[test]
    fn process_scan_waits_for_accounting_window() {
        let mut sm = Sampler::new();
        let baseline = Instant::now();
        sm.last_process_scan = Some(baseline);
        sm.tick();
        assert_eq!(sm.last_process_scan, Some(baseline));
    }

    #[test]
    fn parses_stat_with_spaces_and_parens_in_name() {
        let line = b"1234 (Web (Content) x) S 1 2 3 4 5 6 7 8 9 10 111 22 0 0 20 0 1 0";
        let sample = ProcScan::parse(line).unwrap();
        assert_eq!(sample.name, "Web (Content) x");
        assert_eq!(sample.ticks, 133);
        assert_eq!(sample.start, 0);
        let line = b"7 (x) S 1 2 3 4 5 6 7 8 9 10 111 22 0 0 20 0 1 0 98765 4096 12";
        assert_eq!(ProcScan::parse(line).unwrap().start, 98765);
    }

    /// With more processes than descriptors to spare, every process must
    /// still be accounted. The monitor once kept one fd per process, ran into
    /// the 1024 limit on a machine with ~950 processes, and showed none.
    #[test]
    fn process_scan_accounts_processes_past_the_fd_budget() {
        for max_open in [0, 2] {
            let mut scan = ProcScan {
                procs: HashMap::new(),
                max_open,
            };
            scan.scan();
            let held = scan.procs.values().filter(|p| p.0.is_some()).count();
            assert_eq!(held, max_open.min(scan.procs.len()));
            assert!(scan.procs.len() > max_open);

            let me = std::process::id() as i32;
            let mine = ProcScan::open_read(me).unwrap().1;
            // Burn enough CPU for several clock ticks.
            let until = Instant::now() + Duration::from_millis(150);
            while Instant::now() < until {
                std::hint::black_box(0u64);
            }
            let (deltas, _) = scan.scan();
            assert!(
                deltas.get(&mine.name).is_some_and(|t| *t > 0),
                "own CPU time missing with max_open={max_open}: {deltas:?}"
            );
            let held = scan.procs.values().filter(|p| p.0.is_some()).count();
            assert!(held <= max_open);
        }
    }
}
