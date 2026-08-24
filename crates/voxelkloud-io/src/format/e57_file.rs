//! An E57 file: scans, and no index.
//!
//! The same tier as a bare LAS — points with nothing over them — reached
//! through a different container. What makes it worth its own module rather
//! than a branch inside `las_file` is that everything a [`CloudInfo`] wants is
//! somewhere else: the extent is per scan and in the scan's own frame, the
//! attribute list is an XML prototype rather than a fixed record, and the
//! projection, when there is one, is free text.
//!
//! Opening reads the header and the XML section. No points, no CRC pass over
//! the file — `voxelkloud inspect` on a 40 GB scan is two reads and a parse.

use std::sync::Arc;

use crate::attribute::Attribute;
use crate::bounds::Bounds;
use crate::cloud::{CloudInfo, FormatId, HierarchyStats, NodeInfo};
use crate::crs::Crs;
use crate::e57::{E57Info, E57Points, SIGNATURE};
use crate::error::{Error, Result};
use crate::octree::OctreeKey;
use crate::source::{ByteSource, SourceCursor};

pub struct E57Cloud {
    pub info: CloudInfo,
    /// What the XML said, kept whole: the scan list is the interesting half of
    /// an E57 and [`CloudInfo`] has nowhere to put it.
    pub e57: E57Info,
}

pub fn open(source: Arc<dyn ByteSource>, label: &str) -> Result<E57Cloud> {
    let head = source.read_prefix(SIGNATURE.len())?;
    if !crate::e57::is_e57(&head) {
        return Err(Error::not_format(
            "E57",
            format!("{label} does not start with the ASTM-E57 signature"),
        ));
    }

    let cursor = SourceCursor::new(source.clone())?;
    let e57 = E57Points::open(cursor)?.info().clone();

    let mut info = CloudInfo::new(FormatId::E57, label);
    info.point_count = e57.point_count;
    info.tight_bounds = e57.extent.unwrap_or(Bounds::EMPTY);
    info.bounds = info.tight_bounds.index_cube();
    // E57 stores coordinates as floats or as scaled integers whose scale is
    // per attribute, not per cloud. There is no cloud-wide quantum to report,
    // and 1/0 is the identity that says so.
    info.scale = [1.0; 3];
    info.offset = [0.0; 3];
    info.encoding = Some("bitpack".to_string());
    info.version = Some("1.0".to_string());
    info.crs = e57
        .coordinate_metadata
        .as_deref()
        .and_then(Crs::from_string);
    info.attributes = prototype(&e57);
    // The stored width, not the sum of the decoded ones. E57 bitpacks each
    // field to what its declared range needs, so summing the types a consumer
    // sees would report a 194-bit record as 32 bytes. Rounded up to whole
    // bytes, because the file's own cost per point is fractional and this field
    // is not.
    let bits: usize = e57.prototype.iter().map(|f| f.bits).sum();
    if bits > 0 {
        info.record_bytes = Some(bits.div_ceil(8));
    }

    for warning in &e57.warnings {
        info.warnings.push(warning.clone());
    }

    if e57.scans.len() > 1 {
        info.warn(
            "e57-multi-scan",
            "data3D",
            format!(
                "This file holds {} scans. They are read in order and merged into one cloud, each \
                 keeping its position in the file as a point source id.",
                e57.scans.len()
            ),
        );
    }

    if e57.extent.is_none() {
        info.warn(
            "e57-extent-unknown",
            "data3D",
            "No scan declares where its points are, so the extent shown is empty. Converting the \
             file measures it."
                .to_string(),
        );
    }

    Ok(E57Cloud { info, e57 })
}

impl E57Cloud {
    /// A file with no index is one node, which is the same answer `las_file`
    /// gives and for the same reason.
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

/// The first scan's prototype, in this crate's attribute vocabulary.
///
/// The FIRST scan, not a merge of all of them: E57 lets every scan declare its
/// own prototype, and a union would describe a record no scan actually has.
/// Reading is per scan and tolerates the difference; reporting says what the
/// first one holds and the scan list carries the rest.
fn prototype(e57: &E57Info) -> Vec<Attribute> {
    e57.prototype
        .iter()
        .map(|field| {
            let mut attribute = Attribute::new(field.name.clone(), field.kind, 1);
            attribute.min = vec![field.min];
            attribute.max = vec![field.max];
            attribute
        })
        .collect()
}
