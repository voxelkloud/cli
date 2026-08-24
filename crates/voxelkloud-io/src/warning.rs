//! A tolerated anomaly.
//!
//! Data on the value, never a log line and never a callback — the same single
//! channel `@voxelkloud/core` settled on, for the same reason: it is assertable
//! in a test with no spies, and a CLI can print it or a `--json` consumer can
//! machine-read it without either of them owning a logger.

use std::fmt;

/// One anomaly a reader chose to survive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// A stable kebab-case identifier. Machine-readable; never reworded.
    pub code: &'static str,
    /// Where the anomaly is: a JSON path into a manifest (`attributes[5].size`),
    /// a node name, or a VLR identity.
    pub path: String,
    /// One sentence, for a human, saying what was found and what was done.
    pub message: String,
}

impl Warning {
    pub fn new(code: &'static str, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}: {}", self.code, self.path, self.message)
    }
}
