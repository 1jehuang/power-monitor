//! power-monitor: a live laptop power TUI, styled after jcode.
//!
//! Event loop: one `poll(2)` over
//! - a 1 s `timerfd`: the only periodic wakeup. Battery draw and idle
//!   residency have no change notifications, so they are sampled.
//! - a `signalfd` for SIGWINCH (resize) and SIGINT/SIGTERM/SIGHUP (quit),
//! - stdin for keys (`q`, Esc, Ctrl-C) and terminal hangup,
//! - a kernel uevent netlink socket: plug/unplug and battery status
//!   changes redraw at once instead of waiting for the next tick,
//! - inotify on the battery profiler's log dir: the power-rail split is
//!   re-read only after the profiler writes a new sample,
//! - niri's event stream: while this window's workspace is not shown,
//!   the timer is disarmed and the process sleeps with zero wakeups.

mod niri;
mod sample;
mod ui;

use crossterm::{execute, terminal};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn timerfd() -> io::Result<OwnedFd> {
    // SAFETY: plain syscall; the fd is owned immediately.
    let fd = cvt(unsafe {
        libc::timerfd_create(
            libc::CLOCK_MONOTONIC,
            libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
        )
    })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn arm(fd: &OwnedFd, period: Option<Duration>) {
    let ts = |d: Duration| libc::timespec {
        tv_sec: d.as_secs() as _,
        tv_nsec: d.subsec_nanos() as _,
    };
    let spec = match period {
        Some(p) => libc::itimerspec {
            it_interval: ts(p),
            it_value: ts(p),
        },
        None => libc::itimerspec {
            it_interval: ts(Duration::ZERO),
            it_value: ts(Duration::ZERO),
        },
    };
    // SAFETY: valid fd and spec.
    unsafe { libc::timerfd_settime(fd.as_raw_fd(), 0, &spec, std::ptr::null_mut()) };
}

fn signals() -> io::Result<OwnedFd> {
    // SAFETY: building and installing a signal mask for this thread (the only one).
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGWINCH, libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        cvt(libc::sigprocmask(
            libc::SIG_BLOCK,
            &set,
            std::ptr::null_mut(),
        ))?;
        let fd = cvt(libc::signalfd(
            -1,
            &set,
            libc::SFD_CLOEXEC | libc::SFD_NONBLOCK,
        ))?;
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

/// Kernel uevents (not udevd), filtered to power_supply in `drain_uevents`.
fn uevents() -> Option<OwnedFd> {
    // SAFETY: socket + bind with a zeroed, filled-in sockaddr_nl.
    unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::NETLINK_KOBJECT_UEVENT,
        );
        if fd < 0 {
            return None;
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut sa: libc::sockaddr_nl = std::mem::zeroed();
        sa.nl_family = libc::AF_NETLINK as _;
        sa.nl_groups = 1;
        let r = libc::bind(
            fd.as_raw_fd(),
            &sa as *const _ as *const libc::sockaddr,
            std::mem::size_of_val(&sa) as _,
        );
        (r == 0).then_some(fd)
    }
}

/// True if any queued uevent concerns a power supply.
fn drain_uevents(fd: RawFd) -> bool {
    let mut buf = [0u8; 4096];
    let mut hit = false;
    loop {
        // SAFETY: reading into a local buffer.
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n <= 0 {
            return hit;
        }
        let msg = &buf[..n as usize];
        hit |= msg
            .split(|&b| b == 0)
            .any(|f| f == b"SUBSYSTEM=power_supply");
    }
}

fn inotify_dir(dir: &str) -> Option<OwnedFd> {
    let path = std::ffi::CString::new(dir).ok()?;
    // SAFETY: plain syscalls on a fresh fd.
    unsafe {
        let fd = libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK);
        if fd < 0 {
            return None;
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let wd = libc::inotify_add_watch(
            fd.as_raw_fd(),
            path.as_ptr(),
            libc::IN_CLOSE_WRITE | libc::IN_MODIFY | libc::IN_CREATE,
        );
        (wd >= 0).then_some(fd)
    }
}

/// Read and discard whatever is queued on a nonblocking fd.
fn drain(fd: RawFd) {
    let mut buf = [0u8; 4096];
    // SAFETY: reading into a local buffer.
    while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

enum Quit {
    No,
    Yes,
}

fn read_keys() -> Quit {
    let mut buf = [0u8; 64];
    match io::stdin().read(&mut buf) {
        Ok(0) => Quit::Yes, // terminal closed
        Ok(n) => {
            let keys = &buf[..n];
            // 'q', Ctrl-C (raw mode), or a lone Esc (not the start of an escape sequence).
            if keys.contains(&b'q') || keys.contains(&3) || keys == [0x1b] {
                Quit::Yes
            } else {
                Quit::No
            }
        }
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Quit::No,
        Err(_) => Quit::Yes,
    }
}

fn main() -> io::Result<()> {
    // Spawn the niri watcher before blocking signals: masks survive exec.
    let mut niri = niri::Watcher::spawn();
    let sig = signals()?;
    // Let the kernel batch our timer with other wakeups.
    // SAFETY: prctl with a plain integer argument.
    unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, 50_000_000u64) };

    let mut sm = sample::Sampler::new();
    sm.load_rails();
    sm.tick();

    let timer = timerfd()?;
    let uev = uevents();
    let ino = inotify_dir(sample::PROFILER_DIR);

    terminal::enable_raw_mode()?;
    execute!(
        io::stdout(),
        terminal::EnterAlternateScreen,
        crossterm::cursor::Hide
    )?;
    let restore = || {
        let _ = execute!(
            io::stdout(),
            crossterm::cursor::Show,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    };
    let mut term = match Terminal::new(CrosstermBackend::new(io::stdout())) {
        Ok(t) => t,
        Err(e) => {
            restore();
            return Err(e);
        }
    };

    let result = (|| -> io::Result<()> {
        let mut visible = niri.as_ref().is_none_or(|n| n.visible());
        arm(&timer, visible.then_some(Duration::from_secs(1)));
        term.draw(|f| ui::render(f, &mut sm))?;
        let mut rails_dirty = false;

        loop {
            let mut fds = vec![
                libc::pollfd {
                    fd: timer.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: sig.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: 0,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if visible {
                        uev.as_ref().map_or(-1, |f| f.as_raw_fd())
                    } else {
                        -1
                    },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: if visible && !sm.live_rails {
                        ino.as_ref().map_or(-1, |f| f.as_raw_fd())
                    } else {
                        -1
                    },
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: niri.as_ref().map_or(-1, |n| n.fd()),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: fds is a valid array for its length.
            let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
            if r < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(io::Error::last_os_error());
            }
            let ready = |i: usize| fds[i].revents != 0;
            let mut redraw = false;

            if ready(1) {
                // SAFETY: signalfd reads are whole signalfd_siginfo records.
                let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
                while unsafe {
                    libc::read(
                        sig.as_raw_fd(),
                        (&mut info as *mut libc::signalfd_siginfo).cast(),
                        std::mem::size_of_val(&info),
                    )
                } > 0
                {
                    if info.ssi_signo as i32 == libc::SIGWINCH {
                        redraw = true;
                    } else {
                        return Ok(());
                    }
                }
            }
            if ready(2) {
                if fds[2].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                    return Ok(());
                }
                if let Quit::Yes = read_keys() {
                    return Ok(());
                }
            }
            if ready(4) {
                if let Some(f) = &ino {
                    drain(f.as_raw_fd());
                }
                rails_dirty = true; // read on the next tick: debounces bursty writes
            }
            if ready(5) {
                let alive = niri.as_mut().is_some_and(|n| n.pump());
                if !alive {
                    niri = None; // compositor gone: always draw
                }
                let now_visible = niri.as_ref().is_none_or(|n| n.visible());
                if now_visible != visible {
                    visible = now_visible;
                    if visible {
                        // Rates measured across the hidden gap would be meaningless.
                        sm.rebaseline();
                        sm.read_battery();
                        redraw = true;
                    }
                    arm(&timer, visible.then_some(Duration::from_secs(1)));
                }
            }
            if ready(3) {
                if let Some(f) = &uev {
                    if drain_uevents(f.as_raw_fd()) && visible {
                        sm.read_battery();
                        redraw = true;
                    }
                }
            }
            if ready(0) {
                drain(timer.as_raw_fd());
                if !visible {
                    continue;
                }
                if rails_dirty && !sm.live_rails {
                    sm.load_rails();
                    rails_dirty = false;
                }
                sm.tick();
                redraw = true;
            }
            if redraw && visible {
                term.draw(|f| ui::render(f, &mut sm))?;
            }
        }
    })();

    restore();
    if let Some(n) = niri {
        n.kill();
    }
    result
}
