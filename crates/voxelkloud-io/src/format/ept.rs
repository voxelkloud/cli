//! Entwine Point Tile: `ept.json`, JSON hierarchy pages, one file per node.
//!
//! The cheapest of the three indexed formats to read and the most spread out on
//! disk: a manifest, a directory of hierarchy pages that reference each other
//! by name, and a directory of node payloads in one of three encodings.
//!
//! Its hierarchy pages are the same idea as Potree's proxy chunks and COPC's
//! page entries in a third spelling — a node whose point count is `-1` is a
//! reference to the page named after it.

use std::sync::Arc;

use serde_json::Value;

use crate::attribute::{lay_out, Attribute, AttributeType};
use crate::bounds::Bounds;
use crate::cloud::{CloudInfo, FormatId, HierarchyStats, NodeInfo};
use crate::crs::Crs;
use crate::error::{Error, Result};
use crate::octree::OctreeKey;
use crate::source::Store;

/// A page entry whose point count is this is a pointer at another page.
const PAGE_REFERENCE: i64 = -1;

pub struct EptCloud {
    pub info: CloudInfo,
    /// `binary`, `laszip` or `zstandard`.
    pub data_type: String,
    /// The grid resolution per node edge. EPT's statement of density: `span`
    /// points across a node, so the spacing is the node's edge over it.
    pub span: u32,
    store: Arc<dyn Store>,
    /// The page every walk starts from.
    hierarchy_root: String,
}

pub fn open(store: Arc<dyn Store>, label: &str) -> Result<EptCloud> {
    open_manifest(store, "ept.json", label)
}

pub fn open_manifest(store: Arc<dyn Store>, manifest: &str, label: &str) -> Result<EptCloud> {
    let bytes = store.open(manifest)?.read_all()?;
    let json: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::manifest(manifest.to_string(), format!("not JSON: {e}")))?;
    let obj = json
        .as_object()
        .ok_or_else(|| Error::manifest(manifest.to_string(), "the manifest is not a JSON object"))?;

    // `bounds` plus `schema` is the discriminator. A Potree manifest has
    // neither, and a 3D Tiles tileset has neither.
    let raw_bounds = obj
        .get("bounds")
        .and_then(Value::as_array)
        .filter(|b| b.len() >= 6)
        .ok_or_else(|| Error::not_format("EPT", "the manifest declares no 6-element bounds"))?;
    let at = |i: usize| raw_bounds[i].as_f64().unwrap_or(0.0);

    let mut info = CloudInfo::new(FormatId::Ept, label);
    info.version = obj.get("version").and_then(Value::as_str).map(str::to_string);
    info.point_count = obj.get("points").and_then(Value::as_u64).unwrap_or(0);
    info.bounds = Bounds::new([at(0), at(1), at(2)], [at(3), at(4), at(5)]);
    info.tight_bounds = match obj.get("boundsConforming").and_then(Value::as_array) {
        Some(b) if b.len() >= 6 => {
            let c = |i: usize| b[i].as_f64().unwrap_or(0.0);
            Bounds::new([c(0), c(1), c(2)], [c(3), c(4), c(5)])
        }
        _ => info.bounds,
    };

    let data_type = obj
        .get("dataType")
        .and_then(Value::as_str)
        .unwrap_or("binary")
        .to_string();
    info.encoding = Some(data_type.clone());
    let span = obj.get("span").and_then(Value::as_u64).unwrap_or(0) as u32;
    if span > 0 {
        // The root node holds `span³` points across the root cube, so
        // neighbours sit one grid cell apart. That is the same quantity Potree
        // and COPC call spacing, stated as a resolution instead.
        info.spacing = Some(info.bounds.longest_edge() / f64::from(span));
    }

    info.crs = obj.get("srs").and_then(|srs| {
        if let Some(wkt) = srs.get("wkt").and_then(Value::as_str) {
            if let Some(crs) = Crs::from_string(wkt) {
                return Some(crs);
            }
        }
        // `authority` + `horizontal` is the other spelling, and the two halves
        // are separate fields: `{"authority": "EPSG", "horizontal": "3857"}`.
        let authority = srs.get("authority").and_then(Value::as_str)?;
        let horizontal = srs.get("horizontal").and_then(Value::as_str)?;
        if !authority.eq_ignore_ascii_case("EPSG") {
            return None;
        }
        let mut crs = Crs::from_epsg(horizontal.parse().ok()?);
        crs.vertical_epsg = srs
            .get("vertical")
            .and_then(Value::as_str)
            .and_then(|v| v.parse().ok());
        Some(crs)
    });

    let schema = obj
        .get("schema")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::not_format("EPT", "the manifest declares no schema"))?;
    let mut attributes = Vec::with_capacity(schema.len());
    let mut xyz: Vec<(f64, f64)> = Vec::new();
    for (i, field) in schema.iter().enumerate() {
        let path = format!("schema[{i}]");
        let name = field
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::manifest(path.clone(), "field has no name"))?;
        let size = field.get("size").and_then(Value::as_u64).unwrap_or(0) as usize;
        let type_name = field.get("type").and_then(Value::as_str).unwrap_or("unsigned");
        let Some(kind) = ept_type(type_name, size) else {
            return Err(Error::manifest(
                path,
                format!("{name:?} is {type_name} of {size} bytes, which is not a type"),
            ));
        };
        if matches!(name, "X" | "Y" | "Z") {
            xyz.push((
                field.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
                field.get("offset").and_then(Value::as_f64).unwrap_or(0.0),
            ));
        }
        attributes.push(Attribute {
            name: ept_attribute_name(name),
            description: String::new(),
            kind,
            num_elements: 1,
            byte_offset: 0,
            min: vec![0.0],
            max: vec![0.0],
            scale: vec![field.get("scale").and_then(Value::as_f64).unwrap_or(1.0)],
            offset: vec![field.get("offset").and_then(Value::as_f64).unwrap_or(0.0)],
            histogram: None,
        });
    }
    if xyz.len() == 3 {
        info.scale = [xyz[0].0, xyz[1].0, xyz[2].0];
        info.offset = [xyz[0].1, xyz[1].1, xyz[2].1];
    }
    let stride = lay_out(&mut attributes);
    info.attributes = attributes;
    info.record_bytes = Some(stride);

    if obj.get("hierarchyType").and_then(Value::as_str).unwrap_or("json") != "json" {
        info.warn(
            "hierarchy-type",
            "ept.json.hierarchyType",
            "The manifest declares a non-JSON hierarchy, which this reader does not \
             know how to walk."
                .to_string(),
        );
    }

    Ok(EptCloud {
        info,
        data_type,
        span,
        store,
        hierarchy_root: "ept-hierarchy/0-0-0-0.json".to_string(),
    })
}

/// EPT's dimension names are PDAL's; ours are PotreeConverter's. Translating
/// here is what lets one colour mode work on a cloud from either.
fn ept_attribute_name(name: &str) -> String {
    match name {
        "X" => "X",
        "Y" => "Y",
        "Z" => "Z",
        "Intensity" => "intensity",
        "ReturnNumber" => "return number",
        "NumberOfReturns" => "number of returns",
        "ScanDirectionFlag" => "scan direction flag",
        "EdgeOfFlightLine" => "edge of flight line",
        "Classification" => "classification",
        "ScanAngleRank" => "scan angle rank",
        "UserData" => "user data",
        "PointSourceId" => "point source id",
        "GpsTime" => "gps-time",
        "Red" => "red",
        "Green" => "green",
        "Blue" => "blue",
        other => other,
    }
    .to_string()
}

fn ept_type(kind: &str, size: usize) -> Option<AttributeType> {
    Some(match (kind, size) {
        ("signed", 1) => AttributeType::Int8,
        ("signed", 2) => AttributeType::Int16,
        ("signed", 4) => AttributeType::Int32,
        ("signed", 8) => AttributeType::Int64,
        ("unsigned", 1) => AttributeType::Uint8,
        ("unsigned", 2) => AttributeType::Uint16,
        ("unsigned", 4) => AttributeType::Uint32,
        ("unsigned", 8) => AttributeType::Uint64,
        ("float", 4) => AttributeType::Float,
        ("float", 8) => AttributeType::Double,
        _ => return None,
    })
}

impl EptCloud {
    /// Walk the JSON hierarchy pages.
    pub fn hierarchy(&self) -> Result<HierarchyStats> {
        let mut stats = HierarchyStats::default();
        let mut queue = vec![self.hierarchy_root.clone()];
        let mut visited: Vec<String> = Vec::new();

        while let Some(path) = queue.pop() {
            if visited.contains(&path) {
                continue;
            }
            visited.push(path.clone());

            let bytes = self.store.open(&path)?.read_all()?;
            stats.hierarchy_bytes += bytes.len() as u64;
            stats.hierarchy_requests += 1;

            let page: Value = serde_json::from_slice(&bytes)
                .map_err(|e| Error::manifest(path.clone(), format!("not JSON: {e}")))?;
            let entries = page
                .as_object()
                .ok_or_else(|| Error::manifest(path.clone(), "a hierarchy page must be an object"))?;

            for (name, count) in entries {
                let Some(key) = parse_ept_key(name) else {
                    stats.warnings.push(crate::warning::Warning::new(
                        "hierarchy-key",
                        format!("{path}#{name}"),
                        format!("{name:?} is not a level-x-y-z key; the entry is skipped."),
                    ));
                    continue;
                };
                let count = count.as_i64().unwrap_or(0);
                if count == PAGE_REFERENCE {
                    queue.push(format!("ept-hierarchy/{name}.json"));
                    continue;
                }
                stats.add(NodeInfo {
                    key,
                    point_count: count.max(0) as u64,
                    // EPT states no payload size in the hierarchy. A walk that
                    // wanted bytes would have to stat every node file, which is
                    // one request each — deliberately not done.
                    byte_size: 0,
                });
            }
        }
        Ok(stats)
    }

    /// The path of a node's payload, in the encoding the manifest declared.
    pub fn node_path(&self, key: OctreeKey) -> String {
        let extension = match self.data_type.as_str() {
            "laszip" => "laz",
            "zstandard" => "zst",
            _ => "bin",
        };
        format!("ept-data/{}.{extension}", key.ept_name())
    }
}

fn parse_ept_key(name: &str) -> Option<OctreeKey> {
    let mut parts = name.split('-');
    let level = parts.next()?.parse().ok()?;
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    let z = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(OctreeKey::new(level, x, y, z))
}
