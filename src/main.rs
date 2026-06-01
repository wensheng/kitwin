mod audio;
mod capture;
mod config;
mod ffmpeg;
mod input;
mod kitty;
mod layout;
mod renderer;
mod session;
mod wm;

use capture::CaptureMsg;
use clap::Parser;
use config::Config;
use crossterm::{
    cursor::{Hide, Show},
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use input::ControlCmd;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread;
use wm::WmSession;

fn main() {
    let config = Config::parse();

    // Session management commands work in any terminal and render no graphics,
    // so handle them before requiring Kitty.
    if config.list_sessions {
        for (name, alive) in session::list() {
            println!("{}\t{}", name, if alive { "alive" } else { "dead" });
        }
        return;
    }
    if let Some(name) = config.kill_session.as_deref() {
        if !session::valid_name(name) {
            eprintln!("kitwin: invalid session name '{}'", name);
            std::process::exit(1);
        }
        if session::kill_by_name(name) {
            println!("kitwin: terminated session '{}'", name);
        } else {
            println!("kitwin: no such session '{}'", name);
        }
        return;
    }

    // Require Kitty terminal for the interactive UI.
    if std::env::var("KITTY_WINDOW_ID").is_err() {
        let term = std::env::var("TERM").unwrap_or_default();
        if term != "xterm-kitty" {
            eprintln!("kitwin requires the Kitty terminal emulator (KITTY_WINDOW_ID not set).");
            std::process::exit(1);
        }
    }

    let session_name = config.session.clone();
    let session_mode = session_name.is_some();
    if let Some(name) = session_name.as_deref() {
        if !session::valid_name(name) {
            eprintln!("kitwin: invalid session name '{}'", name);
            std::process::exit(1);
        }
    }
    // Audio isolation/playback can't survive detach in this version, so session
    // mode runs without it — this leaves the audio path entirely untouched.
    let no_audio = config.no_audio || session_mode;

    // For --session, decide whether to create a new X session or attach to a
    // live one. (Default mode never consults this.)
    let role = session_name.as_deref().map(session::resolve);
    let is_attach = matches!(&role, Some(session::Role::Attach(_)));
    let is_create = matches!(&role, Some(session::Role::Create));

    // Build the WmSession. Attach reuses the running display + geometry; create
    // and default both spawn a fresh Xvfb sized to the terminal.
    //
    // Sizing rationale (create/default): match the terminal's actual pixel
    // dimensions so the desktop fills the window at native 1:1. On HiDPI/Retina
    // Kitty reports physical device pixels (~1.5x the logical size), so a fixed
    // 1920x1200 would only cover part of the window. Explicit --width/--height
    // still win.
    let mut wm = match role {
        Some(session::Role::Attach(state)) => WmSession::attach(
            state.display,
            state.width,
            state.height,
            state.pulse_sink,
            state.pulse_server,
        ),
        _ => {
            let (detected_w, detected_h) = detect_display_size().unwrap_or((1920, 1200));
            let width = config.width.unwrap_or(detected_w);
            let height = config.height.unwrap_or(detected_h);
            let result = if session_mode {
                WmSession::new_session(config.display, width, height)
            } else {
                WmSession::new(config.display, width, height)
            };
            match result {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("kitwin: {}", e);
                    std::process::exit(1);
                }
            }
        }
    };

    let running = Arc::new(AtomicBool::new(true));
    let status_msg: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    let (audio_runtime, startup_audio_status) = if no_audio {
        (None, None)
    } else {
        match audio::AudioRuntime::start(
            config.audio_capture_server.as_deref(),
            running.clone(),
            status_msg.clone(),
        ) {
            Ok(runtime) => {
                let status = runtime.status();
                (Some(runtime), Some(status))
            }
            Err(err) => (None, Some(format!("audio direct fallback: {}", err))),
        }
    };
    let audio_control_unavailable_status = if no_audio {
        String::from("audio disabled")
    } else {
        startup_audio_status
            .clone()
            .unwrap_or_else(|| String::from("audio unavailable"))
    };

    // jwm is already running when attaching; only start it when we own the X
    // server (default or session-create).
    if !is_attach {
        if let Err(e) = wm.start_jwm(
            audio_runtime
                .as_ref()
                .map(|runtime| runtime.pulse_audio_env()),
            config.jwm_config.as_deref(),
        ) {
            running.store(false, Ordering::SeqCst);
            drop(audio_runtime);
            drop(wm); // kill the Xvfb we just spawned
            eprintln!("kitwin: {}", e);
            std::process::exit(1);
        }
    }

    // Record session metadata once the X server + jwm are up so a later run can
    // reattach to this display.
    if is_create {
        if let Some(name) = session_name.as_deref() {
            let state = session::State {
                display: wm.display,
                width: wm.width,
                height: wm.height,
                xvfb_pid: wm.xvfb_pid().unwrap_or(0),
                jwm_pid: wm.jwm_pid(),
                pulse_sink: None,
                pulse_server: None,
            };
            if let Err(e) = session::write_state(name, &state) {
                eprintln!("kitwin: could not write session state: {}", e);
            }
        }
    }

    if let Some(cmd) = config.exec.as_deref() {
        if let Err(e) = wm.exec_on_display(cmd) {
            let mut s = status_msg.lock().unwrap();
            *s = format!("Exec error: {}", e);
        }
    }

    let display = wm.display;
    let width = wm.width;
    let height = wm.height;
    let wm_session = Arc::new(Mutex::new(wm));

    // Ctrl+C handler
    {
        let running2 = running.clone();
        let _ = ctrlc::set_handler(move || {
            running2.store(false, Ordering::SeqCst);
        });
    }

    // Enter terminal UI
    let mut stdout = io::stdout();
    let _ = enable_raw_mode();
    let _ = execute!(
        stdout,
        EnterAlternateScreen,
        Hide,
        Clear(ClearType::All),
        EnableMouseCapture,
    );
    let _ = stdout.flush();

    // Shared stdout so the render and input threads never interleave writes.
    let shared_stdout = Arc::new(Mutex::new(io::stdout()));

    // Set while an input prompt is up: the render thread keeps drawing the image
    // but yields the bottom row to the input thread's prompt.
    let prompt_active = Arc::new(AtomicBool::new(false));

    // Capture → Render channel (bounded; drop frames if renderer is slow)
    let (capture_tx, capture_rx) = sync_channel::<CaptureMsg>(4);
    // Render → Capture buffer recycling channel (P4): the renderer returns the
    // frame buffer it has finished with so capture can refill it instead of
    // allocating ~9.2 MB per frame.
    let (recycle_tx, recycle_rx) = sync_channel::<Vec<u8>>(4);
    let (control_tx, control_rx) = sync_channel::<ControlCmd>(16);

    // Capture thread
    let cap_running = running.clone();
    let cap_fps = config.fps;
    let cap_w = width;
    let cap_h = height;
    let capture_handle = thread::spawn(move || {
        capture::run_capture(display, cap_w, cap_h, cap_fps, capture_tx, recycle_rx, cap_running);
    });

    // Render thread
    let rend_running = running.clone();
    let rend_fps = config.fps;
    let rend_status = status_msg.clone();
    let rend_stdout = shared_stdout.clone();
    let rend_prompt_active = prompt_active.clone();
    let render_handle = thread::spawn(move || {
        renderer::run_renderer(
            capture_rx,
            recycle_tx,
            rend_running,
            rend_fps,
            rend_status,
            rend_stdout,
            rend_prompt_active,
        );
    });

    // Input thread
    let inp_running = running.clone();
    let inp_wm = wm_session.clone();
    let inp_status = status_msg.clone();
    let inp_control = control_tx;
    let inp_stdout = shared_stdout.clone();
    let inp_prompt_active = prompt_active.clone();
    let input_handle = thread::spawn(move || {
        input::run_input(
            inp_control,
            inp_running,
            inp_wm,
            inp_status,
            inp_stdout,
            inp_prompt_active,
            session_mode,
        );
    });
    if let Some(status) = startup_audio_status {
        let mut s = status_msg.lock().unwrap();
        *s = status;
    }

    // In session mode the X session persists by default — only an explicit quit
    // (Ctrl+B q) or --kill-session tears it down — so an accidental Ctrl+C or a
    // closed terminal leaves the session alive to reattach. Always false outside
    // session mode, so default teardown is unchanged.
    let mut keep_session = session_mode;

    // Main thread: handle control commands
    loop {
        if !running.load(Ordering::SeqCst) {
            break;
        }
        match control_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(ControlCmd::Exec(cmd)) => {
                let mut b = wm_session.lock().unwrap();
                if let Err(e) = b.exec_on_display(&cmd) {
                    let mut s = status_msg.lock().unwrap();
                    *s = format!("Exec error: {}", e);
                }
            }
            Ok(ControlCmd::ToggleMute) => {
                let mut status = status_msg.lock().unwrap();
                *status = audio_runtime
                    .as_ref()
                    .map(|runtime| runtime.toggle_mute())
                    .unwrap_or_else(|| audio_control_unavailable_status.clone());
            }
            Ok(ControlCmd::VolumeBy(delta)) => {
                let mut status = status_msg.lock().unwrap();
                *status = audio_runtime
                    .as_ref()
                    .map(|runtime| runtime.volume_by(delta))
                    .unwrap_or_else(|| audio_control_unavailable_status.clone());
            }
            Ok(ControlCmd::Resize(_, _)) => {}
            Ok(ControlCmd::Detach) => {
                keep_session = session_mode;
                running.store(false, Ordering::SeqCst);
                break;
            }
            Ok(ControlCmd::Quit) => {
                keep_session = false;
                running.store(false, Ordering::SeqCst);
                break;
            }
            Err(_) => {}
        }
    }

    running.store(false, Ordering::SeqCst);

    let _ = input_handle.join();
    let _ = render_handle.join();
    let _ = capture_handle.join();
    drop(audio_runtime);

    // Session lifecycle. Detach: stop owning the children so Drop leaves the X
    // session running. Kill: terminate the recorded processes and drop the
    // state file (Drop also kills any children this process owns).
    if session_mode {
        if keep_session {
            wm_session.lock().unwrap().set_detach();
        } else if let Some(name) = session_name.as_deref() {
            session::kill_by_name(name);
        }
    }

    // Restore terminal
    let _ = execute!(
        stdout,
        DisableMouseCapture,
        Show,
        Clear(ClearType::All),
        LeaveAlternateScreen,
    );
    let _ = stdout.flush();
    let _ = disable_raw_mode();

    // Tell the user what happened to the session (after the screen is restored).
    if session_mode {
        if let Some(name) = session_name.as_deref() {
            if keep_session {
                println!(
                    "kitwin: detached from session '{}'. Reattach with: kitwin --session {}",
                    name, name
                );
            } else {
                println!("kitwin: terminated session '{}'", name);
            }
        }
    }
}

/// Detect the terminal's drawable size in pixels for the virtual display.
///
/// Kitty reports the window's size in *physical device pixels* via
/// `TIOCGWINSZ` (`crossterm::terminal::window_size`). On HiDPI/Retina screens
/// that is larger than the logical window size (e.g. ~1.5x on a Mac driving a
/// scaled 4K display), which is exactly what we want: matching it makes each
/// virtual-desktop pixel map to one device pixel, so the desktop fills the
/// window and stays crisp.
///
/// The bottom row is reserved for the status bar, so we subtract one cell of
/// height. Dimensions are rounded down to even numbers (friendlier to apps and
/// scalers). Returns `None` if the terminal does not report pixel sizes.
fn detect_display_size() -> Option<(u32, u32)> {
    let ws = crossterm::terminal::window_size().ok()?;
    if ws.width == 0 || ws.height == 0 || ws.rows == 0 {
        return None;
    }
    let cell_h = ws.height as f64 / ws.rows as f64;
    let usable_h = (ws.height as f64 - cell_h).max(1.0);

    let w = (ws.width as u32) & !1;
    let h = (usable_h as u32) & !1;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}
