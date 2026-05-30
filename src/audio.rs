use crate::ffmpeg::*;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;
use std::ffi::{c_int, CString};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const DEFAULT_VOLUME_PERCENT: u32 = 100;
const MAX_VOLUME_PERCENT: u32 = 200;

pub struct PulseAudioEnv<'a> {
    pub pulse_sink: &'a str,
    pub pulse_server: Option<&'a str>,
}

pub struct AudioRuntime {
    #[allow(dead_code)]
    sink_name: String,
    capture_server: Option<String>,
    module_id: String,
    volume_percent: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
    available: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl AudioRuntime {
    pub fn start(
        capture_server_override: Option<&str>,
        running: Arc<AtomicBool>,
        status_msg: Arc<Mutex<String>>,
    ) -> Result<Self, String> {
        if !pulse_input_available() {
            return Err(String::from("FFmpeg pulse input device not available"));
        }

        let sink_name = format!("kitwin_{}", std::process::id());
        let monitor_name = format!("{}.monitor", sink_name);

        let candidates = pulse_server_candidates(capture_server_override);
        let mut errors: Vec<String> = Vec::new();
        let mut chosen: Option<(Option<String>, String)> = None;
        for candidate in candidates {
            if !probe_pulse_server(candidate.as_deref()) {
                errors.push(format!(
                    "Pulse server {} not reachable",
                    describe_server(candidate.as_deref())
                ));
                continue;
            }
            match load_null_sink(&sink_name, candidate.as_deref()) {
                Ok(module_id) => {
                    if !pulse_source_exists(&monitor_name, candidate.as_deref()) {
                        let _ = unload_null_sink(&module_id, candidate.as_deref());
                        errors.push(format!(
                            "Pulse monitor source {} was not created on {}",
                            monitor_name,
                            describe_server(candidate.as_deref())
                        ));
                        continue;
                    }
                    chosen = Some((candidate, module_id));
                    break;
                }
                Err(err) => {
                    errors.push(format!(
                        "{} ({})",
                        err,
                        describe_server(candidate.as_deref())
                    ));
                }
            }
        }

        let (capture_server, module_id) = chosen.ok_or_else(|| {
            if errors.is_empty() {
                String::from("no Pulse server available")
            } else {
                errors.join("; ")
            }
        })?;

        if let Err(err) = ensure_unix_protocol(capture_server.as_deref()) {
            set_status(&status_msg, &format!("audio warning: {}", err));
        }

        let volume_percent = Arc::new(AtomicU32::new(DEFAULT_VOLUME_PERCENT));
        let muted = Arc::new(AtomicBool::new(false));
        let available = Arc::new(AtomicBool::new(true));
        let error = Arc::new(Mutex::new(None));

        let thread_running = running.clone();
        let thread_error_running = running.clone();
        let thread_volume = volume_percent.clone();
        let thread_muted = muted.clone();
        let thread_available = available.clone();
        let thread_error = error.clone();
        let thread_capture_server = capture_server.clone();
        let thread_status = status_msg.clone();
        let handle = thread::spawn(move || {
            if let Err(err) = run_audio_capture(
                &monitor_name,
                thread_capture_server.as_deref(),
                thread_running,
                thread_volume,
                thread_muted,
            ) {
                if thread_error_running.load(Ordering::SeqCst) {
                    thread_available.store(false, Ordering::SeqCst);
                    let mut stored_error = thread_error.lock().unwrap();
                    *stored_error = Some(err.clone());
                    set_status(&thread_status, &format!("audio unavailable: {}", err));
                }
            }
        });

        set_status(
            &status_msg,
            &format_audio_status(false, DEFAULT_VOLUME_PERCENT),
        );

        Ok(Self {
            sink_name,
            capture_server,
            module_id,
            volume_percent,
            muted,
            available,
            error,
            running,
            handle: Some(handle),
        })
    }

    pub fn pulse_audio_env(&self) -> PulseAudioEnv<'_> {
        PulseAudioEnv {
            pulse_sink: &self.sink_name,
            pulse_server: self.capture_server.as_deref(),
        }
    }

    pub fn toggle_mute(&self) -> String {
        let muted = !self.muted.load(Ordering::SeqCst);
        self.muted.store(muted, Ordering::SeqCst);
        self.status()
    }

    pub fn volume_by(&self, delta: i32) -> String {
        let current = self.volume_percent.load(Ordering::SeqCst) as i32;
        let next = (current + delta).clamp(0, MAX_VOLUME_PERCENT as i32) as u32;
        self.volume_percent.store(next, Ordering::SeqCst);
        self.status()
    }

    pub fn status(&self) -> String {
        if !self.available.load(Ordering::SeqCst) {
            let error = self.error.lock().unwrap();
            return error
                .as_ref()
                .map(|err| format!("audio unavailable: {}", err))
                .unwrap_or_else(|| String::from("audio unavailable"));
        }

        format_audio_status(
            self.muted.load(Ordering::SeqCst),
            self.volume_percent.load(Ordering::SeqCst),
        )
    }
}

impl Drop for AudioRuntime {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = unload_null_sink(&self.module_id, self.capture_server.as_deref());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_audio_capture(
    monitor_name: &str,
    capture_server: Option<&str>,
    running: Arc<AtomicBool>,
    volume_percent: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
) -> Result<(), String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| String::from("no default audio output device"))?;
    let config_supported = device
        .default_output_config()
        .map_err(|err| format!("could not get default audio output config: {}", err))?;
    let audio_config: cpal::StreamConfig = config_supported.into();
    let target_channels = audio_config.channels;
    let target_sample_rate = audio_config.sample_rate;

    let rb =
        HeapRb::<f32>::new((target_sample_rate as usize * target_channels as usize * 2).max(1));
    let (mut audio_producer, mut audio_consumer) = rb.split();

    let callback_volume = volume_percent.clone();
    let callback_muted = muted.clone();
    let stream = device
        .build_output_stream(
            &audio_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let volume = if callback_muted.load(Ordering::SeqCst) {
                    0.0
                } else {
                    callback_volume.load(Ordering::SeqCst) as f32 / 100.0
                };
                for sample in data.iter_mut() {
                    if let Some(value) = audio_consumer.try_pop() {
                        *sample = value * volume;
                    } else {
                        *sample = 0.0;
                    }
                }
            },
            move |err| {
                if std::env::var_os("KITWIN_CHILD_LOGS").is_some() {
                    eprintln!("kitwin: audio stream error: {}", err);
                }
            },
            None,
        )
        .map_err(|err| format!("could not build audio output stream: {}", err))?;
    stream
        .play()
        .map_err(|err| format!("could not start audio output stream: {}", err))?;

    unsafe {
        capture_pulse_monitor(
            monitor_name,
            capture_server,
            target_channels,
            target_sample_rate,
            &mut audio_producer,
            &running,
        )
    }
}

unsafe fn capture_pulse_monitor<P: Producer<Item = f32>>(
    monitor_name: &str,
    capture_server: Option<&str>,
    target_channels: u16,
    target_sample_rate: u32,
    audio_producer: &mut P,
    running: &AtomicBool,
) -> Result<(), String> {
    avdevice_register_all();
    av_log_set_level(AV_LOG_QUIET);

    let input_fmt = pulse_input_format()?;
    if input_fmt.is_null() {
        return Err(String::from("FFmpeg pulse input device not available"));
    }

    let mut opts: *mut AVDictionary = std::ptr::null_mut();
    let k_sample_rate = CString::new("sample_rate").unwrap();
    let v_sample_rate = CString::new(target_sample_rate.to_string()).unwrap();
    let k_channels = CString::new("channels").unwrap();
    let v_channels = CString::new(target_channels.to_string()).unwrap();
    let k_stream_name = CString::new("stream_name").unwrap();
    let v_stream_name = CString::new("kitwin audio capture").unwrap();
    av_dict_set(&mut opts, k_sample_rate.as_ptr(), v_sample_rate.as_ptr(), 0);
    av_dict_set(&mut opts, k_channels.as_ptr(), v_channels.as_ptr(), 0);
    av_dict_set(&mut opts, k_stream_name.as_ptr(), v_stream_name.as_ptr(), 0);
    if let Some(server) = capture_server {
        let k_server = CString::new("server").unwrap();
        let v_server = CString::new(server).map_err(|err| err.to_string())?;
        av_dict_set(&mut opts, k_server.as_ptr(), v_server.as_ptr(), 0);
    }

    let c_monitor_name = CString::new(monitor_name).map_err(|err| err.to_string())?;
    let mut format_ctx: *mut AVFormatContext = std::ptr::null_mut();
    let ret = avformat_open_input(
        &mut format_ctx,
        c_monitor_name.as_ptr(),
        input_fmt,
        &mut opts,
    );
    av_dict_free(&mut opts);
    if ret != 0 {
        return Err(format!("could not open Pulse monitor {}", monitor_name));
    }

    let result = capture_audio_packets(
        format_ctx,
        target_channels,
        target_sample_rate,
        audio_producer,
        running,
    );
    avformat_close_input(&mut format_ctx);
    result
}

fn pulse_input_available() -> bool {
    unsafe {
        pulse_input_format()
            .map(|fmt| !fmt.is_null())
            .unwrap_or(false)
    }
}

unsafe fn pulse_input_format() -> Result<*mut AVInputFormat, String> {
    avdevice_register_all();
    let fmt_name = CString::new("pulse").map_err(|err| err.to_string())?;
    Ok(av_find_input_format(fmt_name.as_ptr()))
}

unsafe fn capture_audio_packets<P: Producer<Item = f32>>(
    format_ctx: *mut AVFormatContext,
    target_channels: u16,
    target_sample_rate: u32,
    audio_producer: &mut P,
    running: &AtomicBool,
) -> Result<(), String> {
    let ret = avformat_find_stream_info(format_ctx, std::ptr::null_mut());
    if ret < 0 {
        return Err(String::from("could not find Pulse stream info"));
    }

    let mut audio_stream_index = -1;
    let mut audio_codecpar_ptr: *mut AVCodecParameters = std::ptr::null_mut();
    for i in 0..(*format_ctx).nb_streams {
        let stream = *(*format_ctx).streams.add(i as usize);
        if stream.is_null() {
            continue;
        }
        let codecpar = (*stream).codecpar;
        if codecpar.is_null() {
            continue;
        }
        if (*codecpar).codec_type == AVMEDIA_TYPE_AUDIO {
            audio_stream_index = i as i32;
            audio_codecpar_ptr = codecpar;
            break;
        }
    }

    if audio_stream_index == -1 {
        return Err(String::from("could not find Pulse audio stream"));
    }

    let audio_decoder = avcodec_find_decoder((*audio_codecpar_ptr).codec_id);
    if audio_decoder.is_null() {
        return Err(String::from("Pulse audio decoder not found"));
    }

    let mut audio_codec_ctx = avcodec_alloc_context3(audio_decoder);
    if audio_codec_ctx.is_null() {
        return Err(String::from("could not allocate Pulse audio codec context"));
    }

    let result = (|| {
        let ret = avcodec_parameters_to_context(audio_codec_ctx, audio_codecpar_ptr);
        if ret < 0 {
            return Err(String::from("could not copy Pulse audio codec parameters"));
        }
        let ret = avcodec_open2(audio_codec_ctx, audio_decoder, std::ptr::null_mut());
        if ret < 0 {
            return Err(String::from("could not open Pulse audio codec"));
        }

        let input_sample_rate = (*audio_codecpar_ptr).sample_rate;
        if input_sample_rate <= 0 {
            return Err(String::from("Pulse audio stream has no sample rate"));
        }

        let pkt = av_packet_alloc();
        if pkt.is_null() {
            return Err(String::from("could not allocate Pulse audio packet"));
        }
        let audio_frame = av_frame_alloc();
        if audio_frame.is_null() {
            av_packet_free(&mut { pkt });
            return Err(String::from("could not allocate Pulse audio frame"));
        }

        let mut swr_ctx: *mut SwrContext = std::ptr::null_mut();
        while running.load(Ordering::SeqCst) {
            let ret = av_read_frame(format_ctx, pkt);
            if ret != 0 {
                break;
            }

            if (*pkt).stream_index == audio_stream_index {
                let ret = avcodec_send_packet(audio_codec_ctx, pkt);
                if ret == 0 {
                    receive_and_queue_audio(
                        audio_codec_ctx,
                        audio_frame,
                        &mut swr_ctx,
                        audio_codecpar_ptr,
                        input_sample_rate,
                        target_sample_rate,
                        target_channels,
                        audio_producer,
                        running,
                    )?;
                }
            }

            av_packet_unref(pkt);
        }

        if !swr_ctx.is_null() {
            swr_free(&mut swr_ctx);
        }
        av_frame_free(&mut { audio_frame });
        av_packet_free(&mut { pkt });
        Ok(())
    })();

    avcodec_free_context(&mut audio_codec_ctx);
    result
}

unsafe fn receive_and_queue_audio<P: Producer<Item = f32>>(
    audio_codec_ctx: *mut AVCodecContext,
    audio_frame: *mut AVFrame,
    swr_ctx: &mut *mut SwrContext,
    audio_codecpar_ptr: *mut AVCodecParameters,
    input_sample_rate: c_int,
    target_sample_rate: u32,
    target_channels: u16,
    audio_producer: &mut P,
    running: &AtomicBool,
) -> Result<(), String> {
    while avcodec_receive_frame(audio_codec_ctx, audio_frame) == 0 {
        if !running.load(Ordering::SeqCst) {
            break;
        }

        let frame_sample_rate = if (*audio_frame).sample_rate > 0 {
            (*audio_frame).sample_rate
        } else {
            input_sample_rate
        };
        if frame_sample_rate <= 0 {
            continue;
        }

        if (*swr_ctx).is_null() {
            let frame_format = (*audio_frame).format;
            let mut in_ch_layout = AVChannelLayout::default();
            if (*audio_codecpar_ptr).ch_layout.nb_channels > 0 {
                av_channel_layout_copy(&mut in_ch_layout, &(*audio_codecpar_ptr).ch_layout);
            } else {
                av_channel_layout_default(&mut in_ch_layout, 2);
            }

            let mut out_ch_layout = AVChannelLayout::default();
            av_channel_layout_default(&mut out_ch_layout, target_channels as c_int);

            let ret = swr_alloc_set_opts2(
                swr_ctx,
                &out_ch_layout,
                AV_SAMPLE_FMT_FLT,
                target_sample_rate as c_int,
                &in_ch_layout,
                frame_format,
                frame_sample_rate,
                0,
                std::ptr::null_mut(),
            );

            av_channel_layout_uninit(&mut in_ch_layout);
            av_channel_layout_uninit(&mut out_ch_layout);

            if ret < 0 || (*swr_ctx).is_null() || swr_init(*swr_ctx) < 0 {
                if !(*swr_ctx).is_null() {
                    swr_free(swr_ctx);
                }
                return Err(format!(
                    "could not initialize Pulse audio resampler for decoded sample format {}",
                    frame_format
                ));
            }
        }

        let max_out_samples = ((*audio_frame).nb_samples as i64 * target_sample_rate as i64
            / frame_sample_rate as i64
            + 256) as c_int;
        let mut resampled_buffer =
            vec![0.0f32; (max_out_samples * target_channels as c_int) as usize];
        let mut out_ptrs: [*mut u8; 8] = [std::ptr::null_mut(); 8];
        out_ptrs[0] = resampled_buffer.as_mut_ptr() as *mut u8;

        let in_ptrs = if (*audio_frame).extended_data.is_null() {
            (*audio_frame).data.as_ptr() as *const *const u8
        } else {
            (*audio_frame).extended_data as *const *const u8
        };

        let converted = swr_convert(
            *swr_ctx,
            out_ptrs.as_mut_ptr(),
            max_out_samples,
            in_ptrs,
            (*audio_frame).nb_samples,
        );
        if converted <= 0 {
            continue;
        }

        let sample_count = (converted * target_channels as c_int) as usize;
        for sample in resampled_buffer.iter().take(sample_count) {
            while running.load(Ordering::SeqCst) {
                match audio_producer.try_push(*sample) {
                    Ok(_) => break,
                    Err(_) => thread::sleep(Duration::from_micros(500)),
                }
            }
        }
    }

    Ok(())
}

fn load_null_sink(sink_name: &str, capture_server: Option<&str>) -> Result<String, String> {
    let mut command = pactl_command(capture_server);
    let output = command
        .args([
            "load-module",
            "module-null-sink",
            &format!("sink_name={}", sink_name),
            &format!("sink_properties=device.description={}", sink_name),
        ])
        .output()
        .map_err(|err| format!("could not run pactl: {}", err))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "could not create Pulse null sink: {}",
            stderr.trim()
        ));
    }

    let module_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if module_id.is_empty() {
        return Err(String::from("pactl did not return a module id"));
    }

    Ok(module_id)
}

fn pulse_source_exists(source_name: &str, capture_server: Option<&str>) -> bool {
    let output = pactl_command(capture_server)
        .args(["list", "short", "sources"])
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .any(|name| name == source_name)
}

fn unload_null_sink(module_id: &str, capture_server: Option<&str>) -> Result<(), String> {
    let status = pactl_command(capture_server)
        .args(["unload-module", module_id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| format!("could not run pactl unload-module: {}", err))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("pactl unload-module {} failed", module_id))
    }
}

fn pactl_command(capture_server: Option<&str>) -> Command {
    let mut command = Command::new("pactl");
    if let Some(server) = capture_server {
        command.env("PULSE_SERVER", server);
    }
    command
}

fn local_pulse_server() -> Option<String> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    let path = Path::new(&runtime_dir).join("pulse/native");
    path.exists().then(|| format!("unix:{}", path.display()))
}

fn env_pulse_server() -> Option<String> {
    std::env::var("PULSE_SERVER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn pulse_server_candidates(override_server: Option<&str>) -> Vec<Option<String>> {
    if let Some(server) = override_server {
        return vec![Some(server.to_string())];
    }
    let mut out: Vec<Option<String>> = Vec::new();
    let mut push = |value: Option<String>| {
        if !out.iter().any(|existing| existing == &value) {
            out.push(value);
        }
    };
    if let Some(server) = local_pulse_server() {
        push(Some(server));
    }
    push(Some(String::from("127.0.0.1:4713")));
    if let Some(server) = env_pulse_server() {
        push(Some(server));
    }
    if out.is_empty() {
        out.push(None);
    }
    out
}

fn probe_pulse_server(server: Option<&str>) -> bool {
    let output = pactl_command(server)
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output();
    matches!(output, Ok(out) if out.status.success())
}

fn describe_server(server: Option<&str>) -> String {
    server
        .map(str::to_string)
        .unwrap_or_else(|| String::from("(default)"))
}

fn ensure_unix_protocol(capture_server: Option<&str>) -> Result<(), String> {
    let listing = pactl_command(capture_server)
        .args(["list", "modules", "short"])
        .output()
        .map_err(|err| format!("pactl list modules failed: {}", err))?;
    if !listing.status.success() {
        return Err(String::from("pactl list modules failed"));
    }
    let stdout = String::from_utf8_lossy(&listing.stdout);
    if stdout
        .lines()
        .any(|line| line.contains("module-native-protocol-unix"))
    {
        return Ok(());
    }
    let load = pactl_command(capture_server)
        .args(["load-module", "module-native-protocol-unix"])
        .output()
        .map_err(|err| format!("pactl load-module failed: {}", err))?;
    if !load.status.success() {
        let stderr = String::from_utf8_lossy(&load.stderr);
        return Err(format!(
            "could not load module-native-protocol-unix: {}",
            stderr.trim()
        ));
    }
    Ok(())
}

fn format_audio_status(muted: bool, volume_percent: u32) -> String {
    if muted {
        String::from("audio muted")
    } else {
        format!("audio {}%", volume_percent)
    }
}

fn set_status(status_msg: &Arc<Mutex<String>>, msg: &str) {
    let mut status = status_msg.lock().unwrap();
    *status = msg.to_string();
}
