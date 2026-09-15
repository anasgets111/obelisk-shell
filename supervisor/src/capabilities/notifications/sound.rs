//! Testable Ogg Vorbis decode plus a dedicated one-shot PipeWire playback thread. Split from
//! `dbus::notifications`, see `dbus/notifications/mod.rs` for the module-level doc.

use std::path::{Path, PathBuf};

use pipewire as pw;

use super::icon::validate_trusted_path;

/// One slot: a sound arriving mid-play waits, so a critical one is not lost; a burst plays at most two.
pub type SoundSender = std::sync::mpsc::SyncSender<PathBuf>;

struct DecodedSound {
    channels: u32,
    sample_rate: u32,
    samples: Vec<i16>,
}

const MAX_SOUND_FILE_BYTES: u64 = 4 << 20;
const MAX_SOUND_SECONDS: usize = 30;

/// Roots for `set_sound`, `sound-file` and `sound-name`, apart from the icon roots so `image-path`
/// never reaches `/opt`. Apps ship sounds under `/usr/share/<app>/`; the decode caps bound the rest.
pub(super) fn default_trusted_sound_roots() -> Vec<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    ["/usr/share", "/usr/local/share", "/opt"].into_iter().map(PathBuf::from).chain(data_home).collect()
}

/// A `sound-name` as the freedesktop theme's `<name>.oga`: the only theme installed, so no
/// `index.theme` walk. Any client sends it, so no `/` or leading `.` leaves the theme directory.
pub(super) fn resolve_sound_name(name: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    if name.contains('/') || name.starts_with('.') {
        return None;
    }
    roots.iter().find_map(|root| {
        validate_trusted_path(root.join(format!("sounds/freedesktop/stereo/{name}.oga")).to_str()?, roots)
    })
}

/// ponytail: Ogg Vorbis only, what the freedesktop theme ships; WAV, FLAC or Opus need another
/// decoder. Any client can name the file, so size, length, channels, rate and chaining are capped.
fn decode_sound(path: &Path) -> Result<DecodedSound, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > MAX_SOUND_FILE_BYTES {
        return Err("larger than 4 MiB".into());
    }
    let mut reader = lewton::inside_ogg::OggStreamReader::new(std::io::BufReader::new(file))?;
    let channels = u32::from(reader.ident_hdr.audio_channels);
    let sample_rate = reader.ident_hdr.audio_sample_rate;
    if channels > 2 || !(8_000..=192_000).contains(&sample_rate) {
        return Err(format!("unsupported {channels} channels at {sample_rate} Hz").into());
    }
    // lewton silently rereads headers for a chained stream, which can change channels mid-file.
    let serial = reader.stream_serial();
    let mut samples = Vec::new();
    while let Some(packet) = reader.read_dec_packet_itl()? {
        if reader.stream_serial() != serial {
            return Err("chained Ogg stream".into());
        }
        samples.extend(packet);
        if samples.len() > MAX_SOUND_SECONDS * sample_rate as usize * channels as usize {
            return Err("longer than 30 seconds".into());
        }
    }
    Ok(DecodedSound { channels, sample_rate, samples })
}

/// Decodes and plays each request through a fresh one-shot PipeWire stream (ADR-0033: no
/// cancellation handles or Lua/wire round trip). Blocks its caller; run it in a dedicated
/// `std::thread::spawn` because this `pw::stream::Stream` loop is `!Send`, as `audio::mixer::run`
/// established for the same `pipewire-rs` loop constraint. This is a different API surface:
/// `pw::stream::Stream` writes audio, while `pw::registry` listens for nodes.
///
/// ponytail: real PipeWire I/O is live-test-only, like idle's raw Wayland dispatch (ADR-0032); this
/// and [`play_one`] have no unit tests. [`decode_sound`] and [`should_play_sound`] are
/// the tested seams around it.
pub fn run_sound_player(requests: std::sync::mpsc::Receiver<PathBuf>) {
    while let Ok(path) = requests.recv() {
        if let Err(err) = play_one(&path) {
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

fn play_one(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let decoded = decode_sound(path)?;
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

    let length = std::time::Duration::from_secs_f64(
        decoded.samples.len() as f64 / f64::from(decoded.sample_rate * decoded.channels),
    );
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
                // Indexed, not `[0]`: this is another process's buffer, and a release build turns
                // an out-of-bounds panic into the whole shell aborting. Quit rather than skip the
                // turn, because `run_sound_player` plays one file at a time and a loop that never
                // ends takes every later notification sound with it. Silent because this runs on
                // the `RT_PROCESS` data thread, where `eprintln!` would both block on stderr and
                // panic if the write failed -- the abort this branch exists to avoid.
                let Some(data) = datas.first_mut() else {
                    state.main_loop.quit();
                    return;
                };
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
                // Quitting here dropped what PipeWire had not played yet: all of a 0.14s bell. Queue
                // the last buffer, then drain; `drained` quits on the main loop once it is heard.
                drop(buffer);
                if n_frames > 0 && state.position >= state.samples.len() {
                    let _ = stream.flush(true);
                }
            }
        })
        .drained(|_, state| state.main_loop.quit())
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

    // A stream that never plays (no sink, a stream error, unmapped buffers) never drains.
    let deadline = main_loop.loop_().add_timer({
        let main_loop = main_loop.clone();
        move |_| main_loop.quit()
    });
    deadline.update_timer(Some(length + std::time::Duration::from_secs(2)), None).into_sync_result()?;
    main_loop.run();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_sound_reads_the_system_sound_theme() {
        let decoded = decode_sound(Path::new("/usr/share/sounds/freedesktop/stereo/message-new-instant.oga"))
            .expect("must decode the freedesktop theme");
        assert!(decoded.channels <= 2 && !decoded.samples.is_empty());
    }

    #[test]
    fn decode_sound_refuses_a_file_over_the_size_cap_before_decoding() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(MAX_SOUND_FILE_BYTES + 1).unwrap();
        assert_eq!(decode_sound(file.path()).err().map(|err| err.to_string()), Some("larger than 4 MiB".into()));
    }

    #[test]
    fn resolve_sound_name_stays_inside_the_theme_directory() {
        let root = tempfile::tempdir().unwrap();
        let stereo = root.path().join("sounds/freedesktop/stereo");
        std::fs::create_dir_all(&stereo).unwrap();
        std::fs::write(stereo.join("bell.oga"), b"").unwrap();
        std::fs::write(root.path().join("other.oga"), b"").unwrap();
        let roots = [root.path().to_path_buf()];
        assert_eq!(resolve_sound_name("bell", &roots), Some(stereo.join("bell.oga").canonicalize().unwrap()));
        assert_eq!(resolve_sound_name("../../../other", &roots), None, "a trusted root is not the theme");
    }
}
