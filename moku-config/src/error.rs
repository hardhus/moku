use std::fmt;

/// A problem found in config.toml that didn't stop moku from running: a
/// section or module table with a wrong-typed value (that part falls back
/// to its defaults and is write-locked, see `ConfigHandle`), or the whole
/// file being unusable (moku then runs on defaults, read-only). Shown to
/// the user as a warning; never silently swallowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigIssue {
    /// Dotted location in config.toml, e.g. `modules.pomodoro.profiles[0].plan`,
    /// or `config.toml` for a whole-file problem.
    pub path: String,
    pub message: String,
}

impl fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path == "config.toml" {
            // A whole-file problem: no section to point at.
            write!(f, "config.toml: {}", self.message)
        } else {
            write!(f, "config.toml [{}]: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for ConfigIssue {}

/// Why a config change couldn't be saved. In every case the file on disk
/// and the in-memory config are both left unchanged.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The whole file is in read-only mode (it couldn't be parsed or read,
    /// or it was written by a newer moku). Nothing is written until the
    /// user fixes it and restarts.
    #[error("config.toml is read-only this session: {reason}")]
    ReadOnly { reason: String },
    /// The change touches a section or module table whose current contents
    /// don't match their expected types. Writing it would replace what the
    /// user wrote by hand with defaults, so it is refused.
    #[error(
        "config.toml [{}] has an invalid value, so moku won't overwrite it; fix it by hand first ({})",
        .0.path,
        .0.message
    )]
    SectionInvalid(ConfigIssue),
    #[error("failed to encode the config change")]
    Encode(#[source] toml::ser::Error),
    #[error(transparent)]
    Write(#[from] moku_sys::util::AtomicWriteError),
}
