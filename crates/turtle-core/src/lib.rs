//! `turtle-core` — data model, bundle load/validate, and sample-time math for
//! the Turtle show player.
//!
//! This crate is deliberately free of real-time, hardware, and OS concerns: it
//! only understands *show bundles* (see `docs/turtle-spec.md` §7) and the
//! sample-time arithmetic used to compile them. `turtled` builds the real-time
//! engine on top of the types defined here.

pub mod error;
pub mod model;
pub mod timing;

pub use error::Error;
pub use model::{Show, Song};
