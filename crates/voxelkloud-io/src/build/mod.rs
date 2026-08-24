//! Building an octree out of a stream of points.
//!
//! One algorithm, shared by all three writers, because the tree Potree v2,
//! COPC and EPT each describe is the same tree — they disagree about how to
//! spell a node, not about which points are in it.
//!
//! **The rule.** Every node owns a `span`-cubed grid over its own box. A point
//! is offered to a node; if its cell is free it stays, and if the cell is
//! taken it falls to the child that contains it. So each node holds at most one
//! point per cell, the spacing between neighbours halves with every level, and
//! the LOD metric the renderer is written against — spacing at level L is
//! `s0 / 2^L` — is true by construction rather than by assertion.
//!
//! This is Entwine's rule, and PotreeConverter's within a constant: autzen's
//! manifest states a root spacing of 36.371 against a 4655.51 cube, which is
//! exactly `edge / 128`. Matching it is what lets the converter's output be
//! compared against theirs point for point.
//!
//! **Depth first, and one bucket at a time.** The recursion holds the points of
//! one path, not of one level: a node partitions its input into what it keeps
//! and eight child buckets, emits itself, and then recurses. Peak memory is
//! therefore the input plus one partition of it, and every byte is freed on the
//! way back up.

pub mod chunked;

use std::collections::HashSet;

use crate::bounds::Bounds;
use crate::error::Result;
use crate::octree::{child_index, OctreeKey};
use crate::record::{dequantize, position, RecordLayout};

/// Points across a node's edge. The density knob, and the only one.
///
/// 128 is what Entwine and PotreeConverter both settle on, and the number the
/// LOD defaults in `@voxelkloud/view` were tuned against. Halving it doubles
/// the spacing and quarters the points in every node.
pub const DEFAULT_SPAN: u32 = 128;

/// Points a node may hold without being subdivided.
///
/// The rule that decides the *shape* of the tree, and the one that took a
/// measurement to get right. Without it the recursion runs until every cell of
/// every grid holds one point, which for a 342k-point scan produced 832 nodes
/// of 3 KB each — a tree that is correct, and that a viewer opens in 832 round
/// trips. untwine writes the same cloud as 9 nodes.
///
/// So a node whose whole input fits takes it whole. 50,000 records is roughly
/// 1.8 MB before compression and 400 KB after, which is the size a streaming
/// reader wants: big enough that the request overhead disappears, small enough
/// that the first frame does not wait on it.
pub const DEFAULT_LEAF_POINTS: usize = 50_000;

/// Deepest level a node may reach.
///
/// A safety rail, not a design parameter: 21 levels of a 32-bit grid is where
/// the key arithmetic stops being exact, and a cloud with two coincident points
/// would otherwise recurse until it ran out of something.
pub const MAX_DEPTH: u32 = 21;

#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// The cube the octree subdivides, in absolute CRS units.
    pub cube: Bounds,
    pub scale: [f64; 3],
    pub offset: [f64; 3],
    pub span: u32,
    pub max_depth: u32,
    /// Points a node may hold without being subdivided.
    pub leaf_points: usize,
}

impl BuildOptions {
    pub fn new(cube: Bounds, scale: [f64; 3], offset: [f64; 3]) -> Self {
        Self {
            cube,
            scale,
            offset,
            span: DEFAULT_SPAN,
            max_depth: MAX_DEPTH,
            leaf_points: DEFAULT_LEAF_POINTS,
        }
    }

    /// Distance between neighbouring points at the root.
    ///
    /// The number every format states in its manifest, under three names:
    /// Potree's `spacing`, COPC's `spacing`, EPT's `span` (which states the
    /// grid instead and means the same thing).
    pub fn root_spacing(&self) -> f64 {
        self.cube.longest_edge() / f64::from(self.span)
    }
}

/// One finished node: its key and the records that stay in it.
pub struct BuiltNode {
    pub key: OctreeKey,
    pub records: Vec<u8>,
}

impl BuiltNode {
    pub fn point_count(&self, stride: usize) -> usize {
        self.records.len() / stride
    }
}

/// Where finished nodes go.
///
/// A callback rather than a returned collection: the whole cloud never exists
/// in one place, and a writer that took a `Vec<BuiltNode>` would put it there.
pub trait NodeSink {
    fn node(&mut self, node: BuiltNode) -> Result<()>;
}

impl<F: FnMut(BuiltNode) -> Result<()>> NodeSink for F {
    fn node(&mut self, node: BuiltNode) -> Result<()> {
        self(node)
    }
}

/// Subdivide `records` under `key`, emitting every node.
///
/// `stop_at` is the level the recursion refuses to emit: nodes at or below it
/// are left in `leftover` instead. The in-memory build passes `None` and gets
/// the whole subtree; the chunked build uses it to hand the deeper levels to
/// somebody else.
pub fn build_subtree(
    records: Vec<u8>,
    key: OctreeKey,
    stride: usize,
    options: &BuildOptions,
    sink: &mut dyn NodeSink,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let (kept, children) = partition(&records, key, stride, options);
    // The parent is emitted before its children, always. Every consumer of this
    // — a hierarchy page, a chunk table, a streaming viewer — wants a parent to
    // exist by the time a child refers to it.
    sink.node(BuiltNode { key, records: kept })?;
    drop(records);

    for (index, bucket) in children.into_iter().enumerate() {
        if bucket.is_empty() {
            continue;
        }
        build_subtree(bucket, key.child(index as u8), stride, options, sink)?;
    }
    Ok(())
}

/// Split one node's input into what it keeps and eight child buckets.
pub fn partition(
    records: &[u8],
    key: OctreeKey,
    stride: usize,
    options: &BuildOptions,
) -> (Vec<u8>, [Vec<u8>; 8]) {
    let mut children: [Vec<u8>; 8] = Default::default();
    let mut kept = Vec::new();

    // A node takes everything when there is little enough of it, and at the
    // depth limit whatever is left. The first is the leaf rule; the second is
    // the rail that stops two points at one coordinate — a scanner that stood
    // still — from recursing forever, since they never separate.
    let count = records.len() / stride;
    if count <= options.leaf_points || key.level >= options.max_depth {
        kept.extend_from_slice(records);
        return (kept, children);
    }

    let box_ = key.bounds(&options.cube);
    let size = box_.size();
    let span = f64::from(options.span);
    // A degenerate axis — a perfectly flat scan, or a cloud of one point —
    // would divide by zero. Treating it as a single cell is right: there is
    // nothing to separate along it.
    let cell = [
        if size[0] > 0.0 { size[0] / span } else { f64::INFINITY },
        if size[1] > 0.0 { size[1] / span } else { f64::INFINITY },
        if size[2] > 0.0 { size[2] / span } else { f64::INFINITY },
    ];
    let center = box_.center();
    let limit = options.span - 1;

    let mut occupied: HashSet<u32> = HashSet::with_capacity(records.len() / stride / 4 + 16);

    for record in records.chunks_exact(stride) {
        let raw = position(record);
        let p = [
            dequantize(raw[0], options.scale[0], options.offset[0]),
            dequantize(raw[1], options.scale[1], options.offset[1]),
            dequantize(raw[2], options.scale[2], options.offset[2]),
        ];

        let cx = cell_index(p[0], box_.min[0], cell[0], limit);
        let cy = cell_index(p[1], box_.min[1], cell[1], limit);
        let cz = cell_index(p[2], box_.min[2], cell[2], limit);
        let index = cx + options.span * (cy + options.span * cz);

        if occupied.insert(index) {
            kept.extend_from_slice(record);
        } else {
            children[child_index(center, p) as usize].extend_from_slice(record);
        }
    }

    (kept, children)
}

#[inline]
fn cell_index(value: f64, min: f64, cell: f64, limit: u32) -> u32 {
    if !cell.is_finite() {
        return 0;
    }
    let raw = (value - min) / cell;
    if !raw.is_finite() || raw < 0.0 {
        return 0;
    }
    (raw as u32).min(limit)
}

/// The cube to index a cloud in, given the extent of its points.
///
/// See [`Bounds::index_cube`] for why it is anchored at the minimum rather than
/// centred, and why it is not grown by an epsilon. On the lion scan this
/// reproduces PotreeConverter's own `boundingBox` and `spacing` exactly.
pub fn indexing_cube(extent: &Bounds) -> Bounds {
    extent.index_cube()
}

/// A record layout's stride, for callers that hold only the layout.
pub fn stride_of(layout: &RecordLayout) -> usize {
    layout.stride()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{set_position, quantize};

    fn options() -> BuildOptions {
        let mut o = BuildOptions::new(
            Bounds::new([0.0; 3], [128.0; 3]),
            [0.01, 0.01, 0.01],
            [0.0; 3],
        );
        o.span = 128;
        // The leaf rule is what the shipped defaults use and what
        // `the_leaf_rule_stops_the_recursion` covers. These tests are about the
        // sampling underneath it, so it is turned down to nothing: a node keeps
        // a point only by winning a cell.
        o.leaf_points = 1;
        o
    }

    /// One record per point, position only. The builder reads nothing else.
    fn records(points: &[[f64; 3]], options: &BuildOptions) -> Vec<u8> {
        let mut out = vec![0u8; points.len() * 12];
        for (i, p) in points.iter().enumerate() {
            set_position(
                &mut out[i * 12..i * 12 + 12],
                [
                    quantize(p[0], options.scale[0], options.offset[0]),
                    quantize(p[1], options.scale[1], options.offset[1]),
                    quantize(p[2], options.scale[2], options.offset[2]),
                ],
            );
        }
        out
    }

    #[test]
    fn every_point_lands_in_exactly_one_node() {
        let options = options();
        // A grid dense enough that the root's cells fill several times over.
        let mut points = Vec::new();
        for x in 0..40 {
            for y in 0..40 {
                for z in 0..4 {
                    points.push([x as f64 * 0.4, y as f64 * 0.4, z as f64 * 0.4]);
                }
            }
        }
        let total = points.len();
        let input = records(&points, &options);

        let mut counted = 0usize;
        let mut nodes = 0usize;
        build_subtree(input, OctreeKey::ROOT, 12, &options, &mut |node: BuiltNode| {
            counted += node.point_count(12);
            nodes += 1;
            Ok(())
        })
        .unwrap();

        // Conservation is the property the whole converter rests on: sampling
        // moves points between levels and must never drop or duplicate one.
        assert_eq!(counted, total);
        assert!(nodes > 1, "the input is denser than one node's grid");
    }

    #[test]
    fn a_node_holds_at_most_one_point_per_cell() {
        let options = options();
        // Sixteen points inside one cell of the root grid: the cell is 1 unit
        // across, so all of these compete for it and exactly one wins.
        let points: Vec<[f64; 3]> = (0..16).map(|i| [0.1 + i as f64 * 0.02, 0.1, 0.1]).collect();
        let input = records(&points, &options);
        let (kept, children) = partition(&input, OctreeKey::ROOT, 12, &options);
        assert_eq!(kept.len() / 12, 1);
        assert_eq!(children.iter().map(|c| c.len() / 12).sum::<usize>(), 15);
    }

    #[test]
    fn the_leaf_rule_stops_the_recursion_where_a_node_can_hold_everything() {
        let mut options = options();
        options.leaf_points = 500;
        // Enough points to fill the root's grid several times over, and few
        // enough that one node can hold them all.
        let points: Vec<[f64; 3]> = (0..400)
            .map(|i| [(i % 20) as f64 * 0.05, (i / 20) as f64 * 0.05, 0.0])
            .collect();
        let input = records(&points, &options);

        let mut nodes = 0;
        let mut counted = 0;
        build_subtree(input, OctreeKey::ROOT, 12, &options, &mut |node: BuiltNode| {
            nodes += 1;
            counted += node.point_count(12);
            Ok(())
        })
        .unwrap();
        assert_eq!(nodes, 1, "the whole input fits in one node");
        assert_eq!(counted, points.len());
    }

    #[test]
    fn duplicate_points_stop_at_the_depth_limit_instead_of_recursing_forever() {
        let mut options = options();
        options.max_depth = 4;
        let points = vec![[5.0, 5.0, 5.0]; 100];
        let input = records(&points, &options);

        let mut deepest = 0;
        let mut counted = 0;
        build_subtree(input, OctreeKey::ROOT, 12, &options, &mut |node: BuiltNode| {
            deepest = deepest.max(node.key.level);
            counted += node.point_count(12);
            Ok(())
        })
        .unwrap();
        assert_eq!(counted, 100);
        assert_eq!(deepest, 4);
    }
}
