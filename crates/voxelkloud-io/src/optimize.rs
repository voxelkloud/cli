//! `optimize` — the same cloud, better bytes.
//!
//! Re-encoding, not reconverting. The tree stays exactly as it is: every node
//! keeps its key, its point count and its place, and what changes is how the
//! points inside it are stored. That distinction is the whole feature. Building
//! an octree over a hundred million points is minutes of work and produces a
//! *different* tree; this reads the one that exists and rewrites its payloads,
//! which is bounded by the size of the cloud and by nothing else.
//!
//! Two things it does, and both are things [`crate::cloud`]'s doctor complains
//! about on real deployments:
//!
//! 1. **DEFAULT to BROTLI.** Potree v2's second encoding is per-attribute
//!    blocks with morton-coded positions and colour, compressed. On the clouds
//!    here it is about a quarter of the size on the wire, and no reader needs
//!    changing — the manifest says which encoding it is.
//! 2. **Dropping attributes nobody reads.** A record carrying eight bytes of
//!    GPS time that every point sets to zero is a fifth of the file. The
//!    attribute list is the cloud's own; this only removes from it.
//!
//! **The intermediate form is planar, at natural widths.** A node is decoded
//! into one block per attribute — `numElements * elementSize` bytes per point —
//! and re-encoded from that. Interleaving those blocks is DEFAULT; morton-coding
//! position and colour and compressing is BROTLI. Nothing is decoded into
//! values, which is why an attribute this code has never heard of survives the
//! round trip untouched.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::attribute::{Attribute, AttributeRole};
use crate::error::{Error, Result};
use crate::format::potree::PotreeCloud;
use crate::format::{self, Cloud};
use crate::octree::OctreeKey;
use crate::source::{FileStore, Store};
use crate::warning::Warning;
use crate::write::morton;
use crate::write::potree::{hierarchy_bytes, PotreeEncoding, Written};

/// Bytes one attribute occupies per point in a BROTLI block.
///
/// Position and colour are morton codes and are *wider* than the values they
/// replace — 16 against 12, and 8 against 6. The manifest declares the plain
/// widths either way, because it describes the values and not the encoding, so
/// this rule has to be known rather than read.
fn brotli_width(attribute: &Attribute) -> usize {
    match attribute.role() {
        Some(AttributeRole::Position) => 16,
        Some(AttributeRole::Color) => 8,
        None => attribute.byte_size(),
    }
}

#[derive(Debug, Clone)]
pub struct OptimizeOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    /// `None` keeps whatever the source used.
    pub encoding: Option<PotreeEncoding>,
    /// Attribute names to leave out, verbatim as the manifest spells them.
    pub drop: Vec<String>,
    /// Brotli quality, when the output is BROTLI.
    pub quality: u32,
}

impl OptimizeOptions {
    pub fn new(input: PathBuf, output: PathBuf) -> Self {
        Self {
            input,
            output,
            encoding: None,
            drop: Vec::new(),
            quality: 6,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct OptimizeReport {
    pub nodes: u64,
    pub points: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// Bytes of one record before and after, for the human summary.
    pub record_before: usize,
    pub record_after: usize,
    pub encoding_before: String,
    pub encoding_after: String,
    pub dropped: Vec<String>,
    pub warnings: Vec<Warning>,
}

impl OptimizeReport {
    /// What fraction of the payload survived. `None` when there was none.
    pub fn ratio(&self) -> Option<f64> {
        (self.bytes_before > 0).then(|| self.bytes_after as f64 / self.bytes_before as f64)
    }
}

/// Re-encode the Potree v2 cloud at `options.input` into `options.output`.
pub fn optimize(
    options: &OptimizeOptions,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<OptimizeReport> {
    let store: Arc<dyn Store> = Arc::new(FileStore::new(&options.input));
    let label = options.input.display().to_string();
    let cloud = format::open(store.clone(), "", &label)?;
    let Cloud::Potree(potree) = cloud else {
        return Err(Error::Unsupported(format!(
            "{label} is not a Potree v2 cloud. `optimize` re-encodes one in place of its \
             payloads; for anything else, `convert` builds a new tree"
        )));
    };

    let mut report = OptimizeReport {
        encoding_before: potree.metadata.encoding.clone(),
        ..OptimizeReport::default()
    };
    let source_brotli = potree.metadata.encoding == "BROTLI";
    let target = options.encoding.unwrap_or(if source_brotli {
        PotreeEncoding::Brotli
    } else {
        PotreeEncoding::Default
    });
    report.encoding_after = match target {
        PotreeEncoding::Brotli => "BROTLI".to_string(),
        PotreeEncoding::Default => "DEFAULT".to_string(),
    };

    // Which attributes survive, in the manifest's own order. Position is not
    // droppable: a cloud without it is not a cloud.
    let attributes = &potree.info.attributes;
    let mut keep: Vec<&Attribute> = Vec::with_capacity(attributes.len());
    for attribute in attributes {
        let dropped = options.drop.iter().any(|name| name == &attribute.name);
        if dropped && attribute.role() == Some(AttributeRole::Position) {
            report.warnings.push(Warning::new(
                "position-kept",
                attribute.name.clone(),
                "Position cannot be dropped; every other request was honoured.",
            ));
        } else if dropped {
            report.dropped.push(attribute.name.clone());
            continue;
        }
        keep.push(attribute);
    }
    for name in &options.drop {
        if !attributes.iter().any(|a| &a.name == name) {
            report.warnings.push(Warning::new(
                "no-such-attribute",
                name.clone(),
                format!("The cloud has no attribute named {name:?}, so nothing was dropped for it."),
            ));
        }
    }

    report.record_before = attributes.iter().map(Attribute::byte_size).sum();
    report.record_after = keep.iter().map(|a| a.byte_size()).sum();

    let nodes = collect_nodes(&potree)?;
    let total: u64 = nodes.iter().map(|n| n.points as u64).sum();
    let octree = store.open("octree.bin")?;

    std::fs::create_dir_all(&options.output)?;
    let mut out_file = std::io::BufWriter::with_capacity(
        1 << 20,
        std::fs::File::create(options.output.join("octree.bin"))?,
    );
    let mut written: Vec<Written> = Vec::with_capacity(nodes.len());
    let mut at = 0u64;
    let mut done = 0u64;
    let mut planar: Vec<u8> = Vec::new();
    let mut negative_position = false;

    for node in &nodes {
        let count = node.points as usize;
        if count == 0 || node.byte_size == 0 {
            // A node with no payload of its own. 47 of autzen's are like this,
            // and they keep their place in the tree with a zero-length range.
            written.push(Written {
                key: node.key,
                byte_offset: at,
                byte_size: 0,
                points: node.points,
            });
            continue;
        }

        let bytes = octree.read_at(node.byte_offset, node.byte_size as usize)?;
        report.bytes_before += node.byte_size;
        decode_node(&bytes, count, attributes, source_brotli, &mut planar)?;

        let encoded = match target {
            PotreeEncoding::Default => encode_default(&planar, count, attributes, &keep),
            PotreeEncoding::Brotli => encode_brotli(
                &planar,
                count,
                attributes,
                &keep,
                options.quality,
                &mut negative_position,
            )?,
        };

        use std::io::Write;
        out_file.write_all(&encoded)?;
        written.push(Written {
            key: node.key,
            byte_offset: at,
            byte_size: encoded.len() as u64,
            points: node.points,
        });
        at += encoded.len() as u64;
        report.bytes_after += encoded.len() as u64;
        report.points += node.points as u64;
        done += node.points as u64;
        progress(done, total);
    }

    use std::io::Write as _;
    out_file.flush()?;
    report.nodes = written.len() as u64;

    if negative_position {
        report.warnings.push(Warning::new(
            "negative-position",
            "attributes[position]",
            "Some positions are negative integers, and BROTLI's morton coding has no sign: a \
             reader decodes them as very large positive values. The cloud's origin is not at \
             its minimum, which is what makes this possible — re-run `convert` on the source \
             to move it.",
        ));
    }

    let (hierarchy, first_chunk_size) = hierarchy_bytes(&written);
    std::fs::write(options.output.join("hierarchy.bin"), &hierarchy)?;
    let metadata = rewrite_metadata(&potree, &keep, &report, first_chunk_size)?;
    std::fs::write(
        options.output.join("metadata.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&metadata).unwrap_or_default()
        ),
    )?;

    Ok(report)
}

/// One node of the source tree, in the order the file stores them.
struct NodeRef {
    key: OctreeKey,
    byte_offset: u64,
    byte_size: u64,
    points: u32,
}

/// Every node of the source hierarchy, sorted by where its payload sits.
///
/// By file offset rather than by key: the source laid its nodes out in some
/// order and reading them in that order turns a walk over a 372 MB `octree.bin`
/// into a forward scan instead of a seek per node.
fn collect_nodes(potree: &PotreeCloud) -> Result<Vec<NodeRef>> {
    let mut out = Vec::new();
    potree.for_each_node(&mut |node| {
        out.push(NodeRef {
            key: node.key,
            byte_offset: node.byte_offset,
            byte_size: node.byte_size,
            points: node.point_count,
        });
    })?;
    out.sort_by_key(|n| n.byte_offset);
    Ok(out)
}

/// Decode one node into per-attribute blocks at natural widths.
fn decode_node(
    bytes: &[u8],
    count: usize,
    attributes: &[Attribute],
    brotli: bool,
    out: &mut Vec<u8>,
) -> Result<()> {
    let plain_stride: usize = attributes.iter().map(Attribute::byte_size).sum();
    out.clear();
    out.resize(plain_stride * count, 0);

    if !brotli {
        // Interleaved records in, blocks out. A transpose, and the only place
        // the DEFAULT stride is read.
        if bytes.len() < plain_stride * count {
            return Err(Error::Truncated {
                need: (plain_stride * count) as u64,
                got: bytes.len() as u64,
                what: "a DEFAULT node payload".to_string(),
            });
        }
        let mut block = 0usize;
        for attribute in attributes {
            let width = attribute.byte_size();
            let base = block * count;
            for i in 0..count {
                let from = i * plain_stride + attribute.byte_offset;
                out[base + i * width..base + i * width + width]
                    .copy_from_slice(&bytes[from..from + width]);
            }
            block += width;
        }
        return Ok(());
    }

    let raw = brotli_decompress(bytes)?;
    let brotli_stride: usize = attributes.iter().map(brotli_width).sum();
    if raw.len() < brotli_stride * count {
        return Err(Error::Truncated {
            need: (brotli_stride * count) as u64,
            got: raw.len() as u64,
            what: "a BROTLI node payload".to_string(),
        });
    }

    let mut in_block = 0usize;
    let mut out_block = 0usize;
    for attribute in attributes {
        let width = brotli_width(attribute);
        let plain = attribute.byte_size();
        let src = in_block * count;
        let dst = out_block * count;
        for i in 0..count {
            let from = &raw[src + i * width..src + i * width + width];
            let to = &mut out[dst + i * plain..dst + i * plain + plain];
            match attribute.role() {
                Some(AttributeRole::Position) => {
                    let value = morton::decode_position(from.try_into().unwrap());
                    for (axis, component) in value.iter().enumerate() {
                        to[axis * 4..axis * 4 + 4]
                            .copy_from_slice(&(*component as i32).to_le_bytes());
                    }
                }
                Some(AttributeRole::Color) => {
                    let value = morton::decode_color(from.try_into().unwrap());
                    for (channel, component) in value.iter().enumerate() {
                        to[channel * 2..channel * 2 + 2].copy_from_slice(&component.to_le_bytes());
                    }
                }
                None => to.copy_from_slice(from),
            }
        }
        in_block += width;
        out_block += plain;
    }
    Ok(())
}

/// Blocks back to interleaved records, keeping only `keep`.
fn encode_default(
    planar: &[u8],
    count: usize,
    attributes: &[Attribute],
    keep: &[&Attribute],
) -> Vec<u8> {
    let stride: usize = keep.iter().map(|a| a.byte_size()).sum();
    let mut out = vec![0u8; stride * count];
    let mut at_out = 0usize;
    for attribute in keep {
        let width = attribute.byte_size();
        let base = block_offset(attributes, attribute) * count;
        for i in 0..count {
            out[i * stride + at_out..i * stride + at_out + width]
                .copy_from_slice(&planar[base + i * width..base + i * width + width]);
        }
        at_out += width;
    }
    out
}

/// Blocks to morton-coded blocks, compressed.
fn encode_brotli(
    planar: &[u8],
    count: usize,
    attributes: &[Attribute],
    keep: &[&Attribute],
    quality: u32,
    negative_position: &mut bool,
) -> Result<Vec<u8>> {
    let stride: usize = keep.iter().map(|a| brotli_width(a)).sum();
    let mut raw = vec![0u8; stride * count];
    let mut out_block = 0usize;

    for attribute in keep {
        let width = brotli_width(attribute);
        let plain = attribute.byte_size();
        let src = block_offset(attributes, attribute) * count;
        let dst = out_block * count;
        for i in 0..count {
            let from = &planar[src + i * plain..src + i * plain + plain];
            let to = &mut raw[dst + i * width..dst + i * width + width];
            match attribute.role() {
                Some(AttributeRole::Position) => {
                    let component = |axis: usize| {
                        i32::from_le_bytes(from[axis * 4..axis * 4 + 4].try_into().unwrap())
                    };
                    let (x, y, z) = (component(0), component(1), component(2));
                    if x < 0 || y < 0 || z < 0 {
                        *negative_position = true;
                    }
                    to.copy_from_slice(&morton::encode_position(x as u32, y as u32, z as u32));
                }
                Some(AttributeRole::Color) => {
                    let channel = |i: usize| {
                        u16::from_le_bytes(from[i * 2..i * 2 + 2].try_into().unwrap())
                    };
                    to.copy_from_slice(&morton::encode_color(channel(0), channel(1), channel(2)));
                }
                None => to.copy_from_slice(from),
            }
        }
        out_block += width;
    }

    let mut out = Vec::with_capacity(raw.len() / 3);
    let mut input = std::io::Cursor::new(&raw);
    brotli::BrotliCompress(
        &mut input,
        &mut out,
        &brotli::enc::BrotliEncoderParams {
            quality: quality as i32,
            lgwin: 22,
            ..Default::default()
        },
    )
    .map_err(|e| Error::Codec(format!("brotli: {e}")))?;
    Ok(out)
}

fn brotli_decompress(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len() * 4);
    let mut input = std::io::Cursor::new(bytes);
    brotli::BrotliDecompress(&mut input, &mut out)
        .map_err(|e| Error::Codec(format!("brotli: {e}")))?;
    Ok(out)
}

/// Where an attribute's block starts, as a byte offset per point.
fn block_offset(attributes: &[Attribute], attribute: &Attribute) -> usize {
    let mut at = 0;
    for candidate in attributes {
        if std::ptr::eq(candidate, attribute) || candidate.name == attribute.name {
            return at;
        }
        at += candidate.byte_size();
    }
    at
}

/// The source manifest with the attribute list and the encoding replaced.
///
/// Everything else is copied through, including the fields this code has no
/// opinion about — `name`, `description`, `projection`, and any key a writer
/// added that is not in the spec. An optimizer that rebuilt the manifest from
/// what it understands would quietly discard the rest.
fn rewrite_metadata(
    potree: &PotreeCloud,
    keep: &[&Attribute],
    report: &OptimizeReport,
    first_chunk_size: u64,
) -> Result<Value> {
    let bytes = potree.store.open("metadata.json")?.read_all()?;
    let mut manifest: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::manifest("metadata.json", format!("not JSON: {e}")))?;

    let object = manifest
        .as_object_mut()
        .ok_or_else(|| Error::manifest("metadata.json", "the manifest is not a JSON object"))?;
    object.insert("encoding".into(), Value::String(report.encoding_after.clone()));
    if let Some(hierarchy) = object.get_mut("hierarchy").and_then(Value::as_object_mut) {
        hierarchy.insert("firstChunkSize".into(), Value::from(first_chunk_size));
        hierarchy.insert(
            "stepSize".into(),
            Value::from(crate::write::potree::STEP_SIZE),
        );
    }

    // The attribute entries, filtered — and taken from the ORIGINAL JSON rather
    // than rebuilt from the parsed form, so histograms, descriptions and the
    // per-element transforms survive untouched.
    if let Some(list) = object.get("attributes").and_then(Value::as_array) {
        let names: Vec<&str> = keep.iter().map(|a| a.name.as_str()).collect();
        let filtered: Vec<Value> = list
            .iter()
            .filter(|entry| {
                entry
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|name| names.contains(&name))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        object.insert("attributes".into(), Value::Array(filtered));
    }

    Ok(manifest)
}

/// The hierarchy, as a map from key to node. Used by the tests.
pub fn node_index(nodes: &[Written]) -> HashMap<OctreeKey, (u64, u64, u32)> {
    nodes
        .iter()
        .map(|n| (n.key, (n.byte_offset, n.byte_size, n.points)))
        .collect()
}

/// Open a Potree cloud at `path`, or say why it is not one.
pub fn open_potree(path: &Path) -> Result<PotreeCloud> {
    let store: Arc<dyn Store> = Arc::new(FileStore::new(path));
    crate::format::potree::open(store, &path.display().to_string())
}
