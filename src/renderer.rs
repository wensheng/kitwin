use crate::capture::CaptureMsg;
use crate::kitty;
use crossterm::{
    cursor::MoveTo,
    execute,
    terminal::{self, Clear, ClearType},
};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn run_renderer(
    rx: Receiver<CaptureMsg>,
    recycle_tx: SyncSender<Vec<u8>>,
    running: Arc<AtomicBool>,
    fps: u32,
    status_msg: Arc<Mutex<String>>,
    stdout: Arc<Mutex<io::Stdout>>,
    prompt_active: Arc<AtomicBool>,
) {
    let frame_duration = Duration::from_micros(1_000_000 / fps.max(1) as u64);
    let mut last_frame = Instant::now();
    let mut current_image_id: Option<u32> = None;
    let mut next_image_id = kitty::STREAM_IMAGE_ID_START;
    let mut last_terminal_size: Option<(u16, u16)> = None;

    while running.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(CaptureMsg::Frame {
                rgba,
                width,
                height,
            }) => {
                let now = Instant::now();
                let elapsed = now.duration_since(last_frame);
                if elapsed < frame_duration {
                    std::thread::sleep(frame_duration - elapsed);
                }
                last_frame = Instant::now();

                let (cols, rows) = terminal::size().unwrap_or((80, 24));

                // Center the captured desktop in the terminal, scaling it down
                // to fit when it is larger than the visible cell grid. The
                // bottom row is reserved for the status bar / prompt.
                let (cell_w, cell_h) = match terminal::window_size() {
                    Ok(w) if w.columns > 0 && w.rows > 0 && w.width > 0 && w.height > 0 => (
                        w.width as f64 / w.columns as f64,
                        w.height as f64 / w.rows as f64,
                    ),
                    _ => (8.0, 16.0),
                };
                let render_rows = rows.saturating_sub(1).max(1);
                let placement =
                    crate::layout::placement(width, height, cols, render_rows, cell_w, cell_h);

                // While an input prompt is up, the input thread owns the bottom
                // row. Keep streaming the image so video doesn't freeze, but skip
                // the status bar so we don't clobber the prompt.
                let draw_status = !prompt_active.load(Ordering::SeqCst);

                let mut guard = stdout.lock().unwrap();
                let out = &mut *guard;

                if last_terminal_size != Some((cols, rows)) {
                    let _ = kitty::delete_visible_images_to(out);
                    let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
                    current_image_id = None;
                    last_terminal_size = Some((cols, rows));
                }

                let image_id = next_image_id;
                next_image_id = next_stream_image_id(next_image_id);
                let old_image_id = current_image_id;

                let _ = execute!(out, MoveTo(placement.col_offset, placement.row_offset));
                if kitty::write_rgba_frame_scaled_with_id_to(
                    out,
                    &rgba,
                    width,
                    height,
                    placement.cols as u32,
                    placement.rows as u32,
                    image_id,
                    true,
                )
                .is_ok()
                {
                    current_image_id = Some(image_id);
                    if let Some(old_image_id) = old_image_id {
                        let _ = kitty::delete_image_data_to(out, old_image_id);
                    }
                }

                let _ = recycle_tx.try_send(rgba);

                if draw_status {
                    let status = {
                        let lock = status_msg.lock().unwrap();
                        lock.clone()
                    };
                    let _ = execute!(out, MoveTo(0, rows.saturating_sub(1)));
                    let bar = format!(
                        " {:<width$}",
                        status,
                        width = cols.saturating_sub(1) as usize
                    );
                    let _ = write!(
                        out,
                        "\x1b[7m{}\x1b[0m",
                        &bar[..bar.len().min(cols as usize)]
                    );
                }
                let _ = out.flush();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        }
    }

    // Clear screen on exit
    let mut guard = stdout.lock().unwrap();
    let out = &mut *guard;
    let _ = kitty::delete_visible_images_to(out);
    let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
    let _ = out.flush();
}

fn next_stream_image_id(image_id: u32) -> u32 {
    image_id
        .checked_add(1)
        .filter(|next| *next >= kitty::STREAM_IMAGE_ID_START)
        .unwrap_or(kitty::STREAM_IMAGE_ID_START)
}
