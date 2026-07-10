//! `gen-tone` — write a minimal, playable Turtle bundle (spec §7).
//!
//! There is no real bundle checked into the repo, and the Ableton converter is a
//! separate path, so this produces a self-contained test bundle: a show with one
//! song whose single stereo pair is a sine tone. It is enough to exercise the
//! whole offline audio chain end to end — loader → mixer → RT loop → device — and
//! actually *hear* output on the Pi.
//!
//! Layout produced (matches §7):
//!
//! ```text
//! <out>/
//!   show.toml
//!   songs/tone/
//!     song.toml
//!     stems/pair1.wav      # stereo int24 @ 48 kHz
//! ```

use std::error::Error;
use std::f64::consts::TAU;
use std::path::Path;

/// The show's fixed playback rate (§4); the stem is written at the same rate so
/// the engine never has to resample.
const SAMPLE_RATE: u32 = 48_000;
/// Keep the tone well below full scale — easy on the ears and the limiter.
const AMPLITUDE: f64 = 0.3;
/// The song's nominal tempo; the test MIDI track pulses one note per beat.
const BPM: f64 = 120.0;

/// Generate the bundle at `out`. `seconds` sets the song length; `hz` the pitch.
///
/// Returns `Box<dyn Error>` so the two unrelated failure kinds here — filesystem
/// (`std::io::Error`) and WAV encoding (`hound::Error`) — can both flow through
/// the same `?` without a bespoke error enum. Fine for a CLI helper.
pub fn gen_tone(out: &Path, seconds: f64, hz: f64) -> Result<(), Box<dyn Error>> {
    let frames = (seconds * SAMPLE_RATE as f64) as u64;

    let song_dir = out.join("songs").join("tone");
    let stems_dir = song_dir.join("stems");
    let midi_dir = song_dir.join("midi");
    // `create_dir_all` makes every missing parent, like `mkdir -p`.
    std::fs::create_dir_all(&stems_dir)?;
    std::fs::create_dir_all(&midi_dir)?;

    write_sine(&stems_dir.join("pair1.wav"), frames, hz)?;
    // One MIDI note per beat, for the `lights` destination in show.toml. This is
    // what the §5 scheduler dispatches in sync with the audio.
    let beats = (seconds * BPM / 60.0) as u32;
    write_test_smf(&midi_dir.join("lights.mid"), beats)?;

    std::fs::write(out.join("show.toml"), SHOW_TOML)?;
    std::fs::write(song_dir.join("song.toml"), song_toml(frames))?;

    Ok(())
}

/// Write a Standard MIDI File that pulses note 36 once per beat: note-on at the
/// beat, note-off half a beat later. Used as a per-destination test track.
fn write_test_smf(path: &Path, beats: u32) -> Result<(), Box<dyn Error>> {
    use midly::num::{u15, u24, u28, u4, u7};
    use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};

    // Pulses-per-quarter-note: the tick resolution. Half a beat = PPQ/2 ticks.
    const PPQ: u16 = 480;
    let note_off = |key, vel| MidiMessage::NoteOff { key: u7::new(key), vel: u7::new(vel) };

    let mut track = Vec::new();
    // Set 120 BPM so the compiler's tick→sample math matches BPM above.
    track.push(TrackEvent {
        delta: u28::new(0),
        kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::new(500_000))), // µs per quarter
    });
    for beat in 0..beats {
        // First note-on lands at tick 0; each subsequent one PPQ/2 after the
        // previous note-off, i.e. one full beat apart.
        let on_delta = if beat == 0 { 0 } else { PPQ as u32 / 2 };
        track.push(TrackEvent {
            delta: u28::new(on_delta),
            kind: TrackEventKind::Midi {
                channel: u4::new(0),
                message: MidiMessage::NoteOn { key: u7::new(36), vel: u7::new(100) },
            },
        });
        track.push(TrackEvent {
            delta: u28::new(PPQ as u32 / 2),
            kind: TrackEventKind::Midi { channel: u4::new(0), message: note_off(36, 0) },
        });
    }
    track.push(TrackEvent { delta: u28::new(0), kind: TrackEventKind::Meta(MetaMessage::EndOfTrack) });

    let smf = Smf {
        header: Header { format: Format::SingleTrack, timing: Timing::Metrical(u15::new(PPQ)) },
        tracks: vec![track],
    };
    let mut buf = Vec::new();
    smf.write_std(&mut buf)?;
    std::fs::write(path, buf)?;
    Ok(())
}

/// Write a stereo int24 WAV of a sine wave.
fn write_sine(path: &Path, frames: u64, hz: f64) -> Result<(), hound::Error> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    // Full-scale magnitude for a signed 24-bit sample.
    let full_scale = (1i32 << 23) as f64;
    for i in 0..frames {
        // Compute the phase in f64 to avoid the drift a running f32 accumulator
        // would pick up over hundreds of thousands of samples.
        let t = i as f64 / SAMPLE_RATE as f64;
        let sample = (AMPLITUDE * (TAU * hz * t).sin() * full_scale) as i32;
        // Same signal to both channels (centre-panned mono).
        w.write_sample(sample)?; // L
        w.write_sample(sample)?; // R
    }
    // `finalize` back-fills the WAV header lengths; dropping without it corrupts.
    w.finalize()
}

/// A minimal valid `show.toml`: one destination, a full control map, and a
/// setlist entry (PC 0) pointing at the `tone` song directory.
///
/// The `[audio] device` and `[control] input_port` here are the **dev rig's**
/// ALSA names (`hw:L6` USB interface, `hw:4,0,0` MIDI controller) and the
/// controller's actual `start`/`stop` note numbers, so `gen-tone` output is
/// playable on that Pi without editing. Change these for a different setup —
/// `aplay -l` lists audio cards, `amidi -l` lists MIDI ports.
const SHOW_TOML: &str = r#"[show]
name = "Tone Test"
playback_rate = 48000

[audio]
device = "hw:L6"
buffer_frames = 1024

[[destinations]]
name = "lights"
port = "CME:1"

[control]
input_port = "hw:4,0,0"
select_channel = 2
start = { type = "note", note = 14 }
stop  = { type = "note", note = 15 }
next  = { type = "note", note = 62 }
prev  = { type = "note", note = 63 }
panic = { type = "note", note = 65 }
mute  = { type = "note", notes = [72, 73, 74, 75] }

[[setlist]]
pc = 0
song = "tone"
"#;

/// The song manifest, with `length_samples` filled in from the generated stem.
fn song_toml(frames: u64) -> String {
    format!(
        r#"[song]
name = "Tone"
bpm = 120.0
length_samples = {frames}

[[pairs]]
index = 0
file = "stems/pair1.wav"
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("turtle-gen-{}-{}-{}", std::process::id(), tag, n))
    }

    #[test]
    fn generates_a_valid_bundle() {
        let out = temp_dir("bundle");
        gen_tone(&out, 0.5, 440.0).unwrap();

        // The show must load and pass semantic validation.
        let show = turtle_core::Show::load(out.join("show.toml")).unwrap();
        show.validate().unwrap();

        // The song must load and validate, and its length must be 0.5 s of frames.
        let song = turtle_core::Song::load(out.join("songs/tone/song.toml")).unwrap();
        song.validate().unwrap();
        assert_eq!(song.song.length_samples, 24_000);

        // The stem must be the stereo int24 @ 48k we asked for.
        let reader = hound::WavReader::open(out.join("songs/tone/stems/pair1.wav")).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.bits_per_sample, 24);
        assert_eq!(reader.len(), 24_000 * 2, "frames * channels");

        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn generates_a_dispatchable_midi_track() {
        let out = temp_dir("midi");
        gen_tone(&out, 2.0, 440.0).unwrap(); // 2 s @ 120 BPM = 4 beats

        // The generated SMF must compile via the same timeline the daemon uses.
        let bytes = std::fs::read(out.join("songs/tone/midi/lights.mid")).unwrap();
        let tl = turtle_core::Timeline::compile_smf(&bytes, 48_000).unwrap();

        // 4 beats * (note-on + note-off) = 8 events, sorted by sample time.
        assert_eq!(tl.events.len(), 8);
        assert_eq!(tl.events[0].sample_time, 0); // first note-on at the top
        assert_eq!(tl.events[1].sample_time, 12_000); // note-off 0.25 s later
        assert_eq!(tl.events[2].sample_time, 24_000); // next beat at 0.5 s
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn tone_is_audible_not_silence() {
        let out = temp_dir("audible");
        gen_tone(&out, 0.1, 440.0).unwrap();
        let mut reader = hound::WavReader::open(out.join("songs/tone/stems/pair1.wav")).unwrap();
        // At least one sample must be non-zero (a real waveform, not silence).
        let any_nonzero = reader.samples::<i32>().any(|s| s.unwrap() != 0);
        assert!(any_nonzero, "generated stem should not be silent");
        std::fs::remove_dir_all(&out).ok();
    }
}
