//! `voxelkloud inspect` — what is this cloud?
//!
//! The command people run first, on a file or a URL they did not produce. It
//! answers in the neutral vocabulary, so the output shape is the same for a
//! Potree directory, a COPC file and an EPT prefix — which is itself the
//! argument the project makes.
//!
//! Two levels. Without `--deep` it reads manifests and headers only: one or two
//! requests, and it works against a remote deployment without pulling a
//! gigabyte. With `--deep` it walks the whole hierarchy, which is the only way
//! to learn what the manifest does not state — how many nodes there are, how
//! deep, and whether the counts add up to the total the file claims.

use std::sync::Arc;
use std::time::Duration;

use clap::Args as ClapArgs;
use serde_json::{json, Map, Value};

use voxelkloud_io::cloud::{CloudInfo, HierarchyStats};
use voxelkloud_io::error::Result;
use voxelkloud_io::format::{self, Cloud};
use voxelkloud_io::source::{FileStore, Store};

use crate::http::{self, HttpStore};
use crate::out::{bytes, count, Output};

#[derive(ClapArgs)]
pub struct Args {
    /// A directory, a manifest, a `.las`/`.laz`/`.copc.laz`, or an `http(s)` URL.
    pub target: String,

    /// Walk the whole hierarchy and report what is in it.
    #[arg(long)]
    pub deep: bool,

    /// Seconds to wait on any one request.
    #[arg(long, default_value_t = 30)]
    pub timeout: u64,
}

pub fn run(args: &Args, out: &Output) -> Result<bool> {
    let cloud = open(&args.target, args.timeout)?;
    let info = cloud.info();

    let hierarchy = if args.deep {
        Some(cloud.hierarchy()?)
    } else {
        None
    };

    if out.json {
        out.json(&document(info, hierarchy.as_ref()));
    } else {
        print(out, info, hierarchy.as_ref());
    }

    // Warnings are anomalies the file survived, not failures. The exit code
    // stays 0: `inspect` reports, it does not judge. `doctor` judges.
    for warning in &info.warnings {
        out.warn(format!("{} — {}", warning.path, warning.message));
    }
    if let Some(stats) = &hierarchy {
        for warning in &stats.warnings {
            out.warn(format!("{} — {}", warning.path, warning.message));
        }
    }
    Ok(true)
}

/// Open a local path or a URL, whichever the target is.
pub fn open(target: &str, timeout: u64) -> Result<Cloud> {
    if http::is_url(target) {
        let (base, name) = http::split_target(target);
        let store: Arc<dyn Store> = Arc::new(HttpStore::new(base, Duration::from_secs(timeout)));
        return format::open(store, &name, target);
    }
    let path = std::path::Path::new(target);
    if path.is_dir() {
        return format::open(Arc::new(FileStore::new(path)), "", target);
    }
    format::open_path(path)
}

fn print(out: &Output, info: &CloudInfo, hierarchy: Option<&HierarchyStats>) {
    out.line("");
    out.heading(&format!(
        "{}  {}",
        info.format.title(),
        info.version.clone().unwrap_or_default()
    ));
    out.field("source", &info.label);
    out.field("points", count(info.point_count));

    // An empty extent is a real answer, not a zero: an E57 whose scans declare
    // no bounds has none until the points are read. Printing the infinities it
    // is stored as would be arithmetic leaking into a report.
    if info.tight_bounds.is_empty() {
        out.field("extent", "not declared by the file");
    } else {
        let size = info.tight_bounds.size();
        out.field(
            "extent",
            format!("{:.2} x {:.2} x {:.2}", size[0], size[1], size[2]),
        );
        out.field(
            "min",
            format!(
                "{:.3}, {:.3}, {:.3}",
                info.tight_bounds.min[0], info.tight_bounds.min[1], info.tight_bounds.min[2]
            ),
        );
        out.field(
            "max",
            format!(
                "{:.3}, {:.3}, {:.3}",
                info.tight_bounds.max[0], info.tight_bounds.max[1], info.tight_bounds.max[2]
            ),
        );
    }
    // The cube is the indexing volume, and on a real survey it is far larger
    // than the data in at least one axis — autzen's is 22x the true Z extent.
    // The honest number is the WORST axis, not the longest edge over the
    // longest edge, which is 1.0 by construction and says nothing.
    let cube = info.bounds.longest_edge();
    if cube > 0.0 && !info.tight_bounds.is_empty() {
        let extent = info.tight_bounds.size();
        let axis = (0..3)
            .filter(|i| extent[*i] > 0.0)
            .max_by(|a, b| {
                (cube / extent[*a])
                    .partial_cmp(&(cube / extent[*b]))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        match axis {
            Some(i) if cube / extent[i] > 1.05 => out.field(
                "index cube",
                format!(
                    "{cube:.2} per side, {:.1}x the data in {}",
                    cube / extent[i],
                    ["X", "Y", "Z"][i]
                ),
            ),
            _ => out.field("index cube", format!("{cube:.2} per side")),
        }
    }
    if let Some(spacing) = info.spacing {
        out.field("root spacing", format!("{spacing:.4}"));
    }
    if let Some(encoding) = &info.encoding {
        out.field("encoding", encoding);
    }
    out.field(
        "crs",
        info.crs
            .as_ref()
            .map(|crs| crs.label())
            .unwrap_or_else(|| "none declared".to_string()),
    );

    out.line("");
    out.heading(&format!(
        "attributes  {} fields, {} bytes per point",
        info.attributes.len(),
        info.bytes_per_point()
    ));
    for attribute in &info.attributes {
        let role = match attribute.role() {
            Some(voxelkloud_io::attribute::AttributeRole::Position) => "  (position)",
            Some(voxelkloud_io::attribute::AttributeRole::Color) => "  (colour)",
            None => "",
        };
        out.line(format!(
            "  {:<22} {}{}{}",
            attribute.name,
            attribute.kind.name(),
            if attribute.num_elements > 1 {
                format!(" x{}", attribute.num_elements)
            } else {
                String::new()
            },
            role
        ));
    }

    let Some(stats) = hierarchy else {
        out.line("");
        if info.format.is_indexed() {
            out.note("Run with --deep to walk the hierarchy.");
        } else {
            out.note(
                "No index: this file has to be read whole. `voxelkloud convert` gives it one.",
            );
        }
        return;
    };

    out.line("");
    out.heading(&format!("hierarchy  {} nodes, depth {}", count(stats.nodes), stats.depth));
    if stats.hierarchy_requests > 0 {
        out.field(
            "index size",
            format!(
                "{} in {} read{}",
                bytes(stats.hierarchy_bytes),
                stats.hierarchy_requests,
                if stats.hierarchy_requests == 1 { "" } else { "s" }
            ),
        );
    }
    if stats.data_bytes > 0 {
        out.field("point data", bytes(stats.data_bytes));
    }

    let walked = stats.total_points();
    if walked != info.point_count {
        // Not an error for every format — a Potree v2 octree stores a sample of
        // its parents' points at each level, so the sum over nodes is the total
        // and any mismatch is real. EPT and COPC are the same. Saying the two
        // numbers is more useful than deciding which is wrong.
        out.field(
            "counted",
            format!(
                "{} points across the nodes, against {} in the manifest",
                count(walked),
                count(info.point_count)
            ),
        );
    }

    out.line("");
    out.line(format!(
        "  {:<7} {:>10} {:>16}",
        out.dim("level"),
        out.dim("nodes"),
        out.dim("points")
    ));
    for (level, nodes) in stats.nodes_by_level.iter().enumerate() {
        if *nodes == 0 {
            continue;
        }
        out.line(format!(
            "  {:<7} {:>10} {:>16}",
            level,
            count(*nodes),
            count(stats.points_by_level[level])
        ));
    }
}

fn document(info: &CloudInfo, hierarchy: Option<&HierarchyStats>) -> Value {
    let mut root = Map::new();
    root.insert("format".into(), json!(info.format.name()));
    root.insert("source".into(), json!(info.label));
    root.insert("version".into(), json!(info.version));
    root.insert("points".into(), json!(info.point_count));
    root.insert(
        "bounds".into(),
        json!({ "min": info.bounds.min, "max": info.bounds.max }),
    );
    root.insert(
        "tightBounds".into(),
        json!({ "min": info.tight_bounds.min, "max": info.tight_bounds.max }),
    );
    root.insert("scale".into(), json!(info.scale));
    root.insert("offset".into(), json!(info.offset));
    root.insert("spacing".into(), json!(info.spacing));
    root.insert("levels".into(), json!(info.levels));
    root.insert("encoding".into(), json!(info.encoding));
    root.insert("bytesPerPoint".into(), json!(info.bytes_per_point()));
    root.insert(
        "crs".into(),
        match &info.crs {
            Some(crs) => json!({
                "format": crs.format.name(),
                "epsg": crs.epsg,
                "verticalEpsg": crs.vertical_epsg,
                "name": crs.name,
                "raw": crs.raw,
            }),
            None => Value::Null,
        },
    );
    root.insert(
        "attributes".into(),
        Value::Array(
            info.attributes
                .iter()
                .map(|a| {
                    json!({
                        "name": a.name,
                        "type": a.kind.name(),
                        "numElements": a.num_elements,
                        "byteOffset": a.byte_offset,
                        "byteSize": a.byte_size(),
                        "role": a.role().map(|r| match r {
                            voxelkloud_io::attribute::AttributeRole::Position => "position",
                            voxelkloud_io::attribute::AttributeRole::Color => "color",
                        }),
                        "min": a.min,
                        "max": a.max,
                    })
                })
                .collect(),
        ),
    );

    let mut warnings: Vec<Value> = info
        .warnings
        .iter()
        .map(|w| json!({ "code": w.code, "path": w.path, "message": w.message }))
        .collect();

    if let Some(stats) = hierarchy {
        root.insert(
            "hierarchy".into(),
            json!({
                "nodes": stats.nodes,
                "depth": stats.depth,
                "pointsByLevel": stats.points_by_level,
                "nodesByLevel": stats.nodes_by_level,
                "dataBytes": stats.data_bytes,
                "indexBytes": stats.hierarchy_bytes,
                "indexReads": stats.hierarchy_requests,
                "countedPoints": stats.total_points(),
            }),
        );
        warnings.extend(
            stats
                .warnings
                .iter()
                .map(|w| json!({ "code": w.code, "path": w.path, "message": w.message })),
        );
    }
    root.insert("warnings".into(), Value::Array(warnings));
    Value::Object(root)
}
