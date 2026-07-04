use thiserror::Error;

/// Errors from loading or validating a show bundle.
#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("MIDI/SMF error: {0}")]
    Midi(String),

    /// One or more validation checks failed. Messages are joined with "; ".
    #[error("validation failed: {0}")]
    Validate(String),
}

impl Error {
    /// Build a [`Error::Validate`] from a list of problems, or `Ok(())` if empty.
    pub(crate) fn from_problems(problems: Vec<String>) -> Result<(), Error> {
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::Validate(problems.join("; ")))
        }
    }
}
