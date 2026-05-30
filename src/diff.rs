//! P2 — dirty-rectangle (tile) diffing.
//!
//! The frame is split into a grid of tiles aligned to the terminal's *cell*
//! grid (not to a fixed pixel size). Cell alignment matters: each tile is
//! placed at a whole-cell cursor position and scaled to a whole number of
//! cells via the Kitty protocol's `c`/`r` keys, so tiles abut exactly with no
//! sub-cell overlap or drift — even though the float cell size derived from the
//! window geometry rarely matches the terminal's true integer cell pixels.
//!
//! Each tile maps to a stable image id (its index + 1), so re-transmitting a
//! changed tile replaces the previous one in place rather than accumulating
//! images terminal-side.

/// Target tile edge in pixels; converted to a whole number of cells per axis.
const TILE_TARGET_PX: f64 = 256.0;

/// A single tile: its origin/extent in both cell units (for placement) and
/// pixel units (for slicing out of the RGBA frame).
#[derive(Clone, Copy)]
pub struct Tile {
    pub col: u32,
    pub row: u32,
    pub cols: u32,
    pub rows: u32,
    pub px: u32,
    pub py: u32,
    pub pw: u32,
    pub ph: u32,
}

pub struct TileGrid {
    pub tiles: Vec<Tile>,
}

impl TileGrid {
    /// Build a grid covering a `width_px` × `height_px` frame, given the
    /// (possibly fractional) terminal cell size in pixels.
    pub fn new(width_px: u32, height_px: u32, cell_w: f64, cell_h: f64) -> Self {
        let cell_w = if cell_w > 0.0 { cell_w } else { 1.0 };
        let cell_h = if cell_h > 0.0 { cell_h } else { 1.0 };

        // How many cells the image spans (ceil so the right/bottom edges are
        // covered), and pixel boundaries snapped to cell edges.
        let image_cols = (width_px as f64 / cell_w).ceil() as u32;
        let image_rows = (height_px as f64 / cell_h).ceil() as u32;
        let px_at = |c: u32| ((c as f64 * cell_w).round() as u32).min(width_px);
        let py_at = |r: u32| ((r as f64 * cell_h).round() as u32).min(height_px);

        let tile_cw = ((TILE_TARGET_PX / cell_w).round() as u32).max(1);
        let tile_ch = ((TILE_TARGET_PX / cell_h).round() as u32).max(1);

        let mut tiles = Vec::new();
        let mut r0 = 0;
        while r0 < image_rows {
            let r1 = (r0 + tile_ch).min(image_rows);
            let py = py_at(r0);
            let ph = py_at(r1) - py;
            let mut c0 = 0;
            while c0 < image_cols {
                let c1 = (c0 + tile_cw).min(image_cols);
                let px = px_at(c0);
                let pw = px_at(c1) - px;
                if pw > 0 && ph > 0 {
                    tiles.push(Tile {
                        col: c0,
                        row: r0,
                        cols: c1 - c0,
                        rows: r1 - r0,
                        px,
                        py,
                        pw,
                        ph,
                    });
                }
                c0 = c1;
            }
            r0 = r1;
        }

        TileGrid { tiles }
    }
}

/// True if any pixel in the tile differs between the two frames. `stride` is
/// the byte length of one image row (`width_px * 4`).
pub fn tile_dirty(cur: &[u8], prev: &[u8], stride: usize, t: &Tile) -> bool {
    let row_bytes = t.pw as usize * 4;
    let x_off = t.px as usize * 4;
    for y in t.py..t.py + t.ph {
        let off = y as usize * stride + x_off;
        if cur[off..off + row_bytes] != prev[off..off + row_bytes] {
            return true;
        }
    }
    false
}

/// Copy a tile's pixels out of the frame into a contiguous RGBA buffer
/// (`pw * ph * 4` bytes), reusing `out`'s allocation.
pub fn extract_tile(cur: &[u8], stride: usize, t: &Tile, out: &mut Vec<u8>) {
    out.clear();
    let row_bytes = t.pw as usize * 4;
    let x_off = t.px as usize * 4;
    for y in t.py..t.py + t.ph {
        let off = y as usize * stride + x_off;
        out.extend_from_slice(&cur[off..off + row_bytes]);
    }
}
