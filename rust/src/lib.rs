use std::ffi::CStr;
use std::io::{Error as IoError, ErrorKind, Read, Result as IoResult, Seek, SeekFrom};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use futures::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderValue};
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

async fn stream_and_play(url: String) -> Result<()> {
    let client = reqwest::Client::new();
    let response = client
        .get(&url)
        .header("Icy-MetaData", HeaderValue::from_static("1"))
        .header("User-Agent", HeaderValue::from_static("cascadia-audio-win-mvp/0.1"))
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
    let source = ChannelSource::new(rx, icy_interval);
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
    let codec_params = &track.codec_params;
    let sample_rate = codec_params.sample_rate.unwrap_or(44_100);
    let channel_count = codec_params
        .channels
        .map(|channels| channels.count() as u16)
        .unwrap_or(2);

    let mut decoder = get_codecs().make(codec_params, &DecoderOptions::default())?;
    let (ring_tx, ring_rx) = mpsc::sync_channel::<Vec<f32>>(64);

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device"))?;
    let mut config = device.default_output_config()?.config();
    config.sample_rate = cpal::SampleRate(sample_rate);
    config.channels = channel_count;

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

    decode_loop(&mut *format, &mut *decoder, ring_tx)?;
    std::thread::sleep(Duration::from_millis(10));
    Ok(())
}

fn decode_loop(
    format: &mut dyn symphonia::core::formats::FormatReader,
    decoder: &mut dyn symphonia::core::codecs::Decoder,
    ring_tx: SyncSender<Vec<f32>>,
) -> Result<()> {
    while PLAYING.load(Ordering::SeqCst) {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(_)) => break,
            Err(error) => return Err(anyhow!("format read error: {error}")),
        };

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(_)) => break,
            Err(error) => return Err(anyhow!("decoder error: {error}")),
        };

        let mut sample_buffer =
            SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        sample_buffer.copy_interleaved_ref(decoded);
        if ring_tx.send(sample_buffer.samples().to_vec()).is_err() {
            break;
        }
    }

    Ok(())
}

struct ChannelSource {
    rx: Mutex<Receiver<Bytes>>,
    icy_interval: Option<usize>,
    bytes_until_meta: usize,
    current_chunk: Option<Bytes>,
    chunk_offset: usize,
}

impl ChannelSource {
    fn new(rx: Receiver<Bytes>, icy_interval: Option<usize>) -> Self {
        let bytes_until_meta = icy_interval.unwrap_or(0);
        Self {
            rx: Mutex::new(rx),
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

