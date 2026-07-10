//! `turtled play <bundle> [song]` — load a bundle and play a song to the device.
//!
//! Split like [`crate::rt`]: [`load_playable`] is the **portable** part (resolve
//! the song directory, load + validate, preload stems, build the mixer) and is
//! unit-tested on the dev Mac; [`run`] is the **`cfg(linux)`** part that opens
//! ALSA, spawns the audio thread, and actually makes sound.

use std::path::Path;

use turtle_core::{Show, Song};

use crate::mixer::Mixer;
use crate::stems;

/// Everything needed to start playback: the parsed show (for device/rate), the
/// mixer primed with the song's stems, and the song length in frames.
pub struct Playable {
    pub show: Show,
    pub mixer: Mixer,
    pub frames: u64,
}

/// Load a bundle and prepare one song for playback.
///
/// `song` selects a setlist entry's song-directory name; when `None`, the first
/// setlist entry is used. Errors are returned as `String` — this is top-level
/// glue reporting straight to the user, so a message is friendlier than a typed
/// error here. `map_err(|e| format!(...))` adds context to each failure.
pub fn load_playable(bundle: &Path, song: Option<&str>) -> Result<Playable, String> {
    let show = Show::load(bundle.join("show.toml")).map_err(|e| format!("show.toml: {e}"))?;
    show.validate().map_err(|e| format!("show invalid: {e}"))?;

    // Pick the song directory name: the caller's choice, else the first setlist
    // entry. `ok_or` turns the `None` (empty setlist) into an error.
    let song_name = match song {
        Some(name) => name.to_string(),
        None => show
            .setlist
            .first()
            .map(|entry| entry.song.clone())
            .ok_or("no song given and the setlist is empty")?,
    };

    let song_dir = bundle.join("songs").join(&song_name);
    let song = Song::load(song_dir.join("song.toml"))
        .map_err(|e| format!("song '{song_name}': {e}"))?;
    song.validate().map_err(|e| format!("song '{song_name}' invalid: {e}"))?;

    let rate = show.show.playback_rate;
    // Decode the stems into RAM (§4). Stem file paths in song.toml are relative
    // to the song directory.
    let preloaded =
        stems::load_song(&song, &song_dir, rate).map_err(|e| format!("stems: {e}"))?;
    let frames = preloaded.frames as u64;
    let mixer = Mixer::new(preloaded, rate);

    Ok(Playable { show, mixer, frames })
}

/// Open the device, spawn the audio RT thread, play the song, and stop. Linux
/// only (drives `AlsaAudio`).
#[cfg(target_os = "linux")]
pub fn run(bundle: &Path, song: Option<&str>) -> Result<(), String> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use crate::alsa_backend::AlsaAudio;
    use crate::clock::TransportClock;
    use crate::engine::{rt_channel, RtCommand};

    let Playable { show, mut mixer, frames } = load_playable(bundle, song)?;
    let rate = show.show.playback_rate;

    let audio = AlsaAudio::open(&show.audio.device, rate, show.audio.buffer_frames as usize)
        .map_err(|e| format!("open audio '{}': {e}", show.audio.device))?;
    let clock = TransportClock::new(rate);
    // Small SPSC command queue to the audio thread (§3). Producer stays here.
    let (mut tx, mut rx) = rt_channel(64);
    // Shared flag so this thread can ask the audio loop to exit.
    let running = AtomicBool::new(true);
    // Shared monotonic reference for the clock timestamps.
    let epoch = Instant::now();

    // Play the song through, plus a short tail so the last buffer flushes.
    let secs = frames as f64 / rate as f64 + 0.5;
    println!(
        "playing \"{}\" ({:.1}s) on {} ...",
        show.show.name, secs, show.audio.device
    );

    // `thread::scope` lets the spawned thread borrow these stack locals (`mixer`,
    // `audio`, ...) without `'static` — the scope guarantees the thread joins
    // before they drop. NOTE: v1 uses a normal-priority thread; the design's big
    // xrun-proof buffers (§3.1) make this fine. SCHED_FIFO is a later hardening.
    std::thread::scope(|s| {
        // `AlsaAudio` wraps a raw ALSA handle: it is `Send` (safe to hand to one
        // other thread) but `!Sync` (a `&` to it can't be shared across threads).
        // So MOVE `audio`/`mixer`/`rx` into the audio thread and borrow them
        // locally inside it. `clock` and `running` are atomic-backed (`Sync`), so
        // those we share by reference — the control thread flips `running` to
        // stop the loop, and a future MIDI thread will read `clock`. References
        // are `Copy`, so the `move` closure copies them and we keep ours.
        let clock = &clock;
        let running = &running;
        s.spawn(move || {
            rt::run_audio(&audio, &mut mixer, clock, &mut rx, epoch, running);
        });

        // Kick off playback, wait out the song, then signal the loop to stop.
        let _ = tx.push(RtCommand::Start);
        std::thread::sleep(Duration::from_secs_f64(secs));
        running.store(false, Ordering::Release);
        // Leaving the scope joins the audio thread.
    });

    println!("done.");
    Ok(())
}

// The `rt` path above is only referenced on Linux; import it there to avoid an
// unused-import warning on other hosts.
#[cfg(target_os = "linux")]
use crate::rt;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Write a tiny valid bundle (show + one song + one stereo int24 stem) so
    /// `load_playable` has something real to resolve and decode.
    fn write_bundle(frames: u32) -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("turtle-play-{}-{}", std::process::id(), n));
        let stems = dir.join("songs/opener/stems");
        std::fs::create_dir_all(&stems).unwrap();

        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(stems.join("pair1.wav"), spec).unwrap();
        for _ in 0..frames {
            w.write_sample(1000i32).unwrap(); // L
            w.write_sample(1000i32).unwrap(); // R
        }
        w.finalize().unwrap();

        std::fs::write(
            dir.join("show.toml"),
            "[show]\nname = \"B\"\nplayback_rate = 48000\n\
             [audio]\ndevice = \"hw:0\"\n\
             [[destinations]]\nname = \"lights\"\nport = \"CME:1\"\n\
             [control]\ninput_port = \"x\"\nselect_channel = 1\n\
             start = { type = \"note\", note = 60 }\n\
             stop = { type = \"note\", note = 61 }\n\
             next = { type = \"note\", note = 62 }\n\
             prev = { type = \"note\", note = 63 }\n\
             panic = { type = \"note\", note = 65 }\n\
             mute = { type = \"note\", notes = [72, 73, 74, 75] }\n\
             [[setlist]]\npc = 0\nsong = \"opener\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("songs/opener/song.toml"),
            format!("[song]\nname = \"O\"\nbpm = 120.0\nlength_samples = {frames}\n\
                     [[pairs]]\nindex = 0\nfile = \"stems/pair1.wav\"\n"),
        )
        .unwrap();
        dir
    }

    #[test]
    fn loads_first_setlist_song_by_default() {
        let dir = write_bundle(64);
        let p = load_playable(&dir, None).unwrap();
        assert_eq!(p.show.show.name, "B");
        assert_eq!(p.frames, 64);
        assert_eq!(p.mixer.position(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn errors_on_missing_song() {
        let dir = write_bundle(64);
        // `unwrap_err` would require `Playable: Debug`; match instead so we don't
        // have to derive Debug down the whole DSP chain.
        let err = match load_playable(&dir, Some("nope")) {
            Err(e) => e,
            Ok(_) => panic!("expected an error for a missing song"),
        };
        assert!(err.contains("nope"), "error should name the song: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
