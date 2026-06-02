# kitwin

A full Linux desktop, rendered inside your terminal, with persistent session (like TMUX for desktop).

[![demo]](https://github.com/user-attachments/assets/03f1abcc-2223-4160-959c-3df6e7109ba9)

[longer version (3min)](https://youtu.be/_43YNWlC-oU?si=wTgievSth4RsISxp) on Youtube.

## Installation

    cargo install kitwin

### System dependencies (Debian/Ubuntu)

```bash
sudo apt install xvfb xdotool jwm libavdevice-dev pulseaudio-utils \
                 pipewire-pulse libasound2-dev libasound2-plugins
```

…plus any X11 apps you want to launch from JWM (`xterm`, `firefox`, `thunar`, …).

---

## Why?

- **SSH into a headless box** and get a real X11 desktop in one tab — no X-forwarding, no VNC, no RDP.
- **Run X-only apps on a Wayland host** without juggling Xwayland sessions.
- **Sandbox a desktop** in a process you can `Ctrl+C` to kill cleanly.
- **Demo, screencast, or pair-program** on an X session that lives entirely in stdout.

## Features

- 🖼️ **Native-pixel rendering** via the Kitty Graphics Protocol — no scaling, no tearing
- 🖱️ **Real mouse** — click menus, scroll. Right-click opens JWM's root menu
- ⌨️ **Key forwarding** — arrows, Tab, Home/End, PageUp/Down, Esc, Space go straight to the focused app
- 🚀 **Run-anything prompt** — press `r`, type `firefox`, hit Enter
- 🔊 **Audio routing** — apps inside JWM play through a dedicated PulseAudio sink with volume + mute control
- 🪶 **Tiny footprint** — JWM uses a few MB of RAM; the whole stack is one ~1.6 MB Rust binary
- 🛟 **Clean shutdown** — `q` kills every child it spawned, including Xvfb and JWM

## Usage

```
kitwin [OPTIONS]

  --width <PX>                   Virtual display width   [default: 1920]
  --height <PX>                  Virtual display height  [default: 1200]
  --fps <N>                      Capture frame rate      [default: 30]
  --display <N>                  Xvfb display number     [default: 99]
  --no-audio                     Disable audio capture
  --audio-capture-server <ADDR>  PulseAudio/PipeWire server URL
  --exec <CMD>                   Launch this command after JWM starts
  --jwm-config <PATH>            Custom JWM rc file (passed to `jwm -rc`)
```

### Examples

```bash
kitwin                                  # blank JWM, right-click for menu
kitwin --exec xterm                     # autostart a terminal
kitwin --exec firefox --width 2560 --height 1440 --fps 60
kitwin --jwm-config ~/.jwmrc            # custom menu / keybindings
kitwin --no-audio                       # silent mode
```

## Keyboard

Every keystroke — plain characters, modifier chords (`Ctrl+C`, `Alt+Tab`, …),
arrows, Tab, function keys — is forwarded straight to the focused inner window.
Local kitwin commands live behind a **`Ctrl+B` leader** (tmux-style) so they never
shadow keys the app needs.

| Key                                    | Action                                  |
| -------------------------------------- | --------------------------------------- |
| *(any key)*                            | Forward to the focused window           |
| `Ctrl+B` then `r`                      | Run prompt — execute any shell command  |
| `Ctrl+B` then `i`                      | Type-text into the focused window       |
| `Ctrl+B` then `m`                      | Toggle mute                             |
| `Ctrl+B` then `+` / `-`                | Volume up / down (5% steps)             |
| `Ctrl+B` then `q`                      | Quit (kills JWM, Xvfb, and all children)|
| `Ctrl+B` `Ctrl+B`                      | Send a literal `Ctrl+B` to the app      |
| `Ctrl+C`                               | Quit (kills JWM, Xvfb, and all children)|
| Mouse click · scroll                   | Forward to the virtual display          |

Right-clicking the root area opens **JWM's root menu** — your launcher.

## How it works

`kitwin` boots the [JWM window manager](https://joewing.net/projects/jwm/) inside a hidden X server, captures the screen with FFmpeg, and streams it to a [Kitty-protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/) terminal as native-resolution graphics.


```
┌──────────────────────────────────────────────────────────┐
│                  Kitty terminal (you)                    │
│  stdin  ──crossterm──▶  input thread  ──xdotool──┐       │
│                                                  ▼       │
│  stdout ◀──Kitty graphics protocol── render thread       │
│                                            ▲             │
│                                            │             │
│         ┌──────────────────────────────────┴──────┐      │
│         │ FFmpeg x11grab capture thread (RGBA)   │      │
│         └────────────────────▲────────────────────┘      │
│                              │                           │
│                       :99 Xvfb display                   │
│                       ├── jwm                            │
│                       ├── xterm / firefox / …            │
│                       └── (apps you launch via menu)     │
│                              │                           │
│                       PULSE_SINK=kitwin_<pid>            │
│                              │                           │
│                       FFmpeg pulse → CPAL → speakers     │
└──────────────────────────────────────────────────────────┘
```

Four threads, all `mpsc`-glued:
- **Capture** — FFmpeg `x11grab` from the hidden Xvfb display, decoded to RGBA at the display's native size.
- **Render** — paces frames to your `--fps`, encodes each one as Kitty's `\x1b_Ga=T,f=32,…` graphic, draws a status bar on the last row.
- **Input** — crossterm reads your keys/mouse and forwards them through `xdotool` to the X server.
- **Audio** — a per-process PulseAudio null sink captures all audio from JWM-launched apps and replays it through CPAL.

## Requirements

- A Kitty-protocol terminal: [kitty](https://sw.kovidgoyal.net/kitty/), [Ghostty](https://ghostty.org/), [WezTerm](https://wezfurlong.org/wezterm/), [Konsole 25.04+](https://konsole.kde.org/), or another with graphics-protocol support
- Linux with Xvfb (Wayland host is fine — Xvfb is its own X server)
- PulseAudio or PipeWire (for `--exec`-launched audio apps; `--no-audio` skips this)

