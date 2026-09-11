//! Testable WAV decode plus a dedicated one-shot PipeWire playback thread. Split from
//! `dbus::notifications`, see `dbus/notifications/mod.rs` for the module-level doc.

use std::path::{Path, PathBuf};

use pipewire as pw;

pub type SoundSender = std::sync::mpsc::Sender<PathBuf>;

#[derive(Debug, Clone, PartialEq)]
struct DecodedWav {
    channels: u32,
    sample_rate: u32,
    samples: Vec<i16>,
}

#[derive(Debug)]
enum SoundDecodeError {
    Wav(hound::Error),
    UnsupportedFormat { format: hound::SampleFormat, bits: u16 },
}

impl std::fmt::Display for SoundDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wav(err) => write!(f, "{err}"),
            Self::UnsupportedFormat { format, bits } => {
                write!(f, "unsupported WAV sample format {format:?} at {bits} bits per sample")
            }
        }
    }
}

impl std::error::Error for SoundDecodeError {}

impl From<hound::Error> for SoundDecodeError {
    fn from(err: hound::Error) -> Self {
        Self::Wav(err)
    }
}

/// ponytail: WAV only, with 16-bit integer or 32-bit float samples. `hound` is a small pure-Rust
/// decoder with no transitive bloat, enough for reference daemons' short UI sounds; defer
/// MP3/OGG/FLAC until a Lua config needs one. Other depths fail via
/// [`SoundDecodeError::UnsupportedFormat`].
fn decode_wav_samples(path: &Path) -> Result<DecodedWav, SoundDecodeError> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let samples: Vec<i16> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => reader.samples::<i16>().collect::<Result<_, _>>()?,
        (hound::SampleFormat::Float, 32) => reader
            .samples::<f32>()
            .map(|sample| sample.map(|value| (value.clamp(-1.0, 1.0) * i16::MAX as f32) as i16))
            .collect::<Result<_, _>>()?,
        (format, bits) => return Err(SoundDecodeError::UnsupportedFormat { format, bits }),
    };
    Ok(DecodedWav { channels: u32::from(spec.channels), sample_rate: spec.sample_rate, samples })
}

/// Decodes and plays each request through a fresh one-shot PipeWire stream (ADR-0033: no
/// cancellation handles or Lua/wire round trip). Blocks its caller; run it in a dedicated
/// `std::thread::spawn` because this `pw::stream::Stream` loop is `!Send`, as `audio::mixer::run`
/// established for the same `pipewire-rs` loop constraint. This is a different API surface:
/// `pw::stream::Stream` writes audio, while `pw::registry` listens for nodes.
///
/// ponytail: real PipeWire I/O is live-test-only, like idle's raw Wayland dispatch (ADR-0032); this
/// and [`play_one_wav`] have no unit tests. [`decode_wav_samples`] and [`should_play_sound`] are
/// the tested seams around it.
pub fn run_sound_player(requests: std::sync::mpsc::Receiver<PathBuf>) {
    while let Ok(path) = requests.recv() {
        if let Err(err) = play_one_wav(&path) {
            eprintln!("notifications: failed to play sound {path:?}: {err}");
        }
    }
}

struct PlaybackState {
    samples: Vec<i16>,
    position: usize,
    channels: u32,
    main_loop: pw::main_loop::MainLoopRc,
}

const CHAN_SIZE: usize = std::mem::size_of::<i16>();

fn play_one_wav(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let decoded = decode_wav_samples(path)?;
    if decoded.samples.is_empty() {
        return Ok(());
    }

    pw::init();
    let main_loop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "obelisk-notification-sound",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_ROLE => "Notification",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::AUDIO_CHANNELS => decoded.channels.to_string(),
        },
    )?;

    let playback = PlaybackState {
        samples: decoded.samples,
        position: 0,
        channels: decoded.channels,
        main_loop: main_loop.clone(),
    };

    let _listener = stream
        .add_local_listener_with_user_data(playback)
        .process(|stream, state| match stream.dequeue_buffer() {
            None => {}
            Some(mut buffer) => {
                let datas = buffer.datas_mut();
                let stride = CHAN_SIZE * state.channels.max(1) as usize;
                let data = &mut datas[0];
                let n_frames = if let Some(slice) = data.data() {
                    let remaining_frames =
                        state.samples.len().saturating_sub(state.position) / state.channels.max(1) as usize;
                    let capacity_frames = slice.len() / stride;
                    let n_frames = remaining_frames.min(capacity_frames);
                    for i in 0..n_frames {
                        for c in 0..state.channels as usize {
                            let sample = state.samples[state.position + i * state.channels as usize + c];
                            let start = i * stride + c * CHAN_SIZE;
                            let end = start + CHAN_SIZE;
                            slice[start..end].copy_from_slice(&i16::to_le_bytes(sample));
                        }
                    }
                    state.position += n_frames * state.channels as usize;
                    n_frames
                } else {
                    0
                };
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as _;
                *chunk.size_mut() = (stride * n_frames) as _;
                if state.position >= state.samples.len() {
                    state.main_loop.quit();
                }
            }
        })
        .register()?;

    let mut audio_info = pw::spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(pw::spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(decoded.sample_rate);
    audio_info.set_channels(decoded.channels);
    let mut position = [0; pw::spa::param::audio::MAX_CHANNELS];
    for (i, slot) in position.iter_mut().take(decoded.channels as usize).enumerate() {
        *slot = match i {
            0 => pw::spa::sys::SPA_AUDIO_CHANNEL_FL,
            1 => pw::spa::sys::SPA_AUDIO_CHANNEL_FR,
            _ => pw::spa::sys::SPA_AUDIO_CHANNEL_UNKNOWN,
        };
    }
    audio_info.set_position(position);

    let values = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: pw::spa::sys::SPA_TYPE_OBJECT_Format,
            id: pw::spa::sys::SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    )?
    .0
    .into_inner();
    let mut params = [pw::spa::pod::Pod::from_bytes(&values).ok_or("failed to build the audio format pod")?];

    stream.connect(
        pw::spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    main_loop.run();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_wav(path: &Path, spec: hound::WavSpec, samples: &[i16]) {
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for &sample in samples {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    #[test]
    fn decode_wav_samples_round_trips_16_bit_int_pcm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sound.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let samples = [100i16, -100, 200, -200];
        write_test_wav(&path, spec, &samples);

        let decoded = decode_wav_samples(&path).expect("must decode a real 16-bit WAV file");
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.sample_rate, 44100);
        assert_eq!(decoded.samples, samples);
    }

    #[test]
    fn decode_wav_samples_decodes_32_bit_float_pcm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sound.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 22050,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.write_sample(0.5f32).unwrap();
        writer.write_sample(-0.5f32).unwrap();
        writer.finalize().unwrap();

        let decoded = decode_wav_samples(&path).expect("must decode a real 32-bit float WAV file");
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.samples.len(), 2);
        assert!(
            decoded.samples[0] > 16000 && decoded.samples[0] < 17000,
            "0.5 should map close to i16::MAX/2, got {}",
            decoded.samples[0]
        );
    }

    #[test]
    fn decode_wav_samples_rejects_an_unsupported_bit_depth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sound.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 8,
            sample_format: hound::SampleFormat::Int,
        };
        write_test_wav(&path, spec, &[]);

        assert!(decode_wav_samples(&path).is_err());
    }
}
