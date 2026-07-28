//! `turtled play <bundle> [song]` — load a bundle and play a song to the device.
//!
//! Split like [`crate::rt`]: [`load_playable`] is the **portable** part (resolve
//! the song directory, load + validate, preload stems, build the mixer) and is
//! unit-tested on the dev Mac; [`run`] is the **`cfg(linux)`** part that opens
//! ALSA, spawns the audio thread, and actually makes sound.

use std::path::{Path, PathBuf};

use turtle_core::model::FilterKind;
use turtle_core::{Show, Song, Timeline};
use turtle_dsp::FilterType;

use crate::mixer::Mixer;
use crate::scheduler::PortScheduler;
use crate::stems;

/// Everything needed to start playback: the parsed show (for device/rate), the
/// mixer primed with the song's stems, the song length in frames, and the song
/// directory (so the MIDI scheduler can find the per-destination SMFs).
pub struct Playable {
    pub show: Show,
    pub mixer: Mixer,
    pub frames: u64,
    pub song_dir: PathBuf,
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
    let mut mixer = Mixer::new(preloaded, rate);
    // The song's `[delay]` layered over the show's, field by field (§6). This runs
    // on every load — including the background load behind a song switch — which is
    // what makes per-song settings work with no extra plumbing.
    mixer.apply_delay_defaults(&song.delay.applied_to(show.delay));

    // Apply each pair's fixed filter topology from song.toml's `[dsp.pairN]`
    // (§6); live CC then drives cutoff/resonance within that topology. Keys
    // that don't match `pair{N}` (or name a filter-less/absent entry) are
    // left at the mixer's default (transparent lowpass).
    for (key, pair_dsp) in &song.dsp {
        let Some(idx) = key
            .strip_prefix("pair")
            .and_then(|n| n.parse::<usize>().ok())
        else {
            continue;
        };
        if let Some(filter) = pair_dsp.filter {
            mixer.set_filter_type(idx, to_dsp_filter(filter));
        }
    }

    Ok(Playable { show, mixer, frames, song_dir })
}

/// `turtle_core::model::FilterKind` (song.toml's `filter = "lp"`) ->
/// `turtle_dsp::FilterType` (the mixer's live-DSP currency). Two enums,
/// kept distinct so `turtle-dsp` stays free of the show/song data model.
fn to_dsp_filter(kind: FilterKind) -> FilterType {
    match kind {
        FilterKind::Lp => FilterType::Lowpass,
        FilterKind::Hp => FilterType::Highpass,
        FilterKind::Bp => FilterType::Bandpass,
    }
}

/// Build one [`PortScheduler`] per destination from its SMF (spec §5).
///
/// Each destination `d` reads `<song_dir>/midi/<d.name>.mid` (the bundle layout
/// in §7). A missing file — or one that fails to compile — yields an empty
/// scheduler, so a destination without MIDI is simply silent rather than fatal.
/// The returned Vec is indexed by destination order, matching the MIDI sink's
/// port numbering.
pub fn load_schedulers(show: &Show, song_dir: &Path, rate: u32) -> Vec<PortScheduler> {
    show.destinations
        .iter()
        .map(|dest| {
            let path = song_dir.join("midi").join(format!("{}.mid", dest.name));
            let events = std::fs::read(&path)
                .ok()
                .and_then(|bytes| Timeline::compile_smf(&bytes, rate).ok())
                .map(|tl| tl.events)
                .unwrap_or_default();
            PortScheduler::new(events)
        })
        .collect()
}

/// The offset-adjusted position to dispatch a destination against (spec §5/§9):
/// events fire when `pos >= sample_time + offset`. A **negative** `offset_ms`
/// makes events fire *earlier* (to lead a destination with downstream latency); a
/// positive one *later*.
///
/// Returns `None` when the adjusted position is still before the song start —
/// i.e. we're within a positive offset of `pos = 0`, so nothing is due yet. That
/// is what correctly delays even the `t = 0` event by the offset instead of
/// firing it immediately (which clamping to 0 would, e.g. on restart).
pub fn dispatch_pos(pos: u64, offset_ms: f64, rate: u32) -> Option<u64> {
    let offset_samples = (offset_ms / 1000.0 * rate as f64).round() as i64;
    let adjusted = pos as i64 - offset_samples;
    (adjusted >= 0).then_some(adjusted as u64)
}

/// Open the device, spawn the audio RT thread, play the song, and stop. Linux
/// only (drives `AlsaAudio`).
#[cfg(target_os = "linux")]
pub fn run(
    bundle: &Path,
    song: Option<&str>,
    verbose: bool,
    tuning: crate::sched::Tuning,
    wait_devices: std::time::Duration,
) -> Result<(), String> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use crate::alsa_backend::{AlsaAudio, AlsaMidi};
    use crate::backend::MidiSink;
    use crate::clock::TransportClock;
    use crate::engine::{rt_channel, rt_event_channel, RtCommand};
    use crate::mixer::song_channel;

    // Before the stems are allocated, so `MCL_FUTURE` covers them (§12). Tied to
    // the same switch as thread priority, so `--rt-prio 0` is a fully untuned
    // baseline to A/B against.
    crate::sched::lock_memory_or_warn(tuning.rt_enabled());

    let Playable { show, mut mixer, frames, song_dir } = load_playable(bundle, song)?;
    let rate = show.show.playback_rate;

    // Same bounded wait as the control path (§12): a device that is a second late
    // should not be a failure.
    let audio = crate::retry::open_with_retry(
        &format!("audio device '{}'", show.audio.device),
        wait_devices,
        || AlsaAudio::open(&show.audio.device, rate, show.audio.buffer_frames as usize),
        |msg| println!("{msg}"),
    )
    .map_err(|e| format!("open audio '{}': {e}", show.audio.device))?;

    // MIDI output, best-effort: destinations whose port can't be opened are just
    // logged, so a bad/placeholder MIDI port never blocks audio playback.
    // Logical labels (`"CME:1"`) resolve to ALSA addresses here; raw addresses
    // pass through unchanged (§5/§7.1).
    let midi_names: Vec<String> = show
        .resolved_destination_ports()
        .map_err(|e| format!("destination port: {e}"))?;
    let (mut midi, failed) = AlsaMidi::open(&midi_names);
    for name in &failed {
        eprintln!("warning: MIDI out '{name}' unavailable; its events will be logged only");
    }
    // One scheduler per destination, compiled from the song's per-destination SMF.
    let mut schedulers = load_schedulers(&show, &song_dir, rate);
    // Total dispatch offset per destination (§9): the global audio-output latency
    // plus this destination's own trim, indexed like the schedulers.
    let dest_offsets: Vec<f64> = show
        .destinations
        .iter()
        .map(|d| show.audio.output_latency_ms + d.offset_ms)
        .collect();

    let clock = TransportClock::new(rate);
    // Small SPSC command queue to the audio thread (§3). Producer stays here.
    let (mut tx, mut rx) = rt_channel(64);
    // This one-shot path never swaps songs or needs `EndReached`, but
    // `run_audio` still expects both channel halves — give it unread ones.
    let (_song_tx, mut song_rx) = song_channel(2);
    let (mut events_tx, mut events_rx) = rt_event_channel(8);
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
    // `play` is a fixed-duration dev tool, so it stops after one pass even for a
    // looping song. Saying so avoids reading that as the loop not working; the
    // loop is exercised for real by `turtled control`.
    if mixer.is_looping() {
        println!("  (song loops; `play` still stops after one pass — use `control` to hear it repeat)");
    }

    // `thread::scope` lets the spawned thread borrow these stack locals (`mixer`,
    // `audio`, ...) without `'static` — the scope guarantees the thread joins
    // before they drop. NOTE: v1 uses a normal-priority thread; the design's big
    // xrun-proof buffers (§3.1) make this fine. SCHED_FIFO is a later hardening.
    let outcome: Option<std::io::Error> = std::thread::scope(|s| {
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
            // Same as the control path: claim SCHED_FIFO on the RT thread
            // before rendering anything (§3). Best-effort by design.
            crate::sched::apply_or_warn("audio", tuning.rt_priority);
            crate::sched::pin_or_warn("audio", tuning.audio_cpu);
            rt::run_audio(
                &audio,
                &mut mixer,
                clock,
                &mut rx,
                &mut song_rx,
                &mut events_tx,
                epoch,
                running,
            );
        });

        // This thread is the MIDI scheduler, so it takes RT priority too — one
        // step below audio, matching the control path (§3).
        crate::sched::apply_or_warn("midi", tuning.midi_priority());

        // Kick off playback, then run the MIDI scheduler on this thread (§5):
        // every ~1 ms, interpolate the transport position from the clock the
        // audio thread publishes, and dispatch each destination's due events.
        let _ = tx.push(RtCommand::Start);
        let started = Instant::now();
        // `running` is in the condition so a dead audio thread ends playback
        // instead of ticking out the full duration and then printing "done."
        let mut audio_error = None;
        while started.elapsed() < Duration::from_secs_f64(secs)
            && running.load(Ordering::Acquire)
        {
            while let Ok(event) = events_rx.pop() {
                if let crate::engine::RtEvent::AudioFailed { errno } = event {
                    audio_error = Some(std::io::Error::from_raw_os_error(errno));
                }
            }
            let wall_s = epoch.elapsed().as_secs_f64();
            let pos = clock.interpolate(epoch.elapsed().as_nanos() as u64);
            for (port, sched) in schedulers.iter_mut().enumerate() {
                // Each destination dispatches against its own offset-adjusted pos;
                // `None` = still within the offset of the start, nothing due yet.
                let Some(pos_adj) = dispatch_pos(pos, dest_offsets[port], rate) else { continue };
                for ev in sched.drain_due(pos_adj) {
                    let bytes = ev.message.as_bytes();
                    midi.send(port, bytes);
                    // `wall` is the actual elapsed time since playback armed — used
                    // to diagnose startup timing (compare against the beat grid).
                    // Gated: one line per event floods stdout in a dense show.
                    if verbose {
                        println!(
                            "  midi transport={:.3}s wall={wall_s:.3}s port{port} {bytes:02X?}",
                            pos_adj as f64 / rate as f64
                        );
                    }
                }
            }
            // ~1 ms tick: fine MIDI granularity, decoupled from the audio buffer.
            std::thread::sleep(Duration::from_millis(1));
        }
        running.store(false, Ordering::Release);
        // Drain once more after the loop. The loop's own condition watches
        // `running`, which the audio thread clears *as* it reports the failure — so
        // exiting on that flag skips the drain inside the body and the event would
        // be missed entirely. (It was: this printed "done." and exited 0.)
        while let Ok(event) = events_rx.pop() {
            if let crate::engine::RtEvent::AudioFailed { errno } = event {
                audio_error = Some(std::io::Error::from_raw_os_error(errno));
            }
        }
        // Leaving the scope joins the audio thread.
        audio_error
    });

    if let Some(err) = outcome {
        return Err(format!("audio device '{}' failed: {err}", show.audio.device));
    }
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
             [ports]\nCME = \"H4MIDIWC\"\n\n[[destinations]]\nname = \"lights\"\nport = \"CME:1\"\n\
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

    #[test]
    fn song_toml_filter_topology_reaches_the_mixer() {
        use crate::control_map::DspParam;

        let frames = 10_000;
        let dir = write_bundle(frames);
        // Override the song with a `[dsp.pair0] filter = "hp"` topology.
        std::fs::write(
            dir.join("songs/opener/song.toml"),
            format!(
                "[song]\nname = \"O\"\nbpm = 120.0\nlength_samples = {frames}\n\
                 [[pairs]]\nindex = 0\nfile = \"stems/pair1.wav\"\n\
                 [dsp.pair0]\nfilter = \"hp\"\n"
            ),
        )
        .unwrap();

        let mut p = load_playable(&dir, None).unwrap();
        // Grab the cutoff knob; if song.toml's "hp" topology reached the
        // mixer (rather than the default lowpass), the sustained stem should
        // decay toward silence once the biquad settles.
        p.mixer.set_dsp_param(0, DspParam::Cutoff, 64);
        let mut out = vec![0i32; frames as usize * 2];
        p.mixer.render(&mut out);
        let last = out[out.len() - 2] as f32 / i32::MAX as f32;
        assert!(
            last.abs() < 1e-2,
            "expected the hp topology to block DC, got {last}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Write a one-note SMF at `<dir>/songs/opener/midi/<dest>.mid`.
    fn write_midi(dir: &std::path::Path, dest: &str) {
        use midly::num::{u15, u28, u4, u7};
        use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};
        let midi_dir = dir.join("songs/opener/midi");
        std::fs::create_dir_all(&midi_dir).unwrap();
        let track = vec![
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::NoteOn { key: u7::new(36), vel: u7::new(100) },
                },
            },
            TrackEvent { delta: u28::new(0), kind: TrackEventKind::Meta(MetaMessage::EndOfTrack) },
        ];
        let smf = Smf {
            header: Header { format: Format::SingleTrack, timing: Timing::Metrical(u15::new(480)) },
            tracks: vec![track],
        };
        let mut buf = Vec::new();
        smf.write_std(&mut buf).unwrap();
        std::fs::write(midi_dir.join(format!("{dest}.mid")), buf).unwrap();
    }

    #[test]
    fn load_schedulers_reads_destination_midi() {
        let dir = write_bundle(64);
        write_midi(&dir, "lights"); // matches the "lights" destination
        let show = load_playable(&dir, None).unwrap().show;

        let mut scheds = load_schedulers(&show, &dir.join("songs/opener"), 48_000);
        assert_eq!(scheds.len(), 1, "one scheduler per destination");
        // The single note-on at sample 0 is due immediately.
        assert_eq!(scheds[0].drain_due(0).len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn offset_shifts_dispatch_position() {
        // -8 ms @ 48k = 384 samples earlier; +8 ms = 384 later.
        assert_eq!(dispatch_pos(10_000, -8.0, 48_000), Some(10_384));
        assert_eq!(dispatch_pos(10_000, 8.0, 48_000), Some(9_616));
        assert_eq!(dispatch_pos(5_000, 0.0, 48_000), Some(5_000));
        // Within a positive offset of the start, nothing is due yet — the t=0
        // event is delayed by the offset, not fired immediately.
        assert_eq!(dispatch_pos(100, 100.0, 48_000), None);
        // Exactly at the offset boundary: the start becomes due.
        assert_eq!(dispatch_pos(4_800, 100.0, 48_000), Some(0));
    }

    #[test]
    fn load_schedulers_is_empty_when_no_midi_file() {
        let dir = write_bundle(64);
        let show = load_playable(&dir, None).unwrap().show;
        // No midi/ dir written: the destination gets an empty (silent) scheduler.
        let scheds = load_schedulers(&show, &dir.join("songs/opener"), 48_000);
        assert_eq!(scheds.len(), 1);
        assert!(scheds[0].is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// End to end through the real loader: a song's `[delay]` reaches the mixer.
    ///
    /// The model tests cover the merge arithmetic; this covers the wiring, which is
    /// the part that would silently do nothing if `load_playable` read the wrong
    /// table. It matters more than it looks: the same call runs behind a song
    /// SWITCH, so this is also what makes each song arrive with its own delay.
    #[test]
    fn a_songs_delay_table_reaches_the_mixer() {
        let dir = write_bundle(64);
        let song_toml = dir.join("songs/opener/song.toml");
        let base = std::fs::read_to_string(&song_toml).unwrap();

        // Without a [delay] table: the show's default division (a quarter).
        let quarter = load_playable(&dir, None).unwrap();
        let at_120 = turtle_core::timing::DelayDivision::Quarter.to_samples(120.0, 48_000);

        // With one: the song's division wins.
        std::fs::write(&song_toml, format!("{base}\n[delay]\ntime = \"1/2\"\n")).unwrap();
        let half = load_playable(&dir, None).unwrap();

        // Compare the delay buffers' capacities as a proxy for "the settings landed":
        // both are sized from the longest division, so instead compare what the delay
        // actually does — render an impulse and find the echo.
        let echo_offset = |mut p: Playable| -> usize {
            p.mixer.set_dsp_param(0, crate::control_map::DspParam::Send, 100);
            p.mixer
                .set_dsp_param(0, crate::control_map::DspParam::DelayReturn, 100);
            p.mixer
                .set_dsp_param(0, crate::control_map::DspParam::DelayFeedback, 0);
            p.mixer
                .set_dsp_param(0, crate::control_map::DspParam::DelayCutoff, 127);
            let frames = 120_000usize;
            let mut out = vec![0i32; frames * 2];
            p.mixer.render(&mut out);
            out.iter()
                .step_by(2)
                .enumerate()
                .skip(2_000)
                .max_by_key(|(_, v)| v.abs())
                .map(|(i, _)| i)
                .unwrap()
        };

        let without = echo_offset(quarter);
        let with = echo_offset(half);
        assert!(
            with > without,
            "the song's 1/2 should delay longer than the show's 1/4: {without} then {with}"
        );
        // And it is the *right* longer: a half note is twice a quarter.
        assert!(
            (with as f64 / without as f64 - 2.0).abs() < 0.15,
            "expected roughly 2x ({} vs {}), quarter at 120 BPM is {at_120}",
            with,
            without
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
