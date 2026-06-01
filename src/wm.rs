use crate::audio::PulseAudioEnv;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

pub struct WmSession {
    pub display: u8,
    pub width: u32,
    pub height: u32,
    /// `None` when attached to a session started by another process (we don't
    /// own the X server, so we must not kill it).
    xvfb: Option<Child>,
    jwm: Option<Child>,
    spawned: Vec<Child>,
    pulse_sink: Option<String>,
    pulse_server: Option<String>,
    /// When false (after detach), `Drop` leaves all child processes running so
    /// the X session survives this kitwin exit. Always true outside session mode.
    kill_on_drop: bool,
    /// When true (session mode), children are spawned detached (`setsid`) so
    /// they survive terminal signals and a detached parent.
    detach_children: bool,
}

impl WmSession {
    /// Default session: spawn a fresh Xvfb owned by (and killed with) this
    /// process. Behavior is unchanged from before session support existed.
    pub fn new(display: u8, width: u32, height: u32) -> Result<Self, Box<dyn std::error::Error>> {
        Self::spawn_new(display, width, height, false)
    }

    /// Persistent session: like [`WmSession::new`] but the Xvfb is detached
    /// (`setsid`) so it can outlive this process, and future children are
    /// detached too. The session is still killed on `Drop` unless
    /// [`WmSession::set_detach`] is called first.
    pub fn new_session(
        display: u8,
        width: u32,
        height: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::spawn_new(display, width, height, true)
    }

    fn spawn_new(
        display: u8,
        width: u32,
        height: u32,
        detach_children: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
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
        if detach_children {
            crate::session::detach(&mut xvfb_cmd);
        }

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
            xvfb: Some(xvfb),
            jwm: None,
            spawned: Vec::new(),
            pulse_sink: None,
            pulse_server: None,
            kill_on_drop: true,
            detach_children,
        })
    }

    /// Attach to an already-running persistent session: capture/inject on its
    /// existing display without spawning Xvfb or jwm. We do not own the X server
    /// or window manager, so `Drop` never kills them; only apps launched during
    /// this attach (tracked in `spawned`) are ours.
    pub fn attach(
        display: u8,
        width: u32,
        height: u32,
        pulse_sink: Option<String>,
        pulse_server: Option<String>,
    ) -> Self {
        Self {
            display,
            width,
            height,
            xvfb: None,
            jwm: None,
            spawned: Vec::new(),
            pulse_sink,
            pulse_server,
            kill_on_drop: true,
            detach_children: true,
        }
    }

    /// Detach: stop owning the child processes so `Drop` leaves the whole X
    /// session running for a later reattach.
    pub fn set_detach(&mut self) {
        self.kill_on_drop = false;
    }

    /// PID of the owned Xvfb, if this process started it (for the state file).
    pub fn xvfb_pid(&self) -> Option<u32> {
        self.xvfb.as_ref().map(|c| c.id())
    }

    /// PID of the owned jwm, if this process started it (for the state file).
    pub fn jwm_pid(&self) -> Option<u32> {
        self.jwm.as_ref().map(|c| c.id())
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
        if self.detach_children {
            crate::session::detach(&mut cmd);
        }

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
        if self.detach_children {
            crate::session::detach(&mut command);
        }

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
        // After a detach, leave every child running so the X session persists.
        if !self.kill_on_drop {
            return;
        }
        for mut child in self.spawned.drain(..) {
            let _ = child.kill();
        }
        if let Some(mut jwm) = self.jwm.take() {
            let _ = jwm.kill();
        }
        if let Some(mut xvfb) = self.xvfb.take() {
            let _ = xvfb.kill();
        }
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
