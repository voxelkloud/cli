//! The Entwine Point Tile writer.
//!
//! The most spread out of the three on disk and the simplest to write: a
//! manifest, one JSON page per subtree, and one file per node. No offsets to
//! patch, nothing to seek back to — a node is a file, and its name is its key.
//!
//! Two encodings. `binary` writes the schema's fields as a flat record, which
//! is what a static host can serve with no decoder at all; `laszip` writes each
//! node as a complete little LAZ file, which is what the 3DEP archive on S3
//! looks like and roughly a fifth of the size.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use laz::{LasZipCompressor, LazItemRecordBuilder, LazItemType, LazVlrBuilder};
use serde_json::{json, Value};

use crate::build::{BuiltNode, NodeSink};
use crate::error::{Error, Result};
use crate::octree::OctreeKey;
use crate::record::at;

use super::las_write::{projection_vlrs, write_vlr, OutHeader, OutVlr, HEADER_SIZE};
use super::{WriteOptions, WriteReport};

/// How a node's points are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EptEncoding {
    Binary,
    Laszip,
}

impl EptEncoding {
    pub fn name(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::Laszip => "laszip",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Binary => "bin",
            Self::Laszip => "laz",
        }
    }
}

/// Nodes per hierarchy page before it defers to another.
///
/// EPT names its pages after the node they start at, so the cut is a whole
/// subtree either way; this only decides how many entries one fetch brings
/// back. Entwine's own default behaves like a few hundred.
const PAGE_NODES: usize = 512;

pub struct EptWriter {
    dir: PathBuf,
    options: WriteOptions,
    encoding: EptEncoding,
    /// The EPT schema, which is also the binary record layout.
    schema: Vec<SchemaField>,
    stride: usize,
    counts: HashMap<OctreeKey, u64>,
    points: u64,
    depth: u32,
}

struct SchemaField {
    name: &'static str,
    kind: &'static str,
    size: usize,
    scale: Option<f64>,
    offset: Option<f64>,
}

impl EptWriter {
    pub fn create(dir: &Path, options: WriteOptions, encoding: EptEncoding) -> Result<Self> {
        std::fs::create_dir_all(dir.join("ept-data"))?;
        std::fs::create_dir_all(dir.join("ept-hierarchy"))?;

        let color = options.layout.has_color();
        let mut schema = vec![
            SchemaField { name: "X", kind: "signed", size: 4, scale: Some(options.scale[0]), offset: Some(options.offset[0]) },
            SchemaField { name: "Y", kind: "signed", size: 4, scale: Some(options.scale[1]), offset: Some(options.offset[1]) },
            SchemaField { name: "Z", kind: "signed", size: 4, scale: Some(options.scale[2]), offset: Some(options.offset[2]) },
            SchemaField { name: "Intensity", kind: "unsigned", size: 2, scale: None, offset: None },
            SchemaField { name: "ReturnNumber", kind: "unsigned", size: 1, scale: None, offset: None },
            SchemaField { name: "NumberOfReturns", kind: "unsigned", size: 1, scale: None, offset: None },
            SchemaField { name: "Classification", kind: "unsigned", size: 1, scale: None, offset: None },
            SchemaField { name: "ScanAngleRank", kind: "signed", size: 2, scale: None, offset: None },
            SchemaField { name: "UserData", kind: "unsigned", size: 1, scale: None, offset: None },
            SchemaField { name: "PointSourceId", kind: "unsigned", size: 2, scale: None, offset: None },
        ];
        if options.has_gps_time {
            schema.push(SchemaField { name: "GpsTime", kind: "float", size: 8, scale: None, offset: None });
        }
        if color {
            schema.push(SchemaField { name: "Red", kind: "unsigned", size: 2, scale: None, offset: None });
            schema.push(SchemaField { name: "Green", kind: "unsigned", size: 2, scale: None, offset: None });
            schema.push(SchemaField { name: "Blue", kind: "unsigned", size: 2, scale: None, offset: None });
        }
        let stride = schema.iter().map(|f| f.size).sum();

        Ok(Self {
            dir: dir.to_path_buf(),
            options,
            encoding,
            schema,
            stride,
            counts: HashMap::new(),
            points: 0,
            depth: 0,
        })
    }

    pub fn write_node(&mut self, node: &BuiltNode) -> Result<()> {
        let source_stride = self.options.stride();
        let count = node.records.len() / source_stride;
        if count == 0 {
            return Ok(());
        }

        match self.encoding {
            EptEncoding::Binary => {
                let mut out = vec![0u8; count * self.stride];
                for (i, record) in node.records.chunks_exact(source_stride).enumerate() {
                    self.encode_binary(record, &mut out[i * self.stride..(i + 1) * self.stride]);
                }
                std::fs::write(self.node_path(node.key), out)?;
            }
            // A whole LAS 1.4 file per node, laszip-compressed, which is what
            // an EPT reader expects: it opens each node as a file rather than
            // as a chunk, so the header has to be there.
            EptEncoding::Laszip => self.write_laz_node(node, count)?,
        }

        self.counts.insert(node.key, count as u64);
        self.points += count as u64;
        self.depth = self.depth.max(node.key.level);
        Ok(())
    }

    fn node_path(&self, key: OctreeKey) -> PathBuf {
        self.dir
            .join("ept-data")
            .join(format!("{}.{}", key.ept_name(), self.encoding.extension()))
    }

    fn encode_binary(&self, record: &[u8], out: &mut [u8]) {
        out[0..12].copy_from_slice(&record[at::X..at::X + 12]);
        out[12..14].copy_from_slice(&record[at::INTENSITY..at::INTENSITY + 2]);
        let returns = record[at::RETURNS];
        out[14] = returns & 0x0f;
        out[15] = returns >> 4;
        out[16] = record[at::CLASSIFICATION];
        out[17..19].copy_from_slice(&record[at::SCAN_ANGLE..at::SCAN_ANGLE + 2]);
        out[19] = record[at::USER_DATA];
        out[20..22].copy_from_slice(&record[at::POINT_SOURCE_ID..at::POINT_SOURCE_ID + 2]);
        let mut at_out = 22;
        if self.options.has_gps_time {
            out[at_out..at_out + 8].copy_from_slice(&record[at::GPS_TIME..at::GPS_TIME + 8]);
            at_out += 8;
        }
        if self.options.layout.has_color() {
            out[at_out..at_out + 6].copy_from_slice(&record[at::RGB..at::RGB + 6]);
        }
    }

    fn write_laz_node(&self, node: &BuiltNode, count: usize) -> Result<()> {
        let stride = self.options.stride();
        let mut items = LazItemRecordBuilder::new();
        items.add_item(LazItemType::Point14);
        if self.options.layout.has_color() {
            if self.options.layout.has_nir() {
                items.add_item(LazItemType::RGBNIR14);
            } else {
                items.add_item(LazItemType::RGB14);
            }
        }
        if self.options.layout.extra > 0 {
            items.add_item(LazItemType::Byte14(self.options.layout.extra as u16));
        }
        let laz_vlr = LazVlrBuilder::new(items.build()).build();
        let mut laszip_payload = Vec::new();
        laz_vlr
            .write_to(&mut laszip_payload)
            .map_err(|e| Error::Codec(format!("laszip VLR: {e}")))?;

        let mut vlrs = vec![
            OutVlr::new(
                crate::las::LASZIP_USER_ID,
                crate::las::LASZIP_RECORD_ID,
                "laszip",
                laszip_payload,
            ),
        ];
        let (projection, wkt) = projection_vlrs(&self.options.projection_vlrs);
        vlrs.extend(projection);
        if !self.options.layout.extra_vlr.is_empty() {
            vlrs.push(OutVlr::new(
                "LASF_Spec",
                4,
                "Extra Bytes",
                self.options.layout.extra_vlr.clone(),
            ));
        }
        let offset_to_point_data =
            (HEADER_SIZE + vlrs.iter().map(OutVlr::size).sum::<usize>()) as u32;

        // A node's own extent, not the cloud's: an EPT reader trusts the node
        // header, and the converter learned the hard way that a schema and a
        // record can disagree.
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        for record in node.records.chunks_exact(stride) {
            let p = crate::record::position(record);
            for axis in 0..3 {
                let value = crate::record::dequantize(
                    p[axis],
                    self.options.scale[axis],
                    self.options.offset[axis],
                );
                min[axis] = min[axis].min(value);
                max[axis] = max[axis].max(value);
            }
        }

        let header = OutHeader {
            point_format: self.options.layout.format,
            point_size: stride as u16,
            compressed: true,
            point_count: count as u64,
            scale: self.options.scale,
            offset: self.options.offset,
            min,
            max,
            offset_to_point_data,
            vlr_count: vlrs.len() as u32,
            evlr_offset: 0,
            evlr_count: 0,
            generator: self.options.generator.clone(),
            wkt,
            creation: self.options.creation,
            points_by_return: [0; 15],
        };

        let mut file = BufWriter::new(File::create(self.node_path(node.key))?);
        file.write_all(&header.to_bytes())?;
        for vlr in &vlrs {
            write_vlr(&mut file, vlr)?;
        }
        let mut compressor = LasZipCompressor::new(file, laz_vlr)
            .map_err(|e| Error::Codec(format!("laszip compressor: {e}")))?;
        compressor
            .compress_many(&node.records)
            .map_err(|e| Error::Codec(format!("compressing {}: {e}", node.key.ept_name())))?;
        compressor
            .done()
            .map_err(|e| Error::Codec(format!("finishing {}: {e}", node.key.ept_name())))?;
        compressor.into_inner().flush()?;
        Ok(())
    }

    pub fn finish(self) -> Result<WriteReport> {
        self.write_hierarchy()?;

        let manifest = json!({
            "version": "1.0.0",
            "bounds": [
                self.options.cube.min[0], self.options.cube.min[1], self.options.cube.min[2],
                self.options.cube.max[0], self.options.cube.max[1], self.options.cube.max[2],
            ],
            "boundsConforming": [
                self.options.extent.min[0], self.options.extent.min[1], self.options.extent.min[2],
                self.options.extent.max[0], self.options.extent.max[1], self.options.extent.max[2],
            ],
            "dataType": self.encoding.name(),
            "hierarchyType": "json",
            "points": self.points,
            "span": self.options.span,
            "srs": match &self.options.crs {
                Some(crs) if crs.format == crate::crs::CrsFormat::Wkt => json!({ "wkt": crs.raw }),
                Some(crs) if crs.epsg.is_some() => json!({
                    "authority": "EPSG",
                    "horizontal": crs.epsg.unwrap().to_string(),
                }),
                _ => json!({}),
            },
            "schema": self.schema.iter().map(|f| {
                let mut field = serde_json::Map::new();
                field.insert("name".into(), json!(f.name));
                field.insert("size".into(), json!(f.size));
                field.insert("type".into(), json!(f.kind));
                if let Some(scale) = f.scale { field.insert("scale".into(), json!(scale)); }
                if let Some(offset) = f.offset { field.insert("offset".into(), json!(offset)); }
                Value::Object(field)
            }).collect::<Vec<_>>(),
        });
        std::fs::write(
            self.dir.join("ept.json"),
            format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap_or_default()),
        )?;

        Ok(WriteReport {
            nodes: self.counts.len() as u64,
            points: self.points,
            depth: self.depth,
            bytes: directory_size(&self.dir),
            path: self.dir.display().to_string(),
        })
    }

    /// One JSON object per page: `"level-x-y-z": count`, with `-1` for a node
    /// whose subtree continues in the page named after it.
    fn write_hierarchy(&self) -> Result<()> {
        let mut children: HashMap<OctreeKey, Vec<OctreeKey>> = HashMap::new();
        for key in self.counts.keys() {
            if let Some(parent) = key.parent() {
                children.entry(parent).or_default().push(*key);
            }
        }
        for list in children.values_mut() {
            list.sort();
        }

        let root = self
            .counts
            .keys()
            .min_by_key(|k| (k.level, k.x, k.y, k.z))
            .copied()
            .unwrap_or(OctreeKey::ROOT);

        let mut pages = vec![root];
        while let Some(page_root) = pages.pop() {
            let mut entries = serde_json::Map::new();
            let mut queue = std::collections::VecDeque::from([page_root]);
            let mut deferred = Vec::new();

            while let Some(key) = queue.pop_front() {
                if entries.len() >= PAGE_NODES && key != page_root {
                    // Defer the whole subtree, which is what the -1 means.
                    entries.insert(key.ept_name(), json!(-1));
                    deferred.push(key);
                    continue;
                }
                entries.insert(
                    key.ept_name(),
                    json!(self.counts.get(&key).copied().unwrap_or(0)),
                );
                for child in children.get(&key).into_iter().flatten() {
                    queue.push_back(*child);
                }
            }

            std::fs::write(
                self.dir
                    .join("ept-hierarchy")
                    .join(format!("{}.json", page_root.ept_name())),
                format!("{}\n", serde_json::to_string(&Value::Object(entries)).unwrap_or_default()),
            )?;
            pages.extend(deferred);
        }
        Ok(())
    }
}

fn directory_size(dir: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            total += directory_size(&entry.path());
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

impl NodeSink for EptWriter {
    fn node(&mut self, node: BuiltNode) -> Result<()> {
        self.write_node(&node)
    }
}
