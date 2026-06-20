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
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to create tokio runtime")
});

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
        return 0;
    }

    let url = unsafe { CStr::from_ptr(url) };
    let url = match url.to_str() {
        Ok(value) => value.to_owned(),
        Err(_) => {
            PLAYING.store(false, Ordering::SeqCst);
            return 0;
        }
    };

    DECODED_PACKETS.store(0, Ordering::SeqCst);
    DECODE_ERRORS.store(0, Ordering::SeqCst);
    QUEUED_BUFFERS.store(0, Ordering::SeqCst);

    std::thread::spawn(move || {
        let _ = RUNTIME.block_on(stream_and_play(url));
        PLAYING.store(false, Ordering::SeqCst);
    });

    1
}

#[no_mangle]
pub extern "C" fn cascadia_audio_stop() -> i32 {
    PLAYING.store(false, Ordering::SeqCst);
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

async fn stream_and_play(url: String) -> Result<()> {
    let client = reqwest::Client::new();
    let response = client
        .get(&url)
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

    if content_type
        .as_deref()
        .map(|ct| ct.contains("aac"))
        .unwrap_or(false)
    {
        return decode_aac_ffmpeg(&url);
    }

    let (tx, rx) = mpsc::sync_channel::<Bytes>(32);
    let mut stream = response.bytes_stream();

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

    let playback_result = decode_and_play(rx, content_type, icy_interval);
    let _ = producer.await;
    playback_result
}

fn decode_and_play(
    rx: Receiver<Bytes>,
    content_type: Option<String>,
    icy_interval: Option<usize>,
) -> Result<()> {
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

            for sample in output.iter_mut() {
                if current_idx >= current.len() {
                    match ring_rx.try_recv() {
                        Ok(next) => {
                            current = next;
                            current_idx = 0;
                        }
                        Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => {
                            *sample = 0.0;
                            continue;
                        }
                    }
                }

                *sample = current[current_idx] * volume;
                current_idx += 1;
            }
        },
        move |_err| {},
        None,
    )?;
    stream.play()?;

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
            queue_guard.push_back(samples);
            while queue_guard.len() > 256 {
                queue_guard.pop_front();
            }
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
            PLAYING.store(false, Ordering::SeqCst);
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
            let mut pending_index = 0usize;
            for sample in output.iter_mut() {
                if pending.is_none()
                    || pending_index >= pending.as_ref().map(|frame| frame.len()).unwrap_or(0)
                {
                    pending = queue_guard.pop_front();
                    pending_index = 0;
                }
                if let Some(frame) = pending.as_ref() {
                    *sample = frame[pending_index] * volume;
                } else {
                    *sample = 0.0;
                }
                pending_index += 1;
            }
        },
        move |_err| {},
        None,
    )?;
    stream.play()?;

    while PLAYING.load(Ordering::SeqCst) {
        if ffmpeg.try_wait()?.is_some() {
            PLAYING.store(false, Ordering::SeqCst);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = ffmpeg.kill();
    let _ = ffmpeg.wait();
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    std::thread::sleep(Duration::from_millis(10));
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
