//! One error type for everything that can stop a read.
//!
//! Deliberately small and deliberately *not* a taxonomy of every way a file can
//! be wrong: the ways a file is wrong but still readable are [`Warning`]s, and
//! that is the more common case by a wide margin. What is left here is the set
//! of failures where there is nothing to return.
//!
//! [`Warning`]: crate::warning::Warning

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// The bytes are not the format they were claimed to be.
    NotFormat {
        format: &'static str,
        detail: String,
    },
    /// A structure declared a length or an offset the source cannot satisfy.
    Truncated { need: u64, got: u64, what: String },
    /// Well-formed, and asking for something not implemented.
    Unsupported(String),
    /// A manifest parsed as JSON but did not hold what the format requires.
    Manifest { path: String, detail: String },
    /// The [`ByteSource`] failed. Carries whatever the transport said.
    ///
    /// [`ByteSource`]: crate::source::ByteSource
    Source(String),
    /// A point record could not be decompressed.
    Codec(String),
    /// Writing failed.
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFormat { format, detail } => write!(f, "not {format}: {detail}"),
            Self::Truncated { need, got, what } => write!(
                f,
                "{what} needs {need} bytes and the source has {got}"
            ),
            Self::Unsupported(what) => write!(f, "unsupported: {what}"),
            Self::Manifest { path, detail } => write!(f, "{path}: {detail}"),
            Self::Source(detail) => write!(f, "{detail}"),
            Self::Codec(detail) => write!(f, "decode failed: {detail}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl Error {
    /// Shorthand for the common `NotFormat` construction.
    pub fn not_format(format: &'static str, detail: impl Into<String>) -> Self {
        Self::NotFormat {
            format,
            detail: detail.into(),
        }
    }

    /// Shorthand for a manifest field that is missing or the wrong shape.
    pub fn manifest(path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Manifest {
            path: path.into(),
            detail: detail.into(),
        }
    }
}
