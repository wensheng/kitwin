//! Shared geometry for placing the captured desktop image inside the terminal.
//!
//! The desktop is captured at a fixed resolution (the Xvfb screen size) and is
//! centred within the terminal's cell grid. When it is larger than the grid it
//! is scaled down to fit, preserving its aspect ratio; it is never scaled up.
//!
//! Both the renderer (which draws the image) and the input thread (which maps
//! mouse clicks back to source pixels) compute the same [`Placement`], so they
//! must agree on the math — hence this shared module.

/// Where the source image lands on the terminal cell grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// Left edge of the image, in terminal columns.
    pub col_offset: u16,
    /// Top edge of the image, in terminal rows.
    pub row_offset: u16,
    /// Width of the image, in terminal columns.
    pub cols: u16,
    /// Height of the image, in terminal rows.
    pub rows: u16,
}

/// Compute the centred, scale-to-fit placement of a `src_width`x`src_height`
/// pixel image inside a `grid_cols`x`grid_rows` terminal whose cells measure
/// `cell_w`x`cell_h` pixels. The image is scaled down to fit but never enlarged.
pub fn placement(
    src_width: u32,
    src_height: u32,
    grid_cols: u16,
    grid_rows: u16,
    cell_w: f64,
    cell_h: f64,
) -> Placement {
    let grid_cols = grid_cols.max(1);
    let grid_rows = grid_rows.max(1);
    let cell_w = if cell_w > 0.0 { cell_w } else { 8.0 };
    let cell_h = if cell_h > 0.0 { cell_h } else { 16.0 };

    // Pixels available on the grid.
    let avail_w = grid_cols as f64 * cell_w;
    let avail_h = grid_rows as f64 * cell_h;

    // Scale down to fit, preserving aspect ratio; never enlarge.
    let scale = (avail_w / src_width.max(1) as f64)
        .min(avail_h / src_height.max(1) as f64)
        .min(1.0);

    let disp_w = src_width as f64 * scale;
    let disp_h = src_height as f64 * scale;

    // Round the scaled pixel size to whole cells, clamped to the grid.
    let cols = ((disp_w / cell_w).round() as u32).clamp(1, grid_cols as u32) as u16;
    let rows = ((disp_h / cell_h).round() as u32).clamp(1, grid_rows as u32) as u16;

    Placement {
        col_offset: (grid_cols - cols) / 2,
        row_offset: (grid_rows - rows) / 2,
        cols,
        rows,
    }
}

/// Map an absolute terminal cell `(column, row)` to a source pixel `(x, y)`.
/// Returns `None` when the cell lies outside the placed image (e.g. in the
/// centring margins), so the caller can ignore the event.
pub fn cell_to_source(
    p: Placement,
    column: u16,
    row: u16,
    src_width: u32,
    src_height: u32,
) -> Option<(u32, u32)> {
    if src_width == 0 || src_height == 0 {
        return None;
    }
    if column < p.col_offset || row < p.row_offset {
        return None;
    }
    let rel_col = column - p.col_offset;
    let rel_row = row - p.row_offset;
    if rel_col >= p.cols || rel_row >= p.rows {
        return None;
    }

    // Aim at the centre of the cell, then map proportionally into the source.
    let x = ((rel_col as f64 + 0.5) / p.cols as f64 * src_width as f64).floor() as u32;
    let y = ((rel_row as f64 + 0.5) / p.rows as f64 * src_height as f64).floor() as u32;
    Some((x.min(src_width - 1), y.min(src_height - 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn larger_than_grid_scales_down_and_centers() {
        // 1920x1200 source, 80x24 grid, 10x20 px cells => grid is 800x480 px.
        // Height-limited: 480/1200 (=0.4) < 800/1920, so scale = 0.4.
        let p = placement(1920, 1200, 80, 24, 10.0, 20.0);
        // 1920*0.4 = 768px -> round(76.8) = 77 cols; 1200*0.4 = 480px -> 24 rows.
        assert_eq!(p.cols, 77);
        assert_eq!(p.rows, 24);
        // Centred horizontally with the leftover columns split.
        assert_eq!(p.col_offset, (80 - 77) / 2);
        assert_eq!(p.row_offset, 0);
    }

    #[test]
    fn smaller_than_grid_is_not_enlarged() {
        // 200x100 source easily fits an 80x24 grid of 10x20 px cells.
        let p = placement(200, 100, 80, 24, 10.0, 20.0);
        assert_eq!(p.cols, 20); // 200px / 10px
        assert_eq!(p.rows, 5); //  100px / 20px
        assert_eq!(p.col_offset, (80 - 20) / 2);
        assert_eq!(p.row_offset, (24 - 5) / 2);
    }

    #[test]
    fn margin_clicks_return_none() {
        let p = Placement {
            col_offset: 10,
            row_offset: 2,
            cols: 20,
            rows: 5,
        };
        assert!(cell_to_source(p, 0, 0, 200, 100).is_none());
        assert!(cell_to_source(p, 9, 3, 200, 100).is_none());
        assert!(cell_to_source(p, 30, 3, 200, 100).is_none());
        assert!(cell_to_source(p, 15, 4, 200, 100).is_some());
    }

    #[test]
    fn click_maps_into_source_bounds() {
        let p = placement(1920, 1200, 80, 24, 10.0, 20.0);
        let (x, y) = cell_to_source(
            p,
            p.col_offset + p.cols - 1,
            p.row_offset + p.rows - 1,
            1920,
            1200,
        )
        .unwrap();
        assert!(x < 1920);
        assert!(y < 1200);
    }
}
