//! Reading points out of a file, in the canonical record.
//!
//! One trait, because the converter should not care which of the five formats
//! its input is. The differences that survive — a bare LAZ is one chunk table,
//! a COPC is the same file with an octree over it, an EPT is a directory of
//! little files — are differences in *where the bytes are*, and every one of
//! them ends in the same place: a slice of LAS records.

#[cfg(feature = "e57")]
pub mod e57_points;
pub mod las_points;

use crate::error::Result;
use crate::record::RecordLayout;

/// A source of points, in canonical records.
///
/// Pull-based and batched: `next_batch` fills a buffer and says how many
/// records it wrote. The converter is out-of-core by construction and a reader
/// that returned everything at once would decide that for it.
pub trait PointSource {
    /// The record shape this source produces.
    fn layout(&self) -> &RecordLayout;

    /// Total points, as the file declares. Used for progress and for sizing.
    fn point_count(&self) -> u64;

    /// Append at most `max` records to `out`. Returns how many were appended;
    /// zero means the source is finished.
    fn next_batch(&mut self, max: usize, out: &mut Vec<u8>) -> Result<usize>;
}
