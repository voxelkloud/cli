//! The readers, against files this repo did not write.
//!
//! The unit tests prove the arithmetic; these prove the formats. Every file
//! here was produced by somebody else's tool — PotreeConverter, untwine,
//! Entwine — which is the only way to find out that a reader agrees with the
//! world rather than with itself.
//!
//! The datasets are gitignored (see `demo/data` in the README), so the whole
//! file skips when they are absent rather than failing a clone that never
//! downloaded 5 GB of LiDAR.

use std::path::{Path, PathBuf};

use voxelkloud_io::cloud::FormatId;
use voxelkloud_io::format::{open_path, Cloud};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the crate sits two levels under the repo root")
}

/// `None` when the dataset is not on this machine.
fn dataset(relative: &str) -> Option<PathBuf> {
    let path = repo().join(relative);
    path.exists().then_some(path)
}

macro_rules! need {
    ($relative:expr) => {
        match dataset($relative) {
            Some(path) => path,
            None => {
                eprintln!("skipping: {} is not present", $relative);
                return;
            }
        }
    };
}

#[test]
fn autzen_potree_hierarchy_sums_to_the_manifest() {
    let path = need!("demo/data/real");
    let cloud = open_path(&path).expect("autzen opens");
    let info = cloud.info();
    assert_eq!(info.format, FormatId::PotreeV2);
    assert_eq!(info.point_count, 10_653_336);

    let stats = cloud.hierarchy().expect("the hierarchy walks");
    // Every level of a Potree octree stores its own points, so the walk must
    // account for the manifest's total exactly. A parser that dropped proxy
    // chunks would still produce a plausible tree — 257 nodes instead of 4377 —
    // and only this equality catches it.
    assert_eq!(stats.total_points(), info.point_count);
    assert_eq!(stats.nodes, 4377);
    assert_eq!(stats.depth, 7);
    assert!(stats.warnings.is_empty(), "{:?}", stats.warnings);
}

#[test]
fn the_same_cloud_reads_the_same_through_three_drivers() {
    // lion_takanawa, converted four ways by three different tools. The COPC
    // came out of untwine, the EPT pair out of Entwine, and they agree on a
    // point count and a bounding box or one of them is wrong.
    let copc = need!("demo/data/copc/lion_takanawa.copc.laz");
    let ept_bin = need!("demo/data/ept-bin");
    let ept_laz = need!("demo/data/ept-laz");

    let copc = open_path(&copc).expect("the COPC opens");
    assert_eq!(copc.info().format, FormatId::Copc);
    assert_eq!(copc.info().point_count, 341_989);

    for path in [ept_bin, ept_laz] {
        let ept = open_path(&path).expect("the EPT opens");
        assert_eq!(ept.info().format, FormatId::Ept);
        assert_eq!(
            ept.info().point_count,
            copc.info().point_count,
            "{} disagrees with the COPC on the point count",
            path.display()
        );

        // The tight extents come from different fields and are not the same
        // claim. A LAS header states the measured extremes; EPT's
        // `boundsConforming` is the union of what its sources declared, and
        // Entwine rounds it outward — on this cloud to whole units, which on a
        // 6-unit-wide scan is a third of an axis. So the invariant is
        // CONTAINMENT, not closeness: whatever EPT says must be at least the
        // box the points actually occupy.
        let ept_bounds = ept.info().tight_bounds;
        let copc_bounds = copc.info().tight_bounds;
        for axis in 0..3 {
            assert!(
                ept_bounds.min[axis] <= copc_bounds.min[axis]
                    && ept_bounds.max[axis] >= copc_bounds.max[axis],
                "{}: axis {axis} does not contain the points, {:?} against {:?}",
                path.display(),
                ept_bounds,
                copc_bounds
            );
        }
    }
}

#[test]
fn copc_layers_account_for_every_point_in_the_header() {
    let path = need!("demo/data/copc/lion_takanawa.copc.laz");
    let cloud = open_path(&path).expect("the COPC opens");
    let stats = cloud.hierarchy().expect("the hierarchy walks");
    assert_eq!(stats.total_points(), cloud.info().point_count);
    assert!(stats.nodes > 0);
}

#[test]
fn the_two_ept_builds_hold_the_same_nodes() {
    // Same cloud, two encodings, one written after the other. The node keys and
    // their counts are a property of the indexing, not of the payload, so they
    // must match exactly — this is the check that caught the schema/record
    // disagreement in the laszip build.
    let bin = need!("demo/data/ept-bin");
    let laz = need!("demo/data/ept-laz");

    let bin = open_path(&bin).expect("the binary EPT opens");
    let laz = open_path(&laz).expect("the laszip EPT opens");
    let a = bin.hierarchy().expect("walks");
    let b = laz.hierarchy().expect("walks");

    assert_eq!(a.nodes, b.nodes);
    assert_eq!(a.depth, b.depth);
    assert_eq!(a.points_by_level, b.points_by_level);
}

#[test]
fn a_las_and_its_laz_twin_describe_one_file() {
    // PotreeConverter wrote this node twice, compressed and not. The headers
    // must agree on everything except the compression bit — a reader that got
    // the laszip flag or the legacy point count wrong would differ here.
    let las = need!("demo/potree/pointclouds/lion_takanawa_las/data/r.las");
    let laz = need!("demo/potree/pointclouds/lion_takanawa_laz/data/r.laz");

    let las = open_path(&las).expect("the LAS opens");
    let laz = open_path(&laz).expect("the LAZ opens");

    assert_eq!(las.info().format, FormatId::Las);
    assert_eq!(laz.info().format, FormatId::Las);
    assert_eq!(las.info().point_count, laz.info().point_count);
    assert_eq!(las.info().tight_bounds, laz.info().tight_bounds);
    assert_eq!(las.info().scale, laz.info().scale);
    assert_eq!(
        las.info().attributes.len(),
        laz.info().attributes.len(),
        "the same record, described differently"
    );
    assert_eq!(las.info().encoding.as_deref(), Some("uncompressed"));
    assert_eq!(laz.info().encoding.as_deref(), Some("laszip"));
}

#[test]
fn a_3dep_tile_carries_its_projection_in_geotiff_keys() {
    // LAS 1.2, so the CRS is in the GeoTIFF key directory rather than in WKT —
    // the older of the two paths, and the one a 1.4-only reader misses.
    let path = need!("demo/data/_src/20m/36112C3116.laz");
    let cloud = open_path(&path).expect("the tile opens");
    let crs = cloud.info().crs.as_ref().expect("the tile declares a CRS");
    assert_eq!(crs.epsg, Some(26912), "NAD83 / UTM zone 12N");
}

#[test]
fn a_brotli_potree_cloud_reads_its_manifest() {
    // The BROTLI encoding has no fixed record stride, so `bytes_per_point` must
    // come back as unknown rather than as a number derived from a layout that
    // does not apply.
    let path = need!("demo/data/brotli");
    let cloud = open_path(&path).expect("the cloud opens");
    let Cloud::Potree(potree) = &cloud else {
        panic!("expected a Potree cloud");
    };
    assert_eq!(potree.metadata.encoding, "BROTLI");
    assert!(potree.bytes_per_point.is_none());
    let stats = cloud.hierarchy().expect("the hierarchy walks");
    assert_eq!(stats.total_points(), cloud.info().point_count);
}
