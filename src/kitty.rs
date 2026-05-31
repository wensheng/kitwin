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
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ))
            }
            Ok(n) => buf = &buf[n..],
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(ref e) if e.raw_os_error() == Some(35) => {
                // EAGAIN on mac
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
            Err(ref e) if e.raw_os_error() == Some(35) => {
                // EAGAIN on mac
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
    crossterm::queue!(
        buf,
        crossterm::cursor::MoveUp(rows),
        crossterm::cursor::MoveToColumn(0)
    )?;
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
    delete_image_data_to(writer, FULL_FRAME_IMAGE_ID)?;
    write_rgba_frame_impl(
        writer,
        pixels,
        width_px,
        height_px,
        Some((cols, rows)),
        prevent_cursor_move,
        FULL_FRAME_IMAGE_ID,
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
    delete_image_data_to(writer, FULL_FRAME_IMAGE_ID)?;
    write_rgba_frame_impl(
        writer,
        pixels,
        width_px,
        height_px,
        None,
        prevent_cursor_move,
        FULL_FRAME_IMAGE_ID,
    )
}

#[allow(dead_code)]
pub fn write_rgba_frame_native_with_id_to<W: Write>(
    writer: &mut W,
    pixels: &[u8],
    width_px: u32,
    height_px: u32,
    image_id: u32,
    prevent_cursor_move: bool,
) -> io::Result<()> {
    write_rgba_frame_impl(
        writer,
        pixels,
        width_px,
        height_px,
        None,
        prevent_cursor_move,
        image_id,
    )
}

/// Like [`write_rgba_frame_native_with_id_to`] but scales the image into a
/// `cols`x`rows` cell box (Kitty's `c`/`r` keys), used by the streaming
/// renderer to fit the captured desktop into the terminal. Does not delete any
/// prior image; the renderer manages image-id lifetimes itself.
#[allow(clippy::too_many_arguments)]
pub fn write_rgba_frame_scaled_with_id_to<W: Write>(
    writer: &mut W,
    pixels: &[u8],
    width_px: u32,
    height_px: u32,
    cols: u32,
    rows: u32,
    image_id: u32,
    prevent_cursor_move: bool,
) -> io::Result<()> {
    write_rgba_frame_impl(
        writer,
        pixels,
        width_px,
        height_px,
        Some((cols, rows)),
        prevent_cursor_move,
        image_id,
    )
}

/// Reserved id for the legacy single-image full-frame transmit helpers.
const FULL_FRAME_IMAGE_ID: u32 = 0xFFFF;

/// First id for the streaming renderer's full-frame images. The renderer uses
/// monotonically increasing ids so it can place a new frame before deleting the
/// previous frame, avoiding visible delete-before-redraw flashes.
pub const STREAM_IMAGE_ID_START: u32 = FULL_FRAME_IMAGE_ID + 1;

fn write_rgba_frame_impl<W: Write>(
    writer: &mut W,
    pixels: &[u8],
    width_px: u32,
    height_px: u32,
    cells: Option<(u32, u32)>,
    prevent_cursor_move: bool,
    image_id: u32,
) -> io::Result<()> {
    let c_policy = if prevent_cursor_move { ",C=1" } else { "" };
    let control = if let Some((cols, rows)) = cells {
        format!(
            "a=T,f=32,o=z,i={},p=1,s={},v={},c={},r={}{},q=2",
            image_id, width_px, height_px, cols, rows, c_policy
        )
    } else {
        format!(
            "a=T,f=32,o=z,i={},p=1,s={},v={}{},q=2",
            image_id, width_px, height_px, c_policy
        )
    };
    transmit_payload(writer, &control, pixels)
}

/// Delete all currently visible graphics placements, freeing image data where
/// the terminal can. Used before full redraws and during shutdown so stale
/// terminal-side placements cannot survive a grid/geometry change.
pub fn delete_visible_images_to<W: Write>(writer: &mut W) -> io::Result<()> {
    write_graphics_command(writer, "a=d,d=A,q=2")
}

/// Delete the stored image and all of its placements for an image id. The
/// streaming renderer uses this after the replacement frame has already been
/// placed, so deletion does not create visible blank squares.
pub fn delete_image_data_to<W: Write>(writer: &mut W, image_id: u32) -> io::Result<()> {
    if image_id == 0 {
        return Ok(());
    }
    write_graphics_command(writer, &format!("a=d,d=I,i={},q=2", image_id))
}

fn write_graphics_command<W: Write>(writer: &mut W, control: &str) -> io::Result<()> {
    let mut packet = Vec::new();
    write!(packet, "\x1b_G{}\x1b\\", control)?;
    write_all_robust(&mut *writer, &packet)
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
