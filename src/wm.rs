use crate::audio::PulseAudioEnv;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

pub struct WmSession {
    pub display: u8,
    pub width: u32,
    pub height: u32,
    xvfb: Child,
    jwm: Option<Child>,
    spawned: Vec<Child>,
    pulse_sink: Option<String>,
    pulse_server: Option<String>,
}

impl WmSession {
    pub fn new(display: u8, width: u32, height: u32) -> Result<Self, Box<dyn std::error::Error>> {
        let display = find_free_display(display);
        let mut xvfb_cmd = Command::new("Xvfb");
        xvfb_cmd.args([
            &format!(":{}", display),
            "-screen",
            "0",
            &format!("{}x{}x24", width, height),
            "-ac",
        ]);
        configure_child_output(&mut xvfb_cmd);

        let mut xvfb = xvfb_cmd
            .spawn()
            .map_err(|e| format!("Failed to start Xvfb: {}. Is xvfb installed?", e))?;

        // Give Xvfb time to initialize
        thread::sleep(Duration::from_millis(500));
        if let Some(status) = xvfb.try_wait()? {
            return Err(format!("Xvfb exited immediately with status {}", status).into());
        }

        Ok(Self {
            display,
            width,
            height,
            xvfb,
            jwm: None,
            spawned: Vec::new(),
            pulse_sink: None,
            pulse_server: None,
        })
    }

    pub fn start_jwm(
        &mut self,
        audio_env: Option<PulseAudioEnv<'_>>,
        jwm_config: Option<&Path>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(env) = audio_env.as_ref() {
            self.pulse_sink = Some(env.pulse_sink.to_string());
            self.pulse_server = env.pulse_server.map(|s| s.to_string());
        }

        let display_str = self.display_str();
        let mut cmd = Command::new("jwm");
        cmd.env("DISPLAY", &display_str)
            .env("GTK_IM_MODULE", "xim")
            .env("QT_IM_MODULE", "xim")
            .env("XMODIFIERS", "@im=none")
            .env_remove("WAYLAND_DISPLAY");

        if let Some(env) = audio_env.as_ref() {
            cmd.env("PULSE_SINK", env.pulse_sink);
            if let Some(server) = env.pulse_server {
                cmd.env("PULSE_SERVER", server);
            }
        }
        if let Some(path) = jwm_config {
            cmd.arg("-rc").arg(path);
        }
        configure_child_output(&mut cmd);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to start jwm: {}. Is jwm installed?", e))?;

        thread::sleep(Duration::from_millis(500));
        if let Some(status) = child.try_wait()? {
            return Err(format!("jwm exited immediately with status {}", status).into());
        }

        self.jwm = Some(child);
        Ok(())
    }

    pub fn exec_on_display(&mut self, cmd: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut command = Command::new("sh");
        command.arg("-c").arg(cmd);
        command.env("DISPLAY", self.display_str());
        if let Some(sink) = self.pulse_sink.as_ref() {
            command.env("PULSE_SINK", sink);
        }
        if let Some(server) = self.pulse_server.as_ref() {
            command.env("PULSE_SERVER", server);
        }
        configure_child_output(&mut command);

        let child = command
            .spawn()
            .map_err(|e| format!("Failed to exec '{}': {}", cmd, e))?;
        self.reap_finished();
        self.spawned.push(child);
        Ok(())
    }

    pub fn send_key(&self, key: &str) {
        let _ = Command::new("xdotool")
            .env("DISPLAY", self.display_str())
            .args(["key", "--clearmodifiers", key])
            .output();
    }

    /// Type literal text into the focused window without pressing Enter. `--`
    /// stops xdotool option parsing so text starting with `-` is typed verbatim.
    pub fn type_text(&self, text: &str) {
        let _ = Command::new("xdotool")
            .env("DISPLAY", self.display_str())
            .args(["type", "--clearmodifiers", "--", text])
            .output();
    }

    /// Simulate a mouse scroll: direction 1 = up (button 4), -1 = down (button 5)
    pub fn scroll(&self, direction: i8) {
        let button = if direction > 0 { "4" } else { "5" };
        let _ = Command::new("xdotool")
            .env("DISPLAY", self.display_str())
            .args(["click", button])
            .output();
    }

    /// Simulate a horizontal mouse scroll: direction 1 = left (button 6), -1 = right (button 7)
    pub fn scroll_horizontal(&self, direction: i8) {
        let button = if direction > 0 { "6" } else { "7" };
        let _ = Command::new("xdotool")
            .env("DISPLAY", self.display_str())
            .args(["click", button])
            .output();
    }

    pub fn click_at(&self, button: u8, x: u32, y: u32) -> Result<(), Box<dyn std::error::Error>> {
        Command::new("xdotool")
            .env("DISPLAY", self.display_str())
            .arg("mousemove")
            .arg("--sync")
            .arg(x.to_string())
            .arg(y.to_string())
            .arg("click")
            .arg(button.to_string())
            .output()
            .map_err(|e| format!("xdotool not found: {}. Install xdotool.", e))?;

        Ok(())
    }

    pub fn type_text_and_enter(&self, text: &str) -> Result<(), Box<dyn std::error::Error>> {
        Command::new("xdotool")
            .env("DISPLAY", self.display_str())
            .args(["type", "--clearmodifiers", text])
            .output()
            .map_err(|e| format!("xdotool not found: {}. Install xdotool.", e))?;

        thread::sleep(Duration::from_millis(50));

        Command::new("xdotool")
            .env("DISPLAY", self.display_str())
            .args(["key", "--clearmodifiers", "Return"])
            .output()?;

        Ok(())
    }

    fn display_str(&self) -> String {
        format!(":{}", self.display)
    }

    fn reap_finished(&mut self) {
        self.spawned.retain_mut(|child| match child.try_wait() {
            Ok(Some(_)) => false,
            _ => true,
        });
    }
}

impl Drop for WmSession {
    fn drop(&mut self) {
        for mut child in self.spawned.drain(..) {
            let _ = child.kill();
        }
        if let Some(mut jwm) = self.jwm.take() {
            let _ = jwm.kill();
        }
        let _ = self.xvfb.kill();
    }
}

fn configure_child_output(command: &mut Command) {
    if std::env::var_os("KITWIN_CHILD_LOGS").is_none() {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
}

fn find_free_display(preferred: u8) -> u8 {
    for n in preferred..200 {
        let lock = format!("/tmp/.X{}-lock", n);
        if !std::path::Path::new(&lock).exists() {
            return n;
        }
    }
    preferred
}
