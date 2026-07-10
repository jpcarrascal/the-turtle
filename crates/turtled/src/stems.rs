//! Stem preloading: decode a song's WAV stems fully into RAM (spec §4).
//!
//! When a song is armed, the loader thread (§3) decodes every stereo pair into
//! RAM so that nothing touches storage during playback. This module is the
//! decode step; it is **host-independent** (`hound` is pure Rust), so unlike the
//! ALSA backend it builds and is unit-tested on the dev Mac.
//!
//! Format contract (§4): the converter emits **WAV int PCM** (int24 in v1) at
//! the show's fixed playback rate, one stereo file per pair. We decode those
//! integer samples to normalised `f32` in roughly `[-1.0, 1.0)` — the format the
//! `turtle-dsp` chain (§6) works in. The master stage converts back to `i32` for
//! `AlsaAudio::write_period` much later, so all mixing/DSP stays in float.

use std::path::Path;

use turtle_core::Song;

/// One decoded stereo pair, held as interleaved `f32` samples: `L, R, L, R, …`.
#[derive(Debug)]
pub struct StemPair {
    /// Pair slot 0..=3 (matches `song.toml`'s `[[pairs]] index`).
    pub index: u8,
    /// Interleaved stereo samples; `samples.len() == frames * 2`.
    pub samples: Vec<f32>,
    /// Number of stereo frames (one frame = one L+R sample).
    pub frames: usize,
}

/// A song decoded into RAM: all its pairs, ready for the RT mixer to read.
#[derive(Debug)]
pub struct PreloadedSong {
    pub name: String,
    pub sample_rate: u32,
    /// Longest pair length in frames; shorter pairs are zero-padded by the mixer.
    pub frames: usize,
    pub pairs: Vec<StemPair>,
}

/// Why a stem failed to load. `thiserror` derives the `Display`/`Error` impls
/// from the `#[error("…")]` templates — the `{field}` placeholders read the
/// struct-variant fields by name, so each message carries the offending path.
#[derive(Debug, thiserror::Error)]
pub enum StemError {
    /// `#[from]`-style wrapping done by hand so we can attach the path. `source`
    /// keeps the underlying `hound::Error` in the error chain for `{source}`.
    #[error("stem {path}: {source}")]
    Io { path: String, source: hound::Error },

    #[error("stem {path}: expected stereo (2 channels), found {channels}")]
    NotStereo { path: String, channels: u16 },

    #[error("stem {path}: sample rate {found} Hz != show rate {expected} Hz")]
    RateMismatch { path: String, found: u32, expected: u32 },

    #[error("stem {path}: float WAV unsupported; the converter emits int PCM (§4)")]
    NotInt { path: String },
}

/// Decode every pair of `song`, resolving each `file` relative to `base_dir`
/// (the bundle's song directory). `expected_rate` is the show's fixed playback
/// rate; a pair recorded at any other rate is rejected rather than resampled,
/// because the engine never resamples (§4).
pub fn load_song(
    song: &Song,
    base_dir: &Path,
    expected_rate: u32,
) -> Result<PreloadedSong, StemError> {
    // Preallocate to the known pair count so the push loop never reallocates.
    let mut pairs = Vec::with_capacity(song.pairs.len());
    let mut frames = 0usize;
    for pair in &song.pairs {
        // `Path::join` handles the relative `file` path portably.
        let decoded = load_pair(pair.index, &base_dir.join(&pair.file), expected_rate)?;
        // `max` keeps the running longest length so the mixer knows how far the
        // song runs even when one pair is shorter than another.
        frames = frames.max(decoded.frames);
        pairs.push(decoded);
    }
    Ok(PreloadedSong {
        name: song.song.name.clone(),
        sample_rate: expected_rate,
        frames,
        pairs,
    })
}

/// Decode a single stereo WAV file into a [`StemPair`].
fn load_pair(index: u8, path: &Path, expected_rate: u32) -> Result<StemPair, StemError> {
    // `path.display()` yields something printable even for non-UTF-8 paths; we
    // snapshot it as an owned `String` up front so each error arm can name the
    // file without re-borrowing `path`.
    let name = path.display().to_string();

    // A tiny closure turns any `hound::Error` into our path-tagged `Io` variant.
    // `move` is not needed — it borrows `name` immutably, and `.clone()` inside
    // keeps `name` available for later error arms.
    let io_err = |source| StemError::Io { path: name.clone(), source };

    let mut reader = hound::WavReader::open(path).map_err(io_err)?;
    let spec = reader.spec();

    // Validate the format contract before decoding a single sample.
    if spec.channels != 2 {
        return Err(StemError::NotStereo { path: name, channels: spec.channels });
    }
    if spec.sample_rate != expected_rate {
        return Err(StemError::RateMismatch {
            path: name,
            found: spec.sample_rate,
            expected: expected_rate,
        });
    }
    if spec.sample_format != hound::SampleFormat::Int {
        return Err(StemError::NotInt { path: name });
    }

    // Normalisation scale: full-scale for an N-bit signed sample is 2^(N-1), so
    // dividing by it maps the integer range onto ~[-1.0, 1.0). Computed once,
    // not per sample. `1i64 << 23` for int24 = 8_388_608.
    let scale = 1.0f32 / (1i64 << (spec.bits_per_sample - 1)) as f32;

    // One heap allocation for the whole stem: `len()` is the total sample count
    // across both channels, so the `Vec` never grows during decode (RT-preload).
    let mut samples = Vec::with_capacity(reader.len() as usize);
    // `samples::<i32>()` yields each integer sample sign-extended into an `i32`.
    // The `?` inside the loop propagates a mid-file decode error, tagged with the
    // path via `io_err`.
    for sample in reader.samples::<i32>() {
        samples.push(sample.map_err(io_err)? as f32 * scale);
    }

    let frames = samples.len() / 2;
    Ok(StemPair { index, samples, frames })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    // Unique temp path per test so parallel runs don't collide on the filesystem.
    fn temp_wav(tag: &str) -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("turtle-stem-{}-{}-{}.wav", std::process::id(), tag, n))
    }

    /// Write an interleaved integer WAV so the loader has something real to read.
    fn write_wav(path: &Path, channels: u16, rate: u32, bits: u16, samples: &[i32]) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: bits,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for &s in samples {
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    /// Build a one-pair `Song` pointing at `file` (relative to the base dir).
    fn song_with_pair(file: &str) -> Song {
        let toml = format!(
            r#"
[song]
name = "t"
bpm = 120.0
length_samples = 4
[[pairs]]
index = 0
file = "{file}"
"#
        );
        Song::from_toml_str(&toml).unwrap()
    }

    #[test]
    fn decodes_and_normalises_int24() {
        let path = temp_wav("ok");
        // Two stereo frames: full-scale +/- and zero. 2^23 = 8_388_608.
        write_wav(&path, 2, 48_000, 24, &[8_388_607, -8_388_608, 0, 0]);

        let song = song_with_pair(path.file_name().unwrap().to_str().unwrap());
        let loaded = load_song(&song, path.parent().unwrap(), 48_000).unwrap();

        assert_eq!(loaded.frames, 2);
        assert_eq!(loaded.pairs.len(), 1);
        let s = &loaded.pairs[0].samples;
        assert_eq!(loaded.pairs[0].frames, 2);
        // ~ +1.0 and ~ -1.0 at full scale; exact 0.0 for silence.
        assert!((s[0] - 1.0).abs() < 1e-4, "got {}", s[0]);
        assert!((s[1] + 1.0).abs() < 1e-4, "got {}", s[1]);
        assert_eq!(s[2], 0.0);
        assert_eq!(s[3], 0.0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_mono_stem() {
        let path = temp_wav("mono");
        write_wav(&path, 1, 48_000, 24, &[0, 0]);
        let song = song_with_pair(path.file_name().unwrap().to_str().unwrap());
        let err = load_song(&song, path.parent().unwrap(), 48_000).unwrap_err();
        assert!(matches!(err, StemError::NotStereo { channels: 1, .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_rate_mismatch() {
        let path = temp_wav("rate");
        write_wav(&path, 2, 44_100, 24, &[0, 0]);
        let song = song_with_pair(path.file_name().unwrap().to_str().unwrap());
        let err = load_song(&song, path.parent().unwrap(), 48_000).unwrap_err();
        assert!(matches!(err, StemError::RateMismatch { found: 44_100, expected: 48_000, .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn song_frames_is_longest_pair() {
        // Pair 0: 3 frames; pair 1: 1 frame. Song length is the max (3).
        let p0 = temp_wav("p0");
        let p1 = temp_wav("p1");
        write_wav(&p0, 2, 48_000, 24, &[0, 0, 0, 0, 0, 0]);
        write_wav(&p1, 2, 48_000, 24, &[0, 0]);
        let toml = format!(
            r#"
[song]
name = "t"
bpm = 120.0
length_samples = 3
[[pairs]]
index = 0
file = "{}"
[[pairs]]
index = 1
file = "{}"
"#,
            p0.file_name().unwrap().to_str().unwrap(),
            p1.file_name().unwrap().to_str().unwrap(),
        );
        let song = Song::from_toml_str(&toml).unwrap();
        let loaded = load_song(&song, p0.parent().unwrap(), 48_000).unwrap();
        assert_eq!(loaded.frames, 3);
        assert_eq!(loaded.pairs[0].frames, 3);
        assert_eq!(loaded.pairs[1].frames, 1);
        let _ = std::fs::remove_file(&p0);
        let _ = std::fs::remove_file(&p1);
    }
}
