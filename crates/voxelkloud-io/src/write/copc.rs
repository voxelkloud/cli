//! The COPC writer.
//!
//! One LAS 1.4 file whose laszip chunks are the octree's nodes. The format is
//! almost entirely LAS — what makes it COPC is a 160-byte VLR at the front
//! saying where the root cube and the hierarchy are, and an EVLR at the back
//! holding pages of 32-byte entries.
//!
//! **Ours, deliberately.** `copc-rs` ships a writer and its level distribution
//! is documented as wrong; PDAL's is correct and is a C++ dependency with a
//! different build for every platform. Writing this is a few hundred lines and
//! makes the recommended output of the toolchain something this project can
//! fix on a Tuesday.
//!
//! The order of operations is forced by the format and worth stating: the
//! laszip VLR has to be written before the points it describes, the chunk table
//! after them, the hierarchy after that (it holds file offsets), and the header
//! last of all — because until the hierarchy is written, its offset is not
//! known. So the header is written twice, and the second one is the truth.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use laz::{LasZipCompressor, LazItemRecordBuilder, LazItemType, LazVlrBuilder};

use crate::build::{BuiltNode, NodeSink};
use crate::error::{Error, Result};
use crate::las::copc::{
    COPC_ENTRY_SIZE, COPC_HIERARCHY_RECORD_ID, COPC_INFO_RECORD_ID, COPC_INFO_SIZE, COPC_USER_ID,
};
use crate::octree::OctreeKey;
use crate::record::at;

use super::las_write::{
    patch_header, projection_vlrs, write_evlr, write_f64, write_i32, write_u64, write_vlr,
    OutHeader, OutVlr, HEADER_SIZE, VLR_HEADER_SIZE,
};
use super::{WriteOptions, WriteReport};

/// Levels of the tree one hierarchy page covers before it defers to another.
///
/// Four levels is at most 1 + 8 + 64 + 512 entries, so 18 KB — one read that
/// opens most of a cloud, and small enough that a viewer showing the top of a
/// deep tree does not pull the whole index. The spec fixes nothing here; this
/// is the trade every implementation makes for itself.
const PAGE_LEVELS: u32 = 4;

/// Nodes a subtree must hold before it is worth a page of its own.
///
/// Paging by depth alone made 417 pages for an 832-node tree: every node at the
/// cut got one, most of them holding a handful of entries, and reading the
/// hierarchy became 418 round trips. A page is a request, so it has to buy more
/// than it costs.
const MIN_PAGE_NODES: usize = 32;

struct NodeRef {
    offset: u64,
    size: u64,
    count: u64,
}

/// Generic over where the bytes go.
///
/// A file on a workstation, and a `Cursor<Vec<u8>>` in a browser tab — which is
/// the whole of what "convert without uploading" needs from this side. The COPC
/// layout is seek-heavy by construction — the header is written twice, because
/// until the hierarchy exists its offset is not known — so the bound is
/// `Write + Seek` rather than `Write` alone: a socket cannot be a sink here, and
/// pretending otherwise would produce a file with a lying header. `Send + Sync`
/// comes from laz-rs, which holds the sink inside a boxed compressor it declares
/// thread safe.
pub struct CopcWriter<W: Write + Seek + Send + Sync + 'static> {
    compressor: LasZipCompressor<'static, W>,
    options: WriteOptions,
    /// Where the current chunk starts.
    at: u64,
    nodes: Vec<(OctreeKey, NodeRef)>,
    points: u64,
    depth: u32,
    gps_time: [f64; 2],
    vlr_count: u32,
    offset_to_point_data: u32,
    /// Whether the projection went in as WKT, which the header has to agree
    /// with when it is written for the second time.
    wkt: bool,
    path: String,
}

impl CopcWriter<BufWriter<File>> {
    /// Write to a file.
    pub fn create(path: &Path, options: WriteOptions) -> Result<Self> {
        let file = BufWriter::with_capacity(1 << 20, File::create(path)?);
        Self::new(file, options, path.display().to_string())
    }
}

impl<W: Write + Seek + Send + Sync + 'static> CopcWriter<W> {
    /// Write to anything seekable. `label` is for the report and for messages.
    pub fn new(sink: W, options: WriteOptions, label: String) -> Result<Self> {
        let stride = options.stride();
        let extra = options.layout.extra;

        // The laszip items are the record's fields, in order. `Point14` is the
        // format 6/7/8 core; colour, near infrared and the extra bytes are
        // separate items, and getting the set wrong produces a file that
        // compresses and cannot be read back.
        let mut items = LazItemRecordBuilder::new();
        items.add_item(LazItemType::Point14);
        if options.layout.has_color() {
            if options.layout.has_nir() {
                items.add_item(LazItemType::RGBNIR14);
            } else {
                items.add_item(LazItemType::RGB14);
            }
        }
        if extra > 0 {
            items.add_item(LazItemType::Byte14(extra as u16));
        }
        // Variable-size chunks: a COPC node is a chunk, and nodes hold whatever
        // the sampling gave them.
        let laz_vlr = LazVlrBuilder::new(items.build())
            .with_variable_chunk_size()
            .build();

        let mut laszip_vlr = Vec::new();
        laz_vlr
            .write_to(&mut laszip_vlr)
            .map_err(|e| Error::Codec(format!("laszip VLR: {e}")))?;
        // The compressor takes the vlr by value; the bytes above are the copy
        // that goes in the file.
        let laz_vlr_for_compressor = laz_vlr;

        let mut vlrs = vec![
            // The copc VLR must be first, immediately after the header: that is
            // what lets a reader identify the file from one ranged GET. Its
            // contents are patched at the end.
            OutVlr::new(
                COPC_USER_ID,
                COPC_INFO_RECORD_ID,
                "COPC info VLR",
                vec![0u8; COPC_INFO_SIZE],
            ),
            OutVlr::new(
                crate::las::LASZIP_USER_ID,
                crate::las::LASZIP_RECORD_ID,
                "laszip variable chunks",
                laszip_vlr,
            ),
        ];
        // The source's own projection records, unchanged. `wkt` is whether one
        // of them is the WKT the global encoding bit promises.
        let (projection, wkt) = projection_vlrs(&options.projection_vlrs);
        vlrs.extend(projection);
        if !options.layout.extra_vlr.is_empty() {
            vlrs.push(OutVlr::new(
                "LASF_Spec",
                4,
                "Extra Bytes",
                options.layout.extra_vlr.clone(),
            ));
        }

        let offset_to_point_data =
            (HEADER_SIZE + vlrs.iter().map(OutVlr::size).sum::<usize>()) as u32;

        let mut file = sink;
        let header = OutHeader {
            point_format: options.layout.format,
            point_size: stride as u16,
            compressed: true,
            point_count: 0,
            scale: options.scale,
            offset: options.offset,
            min: options.extent.min,
            max: options.extent.max,
            offset_to_point_data,
            vlr_count: vlrs.len() as u32,
            evlr_offset: 0,
            evlr_count: 1,
            generator: options.generator.clone(),
            wkt,
            creation: options.creation,
            points_by_return: [0; 15],
        };
        file.write_all(&header.to_bytes())?;
        for vlr in &vlrs {
            write_vlr(&mut file, vlr)?;
        }

        let wkt_bit = wkt;
        let mut compressor = LasZipCompressor::new(file, laz_vlr_for_compressor)
            .map_err(|e| Error::Codec(format!("laszip compressor: {e}")))?;
        // Reserved here rather than on the first point, so the offset of the
        // first chunk is known before anything is compressed into it.
        compressor
            .reserve_offset_to_chunk_table()
            .map_err(|e| Error::Codec(format!("chunk table: {e}")))?;
        let at = compressor.get_mut().stream_position()?;

        Ok(Self {
            compressor,
            options,
            at,
            nodes: Vec::new(),
            points: 0,
            depth: 0,
            gps_time: [f64::INFINITY, f64::NEG_INFINITY],
            vlr_count: vlrs.len() as u32,
            offset_to_point_data,
            wkt: wkt_bit,
            path: label,
        })
    }

    /// Compress one node into its own chunk.
    pub fn write_node(&mut self, node: &BuiltNode) -> Result<()> {
        let stride = self.options.stride();
        let count = (node.records.len() / stride) as u64;
        if count == 0 {
            return Ok(());
        }
        if count > i32::MAX as u64 {
            return Err(Error::Unsupported(format!(
                "node {} holds {count} points and a COPC hierarchy entry counts them in an \
                 i32. Convert with a smaller span.",
                node.key.ept_name()
            )));
        }

        for record in node.records.chunks_exact(stride) {
            let time = f64::from_le_bytes(record[at::GPS_TIME..at::GPS_TIME + 8].try_into().unwrap());
            if time < self.gps_time[0] {
                self.gps_time[0] = time;
            }
            if time > self.gps_time[1] {
                self.gps_time[1] = time;
            }
        }

        self.compressor
            .compress_many(&node.records)
            .map_err(|e| Error::Codec(format!("compressing {}: {e}", node.key.ept_name())))?;
        self.compressor
            .finish_current_chunk()
            .map_err(|e| Error::Codec(format!("closing the chunk for {}: {e}", node.key.ept_name())))?;

        let now = self.compressor.get_mut().stream_position()?;
        self.nodes.push((
            node.key,
            NodeRef {
                offset: self.at,
                size: now - self.at,
                count,
            },
        ));
        self.at = now;
        self.points += count;
        self.depth = self.depth.max(node.key.level);
        Ok(())
    }

    /// Chunk table, hierarchy, and the header that finally tells the truth.
    ///
    /// Hands the sink back with the report: a caller writing to memory wants
    /// the bytes, and a caller writing to a file can drop it.
    pub fn finish(mut self) -> Result<(WriteReport, W)> {
        self.compressor
            .done()
            .map_err(|e| Error::Codec(format!("finishing the point data: {e}")))?;

        let Self {
            compressor,
            options,
            nodes,
            points,
            depth,
            gps_time,
            vlr_count,
            offset_to_point_data,
            wkt,
            path,
            ..
        } = self;
        let mut file = compressor.into_inner();

        let evlr_offset = file.stream_position()?;
        // The payload begins after the extended record's own 60-byte header,
        // and every page offset inside it is absolute — so the base has to
        // account for that header before a single page is serialized.
        let payload_at = evlr_offset + super::las_write::EVLR_HEADER_SIZE as u64;
        let (pages, root) = build_pages(&nodes, payload_at);

        write_evlr(
            &mut file,
            &OutVlr::new(
                COPC_USER_ID,
                COPC_HIERARCHY_RECORD_ID,
                "COPC hierarchy",
                pages,
            ),
        )?;

        // Now the copc VLR can be filled in: it names the root page, which did
        // not exist until a moment ago.
        let cube = options.cube;
        let center = cube.center();
        let mut info = vec![0u8; COPC_INFO_SIZE];
        write_f64(&mut info, 0, center[0]);
        write_f64(&mut info, 8, center[1]);
        write_f64(&mut info, 16, center[2]);
        write_f64(&mut info, 24, cube.longest_edge() * 0.5);
        write_f64(&mut info, 32, options.spacing);
        write_u64(&mut info, 40, root.0);
        write_u64(&mut info, 48, root.1);
        let gps = if gps_time[0].is_finite() {
            gps_time
        } else {
            [0.0, 0.0]
        };
        write_f64(&mut info, 56, gps[0]);
        write_f64(&mut info, 64, gps[1]);

        file.seek(SeekFrom::Start((HEADER_SIZE + VLR_HEADER_SIZE) as u64))?;
        file.write_all(&info)?;

        let header = OutHeader {
            point_format: options.layout.format,
            point_size: options.stride() as u16,
            compressed: true,
            point_count: points,
            scale: options.scale,
            offset: options.offset,
            min: options.extent.min,
            max: options.extent.max,
            offset_to_point_data,
            vlr_count,
            evlr_offset,
            evlr_count: 1,
            generator: options.generator.clone(),
            wkt,
            creation: options.creation,
            points_by_return: [0; 15],
        };
        // The length, before the header patch seeks away and back: everything
        // written so far was appended, so the position IS the size — and a
        // `Cursor<Vec<u8>>` has no metadata to ask.
        let bytes = file.stream_position()?;
        patch_header(&mut file, &header)?;
        file.flush()?;

        Ok((
            WriteReport {
                nodes: nodes.len() as u64,
                points,
                depth,
                bytes,
                path,
            },
            file,
        ))
    }
}

/// Serialize the hierarchy into pages, returning the payload and the root
/// page's absolute offset and size.
///
/// Children before parents, because a page reference has to carry the offset of
/// a page that already exists.
fn build_pages(nodes: &[(OctreeKey, NodeRef)], base: u64) -> (Vec<u8>, (u64, u64)) {
    let index: HashMap<OctreeKey, &NodeRef> =
        nodes.iter().map(|(key, node)| (*key, node)).collect();

    // Every key the hierarchy will describe, WRITTEN OR NOT. The parent chain
    // is walked up from each written node, because out of core the build can
    // produce a cell's subtree without ever producing the node above it — the
    // coarse levels come from a sample of the whole cloud, and a sample can
    // miss a cell that later turns out to hold points. A hierarchy is a tree
    // reached from its root, so a gap in the chain does not lose one entry, it
    // loses the whole subtree under it.
    let mut children: HashMap<OctreeKey, Vec<OctreeKey>> = HashMap::new();
    let mut all: HashSet<OctreeKey> = index.keys().copied().collect();
    let mut pending: Vec<OctreeKey> = index.keys().copied().collect();
    while let Some(key) = pending.pop() {
        if let Some(parent) = key.parent() {
            children.entry(parent).or_default().push(key);
            if all.insert(parent) {
                pending.push(parent);
            }
        }
    }
    for list in children.values_mut() {
        list.sort();
    }

    // Subtree sizes, so the cut can ask whether a page is worth it.
    let mut sizes: HashMap<OctreeKey, usize> = HashMap::new();
    let mut order: Vec<OctreeKey> = all.iter().copied().collect();
    order.sort_by_key(|key| std::cmp::Reverse(key.level));
    for key in order {
        let own = 1 + children
            .get(&key)
            .map(|list| list.iter().map(|c| sizes.get(c).copied().unwrap_or(0)).sum())
            .unwrap_or(0);
        sizes.insert(key, own);
    }

    let mut buffer = Vec::new();
    // THE SHALLOWEST node, not the first one written. In core the builder emits
    // a parent before its children and the two are the same node; out of core
    // it builds each cell's subtree first and fills in the levels above them
    // afterwards, so the first arrival is several levels down. Taking it as the
    // root produced a file whose hierarchy described one subtree and dropped
    // every other node — on a 241M-point cloud, 181 entries out of 18,545 and
    // 0.98% of the points reachable, with the points themselves all present and
    // correct in the chunks nothing pointed at.
    let root_key = all
        .iter()
        .copied()
        .min_by_key(|key| (key.level, key.x, key.y, key.z))
        .unwrap_or(OctreeKey::ROOT);
    let root = emit_page(root_key, &index, &children, &sizes, base, &mut buffer);
    (buffer, root)
}

/// Serialize the page rooted at `key`, and every page below it.
fn emit_page(
    key: OctreeKey,
    index: &HashMap<OctreeKey, &NodeRef>,
    children: &HashMap<OctreeKey, Vec<OctreeKey>>,
    sizes: &HashMap<OctreeKey, usize>,
    base: u64,
    buffer: &mut Vec<u8>,
) -> (u64, u64) {
    let mut entries: Vec<[u8; COPC_ENTRY_SIZE]> = Vec::new();
    let mut queue = vec![(key, 0u32)];

    while let Some((node, depth)) = queue.pop() {
        let subtree = sizes.get(&node).copied().unwrap_or(1);
        if depth >= PAGE_LEVELS && subtree >= MIN_PAGE_NODES {
            // Deep enough, and big enough to be worth a request: this subtree
            // gets its own page, which has to exist before this one can point
            // at it.
            let (offset, size) = emit_page(node, index, children, sizes, base, buffer);
            entries.push(entry(node, offset, size, -1));
            continue;
        }
        if let Some(node_ref) = index.get(&node) {
            entries.push(entry(
                node,
                node_ref.offset,
                node_ref.size,
                node_ref.count as i64,
            ));
        } else if children.contains_key(&node) {
            // A node the build never wrote but whose descendants it did. COPC
            // has a spelling for exactly this — offset, size and count all
            // zero — and it is what keeps the chain walkable.
            entries.push(entry(node, 0, 0, 0));
        }
        for child in children.get(&node).into_iter().flatten() {
            queue.push((*child, depth + 1));
        }
    }

    let offset = base + buffer.len() as u64;
    for e in &entries {
        buffer.extend_from_slice(e);
    }
    (offset, (entries.len() * COPC_ENTRY_SIZE) as u64)
}

/// One 32-byte hierarchy entry. `count` of -1 marks a page reference.
fn entry(key: OctreeKey, offset: u64, size: u64, count: i64) -> [u8; COPC_ENTRY_SIZE] {
    let mut e = [0u8; COPC_ENTRY_SIZE];
    write_i32(&mut e, 0, key.level as i32);
    write_i32(&mut e, 4, key.x as i32);
    write_i32(&mut e, 8, key.y as i32);
    write_i32(&mut e, 12, key.z as i32);
    write_u64(&mut e, 16, offset);
    write_i32(&mut e, 24, size as i32);
    write_i32(&mut e, 28, count as i32);
    e
}

impl<W: Write + Seek + Send + Sync + 'static> NodeSink for CopcWriter<W> {
    fn node(&mut self, node: BuiltNode) -> Result<()> {
        self.write_node(&node)
    }
}
