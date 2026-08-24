//! Where bytes come from.
//!
//! Nothing in this library opens a socket or decides on a retry. A reader says
//! which ranges of which relative path it wants and a [`Store`] answers, which
//! is what lets the same Potree reader run against a directory on disk, an S3
//! prefix over HTTP, and — later — a directory handle in a browser tab.
//!
//! It is also the seam `voxelkloud doctor` exists on the other side of: the
//! diagnostics it reports are all properties of the *transport*, and the reader
//! is deliberately unable to see them.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};

/// One addressable blob: a file, an object, a URL.
pub trait ByteSource: Send + Sync {
    /// Total size in bytes.
    fn size(&self) -> Result<u64>;

    /// Exactly `len` bytes starting at `offset`.
    ///
    /// Short reads are an error, not a shorter `Vec`: every caller here derived
    /// the length from a structure that claimed it, so a short read means the
    /// structure lied and silently returning less would push the failure into
    /// arithmetic somewhere further away.
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>>;

    /// The whole thing.
    fn read_all(&self) -> Result<Vec<u8>> {
        let size = self.size()?;
        let len = usize::try_from(size).map_err(|_| {
            Error::Unsupported(format!("{} is {size} bytes, too large to read at once", self.label()))
        })?;
        self.read_at(0, len)
    }

    /// A path or URL, for messages. Never parsed.
    fn label(&self) -> String;

    /// The first `len` bytes, or the whole source when it is shorter.
    ///
    /// The one place a short read is legitimate: a header probe asks for more
    /// than a small file holds, and the reader is written to tolerate a prefix.
    fn read_prefix(&self, len: usize) -> Result<Vec<u8>> {
        let size = self.size()?;
        let want = (len as u64).min(size) as usize;
        self.read_at(0, want)
    }
}

/// A namespace of [`ByteSource`]s addressed by relative path.
///
/// Every format here is more than one blob — Potree has three files, EPT has a
/// manifest plus two directories — and all of them address the rest relative to
/// where the manifest was found. That resolution is the store's, not the
/// reader's, because it is the only part that differs between a filesystem and
/// a URL.
pub trait Store: Send + Sync {
    /// Shared rather than owned, because two readers legitimately want the same
    /// blob: a `.laz` is offered to the COPC reader first and to the bare-LAS
    /// reader when that declines, and neither should reopen the file to do it.
    fn open(&self, relative: &str) -> Result<Arc<dyn ByteSource>>;

    /// Whether `relative` is there, without reading it.
    ///
    /// Sniffing asks this three times and no more, which is what keeps opening
    /// a remote cloud down to a handful of requests.
    fn exists(&self, relative: &str) -> bool;

    /// The directory or prefix, for messages.
    fn label(&self) -> String;
}

/// A file on disk.
pub struct FileSource {
    path: PathBuf,
    file: Mutex<File>,
    size: u64,
}

impl FileSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path,
            file: Mutex::new(file),
            size,
        })
    }
}

impl ByteSource for FileSource {
    fn size(&self) -> Result<u64> {
        Ok(self.size)
    }

    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        if offset.saturating_add(len as u64) > self.size {
            return Err(Error::Truncated {
                need: offset + len as u64,
                got: self.size,
                what: self.path.display().to_string(),
            });
        }
        let mut out = vec![0u8; len];
        let mut file = self
            .file
            .lock()
            .map_err(|_| Error::Source(format!("{}: reader poisoned", self.path.display())))?;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut out)?;
        Ok(out)
    }

    fn label(&self) -> String {
        self.path.display().to_string()
    }
}

/// A directory on disk.
pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// Resolve a relative path, refusing to leave the root.
    ///
    /// The manifests are read from files this tool did not write. An `ept.json`
    /// naming `../../../etc/passwd` as a hierarchy page is a plausible hostile
    /// input, and this is the only place that resolution happens.
    fn resolve(&self, relative: &str) -> Result<PathBuf> {
        let mut out = self.root.clone();
        for part in relative.split('/') {
            match part {
                "" | "." => continue,
                ".." => {
                    return Err(Error::Source(format!(
                        "{relative}: refusing to resolve a path that leaves {}",
                        self.root.display()
                    )))
                }
                _ => out.push(part),
            }
        }
        Ok(out)
    }
}

impl Store for FileStore {
    fn open(&self, relative: &str) -> Result<Arc<dyn ByteSource>> {
        Ok(Arc::new(FileSource::open(self.resolve(relative)?)?))
    }

    fn exists(&self, relative: &str) -> bool {
        self.resolve(relative)
            .map(|p| p.is_file())
            .unwrap_or(false)
    }

    fn label(&self) -> String {
        self.root.display().to_string()
    }
}

/// A [`ByteSource`] over bytes already in memory.
///
/// What tests read from, and what a decoder gets when a caller has the file
/// already — a drag-and-drop in a browser build, or a small manifest a store
/// fetched whole.
pub struct MemorySource {
    bytes: Vec<u8>,
    label: String,
}

impl MemorySource {
    pub fn new(bytes: Vec<u8>, label: impl Into<String>) -> Self {
        Self {
            bytes,
            label: label.into(),
        }
    }
}

impl ByteSource for MemorySource {
    fn size(&self) -> Result<u64> {
        Ok(self.bytes.len() as u64)
    }

    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let end = start.saturating_add(len);
        if end > self.bytes.len() {
            return Err(Error::Truncated {
                need: end as u64,
                got: self.bytes.len() as u64,
                what: self.label.clone(),
            });
        }
        Ok(self.bytes[start..end].to_vec())
    }

    fn read_all(&self) -> Result<Vec<u8>> {
        Ok(self.bytes.clone())
    }

    fn label(&self) -> String {
        self.label.clone()
    }
}

/// A [`ByteSource`] seen as a `Read + Seek` stream.
///
/// For decoders written against `std::io` rather than against ranges — E57's
/// is one, and it is not wrong to be: the format's XML section is at the END of
/// the file and its records are paged, so seeking is the access pattern.
///
/// THE WINDOW IS WHY THIS IS NOT TRIVIAL. An E57 page is 1024 bytes and a
/// decoder reads them one at a time; passing each straight through to
/// [`ByteSource::read_at`] over HTTP would be one range request per kilobyte,
/// which is thousands of requests to open a file. So reads are served from a
/// buffer that is refilled a megabyte at a time — sequential access costs one
/// request per megabyte, and a backwards seek costs one refill.
pub struct SourceCursor {
    source: Arc<dyn ByteSource>,
    size: u64,
    at: u64,
    window: Vec<u8>,
    window_at: u64,
}

/// Bytes per refill. Large enough that a paged decoder stops being chatty,
/// small enough to be an allocation nobody notices.
const WINDOW: usize = 1 << 20;

impl SourceCursor {
    pub fn new(source: Arc<dyn ByteSource>) -> Result<Self> {
        let size = source.size()?;
        Ok(Self {
            source,
            size,
            at: 0,
            window: Vec::new(),
            window_at: 0,
        })
    }

    fn fill(&mut self) -> std::io::Result<()> {
        let want = ((self.size - self.at) as usize).min(WINDOW);
        let bytes = self
            .source
            .read_at(self.at, want)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        self.window = bytes;
        self.window_at = self.at;
        Ok(())
    }
}

impl Read for SourceCursor {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.at >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let held = self.at.checked_sub(self.window_at).map(|o| o as usize);
        let offset = match held {
            Some(offset) if offset < self.window.len() => offset,
            _ => {
                self.fill()?;
                0
            }
        };
        let available = &self.window[offset..];
        let take = available.len().min(buf.len());
        buf[..take].copy_from_slice(&available[..take]);
        self.at += take as u64;
        Ok(take)
    }
}

impl Seek for SourceCursor {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let at = match to {
            SeekFrom::Start(offset) => offset as i128,
            SeekFrom::End(delta) => self.size as i128 + i128::from(delta),
            SeekFrom::Current(delta) => self.at as i128 + i128::from(delta),
        };
        if at < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before the start of the source",
            ));
        }
        // Past the end is legal to seek to and returns nothing to read, which
        // is what a file does.
        self.at = at as u64;
        Ok(self.at)
    }
}
