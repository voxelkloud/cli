//! What every format can say about a cloud, minus its points.
//!
//! The Rust twin of `PointCloudSourceBase`. Nothing here names a file, an
//! encoding or a manifest: a Potree directory, a COPC file and an EPT prefix
//! all produce this shape, and `voxelkloud inspect` prints the same nouns for
//! all three because there is nothing else to print.

use crate::attribute::Attribute;
use crate::bounds::Bounds;
use crate::crs::Crs;
use crate::warning::Warning;

/// Which driver read it.
///
/// Present because a human asked "what is this?" and the answer is the first
/// line of output — not because anything downstream branches on it. The moment
/// something does, that is a format-specific decision and it says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatId {
    PotreeV2,
    Copc,
    Ept,
    /// A bare LAS or LAZ file: points, no index.
    Las,
    /// An E57 file: one or more scans, no index.
    E57,
    /// A 3D Tiles tileset: explicit tree, implicit rule, or both.
    Tiles3D,
}

impl FormatId {
    pub fn name(self) -> &'static str {
        match self {
            Self::PotreeV2 => "potree-v2",
            Self::Copc => "copc",
            Self::Ept => "ept",
            Self::Las => "las",
            Self::E57 => "e57",
            Self::Tiles3D => "3d-tiles",
        }
    }

    /// What a human calls it.
    pub fn title(self) -> &'static str {
        match self {
            Self::PotreeV2 => "Potree v2",
            Self::Copc => "COPC",
            Self::Ept => "Entwine Point Tile",
            Self::Las => "LAS/LAZ",
            Self::E57 => "E57",
            Self::Tiles3D => "3D Tiles",
        }
    }

    /// Whether the format carries its own spatial index.
    ///
    /// The line the whole product sits on: an indexed cloud streams by level of
    /// detail, and a bare file has to be read whole or converted first.
    pub fn is_indexed(self) -> bool {
        !matches!(self, Self::Las | Self::E57)
    }
}

/// The neutral summary.
#[derive(Debug, Clone)]
pub struct CloudInfo {
    pub format: FormatId,
    /// Where it was read from. A path or a URL; never parsed.
    pub label: String,
    /// Points across every level, as the file declares it.
    pub point_count: u64,
    /// The indexing volume — the cube an octree subdivides.
    ///
    /// Not the data extent. On autzen it is 22x taller than the points.
    pub bounds: Bounds,
    /// The genuinely tight extent, in absolute CRS units.
    ///
    /// What fit-to-view and elevation ramps want. Potree's field of this name
    /// is a clone of the cubic box, i.e. not tight at all, so a driver that
    /// copied it would be repeating the file's mistake.
    pub tight_bounds: Bounds,
    /// Position quantum and origin, when the format stores integers.
    pub scale: [f64; 3],
    pub offset: [f64; 3],
    /// In source order. For a record-oriented format this is the field order.
    pub attributes: Vec<Attribute>,
    pub crs: Option<Crs>,
    /// Distance between neighbouring points at the root, where stated.
    ///
    /// Potree v2 states it; COPC states a root spacing implicitly through its
    /// `spacing` field; EPT states a grid `span` instead, which is the same
    /// claim in another unit.
    pub spacing: Option<f64>,
    /// Deepest level the index reaches, where the manifest states it.
    pub levels: Option<u32>,
    /// How the point records are stored: `"DEFAULT"`, `"BROTLI"`, `"laszip"`,
    /// `"binary"`, `"zstandard"`. Verbatim from the file.
    pub encoding: Option<String>,
    /// The version string the file carries, when it carries one.
    pub version: Option<String>,
    /// Total bytes of point data, when the format makes it knowable without
    /// walking every node.
    pub data_bytes: Option<u64>,
    /// Bytes of one stored record, as the format states it.
    ///
    /// Not the sum of the attribute sizes, which over-counts: LAS packs six
    /// dimensions into two bytes, so adding them up makes a 36-byte record
    /// look like 40. Set by the driver, which knows the stride.
    pub record_bytes: Option<usize>,
    /// Tolerated anomalies, in discovery order.
    pub warnings: Vec<Warning>,
}

impl CloudInfo {
    /// An empty summary of a given format, for a reader to fill in.
    pub fn new(format: FormatId, label: impl Into<String>) -> Self {
        Self {
            format,
            label: label.into(),
            point_count: 0,
            bounds: Bounds::EMPTY,
            tight_bounds: Bounds::EMPTY,
            scale: [1.0; 3],
            offset: [0.0; 3],
            attributes: Vec::new(),
            crs: None,
            spacing: None,
            levels: None,
            encoding: None,
            version: None,
            data_bytes: None,
            record_bytes: None,
            warnings: Vec::new(),
        }
    }

    pub fn warn(&mut self, code: &'static str, path: impl Into<String>, message: impl Into<String>) {
        self.warnings.push(Warning::new(code, path, message));
    }

    /// Bytes of one stored point record.
    pub fn bytes_per_point(&self) -> usize {
        self.record_bytes
            .unwrap_or_else(|| self.attributes.iter().map(|a| a.byte_size()).sum())
    }

    pub fn attribute(&self, name: &str) -> Option<&Attribute> {
        self.attributes.iter().find(|a| a.name == name)
    }

    pub fn has_color(&self) -> bool {
        self.attributes.iter().any(|a| a.role() == Some(crate::attribute::AttributeRole::Color))
    }
}

/// One node of an index, as a walk of the hierarchy found it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NodeInfo {
    pub key: crate::octree::OctreeKey,
    pub point_count: u64,
    /// Bytes of stored payload, compressed if it is compressed.
    pub byte_size: u64,
}

/// What a full walk of the hierarchy learned.
///
/// `voxelkloud inspect --deep` prints it and `doctor` judges it: a hierarchy
/// that is 40 levels deep or one whose root page is 30 MB are both deployments
/// that will feel broken in a browser, and neither is visible from the manifest.
#[derive(Debug, Clone, Default)]
pub struct HierarchyStats {
    pub nodes: u64,
    /// Deepest level with at least one node.
    pub depth: u32,
    /// Points per level, index 0 being the root level.
    pub points_by_level: Vec<u64>,
    pub nodes_by_level: Vec<u64>,
    /// Payload bytes summed over every node, where the hierarchy states them.
    pub data_bytes: u64,
    /// Bytes of hierarchy structure itself, and how many requests reading it
    /// took — the number that decides whether a cold open feels instant.
    pub hierarchy_bytes: u64,
    pub hierarchy_requests: u32,
    pub warnings: Vec<Warning>,
}

impl HierarchyStats {
    pub fn add(&mut self, node: NodeInfo) {
        let level = node.key.level as usize;
        if self.points_by_level.len() <= level {
            self.points_by_level.resize(level + 1, 0);
            self.nodes_by_level.resize(level + 1, 0);
        }
        self.points_by_level[level] += node.point_count;
        self.nodes_by_level[level] += 1;
        self.nodes += 1;
        self.data_bytes += node.byte_size;
        self.depth = self.depth.max(node.key.level);
    }

    pub fn total_points(&self) -> u64 {
        self.points_by_level.iter().sum()
    }
}
