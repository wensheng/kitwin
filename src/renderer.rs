use crate::capture::CaptureMsg;
use crate::diff::TileGrid;
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

    // P2 state: the previously-rendered frame (for tile diffing), a reusable
    // scratch buffer for the current tile's pixels, and the cell-aligned tile
    // grid (rebuilt only when frame size or terminal geometry changes).
    let mut prev: Vec<u8> = Vec::new();
    let mut tile_buf: Vec<u8> = Vec::new();
    let mut grid: Option<TileGrid> = None;
    let mut grid_key: (u32, u32, u16, u16) = (0, 0, 0, 0);

    while running.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(CaptureMsg::Frame { rgba, width, height }) => {
                let now = Instant::now();
                let elapsed = now.duration_since(last_frame);
                if elapsed < frame_duration {
                    std::thread::sleep(frame_duration - elapsed);
                }
                last_frame = Instant::now();

                let (cols, rows) = terminal::size().unwrap_or((80, 24));
                let (cell_w, cell_h) = cell_size_px();

                // Rebuild the tile grid when the frame size or terminal geometry
                // changes; force a full redraw so no stale tiles linger.
                let key = (width, height, cols, rows);
                if grid.is_none() || key != grid_key {
                    grid = Some(TileGrid::new(width, height, cell_w, cell_h));
                    grid_key = key;
                    prev.clear();
                }
                let grid = grid.as_ref().unwrap();

                // While an input prompt is up, the input thread owns the bottom
                // row. Keep streaming the image so video doesn't freeze, but skip
                // the status bar so we don't clobber the prompt.
                let draw_status = !prompt_active.load(Ordering::SeqCst);

                let mut guard = stdout.lock().unwrap();
                let out = &mut *guard;

                // No retained frame (first frame or geometry change) → redraw
                // every tile; otherwise only the tiles whose pixels changed.
                let stride = width as usize * 4;
                let full_redraw = prev.len() != rgba.len();
                if full_redraw {
                    let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
                }

                for (idx, tile) in grid.tiles.iter().enumerate() {
                    if full_redraw || crate::diff::tile_dirty(&rgba, &prev, stride, tile) {
                        crate::diff::extract_tile(&rgba, stride, tile, &mut tile_buf);
                        let _ = execute!(out, MoveTo(tile.col as u16, tile.row as u16));
                        let _ = kitty::write_tile_to(
                            out,
                            &tile_buf,
                            tile.pw,
                            tile.ph,
                            tile.cols,
                            tile.rows,
                            idx as u32 + 1,
                        );
                    }
                }

                // Retain this frame for the next diff (move, no copy), and hand
                // the buffer we just retired back to capture for reuse (P4).
                let retired = std::mem::replace(&mut prev, rgba);
                if !retired.is_empty() {
                    let _ = recycle_tx.try_send(retired);
                }

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
                    let _ = write!(out, "\x1b[7m{}\x1b[0m", &bar[..bar.len().min(cols as usize)]);
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
    let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
    let _ = out.flush();
}

/// Terminal cell size in pixels, derived from the reported window geometry.
/// Mirrors the math in `input.rs` (click mapping); falls back to a typical
/// 10×20 cell when the terminal doesn't report pixel dimensions.
fn cell_size_px() -> (f64, f64) {
    const FALLBACK_W: f64 = 10.0;
    const FALLBACK_H: f64 = 20.0;
    match terminal::window_size() {
        Ok(ws) if ws.columns > 0 && ws.rows > 0 && ws.width > 0 && ws.height > 0 => (
            ws.width as f64 / ws.columns as f64,
            ws.height as f64 / ws.rows as f64,
        ),
        _ => (FALLBACK_W, FALLBACK_H),
    }
}
