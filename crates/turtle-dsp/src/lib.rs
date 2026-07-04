//! `turtle-dsp` — the fixed, preallocated per-pair DSP chain (spec §6).
//!
//! Every primitive here is **alloc-free** and real-time safe: construct it off
//! the audio thread, then call `process`/`process_block` from the RT loop. No
//! heap, no locks, no syscalls. Parameters are driven by live incoming CC in
//! `turtled`; this crate only implements the math.

#![forbid(unsafe_code)]

mod biquad;
mod gain;

pub use biquad::{Biquad, FilterType};
pub use gain::Gain;
