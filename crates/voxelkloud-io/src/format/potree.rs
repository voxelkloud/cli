//! Potree v2: `metadata.json`, `hierarchy.bin`, `octree.bin`.
//!
//! The manifest is read tolerantly. Stock PotreeConverter output disagrees with
//! its own schema in half a dozen places — a `uint8` attribute with a negative
//! `min`, an `elementSize` that contradicts the type, a `projection` that is
//! the empty string — and a reader that refused any of them would refuse most
//! of the files in the world. Each becomes a warning instead.

use std::sync::Arc;

use serde_json::Value;

use crate::attribute::{lay_out, Attribute, AttributeType};
use crate::bounds::Bounds;
use crate::cloud::{CloudInfo, FormatId, HierarchyStats, NodeInfo};
use crate::crs::Crs;
use crate::error::{Error, Result};
use crate::octree::OctreeKey;
use crate::source::Store;

/// `hierarchy.bin` node record: `u8` type, `u8` child mask, `u32` point count,
/// `i64` byte offset, `i64` byte size. Confirmed against `demo/potree`'s
/// `NodeLoader.parseHierarchy`.
pub const HIERARCHY_RECORD_SIZE: usize = 22;

/// A record whose offset and size point at a further chunk of `hierarchy.bin`
/// rather than into `octree.bin`.
const TYPE_PROXY: u8 = 2;

/// One node, with the range of `octree.bin` that holds its points.
///
/// `byte_size == 0` is how Potree spells a node with no payload of its own —
/// 47 of autzen's are like that — and NOT `byte_offset == 0`, which is a real
/// offset held by exactly one node.
#[derive(Debug, Clone, Copy)]
pub struct PotreeNodeRef {
    pub key: OctreeKey,
    pub point_count: u32,
    pub byte_offset: u64,
    pub byte_size: u64,
}

/// What the manifest said, kept because the writers and the hierarchy walk need
/// it and [`CloudInfo`] is deliberately format-neutral.
#[derive(Debug, Clone)]
pub struct PotreeMetadata {
    pub version: String,
    pub name: String,
    pub description: String,
    /// Bytes of the root chunk of `hierarchy.bin`.
    pub first_chunk_size: u64,
    /// Levels between chunk boundaries. Every `stepSize` levels the tree
    /// continues in a new chunk, reached through a proxy record.
    pub step_size: u32,
    pub depth: u32,
    /// `"DEFAULT"` or `"BROTLI"`.
    pub encoding: String,
}

pub struct PotreeCloud {
    pub info: CloudInfo,
    pub metadata: PotreeMetadata,
    /// Bytes per point, when the encoding is record-oriented. `None` for
    /// BROTLI, whose blocks have no fixed stride.
    pub bytes_per_point: Option<usize>,
    /// `hierarchy.bin`, whole.
    ///
    /// Eager bytes, lazy tree — the default the loader ships, for the reason it
    /// ships it: the file is 100 KB on a 10M-point cloud, against 192 ranged
    /// requests to read the same thing a chunk at a time.
    hierarchy: Vec<u8>,
    pub store: Arc<dyn Store>,
}

/// Open the cloud in `store`, whose manifest is `metadata.json`.
pub fn open(store: Arc<dyn Store>, label: &str) -> Result<PotreeCloud> {
    open_manifest(store, "metadata.json", label)
}

pub fn open_manifest(store: Arc<dyn Store>, manifest: &str, label: &str) -> Result<PotreeCloud> {
    let bytes = store.open(manifest)?.read_all()?;
    let json: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::manifest(manifest.to_string(), format!("not JSON: {e}")))?;
    let (info, metadata, bytes_per_point) = parse(&json, label)?;

    let hierarchy = match store.open("hierarchy.bin") {
        Ok(source) => source.read_all()?,
        Err(_) => Vec::new(),
    };

    Ok(PotreeCloud {
        info,
        metadata,
        bytes_per_point,
        hierarchy,
        store,
    })
}

fn parse(json: &Value, label: &str) -> Result<(CloudInfo, PotreeMetadata, Option<usize>)> {
    let obj = json
        .as_object()
        .ok_or_else(|| Error::manifest("metadata.json", "the manifest is not a JSON object"))?;

    let version = obj
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::manifest("metadata.json.version", "missing"))?;
    if !version.starts_with("2.") {
        return Err(Error::not_format(
            "Potree v2",
            format!(
                "the manifest declares version {version:?}. Version 1 clouds \
                 (`cloud.js`) are a different format; convert with \
                 `voxelkloud convert`."
            ),
        ));
    }

    let mut info = CloudInfo::new(FormatId::PotreeV2, label);
    info.version = Some(version.to_string());
    info.point_count = obj.get("points").and_then(Value::as_u64).unwrap_or(0);
    info.encoding = Some(
        obj.get("encoding")
            .and_then(Value::as_str)
            .unwrap_or("DEFAULT")
            .to_string(),
    );
    info.spacing = obj.get("spacing").and_then(Value::as_f64);
    info.scale = vec3(obj.get("scale")).unwrap_or([1.0; 3]);
    info.offset = vec3(obj.get("offset")).unwrap_or([0.0; 3]);
    info.crs = obj
        .get("projection")
        .and_then(Value::as_str)
        .and_then(Crs::from_string);

    let bounding = obj
        .get("boundingBox")
        .ok_or_else(|| Error::manifest("metadata.json.boundingBox", "missing"))?;
    info.bounds = Bounds::new(
        vec3(bounding.get("min")).ok_or_else(|| Error::manifest("boundingBox.min", "missing"))?,
        vec3(bounding.get("max")).ok_or_else(|| Error::manifest("boundingBox.max", "missing"))?,
    );

    let hierarchy = obj.get("hierarchy");
    let metadata = PotreeMetadata {
        version: version.to_string(),
        name: obj.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
        description: obj
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        first_chunk_size: hierarchy
            .and_then(|h| h.get("firstChunkSize"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        step_size: hierarchy
            .and_then(|h| h.get("stepSize"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        depth: hierarchy
            .and_then(|h| h.get("depth"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        encoding: info.encoding.clone().unwrap_or_default(),
    };
    info.levels = Some(metadata.depth);

    let attributes = obj
        .get("attributes")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::manifest("metadata.json.attributes", "missing"))?;
    let mut out = Vec::with_capacity(attributes.len());
    for (i, raw) in attributes.iter().enumerate() {
        let path = format!("attributes[{i}]");
        let name = raw
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::manifest(path.clone(), "attribute has no name"))?;
        let type_name = raw
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::manifest(format!("{path}.type"), "missing"))?;
        let Some(kind) = AttributeType::parse(type_name) else {
            return Err(Error::manifest(
                format!("{path}.type"),
                format!("{type_name:?} is not one of the ten attribute types"),
            ));
        };
        let num_elements = raw
            .get("numElements")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;

        // The canonical width wins over the manifest's own. They disagree in
        // real files, and the record stride is derived from the type.
        if let Some(declared) = raw.get("elementSize").and_then(Value::as_u64) {
            if declared as usize != kind.size() {
                info.warn(
                    "element-size-mismatch",
                    format!("{path}.elementSize"),
                    format!(
                        "{name:?} declares an element size of {declared} but its type \
                         {type_name} is {} bytes. The type wins.",
                        kind.size()
                    ),
                );
            }
        }
        if kind.is_undecodable() {
            info.warn(
                "undecodable-attribute",
                format!("{path}.type"),
                format!(
                    "{name:?} is {type_name}, which cannot be decoded without loss. \
                     Its bytes are still counted in the record stride."
                ),
            );
        }

        out.push(Attribute {
            name: name.to_string(),
            description: raw
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            kind,
            num_elements,
            byte_offset: 0,
            min: numbers(raw.get("min"), num_elements, 0.0),
            max: numbers(raw.get("max"), num_elements, 0.0),
            scale: numbers(raw.get("scale"), num_elements, 1.0),
            offset: numbers(raw.get("offset"), num_elements, 0.0),
            histogram: raw.get("histogram").and_then(Value::as_array).map(|h| {
                h.iter().map(|v| v.as_u64().unwrap_or(0)).collect()
            }),
        });
    }

    let stride = lay_out(&mut out);
    info.attributes = out;
    info.record_bytes = Some(stride);

    // Tight bounds come from the position attribute's own domain, which is in
    // absolute CRS units. Potree's `boundingBox` is the cubic indexing volume
    // and on autzen it is 22x taller than the points.
    info.tight_bounds = match info.attribute("position").or_else(|| info.attribute("POSITION_CARTESIAN")) {
        Some(p) if p.num_elements == 3 && p.min.len() == 3 && p.max.len() == 3 => Bounds::new(
            [p.min[0], p.min[1], p.min[2]],
            [p.max[0], p.max[1], p.max[2]],
        ),
        _ => {
            info.warn(
                "missing-position-attribute",
                "attributes",
                "No 3-element position attribute; falling back to the cubic boundingBox \
                 for the tight bounds."
                    .to_string(),
            );
            info.bounds
        }
    };

    let bytes_per_point = (metadata.encoding != "BROTLI").then_some(stride);
    Ok((info, metadata, bytes_per_point))
}

fn vec3(value: Option<&Value>) -> Option<[f64; 3]> {
    let array = value?.as_array()?;
    if array.len() < 3 {
        return None;
    }
    Some([
        array[0].as_f64()?,
        array[1].as_f64()?,
        array[2].as_f64()?,
    ])
}

fn numbers(value: Option<&Value>, n: usize, fill: f64) -> Vec<f64> {
    let mut out = vec![fill; n];
    if let Some(array) = value.and_then(Value::as_array) {
        for (i, v) in array.iter().take(n).enumerate() {
            if let Some(x) = v.as_f64() {
                out[i] = x;
            }
        }
    }
    out
}

impl PotreeCloud {
    /// Walk `hierarchy.bin`.
    ///
    /// Chunks are written in level order with children in ascending child
    /// index, so a chunk is a breadth-first queue: record `i` describes slot
    /// `i`, and the children a record's mask names are appended at the tail.
    /// The chunks themselves are *not* in file order and do not tile the file,
    /// which is why this seeks rather than streams.
    pub fn hierarchy(&self) -> Result<HierarchyStats> {
        let mut stats = HierarchyStats {
            hierarchy_bytes: self.hierarchy.len() as u64,
            hierarchy_requests: if self.hierarchy.is_empty() { 0 } else { 1 },
            ..HierarchyStats::default()
        };
        if self.hierarchy.is_empty() {
            stats.warnings.push(crate::warning::Warning::new(
                "hierarchy-missing",
                "hierarchy.bin",
                "The cloud has no hierarchy.bin, so only the manifest could be read.",
            ));
            return Ok(stats);
        }

        let mut visited: Vec<u64> = Vec::new();
        let mut queue = vec![(OctreeKey::ROOT, 0u64, self.metadata.first_chunk_size)];
        while let Some((seed, offset, size)) = queue.pop() {
            if visited.contains(&offset) {
                stats.warnings.push(crate::warning::Warning::new(
                    "hierarchy-cycle",
                    seed.potree_name(),
                    format!("The chunk at byte {offset} is reached twice; the walk stops there."),
                ));
                continue;
            }
            visited.push(offset);
            self.walk_chunk(seed, offset, size, &mut stats, &mut queue, &mut |_| {})?;
        }
        Ok(stats)
    }

    /// Visit every node, with where its payload lives.
    ///
    /// The same walk [`hierarchy`](Self::hierarchy) does, with the two fields
    /// the stats throw away — a re-encoder needs to know which bytes of
    /// `octree.bin` belong to which node, and it is the only consumer that
    /// does.
    pub fn for_each_node(&self, visit: &mut dyn FnMut(PotreeNodeRef)) -> Result<()> {
        if self.hierarchy.is_empty() {
            return Ok(());
        }
        let mut stats = HierarchyStats::default();
        let mut visited: Vec<u64> = Vec::new();
        let mut queue = vec![(OctreeKey::ROOT, 0u64, self.metadata.first_chunk_size)];
        while let Some((seed, offset, size)) = queue.pop() {
            if visited.contains(&offset) {
                continue;
            }
            visited.push(offset);
            self.walk_chunk(seed, offset, size, &mut stats, &mut queue, visit)?;
        }
        Ok(())
    }

    fn walk_chunk(
        &self,
        seed: OctreeKey,
        offset: u64,
        size: u64,
        stats: &mut HierarchyStats,
        queue: &mut Vec<(OctreeKey, u64, u64)>,
        visit: &mut dyn FnMut(PotreeNodeRef),
    ) -> Result<()> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let size = usize::try_from(size).unwrap_or(usize::MAX);
        let end = start.saturating_add(size);
        if size == 0 || size % HIERARCHY_RECORD_SIZE != 0 || end > self.hierarchy.len() {
            return Err(Error::not_format(
                "a Potree v2 hierarchy",
                format!(
                    "the chunk for {} is [{start}, {end}) of a {}-byte hierarchy.bin, \
                     and {size} is not a positive multiple of {HIERARCHY_RECORD_SIZE}",
                    seed.potree_name(),
                    self.hierarchy.len()
                ),
            ));
        }

        let n = size / HIERARCHY_RECORD_SIZE;
        let mut keys = Vec::with_capacity(n);
        keys.push(seed);

        for i in 0..n {
            if i >= keys.len() {
                return Err(Error::not_format(
                    "a Potree v2 hierarchy",
                    format!(
                        "{}#{i}: the chunk declares {n} records but the level-order walk \
                         ran out of parents after {}",
                        seed.potree_name(),
                        keys.len()
                    ),
                ));
            }
            let key = keys[i];
            let at = start + i * HIERARCHY_RECORD_SIZE;
            let record = &self.hierarchy[at..at + HIERARCHY_RECORD_SIZE];
            let kind = record[0];
            let mask = record[1];
            let point_count = u32::from_le_bytes([record[2], record[3], record[4], record[5]]);
            let byte_offset = u64::from_le_bytes(record[6..14].try_into().unwrap());
            let byte_size = u64::from_le_bytes(record[14..22].try_into().unwrap());

            // A proxy names no children here: its real mask lives at record 0
            // of the chunk it points at. Record 0 is never a proxy — it is the
            // node the chunk belongs to.
            if i > 0 && kind == TYPE_PROXY {
                queue.push((key, byte_offset, byte_size));
                continue;
            }

            stats.add(NodeInfo {
                key,
                point_count: u64::from(point_count),
                byte_size,
            });
            visit(PotreeNodeRef {
                key,
                point_count,
                byte_offset,
                byte_size,
            });

            for c in 0..8u8 {
                if mask >> c & 1 == 1 {
                    keys.push(key.child(c));
                }
            }
        }
        Ok(())
    }
}
