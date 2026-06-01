//! N3: detachable persistent sessions ("tmux for X").
//!
//! Normally [`crate::wm::WmSession`]'s `Drop` kills Xvfb/jwm/apps when kitwin
//! exits. A named session (`--session NAME`) instead keeps them alive across
//! exits: the processes are spawned in their own session (`setsid`) so terminal
//! signals (SIGINT/SIGHUP) and the parent exiting don't reap them, and a small
//! state file records how to reattach (display, geometry, pids). Re-running
//! `--session NAME` attaches to the live display instead of spawning a new one.
//!
//! This module is only exercised when `--session` / `--list-sessions` /
//! `--kill-session` are used; the default kitwin path never touches it, so it
//! cannot affect normal behavior or performance.

use std::fs;
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Everything needed to reattach to (or tear down) a running session.
pub struct State {
    pub display: u8,
    pub width: u32,
    pub height: u32,
    pub xvfb_pid: u32,
    pub jwm_pid: Option<u32>,
    pub pulse_sink: Option<String>,
    pub pulse_server: Option<String>,
}

/// What to do for a `--session NAME` invocation.
pub enum Role {
    /// No live session by that name — spawn a fresh Xvfb/jwm.
    Create,
    /// A live session exists — attach to its display without spawning anything.
    Attach(State),
}

/// Spawn a child detached into its own session via `setsid`, so it survives
/// terminal hangup, Ctrl+C aimed at kitwin's process group, and kitwin exiting.
///
/// The `pre_exec` body runs in the forked child before `exec` and is kept
/// async-signal-safe (a single `setsid` syscall, no allocation).
pub fn detach(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Directory holding session state files. Prefers `$XDG_RUNTIME_DIR/kitwin`
/// (per-user, cleaned on logout); falls back to a uid-scoped `/tmp` dir.
fn sessions_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let mut p = PathBuf::from(dir);
        p.push("kitwin");
        p
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/kitwin-{}", uid))
    }
}

fn state_path(name: &str) -> PathBuf {
    let mut p = sessions_dir();
    p.push(format!("{}.session", name));
    p
}

/// Session names become file names, so restrict them to a safe character set
/// (no path separators, no traversal).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn pid_alive(pid: u32) -> bool {
    // kill(pid, 0) probes existence without sending a signal.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn display_socket_exists(display: u8) -> bool {
    Path::new(&format!("/tmp/.X11-unix/X{}", display)).exists()
}

/// A session is alive only if its Xvfb pid is still running *and* the display's
/// X socket is present (guards against a recycled pid).
pub fn is_alive(state: &State) -> bool {
    pid_alive(state.xvfb_pid) && display_socket_exists(state.display)
}

/// Decide whether to attach to an existing live session or create a new one.
/// A stale/unparseable state file is removed so we create cleanly.
pub fn resolve(name: &str) -> Role {
    let path = state_path(name);
    if let Ok(contents) = fs::read_to_string(&path) {
        if let Some(state) = parse(&contents) {
            if is_alive(&state) {
                return Role::Attach(state);
            }
        }
        let _ = fs::remove_file(&path);
    }
    Role::Create
}

/// Persist session metadata atomically (write temp + rename).
pub fn write_state(name: &str, state: &State) -> io::Result<()> {
    let dir = sessions_dir();
    fs::create_dir_all(&dir)?;
    let path = state_path(name);
    let tmp = path.with_extension("session.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        writeln!(f, "display={}", state.display)?;
        writeln!(f, "width={}", state.width)?;
        writeln!(f, "height={}", state.height)?;
        writeln!(f, "xvfb_pid={}", state.xvfb_pid)?;
        if let Some(j) = state.jwm_pid {
            writeln!(f, "jwm_pid={}", j)?;
        }
        if let Some(s) = &state.pulse_sink {
            writeln!(f, "pulse_sink={}", s)?;
        }
        if let Some(s) = &state.pulse_server {
            writeln!(f, "pulse_server={}", s)?;
        }
    }
    fs::rename(&tmp, &path)
}

fn parse(contents: &str) -> Option<State> {
    let mut display = None;
    let mut width = None;
    let mut height = None;
    let mut xvfb_pid = None;
    let mut jwm_pid = None;
    let mut pulse_sink = None;
    let mut pulse_server = None;
    for line in contents.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "display" => display = v.parse().ok(),
            "width" => width = v.parse().ok(),
            "height" => height = v.parse().ok(),
            "xvfb_pid" => xvfb_pid = v.parse().ok(),
            "jwm_pid" => jwm_pid = v.parse().ok(),
            "pulse_sink" => pulse_sink = Some(v.to_string()),
            "pulse_server" => pulse_server = Some(v.to_string()),
            _ => {}
        }
    }
    Some(State {
        display: display?,
        width: width?,
        height: height?,
        xvfb_pid: xvfb_pid?,
        jwm_pid,
        pulse_sink,
        pulse_server,
    })
}

fn signal(pid: u32, sig: libc::c_int) {
    unsafe {
        libc::kill(pid as i32, sig);
    }
}

/// Tear down a session: SIGTERM the recorded pids, give them a moment, then
/// SIGKILL. Killing Xvfb collapses the display, which makes jwm and any launched
/// apps (including those started by other attached kitwin instances) exit on
/// their own via an X I/O error.
pub fn kill_state(state: &State) {
    if let Some(j) = state.jwm_pid {
        signal(j, libc::SIGTERM);
    }
    signal(state.xvfb_pid, libc::SIGTERM);
    std::thread::sleep(Duration::from_millis(300));
    if let Some(j) = state.jwm_pid {
        signal(j, libc::SIGKILL);
    }
    signal(state.xvfb_pid, libc::SIGKILL);
}

/// Kill the named session (if present) and remove its state file. Returns true
/// if a state file existed.
pub fn kill_by_name(name: &str) -> bool {
    let path = state_path(name);
    let existed = match fs::read_to_string(&path) {
        Ok(contents) => {
            if let Some(state) = parse(&contents) {
                kill_state(&state);
            }
            true
        }
        Err(_) => false,
    };
    let _ = fs::remove_file(&path);
    existed
}

/// List known sessions as `(name, alive)`, sorted by name.
pub fn list() -> Vec<(String, bool)> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(sessions_dir()) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("session") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let alive = fs::read_to_string(&path)
                .ok()
                .and_then(|c| parse(&c))
                .map(|s| is_alive(&s))
                .unwrap_or(false);
            out.push((name.to_string(), alive));
        }
    }
    out.sort();
    out
}
