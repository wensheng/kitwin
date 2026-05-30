use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "kitwin",
    version,
    about = "A terminal window-manager (JWM) proxy using the Kitty graphics protocol"
)]
pub struct Config {
    /// Virtual display width in pixels
    #[arg(long, default_value_t = 1920)]
    pub width: u32,

    /// Virtual display height in pixels
    #[arg(long, default_value_t = 1200)]
    pub height: u32,

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
}
