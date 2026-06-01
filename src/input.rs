use crate::wm::WmSession;
use crossterm::{
    cursor::MoveTo,
    event::{
        self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{self, enable_raw_mode, Clear, ClearType},
};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub enum ControlCmd {
    Exec(String),
    ToggleMute,
    VolumeBy(i32),
    Quit,
    /// Leave the persistent X session running and exit (session mode only).
    Detach,
    #[allow(dead_code)]
    Resize(u16, u16),
}

const STATUS_HELP: &str =
    "keys/mouse -> app | C-b leader: r=run i=type m=mute +/-=vol q=quit (C-b C-b sends C-b)";
const FALLBACK_CELL_WIDTH_PX: f64 = 10.0;
const FALLBACK_CELL_HEIGHT_PX: f64 = 20.0;

pub fn run_input(
    control_tx: std::sync::mpsc::SyncSender<ControlCmd>,
    running: Arc<AtomicBool>,
    wm: Arc<Mutex<WmSession>>,
    status_msg: Arc<Mutex<String>>,
    stdout: Arc<Mutex<io::Stdout>>,
    prompt_active: Arc<AtomicBool>,
    session_mode: bool,
) {
    {
        let mut s = status_msg.lock().unwrap();
        *s = if session_mode {
            format!("{} | C-b d=detach", STATUS_HELP)
        } else {
            STATUS_HELP.to_string()
        };
    }

    // F2: all keys are forwarded to the focused inner window. The local commands
    // (run/type/mute/volume/quit) live behind a Ctrl+B leader so they don't
    // shadow keys the app needs. `leader_pending` is set for exactly one keystroke
    // after the leader is pressed.
    let mut leader_pending = false;

    while running.load(Ordering::SeqCst) {
        if !event::poll(Duration::from_millis(50)).unwrap_or(false) {
            continue;
        }

        let ev = match event::read() {
            Ok(e) => e,
            Err(_) => break,
        };

        match ev {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if leader_pending {
                    leader_pending = false;
                    handle_leader_command(
                        key, &wm, &control_tx, &running, &status_msg, &stdout, &prompt_active,
                        session_mode,
                    );
                    if !running.load(Ordering::SeqCst) {
                        break;
                    }
                } else if is_leader(&key) {
                    leader_pending = true;
                } else {
                    forward_key(&wm, key);
                }
            }

            // Mouse click / scroll forwarding (right-click opens JWM root menu).
            // Motion/drag is deliberately not forwarded: doing so spawned an
            // xdotool per event and made interaction unusable.
            Event::Mouse(m) => match m.kind {
                MouseEventKind::Down(button) => {
                    let button = mouse_button_number(button);
                    let b = wm.lock().unwrap();
                    if let Some((x, y)) = click_position(&b, m.column, m.row) {
                        if let Err(e) = b.click_at(button, x, y) {
                            set_status(&status_msg, &format!("Click error: {}", e));
                        }
                    }
                }
                MouseEventKind::ScrollUp => {
                    let b = wm.lock().unwrap();
                    b.scroll(1);
                }
                MouseEventKind::ScrollDown => {
                    let b = wm.lock().unwrap();
                    b.scroll(-1);
                }
                MouseEventKind::ScrollLeft => {
                    let b = wm.lock().unwrap();
                    b.scroll_horizontal(1);
                }
                MouseEventKind::ScrollRight => {
                    let b = wm.lock().unwrap();
                    b.scroll_horizontal(-1);
                }
                _ => {}
            },

            Event::Resize(cols, rows) => {
                let _ = control_tx.try_send(ControlCmd::Resize(cols, rows));
            }

            _ => {}
        }
    }
}

fn set_status(status_msg: &Arc<Mutex<String>>, msg: &str) {
    let mut s = status_msg.lock().unwrap();
    *s = msg.to_string();
}

/// True if `key` is the Ctrl+B leader.
fn is_leader(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('b') | KeyCode::Char('B'))
}

/// Run the local command bound to `key` after the leader was pressed. Anything
/// unrecognized is ignored (the leader is simply cancelled).
#[allow(clippy::too_many_arguments)]
fn handle_leader_command(
    key: KeyEvent,
    wm: &Arc<Mutex<WmSession>>,
    control_tx: &std::sync::mpsc::SyncSender<ControlCmd>,
    running: &Arc<AtomicBool>,
    status_msg: &Arc<Mutex<String>>,
    stdout: &Arc<Mutex<io::Stdout>>,
    prompt_active: &Arc<AtomicBool>,
    session_mode: bool,
) {
    // Leader pressed twice → send a literal leader chord to the app.
    if is_leader(&key) {
        wm.lock().unwrap().send_key("ctrl+b");
        return;
    }

    match key.code {
        KeyCode::Char('q') => {
            running.store(false, Ordering::SeqCst);
            let _ = control_tx.try_send(ControlCmd::Quit);
        }
        // Detach: exit but leave the persistent X session running. Only bound
        // in session mode so it never shadows a key for non-session users.
        KeyCode::Char('d') if session_mode => {
            running.store(false, Ordering::SeqCst);
            let _ = control_tx.try_send(ControlCmd::Detach);
        }
        KeyCode::Char('r') => {
            if let Some(cmd) = prompt_run(status_msg, stdout, prompt_active) {
                let _ = control_tx.try_send(ControlCmd::Exec(cmd));
            }
            set_status(status_msg, STATUS_HELP);
        }
        KeyCode::Char('i') => {
            if let Some(text) = prompt_input(status_msg, stdout, prompt_active) {
                let b = wm.lock().unwrap();
                if let Err(e) = b.type_text_and_enter(&text) {
                    set_status(status_msg, &format!("Input error: {}", e));
                }
            }
        }
        KeyCode::Char('m') => {
            let _ = control_tx.try_send(ControlCmd::ToggleMute);
        }
        KeyCode::Char('+') | KeyCode::Char('=') => {
            let _ = control_tx.try_send(ControlCmd::VolumeBy(5));
        }
        KeyCode::Char('-') => {
            let _ = control_tx.try_send(ControlCmd::VolumeBy(-5));
        }
        _ => {}
    }
}

/// Forward a key event to the focused inner window. Plain printable characters
/// are typed (so the right glyph appears regardless of keysym naming); modifier
/// chords and special keys are injected as `xdotool` key chords.
fn forward_key(wm: &Arc<Mutex<WmSession>>, key: KeyEvent) {
    let mods = key.modifiers;
    let has_chord = mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);

    match key.code {
        KeyCode::Char(c) if !has_chord => {
            // No Ctrl/Alt/Super: type the literal character (Shift is already
            // reflected in `c`'s case/symbol by crossterm).
            wm.lock().unwrap().type_text(&c.to_string());
        }
        KeyCode::Char(c) => {
            let chord = format!("{}{}", modifier_prefix(mods), char_keysym(c));
            wm.lock().unwrap().send_key(&chord);
        }
        KeyCode::F(n) => {
            let chord = format!("{}F{}", modifier_prefix(mods), n);
            wm.lock().unwrap().send_key(&chord);
        }
        KeyCode::BackTab => {
            // Shift+Tab; crossterm folds the shift into BackTab itself.
            wm.lock().unwrap().send_key("shift+Tab");
        }
        other => {
            if let Some(sym) = special_keysym(other) {
                let chord = format!("{}{}", modifier_prefix(mods), sym);
                wm.lock().unwrap().send_key(&chord);
            }
        }
    }
}

/// Build the `mod+...+` prefix for an xdotool key chord. Empty if no modifiers.
fn modifier_prefix(mods: KeyModifiers) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if mods.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl");
    }
    if mods.contains(KeyModifiers::ALT) {
        parts.push("alt");
    }
    if mods.contains(KeyModifiers::SUPER) {
        parts.push("super");
    }
    if mods.contains(KeyModifiers::SHIFT) {
        parts.push("shift");
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{}+", parts.join("+"))
    }
}

/// X keysym name for a non-typed special key (used when forwarding with or
/// without modifiers). Returns `None` for keys we don't forward.
fn special_keysym(code: KeyCode) -> Option<&'static str> {
    Some(match code {
        KeyCode::Up => "Up",
        KeyCode::Down => "Down",
        KeyCode::Left => "Left",
        KeyCode::Right => "Right",
        KeyCode::Home => "Home",
        KeyCode::End => "End",
        KeyCode::PageUp => "Page_Up",
        KeyCode::PageDown => "Page_Down",
        KeyCode::Tab => "Tab",
        KeyCode::Enter => "Return",
        KeyCode::Esc => "Escape",
        KeyCode::Backspace => "BackSpace",
        KeyCode::Delete => "Delete",
        KeyCode::Insert => "Insert",
        _ => return None,
    })
}

/// Base keysym for a character used inside a modifier chord. Letters are
/// lowercased (the chord carries Shift separately); common punctuation maps to
/// its X keysym name; everything else falls back to the literal character.
fn char_keysym(c: char) -> String {
    if c == ' ' {
        return "space".to_string();
    }
    if c.is_ascii_alphabetic() {
        return c.to_ascii_lowercase().to_string();
    }
    let named = match c {
        '/' => "slash",
        '\\' => "backslash",
        '.' => "period",
        ',' => "comma",
        ';' => "semicolon",
        ':' => "colon",
        '\'' => "apostrophe",
        '"' => "quotedbl",
        '`' => "grave",
        '~' => "asciitilde",
        '-' => "minus",
        '_' => "underscore",
        '=' => "equal",
        '+' => "plus",
        '[' => "bracketleft",
        ']' => "bracketright",
        '{' => "braceleft",
        '}' => "braceright",
        '(' => "parenleft",
        ')' => "parenright",
        '<' => "less",
        '>' => "greater",
        '!' => "exclam",
        '@' => "at",
        '#' => "numbersign",
        '$' => "dollar",
        '%' => "percent",
        '^' => "asciicircum",
        '&' => "ampersand",
        '*' => "asterisk",
        '?' => "question",
        '|' => "bar",
        _ => return c.to_string(),
    };
    named.to_string()
}

fn prompt_run(
    status_msg: &Arc<Mutex<String>>,
    stdout: &Arc<Mutex<io::Stdout>>,
    prompt_active: &Arc<AtomicBool>,
) -> Option<String> {
    prompt_line(status_msg, stdout, prompt_active, "Run:")
}

fn prompt_input(
    status_msg: &Arc<Mutex<String>>,
    stdout: &Arc<Mutex<io::Stdout>>,
    prompt_active: &Arc<AtomicBool>,
) -> Option<String> {
    prompt_line(status_msg, stdout, prompt_active, "Input:")
}

fn prompt_line(
    status_msg: &Arc<Mutex<String>>,
    stdout: &Arc<Mutex<io::Stdout>>,
    prompt_active: &Arc<AtomicBool>,
    label: &str,
) -> Option<String> {
    let _ = enable_raw_mode();
    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    // Tell the render thread to stop drawing the bottom row so it won't clobber
    // our prompt. It keeps streaming the image, so video keeps playing while we
    // type. We only grab the stdout lock briefly per redraw (not for the whole
    // session), leaving the render thread free to draw frames between keystrokes.
    prompt_active.store(true, Ordering::SeqCst);

    let mut input = String::new();
    set_prompt_status(status_msg, label, &input);
    redraw_prompt(stdout, rows, cols, label, &input);

    let mut submitted = false;
    loop {
        if !event::poll(Duration::from_millis(200)).unwrap_or(false) {
            continue;
        }
        if let Ok(Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            ..
        })) = event::read()
        {
            match code {
                KeyCode::Enter => {
                    submitted = true;
                    break;
                }
                KeyCode::Esc => {
                    input.clear();
                    break;
                }
                KeyCode::Backspace => {
                    input.pop();
                    set_prompt_status(status_msg, label, &input);
                    redraw_prompt(stdout, rows, cols, label, &input);
                }
                KeyCode::Char(c) => {
                    if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                        continue;
                    }
                    input.push(c);
                    set_prompt_status(status_msg, label, &input);
                    redraw_prompt(stdout, rows, cols, label, &input);
                }
                _ => {}
            }
        }
    }

    prompt_active.store(false, Ordering::SeqCst);
    let _ = enable_raw_mode();

    set_status(status_msg, STATUS_HELP);
    if submitted && !input.is_empty() {
        Some(input)
    } else {
        None
    }
}

fn set_prompt_status(status_msg: &Arc<Mutex<String>>, label: &str, input: &str) {
    let escaped_input: String = input.chars().flat_map(|c| c.escape_default()).collect();
    set_status(status_msg, &format!("{} {}", label, escaped_input));
}

fn redraw_prompt(
    stdout: &Arc<Mutex<io::Stdout>>,
    rows: u16,
    cols: u16,
    label: &str,
    input: &str,
) {
    let mut guard = stdout.lock().unwrap();
    let out = &mut *guard;
    let _ = execute!(out, MoveTo(0, rows.saturating_sub(1)));
    let _ = execute!(out, Clear(ClearType::CurrentLine));
    let _ = write!(out, "\x1b[7m {} \x1b[0m ", label);

    let label_width = label.chars().count() + 3;
    let available = (cols as usize).saturating_sub(label_width + 1);
    let visible_input: String = input.chars().take(available).collect();
    let _ = write!(out, "{}", visible_input);
    let _ = out.flush();
}

fn mouse_button_number(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
    }
}

fn click_position(wm: &WmSession, column: u16, row: u16) -> Option<(u32, u32)> {
    if wm.width == 0 || wm.height == 0 {
        return None;
    }

    let window = terminal::window_size().ok();
    let (cols, rows) = match window.as_ref() {
        Some(size) if size.columns > 0 && size.rows > 0 => (size.columns, size.rows),
        _ => terminal::size().ok()?,
    };
    let render_rows = rows.checked_sub(1)?; // bottom row is the status bar
    if render_rows == 0 || row >= render_rows {
        return None;
    }

    // Map the click through the same centred, scale-to-fit placement the
    // renderer uses, so clicks land on the pixel under the cursor.
    let (cell_width_px, cell_height_px) = terminal_cell_size_px(window.as_ref());
    let placement = crate::layout::placement(
        wm.width,
        wm.height,
        cols,
        render_rows,
        cell_width_px,
        cell_height_px,
    );
    crate::layout::cell_to_source(placement, column, row, wm.width, wm.height)
}

fn terminal_cell_size_px(window: Option<&terminal::WindowSize>) -> (f64, f64) {
    let Some(window) = window else {
        return (FALLBACK_CELL_WIDTH_PX, FALLBACK_CELL_HEIGHT_PX);
    };

    let cell_width = if window.columns > 0 && window.width > 0 {
        window.width as f64 / window.columns as f64
    } else {
        FALLBACK_CELL_WIDTH_PX
    };
    let cell_height = if window.rows > 0 && window.height > 0 {
        window.height as f64 / window.rows as f64
    } else {
        FALLBACK_CELL_HEIGHT_PX
    };

    (cell_width, cell_height)
}
