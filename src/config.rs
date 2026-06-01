use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "kitwin",
    version,
    about = "A terminal window-manager (JWM) proxy using the Kitty graphics protocol"
)]
pub struct Config {
    /// Virtual display width in pixels (defaults to the terminal's reported
    /// pixel width, so the desktop fills the window at native 1:1)
    #[arg(long)]
    pub width: Option<u32>,

    /// Virtual display height in pixels (defaults to the terminal's reported
    /// pixel height minus the status bar row)
    #[arg(long)]
    pub height: Option<u32>,

    /// Capture frame rate
    #[arg(long, default_value_t = 30)]
    pub fps: u32,

    /// Xvfb display number
    #[arg(long, default_value_t = 99)]
    pub display: u8,

    /// Disable audio capture/playback
    #[arg(long)]
    pub no_audio: bool,

    /// PulseAudio/PipeWire server used for audio capture
    #[arg(long)]
    pub audio_capture_server: Option<String>,

    /// Optional command to launch inside the virtual display after JWM starts
    #[arg(long)]
    pub exec: Option<String>,

    /// Path to a JWM rc file (passed to `jwm -rc`)
    #[arg(long)]
    pub jwm_config: Option<PathBuf>,

    /// Create or attach to a named persistent session ("tmux for X"). The X
    /// session (Xvfb/jwm/apps) survives detach (Ctrl+B d) and quitting via a
    /// signal; re-run with the same name to reattach. Audio is disabled in
    /// session mode.
    #[arg(long)]
    pub session: Option<String>,

    /// List persistent sessions and whether each is still alive, then exit.
    #[arg(long)]
    pub list_sessions: bool,

    /// Terminate a named persistent session (kills its Xvfb/jwm/apps) and exit.
    #[arg(long)]
    pub kill_session: Option<String>,
}
