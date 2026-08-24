//! A bare LAS or LAZ file: points, and no index.
//!
//! The tier the whole toolchain exists to move people off. There is nothing to
//! stream — no hierarchy, no level of detail — so a viewer either reads the
//! whole file or converts it first, and `voxelkloud convert` is the second of
//! those. Reading it here is what makes `inspect` and `convert` able to accept
//! the file people actually have.

use std::sync::Arc;

use crate::bounds::Bounds;
use crate::cloud::{CloudInfo, FormatId, HierarchyStats, NodeInfo};
use crate::error::Result;
use crate::las::crs::las_crs;
use crate::las::layout::{las_layout, LasLayout, LasLayoutOptions};
use crate::las::{read_evlrs, LasHeader};
use crate::octree::OctreeKey;
use crate::source::ByteSource;

/// How much of the front to read. The header plus the VLR directory, which on a
/// LAZ file includes the laszip record a decoder needs.
const OPEN_PROBE: usize = 8192;

pub struct LasCloud {
    pub info: CloudInfo,
    pub header: LasHeader,
    pub layout: LasLayout,
    pub source: Arc<dyn ByteSource>,
}

pub fn open(source: Arc<dyn ByteSource>, label: &str) -> Result<LasCloud> {
    let mut head = source.read_prefix(OPEN_PROBE)?;
    let mut header = LasHeader::read(&head)?;

    // One widened read rather than a guess. A file with a long WKT or many
    // Extra Bytes descriptors pushes the point data past 8 KiB, and reading the
    // directory short would drop the projection silently.
    if !header.vlrs_complete && header.offset_to_point_data as usize > head.len() {
        head = source.read_prefix(header.offset_to_point_data as usize)?;
        header = LasHeader::read(&head)?;
    }

    let mut info = CloudInfo::new(FormatId::Las, label);
    info.version = Some(format!("{}.{}", header.version_major, header.version_minor));
    info.point_count = header.point_count;
    info.tight_bounds = Bounds::new(header.min, header.max);
    // A bare file has no indexing volume. The cube it *would* be indexed in is
    // the honest answer, and it is what `convert` will use.
    info.bounds = info.tight_bounds.index_cube();
    info.scale = header.scale;
    info.offset = header.offset;
    info.encoding = Some(if header.compressed { "laszip" } else { "uncompressed" }.to_string());
    info.crs = las_crs(&header.vlrs);
    info.data_bytes = source
        .size()
        .ok()
        .map(|size| size.saturating_sub(u64::from(header.offset_to_point_data)));

    if !header.vlrs_complete {
        info.warn(
            "vlr-truncated",
            "vlrs",
            format!(
                "The header claims {} variable length records but the directory does not \
                 fit before the point data. Anything past the cut — including the \
                 projection — is unread.",
                header.vlr_count
            ),
        );
    }

    // A LAS 1.4 file may put the Extra Bytes VLR in the *extended* directory at
    // the end of the file, where the 8 KiB probe never reaches it.
    let mut extra = header
        .vlrs
        .iter()
        .find(|v| v.is("LASF_Spec", 4))
        .map(|v| v.data.clone());
    if extra.is_none() && header.evlr_count > 0 && header.evlr_offset > 0 {
        if let Ok(size) = source.size() {
            if header.evlr_offset < size {
                let len = usize::try_from(size - header.evlr_offset).unwrap_or(usize::MAX);
                if let Ok(bytes) = source.read_at(header.evlr_offset, len) {
                    let (evlrs, _) = read_evlrs(&bytes, header.evlr_count);
                    extra = evlrs
                        .iter()
                        .find(|v| v.is("LASF_Spec", 4))
                        .map(|v| v.data.clone());
                    if info.crs.is_none() {
                        info.crs = las_crs(&evlrs);
                    }
                }
            }
        }
    }

    let layout = las_layout(&LasLayoutOptions {
        format: header.point_format,
        point_size: header.point_size as usize,
        extra_bytes: extra.as_deref(),
        bounds: info.tight_bounds,
        gps_time_range: None,
    })?;
    info.attributes = layout.plain();
    info.record_bytes = Some(layout.stride);
    info.warnings.extend(layout.warnings.clone());

    Ok(LasCloud {
        info,
        header,
        layout,
        source,
    })
}

impl LasCloud {
    /// The whole file as one node at level zero.
    ///
    /// Not a hierarchy, and the shape says so: one node, depth zero, every
    /// point in it. That is exactly what makes a `.laz` unusable in a browser
    /// at any size, and printing it next to a COPC file's 700 nodes is the
    /// clearest argument the CLI can make.
    pub fn hierarchy(&self) -> HierarchyStats {
        let mut stats = HierarchyStats::default();
        stats.add(NodeInfo {
            key: OctreeKey::ROOT,
            point_count: self.info.point_count,
            byte_size: self.info.data_bytes.unwrap_or(0),
        });
        stats
    }
}
