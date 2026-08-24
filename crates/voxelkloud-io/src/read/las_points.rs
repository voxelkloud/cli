//! LAS and LAZ, streamed.
//!
//! Also the reader for COPC input, and for an EPT node: a COPC file *is* a LAZ
//! file whose chunks happen to be an octree, and reading it front to back gives
//! every point exactly once. Nothing here has to know that.
//!
//! Streaming rather than whole-file: the largest input this repo tests against
//! is 134 MB compressed and 2.7 GB decompressed, and the converter's whole
//! design is to never hold a cloud in memory.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use laz::{LasZipDecompressor, LazVlr};

use crate::error::{Error, Result};
use crate::las::LasHeader;
use crate::record::{RecordConverter, RecordLayout};

use super::PointSource;

/// How many points to decompress at a time.
///
/// A million format-7 records is 36 MB, which is large enough that the
/// per-call overhead vanishes and small enough to stay in a machine's cache
/// hierarchy rather than its swap.
const BATCH: usize = 1 << 20;

enum Decoder {
    /// Uncompressed: the records are already in the file.
    Raw(BufReader<File>),
    Laz(Box<LasZipDecompressor<'static, BufReader<File>>>),
}

pub struct LasPointSource {
    header: LasHeader,
    converter: RecordConverter,
    decoder: Decoder,
    remaining: u64,
    /// Source records, reused between batches.
    scratch: Vec<u8>,
}

impl LasPointSource {
    /// Open `path`, producing records in `layout` at the given quantum.
    ///
    /// The layout is decided by the *converter*, not by this file: several
    /// inputs with different point formats have to land in one output, so the
    /// widest wins and each reader converts up to it.
    pub fn open(
        path: &Path,
        layout: RecordLayout,
        out_scale: [f64; 3],
        out_offset: [f64; 3],
    ) -> Result<Self> {
        let mut file = File::open(path)?;
        // The header plus the VLR directory. The laszip VLR is in there, and
        // for a compressed file nothing can start without it.
        let mut head = vec![0u8; 8192];
        let read = read_prefix(&mut file, &mut head)?;
        head.truncate(read);
        let mut header = LasHeader::read(&head)?;
        if !header.vlrs_complete && (header.offset_to_point_data as usize) > head.len() {
            let mut wider = vec![0u8; header.offset_to_point_data as usize];
            file.seek(SeekFrom::Start(0))?;
            let read = read_prefix(&mut file, &mut wider)?;
            wider.truncate(read);
            header = LasHeader::read(&wider)?;
        }

        let converter = RecordConverter::new(&header, layout, out_scale, out_offset)?;
        let mut reader = BufReader::with_capacity(1 << 20, file);
        reader.seek(SeekFrom::Start(u64::from(header.offset_to_point_data)))?;

        let decoder = if header.compressed {
            let record = header.laszip_record().ok_or_else(|| {
                Error::not_format(
                    "a LAZ file",
                    "the file is flagged compressed and carries no laszip VLR (user id \
                     'laszip encoded')",
                )
            })?;
            let vlr = LazVlr::from_buffer(record)
                .map_err(|e| Error::Codec(format!("{}: unreadable laszip VLR: {e}", path.display())))?;
            Decoder::Laz(Box::new(
                LasZipDecompressor::new(reader, vlr)
                    .map_err(|e| Error::Codec(format!("{}: {e}", path.display())))?,
            ))
        } else {
            Decoder::Raw(reader)
        };

        Ok(Self {
            remaining: header.point_count,
            header,
            converter,
            decoder,
            scratch: Vec::new(),
        })
    }

    pub fn header(&self) -> &LasHeader {
        &self.header
    }
}

/// Read as much as fits, tolerating a file shorter than the buffer.
fn read_prefix(file: &mut File, buffer: &mut [u8]) -> Result<usize> {
    let mut at = 0;
    while at < buffer.len() {
        match file.read(&mut buffer[at..])? {
            0 => break,
            n => at += n,
        }
    }
    Ok(at)
}

impl PointSource for LasPointSource {
    fn layout(&self) -> &RecordLayout {
        self.converter.layout()
    }

    fn point_count(&self) -> u64 {
        self.header.point_count
    }

    fn next_batch(&mut self, max: usize, out: &mut Vec<u8>) -> Result<usize> {
        let want = (max.min(BATCH) as u64).min(self.remaining) as usize;
        if want == 0 {
            return Ok(0);
        }
        let stride = self.converter.source_stride();
        self.scratch.resize(want * stride, 0);

        match &mut self.decoder {
            Decoder::Raw(reader) => {
                // A file that ends early is a truncated file. Converting what
                // is there and reporting how much beats refusing it outright —
                // an interrupted download is the common cause and the points
                // that arrived are real.
                let mut at = 0;
                while at < self.scratch.len() {
                    match reader.read(&mut self.scratch[at..])? {
                        0 => break,
                        n => at += n,
                    }
                }
                let whole = at / stride;
                self.scratch.truncate(whole * stride);
            }
            Decoder::Laz(decoder) => {
                decoder
                    .decompress_many(&mut self.scratch)
                    .map_err(|e| Error::Codec(format!("laszip: {e}")))?;
            }
        }

        let got = self.scratch.len() / stride;
        self.converter.convert_many(&self.scratch, out);
        self.remaining -= got as u64;
        if got < want {
            // Nothing left to read, whatever the header claimed.
            self.remaining = 0;
        }
        Ok(got)
    }
}
