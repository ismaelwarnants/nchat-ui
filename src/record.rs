//! Voice note recording: microphone -> mono 48 kHz -> Opus in Ogg (the format WhatsApp
//! uses for voice notes), plus a "<file>.voicenote" sidecar with duration and waveform so
//! nchat (patch 0002) sends it as a voice note instead of an audio file.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const OUT_RATE: u32 = 48_000;
const FRAME: usize = 960; // 20 ms at 48 kHz
const WAVEFORM_BARS: usize = 64;
pub const MAX_LENGTH: Duration = Duration::from_secs(15 * 60);

struct Shared {
    samples: Vec<f32>, // mono, device rate
    level: f32,        // recent peak, 0..1 (for the level meter)
}

pub struct Recorder {
    _stream: cpal::Stream,
    shared: Arc<Mutex<Shared>>,
    rate: u32,
    started: Instant,
}

thread_local! {
    static ACTIVE: RefCell<Option<Recorder>> = const { RefCell::new(None) };
}

pub fn is_recording() -> bool {
    ACTIVE.with(|a| a.borrow().is_some())
}

/// Starts recording from the default microphone.
pub fn start() -> Result<(), String> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or("No microphone found")?;
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    let channels = supported.channels().max(1) as usize;
    let rate = supported.sample_rate();
    let config = supported.config();
    let shared = Arc::new(Mutex::new(Shared { samples: Vec::with_capacity(rate as usize * 60), level: 0.0 }));

    fn push(shared: &Arc<Mutex<Shared>>, mono: impl Iterator<Item = f32>) {
        let mut s = shared.lock().unwrap();
        let mut peak = 0f32;
        for v in mono {
            peak = peak.max(v.abs());
            s.samples.push(v);
        }
        s.level = s.level * 0.6 + peak * 0.4;
    }
    let err = |e: cpal::Error| eprintln!("nchat-ui: microphone error: {e}");
    let sh = shared.clone();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            config,
            move |data: &[f32], _: &_| push(&sh, data.chunks(channels).map(|c| c.iter().sum::<f32>() / c.len() as f32)),
            err,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            config,
            move |data: &[i16], _: &_| {
                push(&sh, data.chunks(channels).map(|c| c.iter().map(|v| *v as f32 / 32768.0).sum::<f32>() / c.len() as f32))
            },
            err,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream::<u16, _, _>(
            config,
            move |data: &[u16], _: &_| {
                push(
                    &sh,
                    data.chunks(channels).map(|c| c.iter().map(|v| (*v as f32 - 32768.0) / 32768.0).sum::<f32>() / c.len() as f32),
                )
            },
            err,
            None,
        ),
        other => return Err(format!("Unsupported microphone sample format {other:?}")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    ACTIVE.with(|a| *a.borrow_mut() = Some(Recorder { _stream: stream, shared, rate, started: Instant::now() }));
    Ok(())
}

/// (elapsed time, current level 0..1) while recording.
pub fn status() -> Option<(Duration, f32)> {
    ACTIVE.with(|a| a.borrow().as_ref().map(|r| (r.started.elapsed(), r.shared.lock().unwrap().level.min(1.0))))
}

pub fn cancel() {
    ACTIVE.with(|a| a.borrow_mut().take());
}

/// Stops recording; returns the captured audio for encoding (do that off the GUI thread).
pub fn stop() -> Option<(Vec<f32>, u32)> {
    let rec = ACTIVE.with(|a| a.borrow_mut().take())?;
    drop(rec._stream);
    let samples = std::mem::take(&mut rec.shared.lock().unwrap().samples);
    Some((samples, rec.rate))
}

/// Linear resampling to 48 kHz (plenty for speech).
fn resample(input: &[f32], from: u32) -> Vec<f32> {
    if from == OUT_RATE || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from as f64 / OUT_RATE as f64;
    let n = (input.len() as f64 / ratio) as usize;
    (0..n)
        .map(|i| {
            let pos = i as f64 * ratio;
            let i0 = pos as usize;
            let i1 = (i0 + 1).min(input.len() - 1);
            let t = (pos - i0 as f64) as f32;
            input[i0] * (1.0 - t) + input[i1] * t
        })
        .collect()
}

/// 64 bars, 0..100, from the RMS of each slice (WhatsApp's voice-note waveform).
fn waveform(samples: &[f32]) -> Vec<u8> {
    if samples.is_empty() {
        return vec![0; WAVEFORM_BARS];
    }
    let chunk = samples.len().div_ceil(WAVEFORM_BARS).max(1);
    let rms: Vec<f32> = samples
        .chunks(chunk)
        .map(|c| (c.iter().map(|v| v * v).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    let max = rms.iter().cloned().fold(0.0001f32, f32::max);
    let mut bars: Vec<u8> = rms.iter().map(|v| ((v / max) * 100.0).round() as u8).collect();
    bars.resize(WAVEFORM_BARS, 0);
    bars
}

/// Encodes to Ogg/Opus at `path` and writes the `.voicenote` sidecar. Returns the duration.
pub fn encode(samples: &[f32], rate: u32, path: &PathBuf) -> Result<Duration, String> {
    let pcm = resample(samples, rate);
    if pcm.len() < OUT_RATE as usize / 2 {
        return Err("Recording too short".into());
    }
    let mut enc = opus::Encoder::new(OUT_RATE, opus::Channels::Mono, opus::Application::Voip).map_err(|e| e.to_string())?;
    enc.set_bitrate(opus::Bitrate::Bits(32_000)).map_err(|e| e.to_string())?;
    let pre_skip = enc.get_lookahead().unwrap_or(312).max(0) as u16;

    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut writer = ogg::PacketWriter::new(std::io::BufWriter::new(file));
    let serial: u32 = rand_serial();
    // OpusHead (RFC 7845)
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(1); // channels
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&OUT_RATE.to_le_bytes()); // input sample rate (informational)
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family
    writer.write_packet(head, serial, ogg::PacketWriteEndInfo::EndPage, 0).map_err(|e| e.to_string())?;
    let mut tags = Vec::new();
    tags.extend_from_slice(b"OpusTags");
    let vendor = b"nchat-ui";
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes()); // no comments
    writer.write_packet(tags, serial, ogg::PacketWriteEndInfo::EndPage, 0).map_err(|e| e.to_string())?;

    let mut out = vec![0u8; 4000];
    let frames: Vec<&[f32]> = pcm.chunks(FRAME).collect();
    let mut granule: u64 = pre_skip as u64;
    for (i, frame) in frames.iter().enumerate() {
        let mut buf = frame.to_vec();
        buf.resize(FRAME, 0.0); // pad the last frame
        let n = enc.encode_float(&buf, &mut out).map_err(|e| e.to_string())?;
        granule += frame.len() as u64;
        let last = i + 1 == frames.len();
        let end = if last {
            ogg::PacketWriteEndInfo::EndStream
        } else if i % 50 == 49 {
            ogg::PacketWriteEndInfo::EndPage // ~1 s per page
        } else {
            ogg::PacketWriteEndInfo::NormalPacket
        };
        writer.write_packet(out[..n].to_vec(), serial, end, granule).map_err(|e| e.to_string())?;
    }
    drop(writer);

    let duration = Duration::from_secs_f64(pcm.len() as f64 / OUT_RATE as f64);
    let bars = waveform(&pcm);
    let sidecar = format!(
        "{{\"seconds\":{},\"waveform\":[{}]}}",
        duration.as_secs_f64().round().max(1.0) as u32,
        bars.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(",")
    );
    let mut side = path.clone().into_os_string();
    side.push(".voicenote");
    std::fs::write(side, sidecar).map_err(|e| e.to_string())?;
    Ok(duration)
}

fn rand_serial() -> u32 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (t.as_nanos() as u32) ^ std::process::id().rotate_left(16)
}

/// Where recordings are kept until sent (~/.cache/nchat-ui/voice).
pub fn new_recording_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache"));
    let dir = base.join("nchat-ui").join("voice");
    let _ = std::fs::create_dir_all(&dir);
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    dir.join(format!("voice-note-{stamp}.ogg"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_voice_note_that_decodes_back() {
        // 2 s of a 300 Hz tone at 44.1 kHz (exercises resampling)
        let rate = 44_100;
        let samples: Vec<f32> =
            (0..rate * 2).map(|i| (i as f32 * 300.0 * std::f32::consts::TAU / rate as f32).sin() * 0.3).collect();
        let path = std::env::temp_dir().join(format!("nchat-ui-test-{}.ogg", std::process::id()));
        let dur = encode(&samples, rate, &path).expect("encode");
        assert!((dur.as_secs_f32() - 2.0).abs() < 0.05, "duration {dur:?}");

        let decoded = crate::audio::decode(path.to_str().unwrap()).expect("decode");
        assert_eq!(decoded.sample_rate, 48_000);
        assert!((decoded.duration().as_secs_f32() - 2.0).abs() < 0.1, "decoded {:?}", decoded.duration());

        let side = std::fs::read_to_string(format!("{}.voicenote", path.display())).expect("sidecar");
        assert!(side.starts_with("{\"seconds\":2,\"waveform\":["), "{side}");
        assert_eq!(side.matches(',').count(), 64, "64 waveform values + seconds field");
        if std::env::var_os("NCHAT_UI_KEEP_TEST_FILES").is_none() {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}.voicenote", path.display()));
        }
    }
}
