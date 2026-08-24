//! COPC: the parts of the format that are neither reading nor writing.
//!
//! The 160-byte `copc` VLR, the 32-byte hierarchy entry, and the four
//! identifiers that name them. Spec, in other words — and it lives beside the
//! LAS framing rather than beside the driver because both halves of the project
//! need it and neither owns it: `crate::format::copc` reads a file with it and
//! `crate::write::copc` writes one, including in a browser, where the driver's
//! JSON dependencies are not compiled at all.

use crate::bounds::Bounds;
use crate::error::{Error, Result};
use crate::octree::OctreeKey;

/// User id of both COPC records.
pub const COPC_USER_ID: &str = "copc";
/// Record id of the info VLR.
pub const COPC_INFO_RECORD_ID: u16 = 1;
/// Record id of a hierarchy page, in the EVLR directory.
pub const COPC_HIERARCHY_RECORD_ID: u16 = 1000;
/// Bytes of the info VLR payload. Fixed by the spec.
pub const COPC_INFO_SIZE: usize = 160;
/// Bytes of one hierarchy entry. Also fixed.
pub const COPC_ENTRY_SIZE: usize = 32;

/// The `copc` VLR, record 1.
#[derive(Debug, Clone, Copy)]
pub struct CopcInfo {
    /// Centre of the root cube, absolute CRS.
    pub center: [f64; 3],
    /// Half the root cube's edge. The octree subdivides `center ± half_size`.
    pub half_size: f64,
    /// Point spacing at the root level. Halves at each level below.
    pub spacing: f64,
    pub root_hierarchy_offset: u64,
    pub root_hierarchy_size: u64,
    pub gps_time_range: [f64; 2],
}

impl CopcInfo {
    /// The cube the octree subdivides — the indexing volume, not the extent.
    pub fn cube(&self) -> Bounds {
        let h = self.half_size;
        Bounds::new(
            [self.center[0] - h, self.center[1] - h, self.center[2] - h],
            [self.center[0] + h, self.center[1] + h, self.center[2] + h],
        )
    }

    pub fn parse(record: &[u8]) -> Result<Self> {
        if record.len() < COPC_INFO_SIZE {
            return Err(Error::not_format(
                "COPC",
                format!(
                    "the copc VLR is {} bytes; the spec fixes it at {COPC_INFO_SIZE}",
                    record.len()
                ),
            ));
        }
        let f = |at: usize| f64::from_le_bytes(record[at..at + 8].try_into().unwrap());
        let u = |at: usize| u64::from_le_bytes(record[at..at + 8].try_into().unwrap());
        let info = Self {
            center: [f(0), f(8), f(16)],
            half_size: f(24),
            spacing: f(32),
            root_hierarchy_offset: u(40),
            root_hierarchy_size: u(48),
            gps_time_range: [f(56), f(64)],
        };
        // Finite first, then positive: `<= 0.0` is false for NaN, so the order
        // is what makes a NaN half-size fail here rather than pass.
        if !info.half_size.is_finite() || info.half_size <= 0.0 {
            return Err(Error::not_format(
                "COPC",
                format!(
                    "the copc VLR declares a half-size of {}; every node box derives from \
                     it, so nothing can be addressed",
                    info.half_size
                ),
            ));
        }
        if info.root_hierarchy_size == 0
            || info.root_hierarchy_size % COPC_ENTRY_SIZE as u64 != 0
        {
            return Err(Error::not_format(
                "COPC",
                format!(
                    "the copc VLR declares a root hierarchy page of {} bytes, which is not \
                     a whole number of {COPC_ENTRY_SIZE}-byte entries",
                    info.root_hierarchy_size
                ),
            ));
        }
        Ok(info)
    }
}

/// One hierarchy entry: either a node or a pointer at another page.
#[derive(Debug, Clone, Copy)]
pub enum Entry {
    Node {
        key: OctreeKey,
        offset: u64,
        byte_size: u64,
        point_count: u64,
    },
    /// `point_count == -1`: `offset`/`byte_size` name a further page.
    Page {
        key: OctreeKey,
        offset: u64,
        byte_size: u64,
    },
}

/// Parse one hierarchy page.
pub fn parse_page(bytes: &[u8], at: &str) -> Result<Vec<Entry>> {
    if bytes.len() % COPC_ENTRY_SIZE != 0 {
        return Err(Error::not_format(
            "a COPC hierarchy page",
            format!(
                "the page at {at} is {} bytes, not a whole number of \
                 {COPC_ENTRY_SIZE}-byte entries",
                bytes.len()
            ),
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / COPC_ENTRY_SIZE);
    for chunk in bytes.chunks_exact(COPC_ENTRY_SIZE) {
        let i32_at = |at: usize| i32::from_le_bytes(chunk[at..at + 4].try_into().unwrap());
        let level = i32_at(0);
        let x = i32_at(4);
        let y = i32_at(8);
        let z = i32_at(12);
        let offset = u64::from_le_bytes(chunk[16..24].try_into().unwrap());
        let byte_size = i32_at(24);
        let point_count = i32_at(28);

        if level < 0 || x < 0 || y < 0 || z < 0 || byte_size < 0 {
            return Err(Error::not_format(
                "a COPC hierarchy page",
                format!("the entry {level}-{x}-{y}-{z} at {at} has a negative field"),
            ));
        }
        let key = OctreeKey::new(level as u32, x as u32, y as u32, z as u32);
        out.push(if point_count == -1 {
            Entry::Page {
                key,
                offset,
                byte_size: byte_size as u64,
            }
        } else if point_count < 0 {
            return Err(Error::not_format(
                "a COPC hierarchy page",
                format!(
                    "the entry {level}-{x}-{y}-{z} at {at} declares {point_count} points; \
                     only -1 has a meaning below zero"
                ),
            ));
        } else {
            // `byte_size == 0` with a real point count is how COPC spells an
            // empty node: it is in the tree and has no chunk. Kept, so the
            // subtree under it stays reachable.
            Entry::Node {
                key,
                offset,
                byte_size: byte_size as u64,
                point_count: point_count as u64,
            }
        });
    }
    Ok(out)
}

