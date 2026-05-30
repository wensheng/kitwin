use crate::ffmpeg::*;
use std::ffi::{c_int, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::Arc;

pub enum CaptureMsg {
    Frame { rgba: Vec<u8>, width: u32, height: u32 },
}

pub fn run_capture(
    display: u8,
    src_width: u32,
    src_height: u32,
    fps: u32,
    tx: SyncSender<CaptureMsg>,
    recycle_rx: Receiver<Vec<u8>>,
    running: Arc<AtomicBool>,
) {
    unsafe {
        avdevice_register_all();
        av_log_set_level(AV_LOG_QUIET);

        let fmt_name = CString::new("x11grab").unwrap();
        let input_fmt = av_find_input_format(fmt_name.as_ptr());
        if input_fmt.is_null() {
            eprintln!("kitweb: x11grab not available in this FFmpeg build");
            running.store(false, Ordering::SeqCst);
            return;
        }

        // Build options dictionary
        let mut opts: *mut AVDictionary = std::ptr::null_mut();
        let k_video_size = CString::new("video_size").unwrap();
        let v_video_size = CString::new(format!("{}x{}", src_width, src_height)).unwrap();
        let k_framerate = CString::new("framerate").unwrap();
        let v_framerate = CString::new(fps.to_string()).unwrap();
        av_dict_set(&mut opts, k_video_size.as_ptr(), v_video_size.as_ptr(), 0);
        av_dict_set(&mut opts, k_framerate.as_ptr(), v_framerate.as_ptr(), 0);

        let display_url = CString::new(format!(":{}", display)).unwrap();
        let mut format_ctx: *mut AVFormatContext = std::ptr::null_mut();

        let ret = avformat_open_input(
            &mut format_ctx,
            display_url.as_ptr(),
            input_fmt,
            &mut opts,
        );
        av_dict_free(&mut opts);

        if ret != 0 {
            eprintln!("kitweb: could not open x11grab display :{}", display);
            running.store(false, Ordering::SeqCst);
            return;
        }

        let ret = avformat_find_stream_info(format_ctx, std::ptr::null_mut());
        if ret < 0 {
            avformat_close_input(&mut format_ctx);
            eprintln!("kitweb: could not find stream info");
            running.store(false, Ordering::SeqCst);
            return;
        }

        // Find video stream
        let mut video_stream_index: i32 = -1;
        let mut video_codecpar: *mut AVCodecParameters = std::ptr::null_mut();

        for i in 0..(*format_ctx).nb_streams {
            let stream = *(*format_ctx).streams.add(i as usize);
            if stream.is_null() {
                continue;
            }
            let codecpar = (*stream).codecpar;
            if codecpar.is_null() {
                continue;
            }
            if (*codecpar).codec_type == AVMEDIA_TYPE_VIDEO && video_stream_index == -1 {
                video_stream_index = i as i32;
                video_codecpar = codecpar;
            }
        }

        if video_stream_index == -1 {
            avformat_close_input(&mut format_ctx);
            eprintln!("kitweb: no video stream found in x11grab");
            running.store(false, Ordering::SeqCst);
            return;
        }

        let codec = avcodec_find_decoder((*video_codecpar).codec_id);
        if codec.is_null() {
            avformat_close_input(&mut format_ctx);
            eprintln!("kitweb: could not find decoder");
            running.store(false, Ordering::SeqCst);
            return;
        }

        let codec_ctx = avcodec_alloc_context3(codec);
        avcodec_parameters_to_context(codec_ctx, video_codecpar);
        avcodec_open2(codec_ctx, codec, std::ptr::null_mut());

        let src_fmt = (*video_codecpar).format;
        let sws_ctx = sws_getContext(
            src_width as c_int,
            src_height as c_int,
            src_fmt,
            src_width as c_int,
            src_height as c_int,
            AV_PIX_FMT_RGBA,
            SWS_BILINEAR,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );

        let packet = av_packet_alloc();
        let frame = av_frame_alloc();

        let rgba_stride = src_width as usize * 4;
        let frame_len = rgba_stride * src_height as usize;
        let mut rgba_buf: Vec<u8> = vec![0u8; frame_len];
        // P1: retain the last frame we sent so we can skip byte-identical frames.
        // A static desktop produces the same pixels every tick; transmitting nothing
        // takes idle bandwidth from ~370 MB/s to ~0.
        let mut last_sent: Vec<u8> = Vec::new();
        // P4: pool of buffers recycled by the renderer (via recycle_rx) so the
        // per-frame send doesn't allocate ~9.2 MB each time. Bounded so a stalled
        // renderer can't make it grow without limit.
        const MAX_POOL: usize = 4;
        let mut pool: Vec<Vec<u8>> = Vec::with_capacity(MAX_POOL);

        while running.load(Ordering::SeqCst) {
            let ret = av_read_frame(format_ctx, packet);
            if ret < 0 {
                break;
            }

            if (*packet).stream_index != video_stream_index {
                av_packet_unref(packet);
                continue;
            }

            avcodec_send_packet(codec_ctx, packet);
            av_packet_unref(packet);

            loop {
                let ret = avcodec_receive_frame(codec_ctx, frame);
                if ret < 0 {
                    break;
                }

                let src_data = (*frame).data.as_ptr() as *const *const u8;
                let src_stride = (*frame).linesize.as_ptr();
                let dst_ptr = rgba_buf.as_mut_ptr();
                let dst_data: [*mut u8; 1] = [dst_ptr];
                let dst_stride: [c_int; 1] = [rgba_stride as c_int];

                sws_scale(
                    sws_ctx,
                    src_data,
                    src_stride,
                    0,
                    src_height as c_int,
                    dst_data.as_ptr(),
                    dst_stride.as_ptr(),
                );

                // P1: skip frames identical to the last one we transmitted.
                if rgba_buf == last_sent {
                    continue;
                }

                // P4: reclaim any buffers the renderer has returned, then fill an
                // outgoing buffer from the pool (allocating only if the pool is
                // empty) instead of cloning. `rgba_buf` stays as the persistent
                // sws_scale target and P1 comparison source.
                while let Ok(buf) = recycle_rx.try_recv() {
                    if pool.len() < MAX_POOL {
                        pool.push(buf);
                    }
                }
                let mut out = pool.pop().unwrap_or_default();
                out.clear();
                out.extend_from_slice(&rgba_buf);

                match tx.try_send(CaptureMsg::Frame {
                    rgba: out,
                    width: src_width,
                    height: src_height,
                }) {
                    Ok(()) => {
                        // Only record as "sent" once it actually reached the
                        // renderer. A dropped frame must not poison the dedup
                        // check, or the next identical frame would be skipped and
                        // leave the display stale.
                        last_sent.clear();
                        last_sent.extend_from_slice(&rgba_buf);
                    }
                    Err(TrySendError::Full(CaptureMsg::Frame { rgba, .. })) => {
                        // Renderer is behind; drop the frame but recycle its
                        // buffer so we don't allocate again next time.
                        if pool.len() < MAX_POOL {
                            pool.push(rgba);
                        }
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
        }

        av_frame_free(&mut (frame as *mut _));
        av_packet_free(&mut (packet as *mut _));
        sws_freeContext(sws_ctx);
        avcodec_free_context(&mut (codec_ctx as *mut _));
        avformat_close_input(&mut format_ctx);
    }
}
