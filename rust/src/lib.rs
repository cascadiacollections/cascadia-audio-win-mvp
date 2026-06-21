use std::collections::VecDeque;
use std::ffi::CStr;
use std::io::{BufReader, Error as IoError, ErrorKind, Read, Result as IoResult, Seek, SeekFrom};
use std::os::raw::c_char;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    BufferSize,
};
use futures::StreamExt;
use reqwest::header::{HeaderValue, CONTENT_TYPE};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::default::{get_codecs, get_probe};

static PLAYING: AtomicBool = AtomicBool::new(false);
static VOLUME_BITS: AtomicU32 = AtomicU32::new(f32::to_bits(1.0));
static DECODED_PACKETS: AtomicU64 = AtomicU64::new(0);
static DECODE_ERRORS: AtomicU64 = AtomicU64::new(0);
static QUEUED_BUFFERS: AtomicU64 = AtomicU64::new(0);
static UNDERRUNS: AtomicU64 = AtomicU64::new(0);
static LATENCY_MS: AtomicU64 = AtomicU64::new(0);
static RECONNECTS: AtomicU64 = AtomicU64::new(0);
static PLAYBACK_STATE: AtomicU32 = AtomicU32::new(PlaybackState::Stopped as u32);
static LAST_ERROR: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
});

#[repr(u32)]
#[derive(Copy, Clone, Eq, PartialEq)]
enum PlaybackState {
    Stopped = 0,
    Starting = 1,
    Buffering = 2,
    Playing = 3,
    Reconnecting = 4,
    Stopping = 5,
    Error = 6,
}

fn set_state(state: PlaybackState) {
    PLAYBACK_STATE.store(state as u32, Ordering::SeqCst);
}

fn state() -> PlaybackState {
    match PLAYBACK_STATE.load(Ordering::SeqCst) {
        1 => PlaybackState::Starting,
        2 => PlaybackState::Buffering,
        3 => PlaybackState::Playing,
        4 => PlaybackState::Reconnecting,
        5 => PlaybackState::Stopping,
        6 => PlaybackState::Error,
        _ => PlaybackState::Stopped,
    }
}

fn set_error(message: &str) {
    if let Ok(mut lock) = LAST_ERROR.lock() {
        lock.clear();
        lock.push_str(message);
    }
    set_state(PlaybackState::Error);
}

fn clear_error() {
    if let Ok(mut lock) = LAST_ERROR.lock() {
        lock.clear();
    }
}

fn update_latency(sample_rate: u32, channels: usize) {
    let buffered_samples = BUFFERED_SAMPLES.load(Ordering::Relaxed);
    if sample_rate == 0 || channels == 0 {
        LATENCY_MS.store(0, Ordering::Relaxed);
        return;
    }
    let samples_per_ms = (u64::from(sample_rate) * channels as u64).max(1) / 1000;
    if samples_per_ms == 0 {
        LATENCY_MS.store(0, Ordering::Relaxed);
    } else {
        LATENCY_MS.store(buffered_samples / samples_per_ms, Ordering::Relaxed);
    }
}

static BUFFERED_SAMPLES: AtomicU64 = AtomicU64::new(0);

fn consume_buffered_samples(count: u64) {
    let _ = BUFFERED_SAMPLES.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(count))
    });
}

fn consume_queued_buffers(count: u64) {
    let _ = QUEUED_BUFFERS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(count))
    });
}

fn stop_requested() -> bool {
    !PLAYING.load(Ordering::SeqCst) && state() == PlaybackState::Stopping
}

fn trim_queued_frames(queue: &mut VecDeque<Vec<f32>>, max_len: usize) {
    while queue.len() > max_len {
        if let Some(dropped) = queue.pop_front() {
            consume_queued_buffers(1);
            consume_buffered_samples(dropped.len() as u64);
        }
    }
}

#[no_mangle]
pub extern "C" fn cascadia_audio_start(url: *const c_char) -> i32 {
    if PLAYING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return 0;
    }

    if url.is_null() {
        PLAYING.store(false, Ordering::SeqCst);
        set_error("stream URL is null");
        return 0;
    }

    let url = unsafe { CStr::from_ptr(url) };
    let url = match url.to_str() {
        Ok(value) => value.to_owned(),
        Err(_) => {
            PLAYING.store(false, Ordering::SeqCst);
            set_error("stream URL is not valid UTF-8");
            return 0;
        }
    };

    DECODED_PACKETS.store(0, Ordering::SeqCst);
    DECODE_ERRORS.store(0, Ordering::SeqCst);
    QUEUED_BUFFERS.store(0, Ordering::SeqCst);
    UNDERRUNS.store(0, Ordering::SeqCst);
    LATENCY_MS.store(0, Ordering::SeqCst);
    RECONNECTS.store(0, Ordering::SeqCst);
    BUFFERED_SAMPLES.store(0, Ordering::SeqCst);
    clear_error();
    set_state(PlaybackState::Starting);

    std::thread::spawn(move || {
        let result = RUNTIME.block_on(stream_and_play(url));
        PLAYING.store(false, Ordering::SeqCst);
        if let Err(error) = result {
            set_error(&error.to_string());
        } else if state() != PlaybackState::Error {
            set_state(PlaybackState::Stopped);
        }
    });

    1
}

#[no_mangle]
pub extern "C" fn cascadia_audio_stop() -> i32 {
    if PLAYING.swap(false, Ordering::SeqCst) {
        set_state(PlaybackState::Stopping);
    }
    1
}

#[no_mangle]
pub extern "C" fn cascadia_audio_is_playing() -> i32 {
    if PLAYING.load(Ordering::SeqCst) {
        1
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn cascadia_audio_state() -> i32 {
    PLAYBACK_STATE.load(Ordering::SeqCst) as i32
}

#[no_mangle]
pub extern "C" fn cascadia_audio_last_error(buffer: *mut u8, buffer_len: usize) -> i32 {
    if buffer.is_null() || buffer_len == 0 {
        return 0;
    }

    let copied = if let Ok(lock) = LAST_ERROR.lock() {
        let bytes = lock.as_bytes();
        let len = bytes.len().min(buffer_len.saturating_sub(1));
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, len);
            *buffer.add(len) = 0;
        }
        len
    } else {
        0
    };

    copied as i32
}

#[no_mangle]
pub extern "C" fn cascadia_audio_debug_counters(
    decoded_packets: *mut u64,
    decode_errors: *mut u64,
    queued_buffers: *mut u64,
) -> i32 {
    if decoded_packets.is_null() || decode_errors.is_null() || queued_buffers.is_null() {
        return 0;
    }

    unsafe {
        *decoded_packets = DECODED_PACKETS.load(Ordering::SeqCst);
        *decode_errors = DECODE_ERRORS.load(Ordering::SeqCst);
        *queued_buffers = QUEUED_BUFFERS.load(Ordering::SeqCst);
    }

    1
}

#[no_mangle]
pub extern "C" fn cascadia_audio_debug_telemetry(
    underruns: *mut u64,
    latency_ms: *mut u64,
    reconnects: *mut u64,
) -> i32 {
    if underruns.is_null() || latency_ms.is_null() || reconnects.is_null() {
        return 0;
    }

    unsafe {
        *underruns = UNDERRUNS.load(Ordering::SeqCst);
        *latency_ms = LATENCY_MS.load(Ordering::SeqCst);
        *reconnects = RECONNECTS.load(Ordering::SeqCst);
    }

    1
}

const MAX_RECONNECT_ATTEMPTS: usize = 3;

async fn stream_and_play(url: String) -> Result<()> {
    let mut attempts = 0usize;
    loop {
        if !PLAYING.load(Ordering::SeqCst) {
            return if state() == PlaybackState::Stopping {
                Ok(())
            } else {
                Err(anyhow!("playback stopped unexpectedly"))
            };
        }

        if attempts == 0 {
            set_state(PlaybackState::Starting);
        } else {
            RECONNECTS.fetch_add(1, Ordering::SeqCst);
            set_state(PlaybackState::Reconnecting);
            tokio::time::sleep(Duration::from_secs((attempts as u64).min(5))).await;
        }

        let result = stream_and_play_once(&url).await;
        match result {
            Ok(()) => return Ok(()),
            Err(_err) if stop_requested() => return Ok(()),
            Err(err) if attempts >= MAX_RECONNECT_ATTEMPTS => {
                return Err(anyhow!(
                    "playback failed after {} reconnect attempts: {err}",
                    attempts
                ))
            }
            Err(_) => {
                attempts += 1;
            }
        }
    }
}

async fn stream_and_play_once(url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let response = client
        .get(url)
        .header("Icy-MetaData", HeaderValue::from_static("1"))
        .header(
            "User-Agent",
            HeaderValue::from_static("cascadia-audio-win-mvp/0.1"),
        )
        .send()
        .await
        .with_context(|| format!("request failed for stream URL: {url}"))?
        .error_for_status()
        .with_context(|| format!("stream returned non-success status: {url}"))?;

    let icy_interval = response
        .headers()
        .get("icy-metaint")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());

    let mut stream = response.bytes_stream();
    let first_chunk = match stream.next().await {
        Some(Ok(chunk)) => Some(chunk),
        Some(Err(err)) => return Err(anyhow!("stream read failed: {err}")),
        None => None,
    };

    let sniff = first_chunk.as_deref();
    if is_likely_aac(content_type.as_deref(), url, sniff) {
        set_state(PlaybackState::Buffering);
        return decode_aac_ffmpeg(url);
    }

    let (tx, rx) = mpsc::sync_channel::<Bytes>(32);
    if let Some(chunk) = first_chunk {
        let _ = tx.send(chunk);
    }

    let producer = tokio::spawn(async move {
        while PLAYING.load(Ordering::SeqCst) {
            match stream.next().await {
                Some(Ok(chunk)) => {
                    if tx.send(chunk).is_err() {
                        break;
                    }
                }
                Some(Err(_)) | None => break,
            }
        }
    });

    set_state(PlaybackState::Buffering);
    let playback_result = decode_and_play(rx, content_type, icy_interval);
    let _ = producer.await;
    playback_result
}

fn is_likely_aac(content_type: Option<&str>, url: &str, sniff: Option<&[u8]>) -> bool {
    let content_type_aac = content_type
        .map(|ct| ct.to_ascii_lowercase())
        .map(|ct| {
            ct.contains("aac")
                || ct.contains("aacp")
                || ct.contains("x-aac")
                || ct.contains("audio/mp4")
        })
        .unwrap_or(false);
    if content_type_aac {
        return true;
    }

    let url_path = url.split('?').next().unwrap_or(url).to_ascii_lowercase();
    if url_path.ends_with(".aac")
        || url_path.ends_with(".aacp")
        || url_path.ends_with(".m4a")
        || url_path.ends_with(".mp4")
    {
        return true;
    }

    if let Some(bytes) = sniff {
        if bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] & 0xF6) == 0xF0 {
            return true;
        }
        if bytes.len() >= 4 && &bytes[..4] == b"ADIF" {
            return true;
        }
    }

    false
}

fn decode_and_play(
    rx: Receiver<Bytes>,
    content_type: Option<String>,
    icy_interval: Option<usize>,
) -> Result<()> {
    set_state(PlaybackState::Buffering);
    let prebuffer = prebuffer_stream(&rx, 64 * 1024, Duration::from_secs(3));
    let source = ChannelSource::new(rx, icy_interval, prebuffer);

    let mss = MediaSourceStream::new(Box::new(source), Default::default());

    let mut hint = Hint::new();
    if let Some(ct) = content_type.as_deref() {
        if ct.contains("aac") {
            hint.with_extension("aac");
        } else if ct.contains("ogg") {
            hint.with_extension("ogg");
        } else if ct.contains("mpeg") || ct.contains("mp3") {
            hint.with_extension("mp3");
        }
    }

    let probed = get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;
    let mut format = probed.format;

    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("no default audio track found"))?;
    let track_id = track.id;
    let codec_params = &track.codec_params;

    let mut decoder = get_codecs().make(codec_params, &DecoderOptions::default())?;
    let (ring_tx, ring_rx) = mpsc::sync_channel::<Vec<f32>>(64);

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device"))?;
    let config = select_output_config(&device)?;
    let output_channels = usize::from(config.channels);
    let output_sample_rate = config.sample_rate.0;

    let mut current = Vec::<f32>::new();
    let mut current_idx = 0usize;
    let stream = device.build_output_stream(
        &config,
        move |output: &mut [f32], _| {
            if !PLAYING.load(Ordering::SeqCst) {
                output.fill(0.0);
                return;
            }

            let volume = f32::from_bits(VOLUME_BITS.load(Ordering::Relaxed));
            let mut consumed = 0u64;
            let mut had_underrun = false;

            for sample in output.iter_mut() {
                if current_idx >= current.len() {
                    match ring_rx.try_recv() {
                        Ok(next) => {
                            current = next;
                            current_idx = 0;
                            consume_queued_buffers(1);
                        }
                        Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => {
                            had_underrun = true;
                            *sample = 0.0;
                            continue;
                        }
                    }
                }

                *sample = current[current_idx] * volume;
                current_idx += 1;
                consumed += 1;
            }
            if had_underrun {
                UNDERRUNS.fetch_add(1, Ordering::Relaxed);
            }
            consume_buffered_samples(consumed);
            update_latency(output_sample_rate, output_channels);
        },
        move |_err| {},
        None,
    )?;
    stream.play()?;
    set_state(PlaybackState::Playing);

    decode_loop(
        &mut *format,
        &mut *decoder,
        ring_tx,
        track_id,
        output_channels,
    )?;
    std::thread::sleep(Duration::from_millis(10));
    Ok(())
}

fn decode_aac_ffmpeg(url: &str) -> Result<()> {
    set_state(PlaybackState::Buffering);
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device"))?;
    let config = select_output_config(&device)?;
    let output_channels = usize::from(config.channels);
    let output_sample_rate = config.sample_rate.0;

    let queue: Arc<Mutex<VecDeque<Vec<f32>>>> = Arc::new(Mutex::new(VecDeque::new()));
    let queue_for_callback = Arc::clone(&queue);
    let prebuffer_samples = (output_sample_rate as usize * output_channels) * 4;
    let prebuffer_frames = 96usize;

    let mut ffmpeg = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-reconnect")
        .arg("1")
        .arg("-reconnect_streamed")
        .arg("1")
        .arg("-reconnect_delay_max")
        .arg("5")
        .arg("-thread_queue_size")
        .arg("32")
        .arg("-fflags")
        .arg("nobuffer")
        .arg("-flags")
        .arg("low_delay")
        .arg("-i")
        .arg(url)
        .arg("-vn")
        .arg("-f")
        .arg("f32le")
        .arg("-ar")
        .arg(output_sample_rate.to_string())
        .arg("-ac")
        .arg(output_channels.to_string())
        .arg("pipe:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start ffmpeg; install ffmpeg to decode AAC/HE-AAC streams")?;

    let child_stdout = ffmpeg
        .stdout
        .take()
        .ok_or_else(|| anyhow!("ffmpeg stdout unavailable"))?;
    let child_stderr = ffmpeg
        .stderr
        .take()
        .ok_or_else(|| anyhow!("ffmpeg stderr unavailable"))?;
    let child_stdout = BufReader::new(child_stdout);
    let child_stderr = BufReader::new(child_stderr);

    let stdout_queue = Arc::clone(&queue);
    let stdout_thread = std::thread::spawn(move || -> Result<()> {
        let mut child_stdout = child_stdout;
        let mut buffer = [0u8; 65536];
        while PLAYING.load(Ordering::SeqCst) {
            let n = match child_stdout.read(&mut buffer) {
                Ok(n) => n,
                Err(err) if err.kind() == ErrorKind::UnexpectedEof => break,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }

            let mut samples = Vec::with_capacity(n / 4);
            let mut offset = 0usize;
            while offset + 4 <= n {
                let bytes = &buffer[offset..offset + 4];
                samples.push(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
                offset += 4;
            }
            if samples.is_empty() {
                continue;
            }

            let mut queue_guard = stdout_queue.lock().expect("audio queue poisoned");
            BUFFERED_SAMPLES.fetch_add(samples.len() as u64, Ordering::Relaxed);
            queue_guard.push_back(samples);
            QUEUED_BUFFERS.fetch_add(1, Ordering::Relaxed);
            trim_queued_frames(&mut queue_guard, 256);
        }
        Ok(())
    });

    let stderr_thread = std::thread::spawn(move || -> Result<()> {
        let mut child_stderr = child_stderr;
        let mut buffer = [0u8; 2048];
        while let Ok(n) = child_stderr.read(&mut buffer) {
            if n == 0 {
                break;
            }
        }
        Ok(())
    });

    while PLAYING.load(Ordering::SeqCst) {
        if let Some(status) = ffmpeg.try_wait()? {
            return Err(anyhow!("ffmpeg exited unexpectedly: {status}"));
        }

        let queue_guard = queue_for_callback.lock().expect("audio queue poisoned");
        let buffered_samples = queue_guard.iter().map(|frame| frame.len()).sum::<usize>();
        let ready = buffered_samples >= prebuffer_samples || queue_guard.len() >= prebuffer_frames;
        drop(queue_guard);
        if ready {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let stream = device.build_output_stream(
        &config,
        move |output: &mut [f32], _| {
            if !PLAYING.load(Ordering::SeqCst) {
                output.fill(0.0);
                return;
            }

            let volume = f32::from_bits(VOLUME_BITS.load(Ordering::Relaxed));
            let mut queue_guard = queue_for_callback.lock().expect("audio queue poisoned");
            let mut pending = queue_guard.pop_front();
            if pending.is_some() {
                consume_queued_buffers(1);
            }
            let mut pending_index = 0usize;
            let mut consumed = 0u64;
            let mut had_underrun = false;
            for sample in output.iter_mut() {
                if pending.is_none()
                    || pending_index >= pending.as_ref().map(|frame| frame.len()).unwrap_or(0)
                {
                    pending = queue_guard.pop_front();
                    pending_index = 0;
                    if pending.is_some() {
                        consume_queued_buffers(1);
                    }
                }
                if let Some(frame) = pending.as_ref() {
                    *sample = frame[pending_index] * volume;
                    consumed += 1;
                } else {
                    had_underrun = true;
                    *sample = 0.0;
                }
                pending_index += 1;
            }
            if had_underrun {
                UNDERRUNS.fetch_add(1, Ordering::Relaxed);
            }
            consume_buffered_samples(consumed);
            update_latency(output_sample_rate, output_channels);
        },
        move |_err| {},
        None,
    )?;
    stream.play()?;
    set_state(PlaybackState::Playing);

    let mut ffmpeg_exit = None;
    while PLAYING.load(Ordering::SeqCst) {
        if let Some(status) = ffmpeg.try_wait()? {
            ffmpeg_exit = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = ffmpeg.kill();
    let _ = ffmpeg.wait();
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    std::thread::sleep(Duration::from_millis(10));
    if let Some(status) = ffmpeg_exit {
        return Err(anyhow!("ffmpeg exited unexpectedly: {status}"));
    }
    Ok(())
}

fn decode_loop(
    format: &mut dyn symphonia::core::formats::FormatReader,
    decoder: &mut dyn symphonia::core::codecs::Decoder,
    ring_tx: SyncSender<Vec<f32>>,
    track_id: u32,
    output_channels: usize,
) -> Result<()> {
    while PLAYING.load(Ordering::SeqCst) {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(_)) => break,
            Err(error) => return Err(anyhow!("format read error: {error}")),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => {
                DECODE_ERRORS.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Err(SymphoniaError::IoError(_)) => break,
            Err(error) => return Err(anyhow!("decoder error: {error}")),
        };
        DECODED_PACKETS.fetch_add(1, Ordering::Relaxed);

        let input_channels = decoded.spec().channels.count();
        let mut sample_buffer =
            SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        sample_buffer.copy_interleaved_ref(decoded);
        let remixed = remix_channels(sample_buffer.samples(), input_channels, output_channels);
        BUFFERED_SAMPLES.fetch_add(remixed.len() as u64, Ordering::Relaxed);
        if ring_tx.send(remixed).is_err() {
            break;
        }
        QUEUED_BUFFERS.fetch_add(1, Ordering::Relaxed);
    }

    Ok(())
}

fn select_output_config(device: &cpal::Device) -> Result<cpal::StreamConfig> {
    let default_config = device.default_output_config()?;
    if default_config.sample_format() == cpal::SampleFormat::F32 {
        let mut config = default_config.config();
        config.buffer_size = BufferSize::Fixed(8192);
        return Ok(config);
    }

    let mut supported = device.supported_output_configs()?;
    if let Some(config) = supported.find(|cfg| cfg.sample_format() == cpal::SampleFormat::F32) {
        let mut config = config.with_max_sample_rate().config();
        config.buffer_size = BufferSize::Fixed(8192);
        return Ok(config);
    }

    Err(anyhow!("no f32 output stream config available"))
}

fn remix_channels(input: &[f32], input_channels: usize, output_channels: usize) -> Vec<f32> {
    if input_channels == 0 || output_channels == 0 {
        return Vec::new();
    }

    if input_channels == output_channels {
        return input.to_vec();
    }

    let mut remixed = Vec::with_capacity((input.len() / input_channels) * output_channels);
    for frame in input.chunks_exact(input_channels) {
        match (input_channels, output_channels) {
            (1, 2) => {
                remixed.push(frame[0]);
                remixed.push(frame[0]);
            }
            (2, 1) => {
                remixed.push((frame[0] + frame[1]) * 0.5);
            }
            _ => {
                for channel in 0..output_channels {
                    remixed.push(*frame.get(channel).unwrap_or(&0.0));
                }
            }
        }
    }

    remixed
}

struct ChannelSource {
    rx: Mutex<Receiver<Bytes>>,
    prebuffered_chunks: VecDeque<Bytes>,
    icy_interval: Option<usize>,
    bytes_until_meta: usize,
    current_chunk: Option<Bytes>,
    chunk_offset: usize,
}

impl ChannelSource {
    fn new(
        rx: Receiver<Bytes>,
        icy_interval: Option<usize>,
        prebuffered_chunks: VecDeque<Bytes>,
    ) -> Self {
        let bytes_until_meta = icy_interval.unwrap_or(0);
        Self {
            rx: Mutex::new(rx),
            prebuffered_chunks,
            icy_interval,
            bytes_until_meta,
            current_chunk: None,
            chunk_offset: 0,
        }
    }

    fn refill_chunk(&mut self) -> IoResult<bool> {
        if let Some(chunk) = &self.current_chunk {
            if self.chunk_offset < chunk.len() {
                return Ok(true);
            }
        }

        if let Some(chunk) = self.prebuffered_chunks.pop_front() {
            self.current_chunk = Some(chunk);
            self.chunk_offset = 0;
            return Ok(true);
        }

        let recv = self
            .rx
            .lock()
            .map_err(|_| IoError::new(ErrorKind::Other, "receiver lock poisoned"))?
            .recv();

        match recv {
            Ok(bytes) => {
                self.current_chunk = Some(bytes);
                self.chunk_offset = 0;
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }

    fn read_raw(&mut self, out: &mut [u8]) -> IoResult<usize> {
        if out.is_empty() {
            return Ok(0);
        }

        if !self.refill_chunk()? {
            return Ok(0);
        }

        let chunk = self.current_chunk.as_ref().expect("chunk is available");
        let available = chunk.len().saturating_sub(self.chunk_offset);
        let to_copy = available.min(out.len());
        out[..to_copy].copy_from_slice(&chunk[self.chunk_offset..self.chunk_offset + to_copy]);
        self.chunk_offset += to_copy;
        Ok(to_copy)
    }

    fn read_raw_exact(&mut self, out: &mut [u8]) -> IoResult<()> {
        let mut filled = 0usize;
        while filled < out.len() {
            let n = self.read_raw(&mut out[filled..])?;
            if n == 0 {
                return Err(IoError::new(
                    ErrorKind::UnexpectedEof,
                    "stream ended while reading ICY metadata",
                ));
            }
            filled += n;
        }
        Ok(())
    }

    fn skip_raw_exact(&mut self, mut len: usize) -> IoResult<()> {
        let mut scratch = [0u8; 1024];
        while len > 0 {
            let target = len.min(scratch.len());
            self.read_raw_exact(&mut scratch[..target])?;
            len -= target;
        }
        Ok(())
    }
}

impl Read for ChannelSource {
    fn read(&mut self, out: &mut [u8]) -> IoResult<usize> {
        if out.is_empty() {
            return Ok(0);
        }

        let Some(interval) = self.icy_interval else {
            return self.read_raw(out);
        };

        if self.bytes_until_meta == 0 {
            self.bytes_until_meta = interval;
        }

        let mut written = 0usize;
        while written < out.len() {
            if self.bytes_until_meta == 0 {
                let mut length_byte = [0u8; 1];
                match self.read_raw_exact(&mut length_byte) {
                    Ok(()) => {
                        let metadata_len = usize::from(length_byte[0]) * 16;
                        if metadata_len > 0 {
                            self.skip_raw_exact(metadata_len)?;
                        }
                        self.bytes_until_meta = interval;
                    }
                    Err(err) if err.kind() == ErrorKind::UnexpectedEof => break,
                    Err(err) => return Err(err),
                }
                continue;
            }

            let can_read = (out.len() - written).min(self.bytes_until_meta);
            let n = self.read_raw(&mut out[written..written + can_read])?;
            if n == 0 {
                break;
            }
            written += n;
            self.bytes_until_meta -= n;
        }

        Ok(written)
    }
}

impl Seek for ChannelSource {
    fn seek(&mut self, _pos: SeekFrom) -> IoResult<u64> {
        Err(IoError::new(
            ErrorKind::Unsupported,
            "live stream is not seekable",
        ))
    }
}

impl MediaSource for ChannelSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

fn prebuffer_stream(
    rx: &Receiver<Bytes>,
    target_bytes: usize,
    max_wait: Duration,
) -> VecDeque<Bytes> {
    let mut total = 0usize;
    let mut chunks = VecDeque::new();
    let started = Instant::now();

    while PLAYING.load(Ordering::SeqCst) && total < target_bytes && started.elapsed() < max_wait {
        let remaining = max_wait.saturating_sub(started.elapsed());
        let wait = remaining.min(Duration::from_millis(250));

        match rx.recv_timeout(wait) {
            Ok(chunk) => {
                total += chunk.len();
                chunks.push_back(chunk);
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_aac_from_content_type() {
        assert!(is_likely_aac(
            Some("audio/aacp"),
            "https://example.com/stream",
            None
        ));
    }

    #[test]
    fn detects_aac_from_url_extension() {
        assert!(is_likely_aac(
            None,
            "https://example.com/live/channel.aac?token=abc",
            None
        ));
    }

    #[test]
    fn detects_aac_from_adts_sync_word() {
        assert!(is_likely_aac(
            None,
            "https://example.com/live",
            Some(&[0xFF, 0xF1, 0x50, 0x80])
        ));
    }

    #[test]
    fn does_not_misclassify_mp3_stream() {
        assert!(!is_likely_aac(
            Some("audio/mpeg"),
            "https://example.com/live.mp3",
            Some(&[0x49, 0x44, 0x33, 0x04])
        ));
    }

    #[test]
    fn remix_channels_handles_common_layouts() {
        assert_eq!(
            remix_channels(&[0.5, -0.25], 1, 2),
            vec![0.5, 0.5, -0.25, -0.25]
        );
        assert_eq!(remix_channels(&[0.8, -0.2, 0.1, 0.3], 2, 1), vec![0.3, 0.2]);
        assert_eq!(remix_channels(&[1.0, 0.5, 0.25], 3, 2), vec![1.0, 0.5]);
        assert!(remix_channels(&[1.0, 2.0], 0, 2).is_empty());
    }

    #[test]
    fn channel_source_strips_icy_metadata() {
        let (tx, rx) = mpsc::sync_channel(4);
        tx.send(Bytes::from_static(b"ABCD")).expect("send audio");
        tx.send(Bytes::from_static(b"\x01"))
            .expect("send metadata length");
        tx.send(Bytes::from(vec![b'x'; 16]))
            .expect("send metadata payload");
        tx.send(Bytes::from_static(b"EFGH"))
            .expect("send tail audio");
        drop(tx);

        let mut source = ChannelSource::new(rx, Some(4), VecDeque::new());
        let mut output = Vec::new();
        source.read_to_end(&mut output).expect("read stream");
        assert_eq!(output, b"ABCDEFGH");
    }

    #[test]
    fn prebuffer_stream_collects_until_target() {
        let was_playing = PLAYING.swap(true, Ordering::SeqCst);
        let (tx, rx) = mpsc::sync_channel(4);
        tx.send(Bytes::from_static(b"abc")).expect("send first");
        tx.send(Bytes::from_static(b"def")).expect("send second");
        drop(tx);

        let chunks = prebuffer_stream(&rx, 5, Duration::from_millis(100));
        PLAYING.store(was_playing, Ordering::SeqCst);

        assert_eq!(chunks.len(), 2);
        let total: usize = chunks.iter().map(|chunk| chunk.len()).sum();
        assert!(total >= 5);
    }

    #[test]
    fn trim_queued_frames_updates_buffer_telemetry() {
        let queued_before = QUEUED_BUFFERS.swap(2, Ordering::SeqCst);
        let buffered_before = BUFFERED_SAMPLES.swap(10, Ordering::SeqCst);

        let mut queue = VecDeque::from([vec![1.0_f32; 4], vec![1.0_f32; 6]]);
        trim_queued_frames(&mut queue, 1);

        assert_eq!(queue.len(), 1);
        assert_eq!(QUEUED_BUFFERS.load(Ordering::SeqCst), 1);
        assert_eq!(BUFFERED_SAMPLES.load(Ordering::SeqCst), 6);

        QUEUED_BUFFERS.store(queued_before, Ordering::SeqCst);
        BUFFERED_SAMPLES.store(buffered_before, Ordering::SeqCst);
    }

    #[test]
    fn stop_requested_depends_on_stopping_state() {
        let was_playing = PLAYING.swap(false, Ordering::SeqCst);
        let state_before = state();

        set_state(PlaybackState::Stopping);
        assert!(stop_requested());

        set_state(PlaybackState::Error);
        assert!(!stop_requested());

        PLAYING.store(was_playing, Ordering::SeqCst);
        set_state(state_before);
    }

    #[test]
    fn stop_does_not_overwrite_non_playing_state() {
        let was_playing = PLAYING.swap(false, Ordering::SeqCst);
        let state_before = state();

        set_state(PlaybackState::Error);
        assert_eq!(cascadia_audio_stop(), 1);
        assert!(matches!(state(), PlaybackState::Error));

        PLAYING.store(was_playing, Ordering::SeqCst);
        set_state(state_before);
    }
}
