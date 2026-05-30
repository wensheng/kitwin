use base64::{prelude::BASE64_STANDARD, Engine};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::{self, Write};
use std::thread;
use std::time::Duration;

/// Robustly write all bytes to stdout, retrying on EAGAIN / WouldBlock errors.
pub fn write_all_robust<W: Write>(mut writer: W, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match writer.write(buf) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write whole buffer")),
            Ok(n) => buf = &buf[n..],
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(ref e) if e.raw_os_error() == Some(35) => { // EAGAIN on mac
                thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Robustly flush stdout, retrying on EAGAIN / WouldBlock errors.
pub fn flush_robust<W: Write>(mut writer: W) -> io::Result<()> {
    loop {
        match writer.flush() {
            Ok(()) => return Ok(()),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(ref e) if e.raw_os_error() == Some(35) => { // EAGAIN on mac
                thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }
}

#[allow(dead_code)]
pub fn move_up_robust(rows: u16) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    let mut buf = Vec::new();
    crossterm::queue!(buf, crossterm::cursor::MoveUp(rows), crossterm::cursor::MoveToColumn(0))?;
    write_all_robust(&mut stdout, &buf)?;
    flush_robust(&mut stdout)?;
    Ok(())
}

#[allow(dead_code)]
pub fn write_rgba_frame(
    pixels: &[u8],
    width_px: u32,
    height_px: u32,
    cols: u32,
    rows: u32,
    prevent_cursor_move: bool,
) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    write_rgba_frame_to(
        &mut stdout,
        pixels,
        width_px,
        height_px,
        cols,
        rows,
        prevent_cursor_move,
    )
}

pub fn write_rgba_frame_to<W: Write>(
    writer: &mut W,
    pixels: &[u8],
    width_px: u32,
    height_px: u32,
    cols: u32,
    rows: u32,
    prevent_cursor_move: bool,
) -> io::Result<()> {
    write_rgba_frame_impl(
        writer,
        pixels,
        width_px,
        height_px,
        Some((cols, rows)),
        prevent_cursor_move,
    )
}

#[allow(dead_code)]
pub fn write_rgba_frame_native_to<W: Write>(
    writer: &mut W,
    pixels: &[u8],
    width_px: u32,
    height_px: u32,
    prevent_cursor_move: bool,
) -> io::Result<()> {
    write_rgba_frame_impl(writer, pixels, width_px, height_px, None, prevent_cursor_move)
}

/// Stable image id for the single-image full-frame transmit path (P6). Repeated
/// frames reuse this id (with placement `p=1`), so each replaces the previous in
/// place rather than accumulating a fresh image terminal-side. Chosen well above
/// the tile path's id range (`1..=number-of-tiles`, see `write_tile_to`) so the
/// two transmit paths can never collide on an id.
const FULL_FRAME_IMAGE_ID: u32 = 0xFFFF;

fn write_rgba_frame_impl<W: Write>(
    writer: &mut W,
    pixels: &[u8],
    width_px: u32,
    height_px: u32,
    cells: Option<(u32, u32)>,
    prevent_cursor_move: bool,
) -> io::Result<()> {
    let c_policy = if prevent_cursor_move { ",C=1" } else { "" };
    let control = if let Some((cols, rows)) = cells {
        format!(
            "a=T,f=32,o=z,i={},p=1,s={},v={},c={},r={}{},q=2",
            FULL_FRAME_IMAGE_ID, width_px, height_px, cols, rows, c_policy
        )
    } else {
        format!(
            "a=T,f=32,o=z,i={},p=1,s={},v={}{},q=2",
            FULL_FRAME_IMAGE_ID, width_px, height_px, c_policy
        )
    };
    transmit_payload(writer, &control, pixels)
}

/// P2: transmit a single dirty tile as its own image and place it at the
/// current cursor cell. `tile_w`/`tile_h` are the tile's pixel dimensions;
/// `cells_w`/`cells_h` are the whole-cell box the protocol scales it into so
/// tiles abut on the cell grid. `image_id` is stable per tile position (`p=1`),
/// so a repeated update replaces the previous tile in place instead of leaking
/// images terminal-side. The caller must position the cursor first.
pub fn write_tile_to<W: Write>(
    writer: &mut W,
    pixels: &[u8],
    tile_w: u32,
    tile_h: u32,
    cells_w: u32,
    cells_h: u32,
    image_id: u32,
) -> io::Result<()> {
    // C=1: don't advance the cursor after placement. The caller positions the
    // cursor per tile, so without this a tile near the bottom row would push
    // the cursor past the screen and scroll the whole display.
    let control = format!(
        "a=T,f=32,o=z,i={},p=1,s={},v={},c={},r={},C=1,q=2",
        image_id, tile_w, tile_h, cells_w, cells_h
    );
    transmit_payload(writer, &control, pixels)
}

/// Compress (P3 `o=z` zlib), base64-encode, and write `pixels` as a sequence of
/// Kitty graphics escape packets. `first_control` is the control block of the
/// opening packet (everything between `\x1b_G` and the `;`, sans the `m` key,
/// which this fn appends). Continuation packets carry only `q`/`m` per spec.
fn transmit_payload<W: Write>(
    writer: &mut W,
    first_control: &str,
    pixels: &[u8],
) -> io::Result<()> {
    // `s`/`v` in the control block describe the *uncompressed* pixel dimensions;
    // the wire payload is the zlib stream. UI content compresses several-fold.
    let compressed = {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(pixels)?;
        enc.finish()?
    };
    let base64_str = BASE64_STANDARD.encode(&compressed);
    let bytes = base64_str.as_bytes();
    let chunk_size = 4096;
    let mut offset = 0;

    while offset < bytes.len() {
        let is_last = offset + chunk_size >= bytes.len();
        let chunk = &bytes[offset..std::cmp::min(offset + chunk_size, bytes.len())];
        let m_param = if is_last { 0 } else { 1 };

        let mut packet = Vec::new();
        if offset == 0 {
            write!(packet, "\x1b_G{},m={};", first_control, m_param)?;
        } else {
            write!(packet, "\x1b_Gq=2,m={};", m_param)?;
        }

        packet.write_all(chunk)?;
        packet.write_all(b"\x1b\\")?;

        write_all_robust(&mut *writer, &packet)?;
        offset += chunk_size;
    }

    flush_robust(&mut *writer)?;
    Ok(())
}
