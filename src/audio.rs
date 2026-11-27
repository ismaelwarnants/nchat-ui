//! In-app audio playback (voice notes and audio files).
//!
//! WhatsApp voice notes are Ogg/Opus: decoded with the `ogg` + `opus` crates (libopus is
//! compiled in). Other formats (MP3, AAC/M4A, Vorbis, FLAC, WAV) go through rodio's
//! Symphonia decoder. Files are decoded completely in a background thread (voice notes
//! are small), then played from memory so seeking is exact.

use std::cell::RefCell;
use std::fs::File;
use std::io::BufReader;
use std::num::NonZero;
use std::time::Duration;

pub struct Decoded {
    pub channels: u16,
    pub sample_rate: u32,
    pub samples: Vec<f32>,
}

impl Decoded {
    pub fn duration(&self) -> Duration {
        let frames = self.samples.len() as f64 / self.channels.max(1) as f64;
        Duration::from_secs_f64(frames / self.sample_rate.max(1) as f64)
    }
}

/// Decodes a whole audio file to interleaved f32 samples.
pub fn decode(path: &str) -> Result<Decoded, String> {
    let mut head = [0u8; 64];
    {
        use std::io::Read;
        let mut f = File::open(path).map_err(|e| e.to_string())?;
        let n = f.read(&mut head).map_err(|e| e.to_string())?;
        if n >= 36 && &head[..4] == b"OggS" && head[28..36] == *b"OpusHead" {
            return decode_ogg_opus(path);
        }
    }
    decode_symphonia(path)
}

fn decode_ogg_opus(path: &str) -> Result<Decoded, String> {
    let file = BufReader::new(File::open(path).map_err(|e| e.to_string())?);
    let mut reader = ogg::PacketReader::new(file);
    // OpusHead: channels at byte 9, pre-skip (samples at 48 kHz) at 10..12
    let head = reader.read_packet().map_err(|e| e.to_string())?.ok_or("empty file")?;
    if head.data.len() < 19 || &head.data[..8] != b"OpusHead" {
        return Err("not an Opus stream".into());
    }
    let channels = head.data[9].clamp(1, 2) as u16;
    let pre_skip = u16::from_le_bytes([head.data[10], head.data[11]]) as usize;
    let _tags = reader.read_packet().map_err(|e| e.to_string())?;

    let opus_channels = if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
    let mut decoder = opus::Decoder::new(48_000, opus_channels).map_err(|e| e.to_string())?;
    let mut samples = Vec::new();
    let mut buf = vec![0f32; 5760 * channels as usize]; // max 120 ms frame
    while let Some(packet) = reader.read_packet().map_err(|e| e.to_string())? {
        match decoder.decode_float(&packet.data, &mut buf, false) {
            Ok(n) => samples.extend_from_slice(&buf[..n * channels as usize]),
            Err(_) => continue, // skip a corrupt packet
        }
    }
    let skip = (pre_skip * channels as usize).min(samples.len());
    samples.drain(..skip);
    Ok(Decoded { channels, sample_rate: 48_000, samples })
}

fn decode_symphonia(path: &str) -> Result<Decoded, String> {
    use rodio::Source;
    let file = File::open(path).map_err(|e| e.to_string())?;
    let decoder = rodio::Decoder::try_from(file).map_err(|e| e.to_string())?;
    let channels = decoder.channels().get();
    let sample_rate = decoder.sample_rate().get();
    let samples: Vec<f32> = decoder.map(|s| s as f32).collect();
    if samples.is_empty() {
        return Err("no audio".into());
    }
    Ok(Decoded { channels, sample_rate, samples })
}

// ---------------------------------------------------------------------------------------
// Player (GUI thread)
// ---------------------------------------------------------------------------------------

pub struct Playing {
    pub msg_id: String,
    pub duration: Duration,
    player: rodio::Player,
    speed: std::cell::Cell<f32>,
}

struct Output {
    _sink: rodio::stream::MixerDeviceSink,
    mixer: rodio::mixer::Mixer,
}

thread_local! {
    static OUTPUT: RefCell<Option<Output>> = const { RefCell::new(None) };
}

fn mixer() -> Result<rodio::mixer::Mixer, String> {
    OUTPUT.with(|o| {
        let mut o = o.borrow_mut();
        if o.is_none() {
            let mut sink = rodio::stream::DeviceSinkBuilder::open_default_sink().map_err(|e| e.to_string())?;
            sink.log_on_drop(false);
            let mixer = sink.mixer().clone();
            *o = Some(Output { _sink: sink, mixer });
        }
        Ok(o.as_ref().unwrap().mixer.clone())
    })
}

impl Playing {
    pub fn start(msg_id: &str, audio: Decoded, speed: f32) -> Result<Playing, String> {
        let mixer = mixer()?;
        let duration = audio.duration();
        let channels = NonZero::new(audio.channels.max(1)).unwrap();
        let rate = NonZero::new(audio.sample_rate.max(1)).unwrap();
        let samples: Vec<rodio::Sample> = audio.samples.into_iter().map(|s| s as rodio::Sample).collect();
        let player = rodio::Player::connect_new(&mixer);
        player.append(rodio::buffer::SamplesBuffer::new(channels, rate, samples));
        player.set_speed(speed);
        player.play();
        Ok(Playing { msg_id: msg_id.to_string(), duration, player, speed: std::cell::Cell::new(speed) })
    }

    pub fn toggle(&self) {
        if self.player.is_paused() { self.player.play() } else { self.player.pause() }
    }
    pub fn is_paused(&self) -> bool {
        self.player.is_paused()
    }
    pub fn finished(&self) -> bool {
        self.player.empty()
    }
    pub fn position(&self) -> Duration {
        self.player.get_pos().min(self.duration)
    }
    pub fn seek(&self, fraction: f32) {
        // rodio scales seek targets by the playback speed, so pass the unscaled time
        let pos = self.duration.mul_f32(fraction.clamp(0.0, 0.999)).div_f32(self.speed.get().max(0.1));
        let _ = self.player.try_seek(pos);
    }
    pub fn set_speed(&self, speed: f32) {
        self.speed.set(speed);
        self.player.set_speed(speed);
    }
    pub fn stop(&self) {
        self.player.stop();
    }
}

pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}
