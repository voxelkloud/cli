//! 3D Tiles: `tileset.json`, external tilesets, implicit tiling.
//!
//! The sixth format, and the first whose semantics are written TWICE in this
//! project — once here and once in `@voxelkloud/format-3dtiles`. Everywhere
//! else a shared truth had shared bytes to justify a bridge: the LAS framing
//! moved into this crate and the browser's codec package compiles it, the E57
//! reader serves the converter and the browser tier from one implementation.
//! A tileset is JSON and accessor arithmetic. There is no codec to share, so
//! the duplication buys nothing to avoid and the two stay independent.
//!
//! What this side is FOR is the half the browser cannot do: telling someone
//! what is in a deployment they already have, and whether it will serve.
//!
//! Two things a tileset does not declare, and both matter to `inspect`:
//!
//!   * **No point counts.** They live in each tile's payload. `--deep` walks
//!     the tree and can say how many TILES there are; saying how many POINTS
//!     would mean fetching every one of them.
//!   * **No attributes.** What a point carries is in the first tile's feature
//!     table or accessor list, not in the manifest.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;

use crate::bounds::Bounds;
use crate::cloud::{CloudInfo, FormatId, HierarchyStats, NodeInfo};
use crate::crs::Crs;
use crate::error::{Error, Result};
use crate::octree::OctreeKey;
use crate::source::Store;
use crate::warning::Warning;

/// WGS 84, for the `region` bounding volume, which is defined on it.
const WGS84_A: f64 = 6_378_137.0;
const WGS84_B: f64 = WGS84_A * (1.0 - 1.0 / 298.257_223_563);

/// How deep a chain of external tilesets to follow before giving up.
const MAX_EXTERNAL_DEPTH: u32 = 8;

pub struct TilesetCloud {
    pub info: CloudInfo,
    /// `asset.version`, verbatim.
    pub version: String,
    /// Whether the root sits on the WGS 84 ellipsoid.
    pub georeferenced: bool,
    /// What kinds of content the root document names, for the summary line.
    pub content_kinds: Vec<String>,
    /// Whether the tileset describes its tree with a rule instead of writing it.
    pub implicit: Option<ImplicitSummary>,
    store: Arc<dyn Store>,
    manifest: String,
}

#[derive(Debug, Clone)]
pub struct ImplicitSummary {
    pub scheme: String,
    pub subtree_levels: u32,
    pub available_levels: u32,
}

pub fn open(store: Arc<dyn Store>, label: &str) -> Result<TilesetCloud> {
    open_manifest(store, "tileset.json", label)
}

pub fn open_manifest(store: Arc<dyn Store>, manifest: &str, label: &str) -> Result<TilesetCloud> {
    let bytes = store.open(manifest)?.read_all()?;
    let json: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::not_format("3D Tiles", format!("{manifest} is not JSON: {e}")))?;
    let obj = json.as_object().ok_or_else(|| {
        Error::not_format("3D Tiles", format!("{manifest} is not a JSON object"))
    })?;

    // `asset` plus a `root` that carries a `geometricError` is the pair no
    // other manifest here has. `asset` alone would also match a glTF, which is
    // a document this project reads but is not a tileset.
    let root = obj
        .get("root")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::not_format("3D Tiles", format!("{manifest} has no root tile")))?;
    if obj.get("asset").and_then(Value::as_object).is_none() {
        return Err(Error::not_format(
            "3D Tiles",
            format!("{manifest} has no asset object"),
        ));
    }
    if root.get("geometricError").and_then(Value::as_f64).is_none() {
        return Err(Error::not_format(
            "3D Tiles",
            format!("{manifest}'s root tile declares no geometricError"),
        ));
    }

    let mut warnings = Vec::new();
    let version = obj
        .get("asset")
        .and_then(|a| a.get("version"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if version != "1.0" && version != "1.1" {
        warnings.push(Warning::new(
            "tileset-version-unknown",
            "asset.version",
            format!("asset.version is {version:?}; reading it as 1.1"),
        ));
    }

    let transform = read_transform(root);
    let volume = root.get("boundingVolume");
    let (bounds, georeferenced) = volume_bounds(volume, &transform).ok_or_else(|| {
        Error::manifest(
            manifest.to_string(),
            "the root tile has no bounding volume this reader can use",
        )
    })?;

    let mut content_kinds = Vec::new();
    collect_kinds(json.get("root").unwrap(), &mut content_kinds);

    let implicit = root.get("implicitTiling").and_then(Value::as_object).map(|it| {
        let available = it
            .get("availableLevels")
            .and_then(Value::as_u64)
            .or_else(|| it.get("maximumLevel").and_then(Value::as_u64).map(|m| m + 1))
            .unwrap_or(0) as u32;
        ImplicitSummary {
            scheme: it
                .get("subdivisionScheme")
                .and_then(Value::as_str)
                .unwrap_or("OCTREE")
                .to_string(),
            subtree_levels: it.get("subtreeLevels").and_then(Value::as_u64).unwrap_or(0) as u32,
            available_levels: available,
        }
    });

    // Said out loud rather than left as a zero that reads like a fact.
    warnings.push(Warning::new(
        "point-count-unknown",
        "tileset.json",
        "a tileset declares no point counts; they live in each tile's payload, \
         so `points` is 0 and `--deep` counts tiles instead",
    ));

    let info = CloudInfo {
        format: FormatId::Tiles3D,
        label: label.to_string(),
        point_count: 0,
        bounds,
        tight_bounds: bounds,
        scale: [1.0, 1.0, 1.0],
        offset: [0.0, 0.0, 0.0],
        attributes: Vec::new(),
        crs: if georeferenced {
            Some(Crs::from_epsg(4978))
        } else {
            None
        },
        spacing: root.get("geometricError").and_then(Value::as_f64),
        levels: implicit.as_ref().map(|i| i.available_levels.saturating_sub(1)),
        encoding: content_kinds.first().cloned(),
        version: Some(version.clone()),
        data_bytes: None,
        record_bytes: None,
        warnings,
    };

    Ok(TilesetCloud {
        info,
        version,
        georeferenced,
        content_kinds,
        implicit,
        store,
        manifest: manifest.to_string(),
    })
}

impl TilesetCloud {
    /// The first tile payload a viewer will actually ask for.
    ///
    /// What `doctor` should probe. NOT the manifest: a manifest is one small
    /// JSON read once, and every property worth checking is about the file the
    /// viewer fetches hundreds of times. For an implicit tileset there is no
    /// such file until a subtree is read, so the subtree is the next best
    /// thing — it is on the same host with the same headers.
    pub fn probe_path(&self) -> String {
        if let Ok(bytes) = self.store.open(&self.manifest).and_then(|s| s.read_all()) {
            if let Ok(json) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(root) = json.get("root") {
                    if let Some(uri) = first_payload(root) {
                        return join(&self.manifest, &uri);
                    }
                }
            }
        }
        String::new()
    }

    /// Walk the tree: this document, every external tileset it names, and every
    /// `.subtree` an implicit rule points at.
    ///
    /// Counts TILES, never points — see the module note. `data_bytes` stays 0
    /// for the same reason: a tile's payload size is a property of the file on
    /// disk, and asking for all of them is the walk this deliberately is not.
    pub fn hierarchy(&self) -> Result<HierarchyStats> {
        let mut stats = HierarchyStats::default();
        let mut seen: HashSet<String> = HashSet::new();
        self.walk_document(&self.manifest, 0, 0, &mut stats, &mut seen)?;
        Ok(stats)
    }

    fn walk_document(
        &self,
        path: &str,
        base_level: u32,
        depth: u32,
        stats: &mut HierarchyStats,
        seen: &mut HashSet<String>,
    ) -> Result<()> {
        if !seen.insert(path.to_string()) {
            return Ok(());
        }
        if depth > MAX_EXTERNAL_DEPTH {
            stats.warnings.push(Warning::new(
                "external-tileset-depth",
                path.to_string(),
                format!("stopped following external tilesets at {path}, {depth} deep"),
            ));
            return Ok(());
        }
        let source = match self.store.open(path) {
            Ok(source) => source,
            Err(_) => {
                stats.warnings.push(Warning::new(
                    "external-tileset-missing",
                    path.to_string(),
                    format!("{path} is named by a tile and is not there"),
                ));
                return Ok(());
            }
        };
        let bytes = source.read_all()?;
        stats.hierarchy_bytes += bytes.len() as u64;
        stats.hierarchy_requests += 1;
        let json: Value = serde_json::from_slice(&bytes)
            .map_err(|e| Error::manifest(path.to_string(), format!("not JSON: {e}")))?;
        let Some(root) = json.get("root") else {
            return Ok(());
        };
        self.walk_tile(root, path, base_level, depth, stats, seen)
    }

    fn walk_tile(
        &self,
        tile: &Value,
        document: &str,
        level: u32,
        depth: u32,
        stats: &mut HierarchyStats,
        seen: &mut HashSet<String>,
    ) -> Result<()> {
        stats.add(NodeInfo {
            key: OctreeKey {
                level,
                x: 0,
                y: 0,
                z: 0,
            },
            point_count: 0,
            byte_size: 0,
        });
        if level > stats.depth {
            stats.depth = level;
        }

        // An external tileset hangs its own root under this tile.
        for uri in content_uris(tile) {
            if uri.ends_with(".json") {
                let resolved = join(document, &uri);
                self.walk_document(&resolved, level + 1, depth + 1, stats, seen)?;
            }
        }

        if let Some(it) = tile.get("implicitTiling").and_then(Value::as_object) {
            self.walk_implicit(tile, it, document, level, stats)?;
        }

        if let Some(children) = tile.get("children").and_then(Value::as_array) {
            for child in children {
                self.walk_tile(child, document, level + 1, depth, stats, seen)?;
            }
        }
        Ok(())
    }

    /// Follow the subtree files an implicit rule points at, counting the tiles
    /// their availability bits turn on.
    fn walk_implicit(
        &self,
        tile: &Value,
        it: &serde_json::Map<String, Value>,
        document: &str,
        base_level: u32,
        stats: &mut HierarchyStats,
    ) -> Result<()> {
        let branching = match it.get("subdivisionScheme").and_then(Value::as_str) {
            Some("QUADTREE") => 4u64,
            _ => 8u64,
        };
        let subtree_levels = it.get("subtreeLevels").and_then(Value::as_u64).unwrap_or(0);
        let available_levels = it
            .get("availableLevels")
            .and_then(Value::as_u64)
            .or_else(|| it.get("maximumLevel").and_then(Value::as_u64).map(|m| m + 1))
            .unwrap_or(0);
        let Some(template) = it
            .get("subtrees")
            .and_then(|s| s.get("uri"))
            .and_then(Value::as_str)
        else {
            return Ok(());
        };
        if subtree_levels == 0 {
            return Ok(());
        }
        let _ = tile;

        // Breadth-first over subtree ROOTS, each identified by its coordinate.
        let mut queue: Vec<(u64, u64, u64, u64)> = vec![(0, 0, 0, 0)];
        while let Some((level, x, y, z)) = queue.pop() {
            if level >= available_levels {
                continue;
            }
            let path = join(
                document,
                &fill_template(template, level, x, y, z, branching == 8),
            );
            let Ok(source) = self.store.open(&path) else {
                stats.warnings.push(Warning::new(
                    "subtree-missing",
                    path.clone(),
                    format!("{path} is named by an implicit rule and is not there"),
                ));
                continue;
            };
            let bytes = source.read_all()?;
            stats.hierarchy_bytes += bytes.len() as u64;
            stats.hierarchy_requests += 1;
            let Some(subtree) = Subtree::parse(&bytes) else {
                stats.warnings.push(Warning::new(
                    "subtree-unreadable",
                    path.clone(),
                    format!("{path} is not a readable .subtree"),
                ));
                continue;
            };

            // A subtree's level 0 IS a tile, and for every subtree but the
            // first it is one nothing else counts: the declaring tile is
            // counted by `walk_tile`, but a child subtree's root is only
            // reached through this queue. Skipping it leaves a HOLE at every
            // subtree boundary — a level with no nodes between two that have
            // them, which is what the level table showed.
            if level > 0 && subtree.tile_available(branching, 0, 0) {
                let global = base_level + level as u32;
                stats.add(NodeInfo {
                    key: OctreeKey {
                        level: global,
                        x: 0,
                        y: 0,
                        z: 0,
                    },
                    point_count: 0,
                    byte_size: 0,
                });
                if global > stats.depth {
                    stats.depth = global;
                }
            }

            for l in 1..subtree_levels {
                let global = base_level + (level + l) as u32;
                if (level + l) >= available_levels {
                    break;
                }
                let count = branching.pow(l as u32);
                for morton in 0..count {
                    if !subtree.tile_available(branching, l, morton) {
                        continue;
                    }
                    stats.add(NodeInfo {
                        key: OctreeKey {
                            level: global,
                            x: 0,
                            y: 0,
                            z: 0,
                        },
                        point_count: 0,
                        byte_size: 0,
                    });
                    if global > stats.depth {
                        stats.depth = global;
                    }
                }
            }

            // Every child subtree the file says exists.
            let span = 1u64 << subtree_levels;
            let child_count = branching.pow(subtree_levels as u32);
            for morton in 0..child_count {
                if !subtree.child_subtree_available(morton) {
                    continue;
                }
                let (dx, dy, dz) = morton_decode(morton, branching == 8, subtree_levels as u32);
                queue.push((
                    level + subtree_levels,
                    x * span + dx,
                    y * span + dy,
                    z * span + dz,
                ));
            }
        }
        Ok(())
    }
}

/// Just enough of a `.subtree` to count what is available.
struct Subtree {
    tiles: Availability,
    children: Availability,
}

enum Availability {
    Constant(bool),
    Bits(Vec<u8>),
}

impl Availability {
    fn get(&self, index: u64) -> bool {
        match self {
            Self::Constant(v) => *v,
            Self::Bits(bytes) => {
                let byte = (index >> 3) as usize;
                // LSB FIRST inside each byte, which is the spec's order and the
                // opposite of how a bitstream is usually drawn.
                bytes.get(byte).is_some_and(|b| (b >> (index & 7)) & 1 == 1)
            }
        }
    }
}

impl Subtree {
    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 24 || &bytes[0..4] != b"subt" {
            return None;
        }
        let json_len = u64::from_le_bytes(bytes[8..16].try_into().ok()?) as usize;
        if 24 + json_len > bytes.len() {
            return None;
        }
        let json: Value = serde_json::from_slice(&bytes[24..24 + json_len]).ok()?;
        let binary = &bytes[24 + json_len..];

        // An external buffer is refused rather than read as empty: empty would
        // report every tile as unavailable, which looks like an empty tileset.
        if json
            .get("buffers")
            .and_then(Value::as_array)
            .is_some_and(|bs| bs.iter().any(|b| b.get("uri").is_some()))
        {
            return None;
        }

        let views: Vec<Vec<u8>> = json
            .get("bufferViews")
            .and_then(Value::as_array)
            .map(|vs| {
                vs.iter()
                    .map(|v| {
                        let offset = v.get("byteOffset").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let length = v.get("byteLength").and_then(Value::as_u64).unwrap_or(0) as usize;
                        binary
                            .get(offset..offset + length)
                            .map(<[u8]>::to_vec)
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default();

        let read = |key: &str| -> Availability {
            let Some(node) = json.get(key) else {
                return Availability::Constant(false);
            };
            if let Some(c) = node.get("constant").and_then(Value::as_u64) {
                return Availability::Constant(c != 0);
            }
            let view = node
                .get("bitstream")
                .or_else(|| node.get("bufferView"))
                .and_then(Value::as_u64);
            match view.and_then(|i| views.get(i as usize)) {
                Some(bytes) => Availability::Bits(bytes.clone()),
                None => Availability::Constant(false),
            }
        };

        Some(Self {
            tiles: read("tileAvailability"),
            children: read("childSubtreeAvailability"),
        })
    }

    fn tile_available(&self, branching: u64, level_in_subtree: u64, morton: u64) -> bool {
        // Level-order: the offset of a level is the number of nodes above it.
        let offset = (branching.pow(level_in_subtree as u32) - 1) / (branching - 1);
        self.tiles.get(offset + morton)
    }

    fn child_subtree_available(&self, morton: u64) -> bool {
        self.children.get(morton)
    }
}

fn morton_decode(morton: u64, three_d: bool, levels: u32) -> (u64, u64, u64) {
    let (mut x, mut y, mut z) = (0u64, 0u64, 0u64);
    let step = if three_d { 3 } else { 2 };
    for b in 0..levels as u64 {
        x |= ((morton >> (step * b)) & 1) << b;
        y |= ((morton >> (step * b + 1)) & 1) << b;
        if three_d {
            z |= ((morton >> (step * b + 2)) & 1) << b;
        }
    }
    (x, y, z)
}

fn fill_template(template: &str, level: u64, x: u64, y: u64, z: u64, three_d: bool) -> String {
    let mut out = template
        .replace("{level}", &level.to_string())
        .replace("{x}", &x.to_string())
        .replace("{y}", &y.to_string());
    if three_d {
        out = out.replace("{z}", &z.to_string());
    }
    out
}

/// Resolve a relative URI against the document that declared it.
///
/// The bug this format is famous for: a nested tileset changes the base, and a
/// URI resolved against the top-level document lands nowhere.
fn join(document: &str, uri: &str) -> String {
    match document.rfind('/') {
        Some(cut) => format!("{}{}", &document[..=cut], uri),
        None => uri.to_string(),
    }
}

fn content_uris(tile: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(contents) = tile.get("contents").and_then(Value::as_array) {
        for c in contents {
            if let Some(uri) = c.get("uri").or_else(|| c.get("url")).and_then(Value::as_str) {
                out.push(uri.to_string());
            }
        }
    }
    if let Some(c) = tile.get("content") {
        if let Some(uri) = c.get("uri").or_else(|| c.get("url")).and_then(Value::as_str) {
            out.push(uri.to_string());
        }
    }
    out
}

/// The first content URI under a tile that is a payload rather than a document.
fn first_payload(tile: &Value) -> Option<String> {
    for uri in content_uris(tile) {
        let path = uri.split(['?', '#']).next().unwrap_or("");
        if !path.ends_with(".json") && !path.contains('{') {
            return Some(uri);
        }
    }
    for child in tile.get("children").and_then(Value::as_array)? {
        if let Some(uri) = first_payload(child) {
            return Some(uri);
        }
    }
    None
}

fn collect_kinds(tile: &Value, out: &mut Vec<String>) {
    for uri in content_uris(tile) {
        let path = uri.split(['?', '#']).next().unwrap_or("");
        let kind = match path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()) {
            Some(ext) if ext == "pnts" => "pnts",
            Some(ext) if ext == "glb" || ext == "gltf" => "gltf",
            Some(ext) if ext == "json" => "tileset",
            Some(ext) if ext == "b3dm" || ext == "i3dm" || ext == "cmpt" => "mesh",
            _ => "unknown",
        };
        if !out.iter().any(|k| k == kind) {
            out.push(kind.to_string());
        }
    }
    if let Some(children) = tile.get("children").and_then(Value::as_array) {
        for child in children {
            collect_kinds(child, out);
        }
    }
}

fn read_transform(tile: &serde_json::Map<String, Value>) -> [f64; 16] {
    let mut m = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];
    if let Some(t) = tile.get("transform").and_then(Value::as_array) {
        if t.len() >= 16 {
            for (i, v) in t.iter().take(16).enumerate() {
                m[i] = v.as_f64().unwrap_or(m[i]);
            }
        }
    }
    m
}

/// A bounding volume as an axis-aligned box, and whether it sits on the planet.
fn volume_bounds(volume: Option<&Value>, m: &[f64; 16]) -> Option<(Bounds, bool)> {
    let volume = volume?;

    if let Some(b) = volume.get("box").and_then(Value::as_array) {
        if b.len() >= 12 {
            let v: Vec<f64> = b.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect();
            let c = transform_point(m, v[0], v[1], v[2]);
            let axes = [
                transform_vector(m, v[3], v[4], v[5]),
                transform_vector(m, v[6], v[7], v[8]),
                transform_vector(m, v[9], v[10], v[11]),
            ];
            // Every corner is centre +/- h0 +/- h1 +/- h2, so the extreme on an
            // axis is reached by choosing each sign to agree.
            let half: Vec<f64> = (0..3)
                .map(|i| axes.iter().map(|a| a[i].abs()).sum())
                .collect();
            let bounds = Bounds {
                min: [c[0] - half[0], c[1] - half[1], c[2] - half[2]],
                max: [c[0] + half[0], c[1] + half[1], c[2] + half[2]],
            };
            let ecef = looks_ecef(c[0], c[1], c[2]);
            return Some((bounds, ecef));
        }
    }

    if let Some(s) = volume.get("sphere").and_then(Value::as_array) {
        if s.len() >= 4 {
            let v: Vec<f64> = s.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect();
            let c = transform_point(m, v[0], v[1], v[2]);
            let scale = (0..3)
                .map(|col| {
                    (m[col * 4].powi(2) + m[col * 4 + 1].powi(2) + m[col * 4 + 2].powi(2)).sqrt()
                })
                .fold(0.0f64, f64::max);
            let r = v[3] * scale;
            return Some((
                Bounds {
                    min: [c[0] - r, c[1] - r, c[2] - r],
                    max: [c[0] + r, c[1] + r, c[2] + r],
                },
                looks_ecef(c[0], c[1], c[2]),
            ));
        }
    }

    if let Some(r) = volume.get("region").and_then(Value::as_array) {
        if r.len() >= 6 {
            let v: Vec<f64> = r.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect();
            // A region is already georeferenced, so the tile transform does NOT
            // apply to it — the spec's rule, and Cesium's behaviour.
            return Some((region_bounds(&v), true));
        }
    }

    None
}

/// The EXACT axis-aligned box of a `region`, in ECEF.
///
/// Not the hull of the eight corners: an ellipsoidal region bulges outward
/// between them, so a corner hull is too small. `r = (N + h) cos(lat)` is
/// unimodal in latitude with its peak at the equator, and `z` is monotone in
/// both latitude and height, so both ranges are a handful of evaluations.
fn region_bounds(r: &[f64]) -> Bounds {
    let (west, south, east, north, min_h, max_h) = (r[0], r[1], r[2], r[3], r[4], r[5]);
    let e2 = 1.0 - (WGS84_B * WGS84_B) / (WGS84_A * WGS84_A);

    let r_at = |lat: f64, h: f64| {
        let n = WGS84_A / (1.0 - e2 * lat.sin().powi(2)).sqrt();
        (n + h) * lat.cos()
    };
    let z_at = |lat: f64, h: f64| {
        let n = WGS84_A / (1.0 - e2 * lat.sin().powi(2)).sqrt();
        (n * (1.0 - e2) + h) * lat.sin()
    };

    let lat_peak = if south > 0.0 {
        south
    } else if north < 0.0 {
        north
    } else {
        0.0
    };
    let r_max = r_at(lat_peak, max_h);
    let r_min = r_at(south, min_h).min(r_at(north, min_h));

    let two_pi = std::f64::consts::TAU;
    let span = {
        let raw = east - west;
        if raw >= two_pi - 1e-12 {
            two_pi
        } else if raw >= 0.0 {
            raw
        } else {
            raw + two_pi
        }
    };
    let contains = |angle: f64| {
        if span >= two_pi - 1e-12 {
            return true;
        }
        let norm = ((angle - west) % two_pi + two_pi) % two_pi;
        norm <= span + 1e-15
    };
    let trig_range = |f: &dyn Fn(f64) -> f64, critical: &[f64]| {
        let mut lo = f(west).min(f(east));
        let mut hi = f(west).max(f(east));
        for c in critical {
            if contains(*c) {
                lo = lo.min(f(*c));
                hi = hi.max(f(*c));
            }
        }
        (lo, hi)
    };
    let (cos_lo, cos_hi) = trig_range(&f64::cos, &[0.0, std::f64::consts::PI]);
    let (sin_lo, sin_hi) = trig_range(
        &f64::sin,
        &[std::f64::consts::FRAC_PI_2, -std::f64::consts::FRAC_PI_2],
    );
    let product = |(a_lo, a_hi): (f64, f64), (b_lo, b_hi): (f64, f64)| {
        let p = [a_lo * b_lo, a_lo * b_hi, a_hi * b_lo, a_hi * b_hi];
        (
            p.iter().copied().fold(f64::INFINITY, f64::min),
            p.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        )
    };
    let (min_x, max_x) = product((r_min, r_max), (cos_lo, cos_hi));
    let (min_y, max_y) = product((r_min, r_max), (sin_lo, sin_hi));
    let zs = [
        z_at(south, min_h),
        z_at(south, max_h),
        z_at(north, min_h),
        z_at(north, max_h),
    ];

    Bounds {
        min: [
            min_x,
            min_y,
            zs.iter().copied().fold(f64::INFINITY, f64::min),
        ],
        max: [
            max_x,
            max_y,
            zs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        ],
    }
}

/// Whether a point sits near the WGS 84 ellipsoid.
///
/// A `region` says "georeferenced" outright, and it is the case everyone reaches
/// for — but it is not the common one. Most tilesets a pipeline emits use a
/// local box under a root transform whose translation is an ECEF position, and
/// never write a region at all. Looking only for a region calls those local.
fn looks_ecef(x: f64, y: f64, z: f64) -> bool {
    let r = (x * x + y * y + z * z).sqrt();
    if r <= 0.0 {
        return false;
    }
    let sin2 = (z * z) / (r * r);
    let cos2 = 1.0 - sin2;
    let surface =
        (WGS84_A * WGS84_B) / (WGS84_B * WGS84_B * cos2 + WGS84_A * WGS84_A * sin2).sqrt();
    (r - surface).abs() < 100_000.0
}

fn transform_point(m: &[f64; 16], x: f64, y: f64, z: f64) -> [f64; 3] {
    [
        m[0] * x + m[4] * y + m[8] * z + m[12],
        m[1] * x + m[5] * y + m[9] * z + m[13],
        m[2] * x + m[6] * y + m[10] * z + m[14],
    ]
}

fn transform_vector(m: &[f64; 16], x: f64, y: f64, z: f64) -> [f64; 3] {
    [
        m[0] * x + m[4] * y + m[8] * z,
        m[1] * x + m[5] * y + m[9] * z,
        m[2] * x + m[6] * y + m[10] * z,
    ]
}
