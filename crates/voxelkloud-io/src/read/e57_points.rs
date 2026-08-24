//! E57, streamed into canonical records.
//!
//! THE THREAD IS THE POINT. `crate::e57` pushes batches and the converter
//! pulls them, and the only thing that turns one into the other without
//! holding a whole cloud in memory is a decode running beside the caller with
//! a bounded queue between them. Two batches deep: enough that the decoder is
//! never idle waiting for the writer, shallow enough that the queue is 60 MB
//! rather than a second copy of the file.
//!
//! It also happens to be free speed. The E57 side is bitpacking and a CRC per
//! kilobyte; the converter's side is quantisation and laszip. They are
//! different work on different data, so overlapping them costs a channel and
//! saves whichever of the two is smaller.

use std::io::BufReader;
use std::path::Path;
use std::sync::mpsc::{sync_channel, Receiver};
use std::thread::JoinHandle;

use crate::e57::{E57Points, Measured, PointBatch, BATCH};
use crate::error::{Error, Result};
use crate::record::{at, RecordLayout};

use super::PointSource;

/// How many batches may sit between the decoder and the converter.
const QUEUE: usize = 2;

/// A single return of a single-return measurement, in the 4+4 bit field LAS 1.4
/// gives returns. A scanner's own return numbering does not survive E57, so
/// claiming anything else would be inventing it.
const ONE_OF_ONE: u8 = 0b0001_0001;

/// Read an E57 file's header and XML, and nothing else.
pub fn describe(path: &Path) -> Result<crate::e57::E57Info> {
    let file = std::fs::File::open(path)?;
    Ok(E57Points::open(BufReader::new(file))?.info().clone())
}

/// One pass over every point, for the extent and the count that survive it.
///
/// Expensive on purpose. See [`crate::e57::E57Points::measure`]: a posed scan's
/// declared bounds are not where its points are, and a LAS header that says
/// otherwise is a lie that every later reader inherits.
pub fn measure(path: &Path) -> Result<Measured> {
    let file = std::fs::File::open(path)?;
    E57Points::open(BufReader::new(file))?.measure()
}

pub struct E57PointSource {
    layout: RecordLayout,
    point_count: u64,
    scale: [f64; 3],
    offset: [f64; 3],
    /// Batches from the decode thread; `Err` is the decode failing.
    rx: Receiver<std::result::Result<PointBatch, String>>,
    worker: Option<JoinHandle<()>>,
    /// The batch being drained, and how far into it we are.
    current: PointBatch,
    at: usize,
}

impl E57PointSource {
    /// Open `path`, producing records in `layout` at the given quantum.
    pub fn open(
        path: &Path,
        layout: RecordLayout,
        out_scale: [f64; 3],
        out_offset: [f64; 3],
    ) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let mut points = E57Points::open(BufReader::new(file))?;
        let point_count = points.info().point_count;

        let (tx, rx) = sync_channel::<std::result::Result<PointBatch, String>>(QUEUE);
        let worker = std::thread::Builder::new()
            .name("e57-decode".to_string())
            .spawn(move || {
                let outcome = points.read(BATCH, &mut |batch| {
                    // Taken, not copied: the batch is on its way to another
                    // thread and the reader is about to refill an empty one.
                    let handover = std::mem::take(batch);
                    tx.send(Ok(handover)).map_err(|_| {
                        // The receiver is gone, which means the converter
                        // stopped. Not an error worth a message: this error
                        // ends the read and is then dropped with the channel.
                        Error::Source("the reader of this E57 stopped listening".to_string())
                    })
                });
                if let Err(error) = outcome {
                    // Best effort. A full queue with no receiver is exactly the
                    // case above, and there is nobody left to tell.
                    let _ = tx.send(Err(error.to_string()));
                }
            })
            .map_err(|e| Error::Source(format!("could not start the E57 decoder: {e}")))?;

        Ok(Self {
            layout,
            point_count,
            scale: out_scale,
            offset: out_offset,
            rx,
            worker: Some(worker),
            current: PointBatch::default(),
            at: 0,
        })
    }

    /// Bring in the next batch, if the current one is spent.
    ///
    /// `Ok(false)` means the decoder finished and said so by dropping its end.
    fn refill(&mut self) -> Result<bool> {
        if self.at < self.current.len() {
            return Ok(true);
        }
        match self.rx.recv() {
            Ok(Ok(batch)) => {
                self.current = batch;
                self.at = 0;
                Ok(!self.current.is_empty())
            }
            Ok(Err(message)) => Err(Error::Codec(message)),
            // Disconnected: the read ran out, which is how it ends.
            Err(_) => Ok(false),
        }
    }

    /// One point, in the output's record.
    fn write_record(&self, index: usize, out: &mut Vec<u8>) {
        let start = out.len();
        out.resize(start + self.layout.stride(), 0);
        let record = &mut out[start..];

        let position = [
            self.current.x[index],
            self.current.y[index],
            self.current.z[index],
        ];
        for (axis, value) in position.into_iter().enumerate() {
            let quantised = ((value - self.offset[axis]) / self.scale[axis]).round();
            let quantised = quantised.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
            let at = axis * 4;
            record[at..at + 4].copy_from_slice(&quantised.to_le_bytes());
        }

        if !self.current.intensity.is_empty() {
            record[at::INTENSITY..at::INTENSITY + 2]
                .copy_from_slice(&self.current.intensity[index].to_le_bytes());
        }
        record[at::RETURNS] = ONE_OF_ONE;
        // Classification stays 0 — "never classified", which is what a raw
        // scan is. Flags, user data and scan angle have no E57 counterpart
        // that survives the simple reader, and a synthesised angle would be
        // read as a measurement.
        record[at::POINT_SOURCE_ID..at::POINT_SOURCE_ID + 2]
            .copy_from_slice(&self.current.source_id[index].to_le_bytes());

        if self.layout.has_color() {
            // The lane exists because SOME input had colour. This file has
            // none, and writes white for it — the same choice `RecordConverter`
            // makes when it widens a colourless record, and for the same
            // reason: black reads as a shadow rather than as an absence.
            let rgb = self.current.rgb.get(index).copied().unwrap_or([u16::MAX; 3]);
            for (channel, value) in rgb.into_iter().enumerate() {
                let at = at::RGB + channel * 2;
                record[at..at + 2].copy_from_slice(&value.to_le_bytes());
            }
        }
    }
}

impl Drop for E57PointSource {
    fn drop(&mut self) {
        // Dropping the receiver first unblocks a decoder parked on a full
        // queue; without it, a converter that stops early — a write error, a
        // Ctrl-C — would wait here for the rest of the file to decode.
        if let Some(worker) = self.worker.take() {
            drop(std::mem::replace(&mut self.rx, sync_channel(1).1));
            let _ = worker.join();
        }
    }
}

impl PointSource for E57PointSource {
    fn layout(&self) -> &RecordLayout {
        &self.layout
    }

    fn point_count(&self) -> u64 {
        self.point_count
    }

    fn next_batch(&mut self, max: usize, out: &mut Vec<u8>) -> Result<usize> {
        let mut written = 0;
        while written < max {
            if !self.refill()? {
                break;
            }
            let take = (max - written).min(self.current.len() - self.at);
            for index in self.at..self.at + take {
                self.write_record(index, out);
            }
            self.at += take;
            written += take;
        }
        Ok(written)
    }
}
