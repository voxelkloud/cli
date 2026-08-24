//! The Potree v2 writer.
//!
//! Three files: `metadata.json`, `hierarchy.bin`, `octree.bin`. The tree is the
//! same tree COPC gets; what changes is the record — Potree unpacks the bit
//! fields LAS packs, so a viewer can ask for `"classification"` by name — and
//! the index, which is a file of 22-byte records in level order rather than
//! pages of 32-byte entries.
//!
//! **The attribute set is PotreeConverter's, on purpose.** A Potree v2 cloud
//! exists to be read by Potree and by everything written against it, and an
//! attribute list of our own would be a file that only we can use. That costs
//! one thing and it is stated in a warning: the LAS 1.4 scan angle is a signed
//! count of 0.006 degrees and PotreeConverter's `"scan angle rank"` is a single
//! signed byte of whole degrees, so writing this format rounds it.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::build::{BuiltNode, NodeSink};
use crate::error::Result;
use crate::octree::OctreeKey;
use crate::record::at;
use crate::warning::Warning;

use super::morton;
use super::{WriteOptions, WriteReport};

/// How a node's points are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PotreeEncoding {
    /// One interleaved record per point, exactly as the manifest describes it.
    Default,
    /// Per-attribute blocks, positions and colours morton-coded, the whole node
    /// then brotli-compressed.
    ///
    /// Two thirds smaller on the wire and the only encoding that needs a
    /// decoder no browser ships for free — which is why
    /// `@voxelkloud/format-potree` resolves one in tiers and vendors a fallback.
    Brotli,
}

impl PotreeEncoding {
    fn name(self) -> &'static str {
        match self {
            Self::Default => "DEFAULT",
            Self::Brotli => "BROTLI",
        }
    }
}

/// Brotli quality. 6 is where the curve flattens: 11 spends minutes per
/// hundred megabytes for a few per cent, and this runs over whole surveys.
const BROTLI_QUALITY: u32 = 6;
/// Window size, in log2 bytes. 22 is the largest the format defines and costs
/// 4 MB of encoder memory, which against a node of a few hundred kilobytes is
/// free.
const BROTLI_WINDOW: u32 = 22;

/// Bytes of one `hierarchy.bin` record.
const RECORD: usize = 22;

/// Levels a hierarchy chunk covers before the tree continues in another.
///
/// PotreeConverter picks this per cloud; autzen's is 4. Five keeps the first
/// read small — at most `1 + 8 + ... + 8^4` records, so 105 KB — while leaving
/// most real trees at two levels of chunk.
pub const STEP_SIZE: u32 = 5;

/// One attribute of the written record.
///
/// `source` says where its bytes come from in the canonical LAS record. Most
/// are a straight copy; the four that are not are the bit runs LAS packs and
/// Potree does not.
struct Field {
    name: String,
    kind: &'static str,
    elements: usize,
    size: usize,
    source: Source,
}

enum Source {
    /// Copy `size` bytes from this offset of the canonical record.
    Copy(usize),
    /// The low nibble of the returns byte.
    ReturnNumber,
    /// The high nibble of the same.
    NumberOfReturns,
    /// The low four bits of the flags byte.
    ClassificationFlags,
    /// The `int16` scan angle, rounded into a signed byte of whole degrees.
    ScanAngleRank,
}

fn field(name: &str, kind: &'static str, elements: usize, size: usize, source: Source) -> Field {
    Field { name: name.to_string(), kind, elements, size, source }
}

/// The record Potree v2 will hold, in order.
///
/// This is PotreeConverter's own list, and following it exactly is the point:
/// a Potree v2 cloud exists to be read by everything written against Potree,
/// and an attribute list of our own would be a file only we can use. Its two
/// shapes are visible in the fixtures this repo vendors — autzen, from a LAS
/// 1.2 source, carries `"scan angle rank"` as a byte and no classification
/// flags; lion, from a 1.4 source, carries `"scan angle"` as an `int16` and
/// does. Even the field ORDER differs between the two, which is why this
/// branches rather than sorting.
fn fields(
    color: bool,
    gps_time: bool,
    legacy: bool,
    extra: &[crate::las::extra_bytes::ExtraByteField],
) -> Vec<Field> {
    let mut out = vec![
        field("position", "int32", 3, 12, Source::Copy(at::X)),
        field("intensity", "uint16", 1, 2, Source::Copy(at::INTENSITY)),
        field("return number", "uint8", 1, 1, Source::ReturnNumber),
        field("number of returns", "uint8", 1, 1, Source::NumberOfReturns),
    ];
    if legacy {
        out.push(field("classification", "uint8", 1, 1, Source::Copy(at::CLASSIFICATION)));
        out.push(field("scan angle rank", "uint8", 1, 1, Source::ScanAngleRank));
        out.push(field("user data", "uint8", 1, 1, Source::Copy(at::USER_DATA)));
    } else {
        out.push(field("classification flags", "uint8", 1, 1, Source::ClassificationFlags));
        out.push(field("classification", "uint8", 1, 1, Source::Copy(at::CLASSIFICATION)));
        out.push(field("user data", "uint8", 1, 1, Source::Copy(at::USER_DATA)));
        out.push(field("scan angle", "int16", 1, 2, Source::Copy(at::SCAN_ANGLE)));
    }
    out.push(field("point source id", "uint16", 1, 2, Source::Copy(at::POINT_SOURCE_ID)));
    // Eight bytes of zeros per point is a fifth of this record. A scan with no
    // time should not carry it, and a reader that meets a `min == max == 0`
    // gps-time warns about a degenerate range — correctly, and about a field
    // that should not have been written.
    if gps_time {
        out.push(field("gps-time", "double", 1, 8, Source::Copy(at::GPS_TIME)));
    }
    if color {
        out.push(field("rgb", "uint16", 3, 6, Source::Copy(at::RGB)));
    }
    // Dimensions the LAS spec never named, carried through under the names
    // their own VLR gave them. Dropping them would lose the one thing a
    // converter is least able to reconstruct.
    for f in extra {
        let Some(kind) = f.kind else { continue };
        out.push(field(
            &f.name,
            kind.name(),
            f.num_elements,
            f.byte_size,
            Source::Copy(f.byte_offset),
        ));
    }
    out
}

/// One node as it landed in `octree.bin`.
///
/// Public because `hierarchy.bin` has two producers: this writer, which builds
/// a tree, and `crate::optimize`, which re-encodes one that already exists. The
/// 22-byte record and the chunking rule are stated once, here.
pub struct Written {
    pub key: OctreeKey,
    pub byte_offset: u64,
    pub byte_size: u64,
    pub points: u32,
}

pub struct PotreeWriter {
    dir: PathBuf,
    /// Taken and dropped by `finish`, which needs the file closed before it
    /// reads the size back and still needs the rest of the writer's state.
    octree: Option<BufWriter<File>>,
    options: WriteOptions,
    encoding: PotreeEncoding,
    fields: Vec<Field>,
    stride: usize,
    /// Scratch for the planar layout, reused between nodes.
    planar: Vec<u8>,
    written: Vec<Written>,
    at: u64,
    points: u64,
    depth: u32,
    /// Min and max per attribute, in the attribute's own domain.
    intensity: [u16; 2],
    gps: [f64; 2],
    warnings: Vec<Warning>,
    scan_angle_clipped: bool,
}

impl PotreeWriter {
    pub fn create(dir: &Path, options: WriteOptions) -> Result<Self> {
        Self::create_with(dir, options, PotreeEncoding::Default)
    }

    pub fn create_with(
        dir: &Path,
        options: WriteOptions,
        encoding: PotreeEncoding,
    ) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let fields = fields(
            options.layout.has_color(),
            options.has_gps_time,
            options.legacy_fields,
            &options.layout.extra_fields,
        );
        let stride = fields.iter().map(|f| f.size).sum();
        Ok(Self {
            dir: dir.to_path_buf(),
            octree: Some(BufWriter::with_capacity(
                1 << 20,
                File::create(dir.join("octree.bin"))?,
            )),
            options,
            encoding,
            fields,
            stride,
            planar: Vec::new(),
            written: Vec::new(),
            at: 0,
            points: 0,
            depth: 0,
            intensity: [u16::MAX, 0],
            gps: [f64::INFINITY, f64::NEG_INFINITY],
            warnings: Vec::new(),
            scan_angle_clipped: false,
        })
    }

    pub fn write_node(&mut self, node: &BuiltNode) -> Result<()> {
        let source_stride = self.options.stride();
        let count = node.records.len() / source_stride;
        if count == 0 {
            return Ok(());
        }

        let mut out = vec![0u8; count * self.stride];
        for (i, record) in node.records.chunks_exact(source_stride).enumerate() {
            self.encode(record, &mut out[i * self.stride..(i + 1) * self.stride]);
        }
        if self.encoding == PotreeEncoding::Brotli {
            out = self.compress(&out, count)?;
        }
        if let Some(octree) = self.octree.as_mut() {
            octree.write_all(&out)?;
        }

        self.written.push(Written {
            key: node.key,
            byte_offset: self.at,
            byte_size: out.len() as u64,
            points: count as u32,
        });
        self.at += out.len() as u64;
        self.points += count as u64;
        self.depth = self.depth.max(node.key.level);
        Ok(())
    }

    /// The interleaved records of one node, as BROTLI stores them.
    ///
    /// Two transforms, in this order. First the record is turned inside out:
    /// instead of `numPoints` records of every attribute, one block per
    /// attribute of `numPoints` values — which is what makes the brotli window
    /// see a column of similar bytes rather than a repeating struct. Then
    /// position and colour are replaced by their morton codes, 16 and 8 bytes
    /// against 12 and 6, because interleaving the bits of three components puts
    /// the bits that vary together next to each other.
    ///
    /// The second transform makes the block *bigger* and the file smaller. That
    /// is the whole trick, and it is why the block widths here are not the
    /// widths the manifest declares.
    fn compress(&mut self, records: &[u8], count: usize) -> Result<Vec<u8>> {
        let planar_stride: usize = self.fields.iter().map(brotli_width).sum();
        self.planar.clear();
        self.planar.resize(planar_stride * count, 0);

        let mut block = 0usize;
        let mut at_in = 0usize;
        for f in &self.fields {
            let width = brotli_width(f);
            let base = block * count;
            for i in 0..count {
                let from = &records[i * self.stride + at_in..i * self.stride + at_in + f.size];
                let to = &mut self.planar[base + i * width..base + i * width + width];
                match f.name.as_str() {
                    "position" => {
                        // Cloud-relative grid coordinates, the same integers
                        // DEFAULT stores. They are non-negative because the
                        // origin is the extent's minimum.
                        let x = i32::from_le_bytes(from[0..4].try_into().unwrap()) as u32;
                        let y = i32::from_le_bytes(from[4..8].try_into().unwrap()) as u32;
                        let z = i32::from_le_bytes(from[8..12].try_into().unwrap()) as u32;
                        to.copy_from_slice(&morton::encode_position(x, y, z));
                    }
                    "rgb" => {
                        let r = u16::from_le_bytes(from[0..2].try_into().unwrap());
                        let g = u16::from_le_bytes(from[2..4].try_into().unwrap());
                        let b = u16::from_le_bytes(from[4..6].try_into().unwrap());
                        to.copy_from_slice(&morton::encode_color(r, g, b));
                    }
                    _ => to.copy_from_slice(from),
                }
            }
            block += width;
            at_in += f.size;
        }

        let mut out = Vec::with_capacity(self.planar.len() / 3);
        let mut input = std::io::Cursor::new(&self.planar);
        brotli::BrotliCompress(
            &mut input,
            &mut out,
            &brotli::enc::BrotliEncoderParams {
                quality: BROTLI_QUALITY as i32,
                lgwin: BROTLI_WINDOW as i32,
                ..Default::default()
            },
        )
        .map_err(|e| crate::error::Error::Codec(format!("brotli: {e}")))?;
        Ok(out)
    }

    /// One canonical LAS record into one Potree record.
    fn encode(&mut self, record: &[u8], out: &mut [u8]) {
        let mut at_out = 0;
        for f in &self.fields {
            let to = &mut out[at_out..at_out + f.size];
            match f.source {
                // Position needs no arithmetic: the canonical record is
                // already quantized to the scale and offset the manifest
                // declares.
                Source::Copy(from) => to.copy_from_slice(&record[from..from + f.size]),
                // The bit runs, unpacked. This is the difference between the
                // two records, and it is why a Potree viewer can colour by
                // return number without knowing how LAS packs one.
                Source::ReturnNumber => to[0] = record[at::RETURNS] & 0x0f,
                Source::NumberOfReturns => to[0] = record[at::RETURNS] >> 4,
                Source::ClassificationFlags => to[0] = record[at::FLAGS] & 0x0f,
                Source::ScanAngleRank => {
                    // 0.006-degree counts back to whole degrees. Lossy, and
                    // reached only for a legacy source, where the angle WAS
                    // whole degrees before this pipeline widened it.
                    let angle = i16::from_le_bytes(
                        record[at::SCAN_ANGLE..at::SCAN_ANGLE + 2].try_into().unwrap(),
                    );
                    let degrees = (f64::from(angle) * 0.006).round();
                    if !(-128.0..=127.0).contains(&degrees) {
                        self.scan_angle_clipped = true;
                    }
                    to[0] = (degrees.clamp(-128.0, 127.0) as i8) as u8;
                }
            }
            at_out += f.size;
        }

        let intensity =
            u16::from_le_bytes(record[at::INTENSITY..at::INTENSITY + 2].try_into().unwrap());
        self.intensity[0] = self.intensity[0].min(intensity);
        self.intensity[1] = self.intensity[1].max(intensity);
        if self.options.has_gps_time {
            let time =
                f64::from_le_bytes(record[at::GPS_TIME..at::GPS_TIME + 8].try_into().unwrap());
            if time < self.gps[0] {
                self.gps[0] = time;
            }
            if time > self.gps[1] {
                self.gps[1] = time;
            }
        }
    }

    pub fn finish(mut self) -> Result<(WriteReport, Vec<Warning>)> {
        if let Some(mut octree) = self.octree.take() {
            octree.flush()?;
        }

        if self.scan_angle_clipped {
            self.warnings.push(Warning::new(
                "scan-angle-narrowed",
                "attributes[scan angle rank]",
                "Potree v2 stores the scan angle as one signed byte of whole degrees. \
                 Angles past ±127° were clamped; the LAS 1.4 field they came from holds \
                 0.006° increments and keeps them.",
            ));
        }

        let hierarchy = self.write_hierarchy()?;
        let metadata = self.metadata(hierarchy.0, hierarchy.1);
        std::fs::write(
            self.dir.join("metadata.json"),
            format!("{}\n", serde_json::to_string_pretty(&metadata).unwrap_or_default()),
        )?;

        let bytes = [
            self.dir.join("octree.bin"),
            self.dir.join("hierarchy.bin"),
            self.dir.join("metadata.json"),
        ]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
        .sum();

        Ok((
            WriteReport {
                nodes: self.written.len() as u64,
                points: self.points,
                depth: self.depth,
                bytes,
                path: self.dir.display().to_string(),
            },
            self.warnings,
        ))
    }

    /// Write `hierarchy.bin`, returning the first chunk's size and the step.
    ///
    /// Chunks are level-ordered from a seed and cover `STEP_SIZE` levels. A
    /// node at the cut is written as a proxy record, whose offset and size name
    /// its own chunk — which is why the chunks are laid out deepest first: a
    /// proxy has to know where its chunk went before its own chunk is written.
    fn write_hierarchy(&self) -> Result<(u64, u32)> {
        let (bytes, first_chunk_size) = hierarchy_bytes(&self.written);
        std::fs::write(self.dir.join("hierarchy.bin"), &bytes)?;
        Ok((first_chunk_size, STEP_SIZE))
    }

    fn metadata(&self, first_chunk_size: u64, step_size: u32) -> Value {
        let extent = self.options.extent;
        let cube = self.options.cube;
        let attributes: Vec<Value> = self
            .fields
            .iter()
            .map(|f| {
                let (min, max) = self.domain(f);
                json!({
                    "name": f.name.clone(),
                    "description": "",
                    "size": f.size,
                    "numElements": f.elements,
                    "elementSize": f.size / f.elements,
                    "type": f.kind,
                    "min": min,
                    "max": max,
                })
            })
            .collect();

        json!({
            "version": "2.0",
            "name": self.dir.file_name().and_then(|n| n.to_str()).unwrap_or("cloud"),
            "description": "",
            "points": self.points,
            "projection": self.options.crs.as_ref().map(|c| c.raw.clone()).unwrap_or_default(),
            "hierarchy": {
                "firstChunkSize": first_chunk_size,
                "stepSize": step_size,
                "depth": self.depth,
            },
            "offset": self.options.offset,
            "scale": self.options.scale,
            "spacing": self.options.spacing,
            "boundingBox": { "min": cube.min, "max": cube.max },
            "encoding": self.encoding.name(),
            "attributes": attributes,
            // Not a Potree field. A cloud should be able to say what made it,
            // and no reader in this space rejects an unknown key.
            "generator": self.options.generator,
            "tightBoundingBox": { "min": extent.min, "max": extent.max },
        })
    }

    /// The stated domain of one attribute.
    ///
    /// Only the ones actually measured are stated; the rest carry the type's
    /// own range, which is what PotreeConverter writes and what a reader
    /// treating these as a colour ramp needs to be true rather than tight.
    fn domain(&self, field: &Field) -> (Vec<f64>, Vec<f64>) {
        match field.name.as_str() {
            "position" => (
                self.options.extent.min.to_vec(),
                self.options.extent.max.to_vec(),
            ),
            "intensity" => {
                if self.intensity[0] > self.intensity[1] {
                    (vec![0.0], vec![0.0])
                } else {
                    (
                        vec![f64::from(self.intensity[0])],
                        vec![f64::from(self.intensity[1])],
                    )
                }
            }
            "gps-time" => {
                if self.gps[0].is_finite() {
                    (vec![self.gps[0]], vec![self.gps[1]])
                } else {
                    (vec![0.0], vec![0.0])
                }
            }
            "rgb" => (vec![0.0; 3], vec![65535.0; 3]),
            "scan angle rank" => (vec![-128.0], vec![127.0]),
            "scan angle" => (vec![-30000.0], vec![30000.0]),
            _ => {
                let max = match field.kind {
                    "uint8" => 255.0,
                    "uint16" => 65535.0,
                    "uint32" => 4294967295.0,
                    _ => 0.0,
                };
                (vec![0.0; field.elements], vec![max; field.elements])
            }
        }
    }
}

/// The bytes one attribute occupies per point in a BROTLI block.
///
/// Position and colour are morton codes and are WIDER than the values they
/// replace — 16 against 12, and 8 against 6. The manifest still declares the
/// plain widths, because it describes the values and not the encoding, so a
/// reader has to know this rule rather than read it. `point-data-layout.ts`
/// carries the same table on the other side.
fn brotli_width(f: &Field) -> usize {
    match f.name.as_str() {
        "position" => 16,
        "rgb" => 8,
        _ => f.size,
    }
}

/// Serialize a whole `hierarchy.bin`, returning it and the root chunk's size.
///
/// Chunks are level-ordered from a seed and cover [`STEP_SIZE`] levels. A node
/// at the cut becomes a proxy record whose offset and size name its own chunk —
/// which is why the chunks are laid out deepest first: a proxy has to know
/// where its chunk went before its own chunk is written.
pub fn hierarchy_bytes(written: &[Written]) -> (Vec<u8>, u64) {
    let index: HashMap<OctreeKey, &Written> = written.iter().map(|w| (w.key, w)).collect();
    let mut children: HashMap<OctreeKey, Vec<OctreeKey>> = HashMap::new();
    for node in written {
        if let Some(parent) = node.key.parent() {
            children.entry(parent).or_default().push(node.key);
        }
    }
    for list in children.values_mut() {
        list.sort();
    }

    // The shallowest key, not the first node handed in. The tree writer emits
    // parents before children so the two agree; `optimize` hands its nodes in
    // FILE order, because reading a 372 MB octree.bin in the order it is stored
    // beats seeking per node — and under the old assumption that produced a
    // hierarchy seeded at whichever node happened to sit at offset zero, which
    // reads back as a one-node cloud.
    let root = written
        .iter()
        .map(|w| w.key)
        .min_by_key(|key| (key.level, key.x, key.y, key.z))
        .unwrap_or(OctreeKey::ROOT);
    let mut buffer = Vec::new();
    let (_, first_chunk_size) = write_chunk(root, &index, &children, &mut buffer);

    // The root chunk has to be at offset 0: the manifest states its size and
    // nothing states where it starts. Rotating the buffer moves every other
    // chunk forward by exactly the root's size, so the proxies are patched by
    // one fixed shift rather than the file being written twice.
    let root_start = buffer.len() as u64 - first_chunk_size;
    let mut out = Vec::with_capacity(buffer.len());
    out.extend_from_slice(&buffer[root_start as usize..]);
    out.extend_from_slice(&buffer[..root_start as usize]);
    shift_proxies(&mut out, first_chunk_size, root_start);
    (out, first_chunk_size)
}

/// Write the chunk seeded at `key`, and every chunk below it, into `buffer`.
///
/// Returns the chunk's offset and size. Deepest first: a proxy record carries
/// the offset of the chunk it names, so that chunk has to be placed already.
fn write_chunk(
    key: OctreeKey,
    index: &HashMap<OctreeKey, &Written>,
    children: &HashMap<OctreeKey, Vec<OctreeKey>>,
    buffer: &mut Vec<u8>,
) -> (u64, u64) {
    // Level order from the seed, which is the order the format requires:
    // record 0 is the seed, and each record's children follow in ascending
    // index. A reader reconstructs the tree from the child masks alone.
    let mut order: Vec<(OctreeKey, u32)> = Vec::new();
    let mut queue = std::collections::VecDeque::from([(key, 0u32)]);
    while let Some((node, depth)) = queue.pop_front() {
        order.push((node, depth));
        if depth + 1 >= STEP_SIZE {
            continue;
        }
        for child in children.get(&node).into_iter().flatten() {
            queue.push_back((*child, depth + 1));
        }
    }

    // The chunks the cut nodes need, placed before this one.
    let mut sub: HashMap<OctreeKey, (u64, u64)> = HashMap::new();
    for (node, depth) in &order {
        if *depth + 1 == STEP_SIZE && children.contains_key(node) {
            sub.insert(*node, write_chunk(*node, index, children, buffer));
        }
    }

    let offset = buffer.len() as u64;
    for (node, depth) in &order {
        let mut record = [0u8; RECORD];
        let mask = children
            .get(node)
            .map(|list| {
                list.iter()
                    .filter_map(|c| c.child_index())
                    .fold(0u8, |acc, index| acc | (1 << index))
            })
            .unwrap_or(0);

        if *depth + 1 == STEP_SIZE {
            if let Some((sub_offset, sub_size)) = sub.get(node) {
                // A proxy: type 2, and its offset and size name a chunk of
                // hierarchy.bin rather than a range of octree.bin. Its real
                // child mask lives at record 0 of that chunk.
                record[0] = 2;
                record[1] = 0;
                // The point count is written anyway, and the reference does the
                // same. A reader meets this record before it has fetched the
                // chunk behind it, and a zero here means a node that holds a
                // million points reports none until something expands it — on
                // autzen that is 1.8M points missing from any total taken
                // before the whole tree is resident. Record 0 of the chunk
                // states the same number, and a reader REPLACES rather than
                // adds, so the two cannot double-count.
                if let Some(w) = index.get(node) {
                    record[2..6].copy_from_slice(&w.points.to_le_bytes());
                }
                record[6..14].copy_from_slice(&sub_offset.to_le_bytes());
                record[14..22].copy_from_slice(&sub_size.to_le_bytes());
                buffer.extend_from_slice(&record);
                continue;
            }
        }

        let written = index.get(node);
        // Type 1 is "leaf" and type 0 "normal", and no reader branches on
        // either: the child mask is the only has-children signal. Writing the
        // honest value costs nothing and keeps a reader that does branch from
        // dropping the subtree.
        record[0] = if mask == 0 { 1 } else { 0 };
        record[1] = mask;
        if let Some(w) = written {
            record[2..6].copy_from_slice(&w.points.to_le_bytes());
            record[6..14].copy_from_slice(&w.byte_offset.to_le_bytes());
            record[14..22].copy_from_slice(&w.byte_size.to_le_bytes());
        }
        buffer.extend_from_slice(&record);
    }

    (offset, (order.len() * RECORD) as u64)
}

/// Move every proxy's target by the root chunk's size.
///
/// The chunks were laid out with the root last, because a proxy cannot be
/// written before the chunk it points at. The file needs the root first,
/// because the manifest states its size and not its position. Rotating the
/// buffer shifts every non-root chunk forward by exactly the root's size, so
/// each proxy target moves by the same amount.
fn shift_proxies(bytes: &mut [u8], root_size: u64, root_was_at: u64) {
    let mut at = 0;
    while at + RECORD <= bytes.len() {
        if bytes[at] == 2 {
            let target = u64::from_le_bytes(bytes[at + 6..at + 14].try_into().unwrap());
            // Targets that were before the root move forward by its size;
            // nothing was after it, since it was written last.
            let moved = if target < root_was_at {
                target + root_size
            } else {
                target - root_was_at
            };
            bytes[at + 6..at + 14].copy_from_slice(&moved.to_le_bytes());
        }
        at += RECORD;
    }
}

impl NodeSink for PotreeWriter {
    fn node(&mut self, node: BuiltNode) -> Result<()> {
        self.write_node(&node)
    }
}
