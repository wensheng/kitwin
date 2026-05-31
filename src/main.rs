mod audio;
mod capture;
mod config;
mod ffmpeg;
mod input;
mod kitty;
mod renderer;
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
    // Require Kitty terminal
    if std::env::var("KITTY_WINDOW_ID").is_err() {
        let term = std::env::var("TERM").unwrap_or_default();
        if term != "xterm-kitty" {
            eprintln!("kitwin requires the Kitty terminal emulator (KITTY_WINDOW_ID not set).");
            std::process::exit(1);
        }
    }

    let config = Config::parse();

    // Start Xvfb
    let mut session = match WmSession::new(config.display, config.width, config.height) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kitwin: {}", e);
            std::process::exit(1);
        }
    };

    let running = Arc::new(AtomicBool::new(true));
    let status_msg: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    let (audio_runtime, startup_audio_status) = if config.no_audio {
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
    let audio_control_unavailable_status = if config.no_audio {
        String::from("audio disabled")
    } else {
        startup_audio_status
            .clone()
            .unwrap_or_else(|| String::from("audio unavailable"))
    };

    if let Err(e) = session.start_jwm(
        audio_runtime
            .as_ref()
            .map(|runtime| runtime.pulse_audio_env()),
        config.jwm_config.as_deref(),
    ) {
        running.store(false, Ordering::SeqCst);
        drop(audio_runtime);
        eprintln!("kitwin: {}", e);
        std::process::exit(1);
    }

    if let Some(cmd) = config.exec.as_deref() {
        if let Err(e) = session.exec_on_display(cmd) {
            let mut s = status_msg.lock().unwrap();
            *s = format!("Exec error: {}", e);
        }
    }

    let display = session.display;
    let wm_session = Arc::new(Mutex::new(session));

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
    let cap_w = config.width;
    let cap_h = config.height;
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
        );
    });
    if let Some(status) = startup_audio_status {
        let mut s = status_msg.lock().unwrap();
        *s = status;
    }

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
            Ok(ControlCmd::Quit) => {
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
}
