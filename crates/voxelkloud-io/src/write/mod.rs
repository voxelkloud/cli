//! Writing clouds out.
//!
//! Three formats, one tree. Everything that decides *which points are in which
//! node* is [`crate::build`]; these modules decide how a node is spelled on
//! disk, and they share the LAS record they are handed.
//!
//! COPC is the recommended output and is where the care went. A converter that
//! only wrote a format of its own would be asking people to trust it twice —
//! once with their data and once with a format nobody else reads.

pub mod copc;
pub mod las_write;
pub mod morton;

// Both describe themselves in a JSON manifest, so both need serde. COPC does
// not, and that is why a wasm build can write one without compiling either.
#[cfg(feature = "formats")]
pub mod ept;
#[cfg(feature = "formats")]
pub mod potree;
#[cfg(feature = "formats")]
pub mod tileset;

use crate::bounds::Bounds;
use crate::crs::Crs;
use crate::record::RecordLayout;

pub use las_write::creation_today;

/// What every writer needs to know beyond the nodes themselves.
#[derive(Debug, Clone)]
pub struct WriteOptions {
    pub layout: RecordLayout,
    /// The cube the octree subdivides.
    pub cube: Bounds,
    /// The measured extent of the points.
    pub extent: Bounds,
    pub scale: [f64; 3],
    pub offset: [f64; 3],
    /// Distance between neighbouring points at the root.
    pub spacing: f64,
    /// Points across a node's edge.
    pub span: u32,
    /// Whether the points carry GPS time.
    ///
    /// The record always has room for it; a format that states its own
    /// attribute list can leave the field out when nothing filled it.
    pub has_gps_time: bool,
    /// Whether every input was a legacy LAS point format, which decides how two
    /// fields are named. See [`crate::convert::Scan::all_legacy`].
    pub legacy_fields: bool,
    pub crs: Option<Crs>,
    /// The source's `LASF_Projection` records, to be written out unchanged.
    /// See [`crate::convert::Scan::projection_vlrs`].
    pub projection_vlrs: Vec<(u16, Vec<u8>)>,
    /// Written into the file so a cloud says what made it.
    pub generator: String,
    /// Day of the year and year, as LAS spells a creation date.
    ///
    /// Supplied rather than read from the clock, and not for tidiness: this
    /// crate compiles to `wasm32-unknown-unknown`, where `SystemTime::now()`
    /// does not return — it panics, with a trap and no message, from inside a
    /// header writer that has nothing to do with time. A caller that has a
    /// clock passes one; a caller that wants a reproducible file passes a fixed
    /// date, which is the other half of why this is an input.
    pub creation: (u16, u16),
}

impl WriteOptions {
    pub fn stride(&self) -> usize {
        self.layout.stride()
    }
}

/// What a writer produced.
#[derive(Debug, Clone, Default)]
pub struct WriteReport {
    pub nodes: u64,
    pub points: u64,
    pub depth: u32,
    pub bytes: u64,
    pub path: String,
}
