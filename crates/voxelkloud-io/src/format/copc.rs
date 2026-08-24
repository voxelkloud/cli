//! COPC: one LAS 1.4 file whose laszip chunks are an octree.
//!
//! The whole format is two records. A 160-byte VLR that must be the *first* one
//! in the file says where the root cube is and where the hierarchy starts; an
//! EVLR at the end holds pages of 32-byte entries, some of which point at
//! further pages. Everything else is LAS.
//!
//! That layout is why one ranged read of the first 8 KiB opens any COPC file
//! anywhere: the header, the `copc` VLR, the `laszip` VLR and the Extra Bytes
//! VLR all live there by construction.

use crate::bounds::Bounds;
use crate::cloud::{CloudInfo, FormatId, HierarchyStats, NodeInfo};
use crate::error::{Error, Result};
use crate::las::crs::las_crs;
use crate::las::layout::{las_layout, LasLayoutOptions};
use crate::las::{read_evlrs, LasHeader, Vlr};
use std::sync::Arc;

use crate::source::ByteSource;

/// How much of the front of the file to read before anything is known.
///
/// The header is 375 bytes and the VLRs that matter follow it immediately. 8
/// KiB covers every real file this repo has seen, including one with a 1.5 KB
/// WKT and an Extra Bytes VLR, and it is one request either way.
const OPEN_PROBE: usize = 8192;

pub use crate::las::copc::{
    parse_page, CopcInfo, Entry, COPC_ENTRY_SIZE, COPC_HIERARCHY_RECORD_ID, COPC_INFO_RECORD_ID,
    COPC_INFO_SIZE, COPC_USER_ID,
};

pub struct CopcCloud {
    pub info: CloudInfo,
    pub copc: CopcInfo,
    pub header: LasHeader,
    /// The EVLRs. Hierarchy pages live in them, though they are addressed by
    /// absolute file offset rather than by record.
    pub evlrs: Vec<Vlr>,
    source: Arc<dyn ByteSource>,
}

/// Open a COPC file.
///
/// Returns [`Error::NotFormat`] — and not a failure — for a LAS or LAZ file
/// that simply is not COPC, because that is how [`super::open_las_like`] tells
/// the two apart.
pub fn open(source: Arc<dyn ByteSource>, label: &str) -> Result<CopcCloud> {
    let head = source.read_prefix(OPEN_PROBE)?;
    let header = LasHeader::read(&head)?;

    let Some(vlr) = header.vlrs.iter().find(|v| v.is(COPC_USER_ID, COPC_INFO_RECORD_ID)) else {
        return Err(Error::not_format(
            "COPC",
            if header.vlrs_complete {
                "the file carries no copc VLR".to_string()
            } else {
                format!(
                    "the first {} bytes do not reach the end of the VLR directory",
                    head.len()
                )
            },
        ));
    };
    let copc = CopcInfo::parse(&vlr.data)?;

    let mut info = CloudInfo::new(FormatId::Copc, label);
    info.version = Some(format!("{}.{}", header.version_major, header.version_minor));
    info.point_count = header.point_count;
    info.bounds = copc.cube();
    info.tight_bounds = Bounds::new(header.min, header.max);
    info.scale = header.scale;
    info.offset = header.offset;
    info.spacing = Some(copc.spacing);
    info.encoding = Some(if header.compressed { "laszip" } else { "uncompressed" }.to_string());
    info.crs = las_crs(&header.vlrs);
    info.data_bytes = source.size().ok().map(|size| size.saturating_sub(u64::from(header.offset_to_point_data)));

    // The Extra Bytes VLR is what makes an unnamed dimension readable. It sits
    // in the same 8 KiB as everything else.
    let extra = header
        .vlrs
        .iter()
        .find(|v| v.is("LASF_Spec", 4))
        .map(|v| v.data.clone());
    let layout = las_layout(&LasLayoutOptions {
        format: header.point_format,
        point_size: header.point_size as usize,
        extra_bytes: extra.as_deref(),
        bounds: info.tight_bounds,
        gps_time_range: Some(copc.gps_time_range),
    })?;
    info.attributes = layout.plain();
    info.record_bytes = Some(layout.stride);
    info.warnings.extend(layout.warnings);

    if !header.compressed {
        info.warn(
            "copc-uncompressed",
            "header.point_data_record_format",
            "The file declares COPC but its points are not laszip-compressed. Every \
             reader in this space assumes compression; the nodes may not be readable."
                .to_string(),
        );
    }

    // The EVLR directory is one read from the end of the file. Every hierarchy
    // page is in it, so this is the last request an open needs.
    let mut evlrs = Vec::new();
    if header.evlr_count > 0 && header.evlr_offset > 0 {
        let size = source.size()?;
        if header.evlr_offset < size {
            let len = usize::try_from(size - header.evlr_offset).unwrap_or(usize::MAX);
            let bytes = source.read_at(header.evlr_offset, len)?;
            let (found, complete) = read_evlrs(&bytes, header.evlr_count);
            if !complete {
                info.warn(
                    "evlr-truncated",
                    "evlrs",
                    format!(
                        "The header claims {} extended records but only {} fit before the \
                         end of the file.",
                        header.evlr_count,
                        found.len()
                    ),
                );
            }
            evlrs = found;
        }
    }

    Ok(CopcCloud {
        info,
        copc,
        header,
        evlrs,
        source,
    })
}

impl CopcCloud {
    /// Every page, resolved. The root page names the subtrees; each of those
    /// names its own, and the recursion ends at the leaves.
    pub fn hierarchy(&self) -> Result<HierarchyStats> {
        let mut stats = HierarchyStats::default();
        let mut queue = vec![(
            "root".to_string(),
            self.copc.root_hierarchy_offset,
            self.copc.root_hierarchy_size,
        )];
        let mut visited: Vec<u64> = Vec::new();

        while let Some((label, offset, size)) = queue.pop() {
            if visited.contains(&offset) {
                stats.warnings.push(crate::warning::Warning::new(
                    "hierarchy-cycle",
                    label,
                    format!("The page at byte {offset} is reached twice; the walk stops there."),
                ));
                continue;
            }
            visited.push(offset);

            let bytes = self.read_page(offset, size)?;
            stats.hierarchy_bytes += bytes.len() as u64;
            stats.hierarchy_requests += 1;

            for entry in parse_page(&bytes, &label)? {
                match entry {
                    Entry::Node {
                        key,
                        point_count,
                        byte_size,
                        ..
                    } => stats.add(NodeInfo {
                        key,
                        point_count,
                        byte_size,
                    }),
                    Entry::Page {
                        key,
                        offset,
                        byte_size,
                    } => queue.push((key.ept_name(), offset, byte_size)),
                }
            }
        }
        Ok(stats)
    }

    /// A page's bytes.
    ///
    /// The spec addresses pages by absolute file offset, pointing at the EVLR's
    /// *payload* rather than at its record header — so this reads the file and
    /// does not try to match the offset against a record it already holds.
    fn read_page(&self, offset: u64, size: u64) -> Result<Vec<u8>> {
        let len = usize::try_from(size).unwrap_or(usize::MAX);
        self.source.read_at(offset, len)
    }
}
