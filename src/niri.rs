//! Whether our window is on screen, from niri's JSON event stream.
//!
//! "Visible" means the workspace holding our terminal window is the active
//! one on its output, or the overview is open. Our window is the niri window
//! whose pid is one of our ancestors (the terminal emulator).

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdout, Command, Stdio};

pub struct Watcher {
    child: Child,
    out: ChildStdout,
    buf: Vec<u8>,
    ancestors: HashSet<i64>,
    /// window id -> (pid, workspace id, app id)
    windows: HashMap<i64, (i64, Option<i64>, String)>,
    /// workspace id -> (output, is_active)
    workspaces: HashMap<i64, (Option<String>, bool)>,
    overview: bool,
}

fn ancestors() -> HashSet<i64> {
    let mut out = HashSet::new();
    let mut pid = std::process::id() as i64;
    for _ in 0..64 {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { break };
        let Some(ppid) = stat
            .rfind(')')
            .and_then(|i| stat[i + 2..].split_whitespace().nth(1))
            .and_then(|p| p.parse::<i64>().ok())
        else {
            break;
        };
        if ppid <= 1 {
            break;
        }
        out.insert(ppid);
        pid = ppid;
    }
    out
}

impl Watcher {
    pub fn spawn() -> Option<Self> {
        std::env::var_os("NIRI_SOCKET")?;
        let mut child = Command::new("niri")
            .args(["msg", "--json", "event-stream"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let out = child.stdout.take()?;
        // SAFETY: setting O_NONBLOCK on a pipe fd we own.
        unsafe {
            let fd = out.as_raw_fd();
            let fl = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        let mut w = Self {
            child,
            out,
            buf: Vec::new(),
            ancestors: ancestors(),
            windows: HashMap::new(),
            workspaces: HashMap::new(),
            overview: false,
        };
        // niri sends the full state first; read it so the first frame knows.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while w.windows.is_empty() && std::time::Instant::now() < deadline {
            let mut p = libc::pollfd { fd: w.fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            unsafe { libc::poll(&mut p, 1, 50) };
            if !w.pump() {
                return None;
            }
        }
        Some(w)
    }

    pub fn fd(&self) -> i32 {
        self.out.as_raw_fd()
    }

    /// Consume queued events. Returns false once the stream has ended.
    pub fn pump(&mut self) -> bool {
        let mut chunk = [0u8; 16384];
        loop {
            match self.out.read(&mut chunk) {
                Ok(0) => return false,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                self.apply(&v);
            }
        }
        true
    }

    fn put_window(&mut self, w: &Value) {
        if let Some(id) = w["id"].as_i64() {
            let app = w["app_id"].as_str().unwrap_or_default().to_string();
            self.windows.insert(id, (w["pid"].as_i64().unwrap_or(-1), w["workspace_id"].as_i64(), app));
        }
    }

    fn apply(&mut self, v: &Value) {
        let Some((kind, e)) = v.as_object().and_then(|o| o.iter().next()) else { return };
        match kind.as_str() {
            "WorkspacesChanged" => {
                self.workspaces.clear();
                for w in e["workspaces"].as_array().into_iter().flatten() {
                    if let Some(id) = w["id"].as_i64() {
                        let output = w["output"].as_str().map(str::to_string);
                        self.workspaces.insert(id, (output, w["is_active"].as_bool().unwrap_or(false)));
                    }
                }
            }
            "WorkspaceActivated" => {
                let Some(id) = e["id"].as_i64() else { return };
                let output = self.workspaces.get(&id).and_then(|(o, _)| o.clone());
                for (wid, (o, active)) in self.workspaces.iter_mut() {
                    if *o == output {
                        *active = *wid == id;
                    }
                }
            }
            "WindowsChanged" => {
                self.windows.clear();
                for w in e["windows"].as_array().into_iter().flatten() {
                    self.put_window(w);
                }
            }
            "WindowOpenedOrChanged" => self.put_window(&e["window"]),
            "WindowClosed" => {
                if let Some(id) = e["id"].as_i64() {
                    self.windows.remove(&id);
                }
            }
            "OverviewOpenedOrClosed" => self.overview = e["is_open"].as_bool().unwrap_or(false),
            _ => {}
        }
    }

    pub fn visible(&self) -> bool {
        if self.overview {
            return true;
        }
        let ours: Vec<&(i64, Option<i64>, String)> =
            self.windows.values().filter(|(pid, _, _)| self.ancestors.contains(pid)).collect();
        // A single-instance terminal owns many windows under one pid. Prefer
        // the one launched with our class (`kitty --class power-monitor`).
        let tagged: Vec<_> = ours.iter().filter(|(_, _, app)| app == "power-monitor").collect();
        let mine: Vec<Option<i64>> = if tagged.is_empty() {
            ours.iter().map(|(_, ws, _)| *ws).collect()
        } else {
            tagged.iter().map(|(_, ws, _)| *ws).collect()
        };
        if mine.is_empty() {
            return true; // can't tell (e.g. a shared terminal server): stay live
        }
        // A terminal process can own several windows; visible if any is shown.
        mine.iter().any(|ws| match ws {
            None => true, // floating/unknown placement
            Some(id) => self.workspaces.get(id).is_none_or(|(_, active)| *active),
        })
    }

    pub fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
