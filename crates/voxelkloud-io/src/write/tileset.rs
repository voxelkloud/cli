//! The 3D Tiles writer.
//!
//! Writes **1.1 with glTF content**, not `.pnts`. The 1.1 spec puts `.pnts` on
//! the legacy shelf and says new content should be glTF, and a writer is the one
//! place a project gets to choose without also having to read what everyone else
//! already published.
//!
//! Shaped like the EPT writer and for the same reason: a node is a file, its
//! name is its key, and nothing has to be seeked back to. The tree is written at
//! the end, when every key is known.
//!
//! **Explicit rather than implicit**, and the reason is worth stating because
//! the plan said the opposite. Implicit tiling replaces the tree with a rule
//! plus availability bitstreams — smaller on disk and a genuinely better fit
//! for an octree, which this is. But the tree here is SPARSE and the subtree
//! files that describe sparseness are a second format to write, test and get
//! the bit order right in; an explicit tree is valid 1.1, reads in every viewer
//! including this project's own, and is the thing that can be checked against a
//! validator today. Implicit is the follow-up, not the foundation.
//!
//! Two conventions the reader on the other side depends on, and both are here
//! rather than in a comment over there:
//!
//!   * glTF is **Y-up** and 3D Tiles is **Z-up**, so every position is written
//!     rotated: `(x, y, z)` becomes `(x, z, -y)`, the inverse of what the
//!     reader applies.
//!   * Positions are float32 **relative to the tile's own centre**, carried in
//!     the tile `transform`. Absolute ECEF in float32 would quantise the planet
//!     to half-metre steps.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::build::{BuiltNode, NodeSink};
use crate::bounds::Bounds;
use crate::error::{Error, Result};
use crate::octree::OctreeKey;
use crate::record::at;

use super::{WriteOptions, WriteReport};

const GLB_MAGIC: u32 = 0x4654_6c67; // "glTF"
const CHUNK_JSON: u32 = 0x4e4f_534a;
const CHUNK_BIN: u32 = 0x004e_4942;

pub struct TilesetWriter {
    dir: PathBuf,
    options: WriteOptions,
    /// Keys in level order, which is also the order a parent precedes a child.
    nodes: BTreeMap<(u32, u32, u32, u32), NodeEntry>,
    points: u64,
    depth: u32,
    bytes: u64,
}

struct NodeEntry {
    count: u64,
    /// The tile's own centre, which its `transform` carries.
    centre: [f64; 3],
    uri: String,
}

impl TilesetWriter {
    pub fn create(dir: &Path, options: WriteOptions) -> Result<Self> {
        fs::create_dir_all(dir.join("content"))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            options,
            nodes: BTreeMap::new(),
            points: 0,
            depth: 0,
            bytes: 0,
        })
    }

    /// The cube a key covers, derived by halving — never recomputed from the
    /// key's integers, so a node and its parent agree to the last bit.
    fn node_cube(&self, key: OctreeKey) -> Bounds {
        let cube = self.options.cube;
        let size = [
            (cube.max[0] - cube.min[0]) / (1u64 << key.level) as f64,
            (cube.max[1] - cube.min[1]) / (1u64 << key.level) as f64,
            (cube.max[2] - cube.min[2]) / (1u64 << key.level) as f64,
        ];
        Bounds {
            min: [
                cube.min[0] + key.x as f64 * size[0],
                cube.min[1] + key.y as f64 * size[1],
                cube.min[2] + key.z as f64 * size[2],
            ],
            max: [
                cube.min[0] + (key.x + 1) as f64 * size[0],
                cube.min[1] + (key.y + 1) as f64 * size[1],
                cube.min[2] + (key.z + 1) as f64 * size[2],
            ],
        }
    }

    fn write_node(&mut self, node: BuiltNode) -> Result<()> {
        let stride = self.options.stride();
        let count = node.records.len() / stride;
        if count == 0 {
            return Ok(());
        }
        let cube = self.node_cube(node.key);
        let centre = [
            (cube.min[0] + cube.max[0]) / 2.0,
            (cube.min[1] + cube.max[1]) / 2.0,
            (cube.min[2] + cube.max[2]) / 2.0,
        ];

        let has_color = self.options.layout.format == 7 || self.options.layout.format == 8;
        let mut positions: Vec<u8> = Vec::with_capacity(count * 12);
        let mut colors: Vec<u8> = Vec::with_capacity(if has_color { count * 3 } else { 0 });
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];

        for i in 0..count {
            let r = &node.records[i * stride..(i + 1) * stride];
            let xi = i32::from_le_bytes(r[at::X..at::X + 4].try_into().unwrap());
            let yi = i32::from_le_bytes(r[at::Y..at::Y + 4].try_into().unwrap());
            let zi = i32::from_le_bytes(r[at::Z..at::Z + 4].try_into().unwrap());
            let x = xi as f64 * self.options.scale[0] + self.options.offset[0] - centre[0];
            let y = yi as f64 * self.options.scale[1] + self.options.offset[1] - centre[1];
            let z = zi as f64 * self.options.scale[2] + self.options.offset[2] - centre[2];
            // Z-UP TO Y-UP, the inverse of what the reader applies. Written
            // here so the file is a correct glTF rather than one that only this
            // project's reader gets right.
            let g = [x as f32, z as f32, -(y as f32)];
            for (c, v) in g.iter().enumerate() {
                if *v < min[c] {
                    min[c] = *v;
                }
                if *v > max[c] {
                    max[c] = *v;
                }
                positions.extend_from_slice(&v.to_le_bytes());
            }

            if has_color {
                // 16-bit LAS colour narrowed to the 8 bits COLOR_0 carries as
                // a normalized ubyte. `>> 8` rather than `/ 257` because the
                // round trip is not exact either way and the shift is what
                // every other writer here does.
                for c in 0..3 {
                    let off = at::RGB + c * 2;
                    let v = u16::from_le_bytes(r[off..off + 2].try_into().unwrap());
                    colors.push((v >> 8) as u8);
                }
            }
        }

        let name = format!(
            "content/{}-{}-{}-{}.glb",
            node.key.level, node.key.x, node.key.y, node.key.z
        );
        let glb = build_glb(&positions, if has_color { Some(&colors) } else { None }, count, min, max);
        let path = self.dir.join(&name);
        let mut file = BufWriter::new(File::create(&path)?);
        file.write_all(&glb)?;
        file.flush()?;

        self.bytes += glb.len() as u64;
        self.points += count as u64;
        if node.key.level > self.depth {
            self.depth = node.key.level;
        }
        self.nodes.insert(
            (node.key.level, node.key.x, node.key.y, node.key.z),
            NodeEntry {
                count: count as u64,
                centre,
                uri: name,
            },
        );
        Ok(())
    }

    /// Write `tileset.json` and report.
    pub fn finish(self) -> Result<WriteReport> {
        // The root tile always exists, with or without content of its own: a
        // sparse octree can have an empty root over a populated subtree, and
        // dropping it would orphan everything below.
        let root = self.tile(OctreeKey {
            level: 0,
            x: 0,
            y: 0,
            z: 0,
        });

        let doc = json!({
            "asset": {
                "version": "1.1",
                "tilesetVersion": self.options.generator,
            },
            // The error of drawing NOTHING, which is what scores the root.
            "geometricError": self.options.spacing * 2.0,
            "root": root,
        });

        let path = self.dir.join("tileset.json");
        let mut file = BufWriter::new(File::create(&path)?);
        serde_json::to_writer_pretty(&mut file, &doc)
            .map_err(|e| Error::Source(format!("writing {}: {e}", path.display())))?;
        file.flush()?;

        Ok(WriteReport {
            points: self.points,
            nodes: self.nodes.len() as u64,
            depth: self.depth,
            bytes: self.bytes,
            path: path.display().to_string(),
        })
    }

    /// One tile and, recursively, its children.
    fn tile(&self, key: OctreeKey) -> Value {
        let cube = self.node_cube(key);
        let half = [
            (cube.max[0] - cube.min[0]) / 2.0,
            (cube.max[1] - cube.min[1]) / 2.0,
            (cube.max[2] - cube.min[2]) / 2.0,
        ];
        let entry = self.nodes.get(&(key.level, key.x, key.y, key.z));
        let centre = entry.map_or(
            [
                (cube.min[0] + cube.max[0]) / 2.0,
                (cube.min[1] + cube.max[1]) / 2.0,
                (cube.min[2] + cube.max[2]) / 2.0,
            ],
            |e| e.centre,
        );

        let mut children = Vec::new();
        for c in 0..8u32 {
            let child = OctreeKey {
                level: key.level + 1,
                x: key.x * 2 + (c & 1),
                y: key.y * 2 + ((c >> 1) & 1),
                z: key.z * 2 + ((c >> 2) & 1),
            };
            if self.has_descendant(child) {
                children.push(self.tile(child));
            }
        }

        // The tile's own error is the spacing at its level: halving the cube
        // halves the distance between neighbouring points. A LEAF gets 0, which
        // is what the format means by "there is nothing finer".
        let error = if children.is_empty() {
            0.0
        } else {
            self.options.spacing / (1u64 << key.level) as f64
        };

        let mut tile = json!({
            // The box is LOCAL to the tile's centre, and the transform places
            // it — the same split the content uses, so a viewer that reads one
            // in float32 reads the other the same way.
            "boundingVolume": {
                "box": [
                    0.0, 0.0, 0.0,
                    half[0], 0.0, 0.0,
                    0.0, half[1], 0.0,
                    0.0, 0.0, half[2],
                ]
            },
            "transform": [
                1.0, 0.0, 0.0, 0.0,
                0.0, 1.0, 0.0, 0.0,
                0.0, 0.0, 1.0, 0.0,
                centre[0], centre[1], centre[2], 1.0,
            ],
            "geometricError": error,
            // ADD, and it is the truth about this data rather than a default:
            // the builder's nodes each hold points the others do not, so a
            // viewer that hid a parent would delete those points from the
            // picture. See DEC-T4 for what happens when a writer gets this
            // wrong.
            "refine": "ADD",
        });
        if let Some(entry) = entry {
            tile["content"] = json!({ "uri": entry.uri });
        }
        if !children.is_empty() {
            tile["children"] = Value::Array(children);
        }
        tile
    }

    /// Whether a key or anything below it has points.
    ///
    /// A sparse octree has gaps in the middle: a node with no points of its own
    /// may still have a populated descendant, and skipping it would orphan the
    /// subtree.
    fn has_descendant(&self, key: OctreeKey) -> bool {
        let lo = (key.level, key.x, key.y, key.z);
        if self.nodes.contains_key(&lo) {
            return true;
        }
        for (level, x, y, z) in self.nodes.keys() {
            if *level <= key.level {
                continue;
            }
            let shift = level - key.level;
            if (x >> shift, y >> shift, z >> shift) == (key.x, key.y, key.z) {
                return true;
            }
        }
        false
    }
}

impl NodeSink for TilesetWriter {
    fn node(&mut self, node: BuiltNode) -> Result<()> {
        self.write_node(node)
    }
}

/// Assemble a GLB with one POINTS primitive.
fn build_glb(
    positions: &[u8],
    colors: Option<&[u8]>,
    count: usize,
    min: [f32; 3],
    max: [f32; 3],
) -> Vec<u8> {
    // Every bufferView starts 4-byte aligned, which the spec requires of an
    // accessor whose components are 4 bytes wide.
    let mut binary = positions.to_vec();
    let color_offset = binary.len();
    if let Some(colors) = colors {
        binary.extend_from_slice(colors);
    }
    while binary.len() % 4 != 0 {
        binary.push(0);
    }

    let mut accessors = vec![json!({
        "bufferView": 0,
        "componentType": 5126,
        "count": count,
        "type": "VEC3",
        "min": min,
        "max": max,
    })];
    let mut views = vec![json!({
        "buffer": 0,
        "byteOffset": 0,
        "byteLength": positions.len(),
    })];
    let mut attributes = json!({ "POSITION": 0 });
    if let Some(colors) = colors {
        // Normalized unsigned bytes: three per point, which is what COLOR_0
        // takes and a third of the size of floats.
        accessors.push(json!({
            "bufferView": 1,
            "componentType": 5121,
            "normalized": true,
            "count": count,
            "type": "VEC3",
        }));
        views.push(json!({
            "buffer": 0,
            "byteOffset": color_offset,
            "byteLength": colors.len(),
        }));
        attributes["COLOR_0"] = json!(1);
    }

    let doc = json!({
        "asset": { "version": "2.0", "generator": "voxelkloud" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0 }],
        "meshes": [{ "primitives": [{ "attributes": attributes, "mode": 0 }] }],
        "accessors": accessors,
        "bufferViews": views,
        "buffers": [{ "byteLength": binary.len() }],
    });

    let mut json_bytes = serde_json::to_vec(&doc).unwrap_or_default();
    while json_bytes.len() % 4 != 0 {
        json_bytes.push(b' ');
    }

    let total = 12 + 8 + json_bytes.len() + 8 + binary.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&GLB_MAGIC.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&CHUNK_JSON.to_le_bytes());
    out.extend_from_slice(&json_bytes);
    out.extend_from_slice(&(binary.len() as u32).to_le_bytes());
    out.extend_from_slice(&CHUNK_BIN.to_le_bytes());
    out.extend_from_slice(&binary);
    out
}
