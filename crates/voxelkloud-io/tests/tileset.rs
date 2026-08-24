//! 3D Tiles, against tilesets this repo did not write.
//!
//! The interesting claim here is not that the JSON parses — it is that this
//! reader and `@voxelkloud/format-3dtiles` agree. They are two independent
//! implementations of one spec, written in two languages, sharing no code, and
//! that duplication was accepted on purpose (DEC-T8: a tileset is JSON and
//! accessor arithmetic, with no codec to share). What makes it safe is that
//! both walk the same files and come out with the same tree.
//!
//! The numbers below are the TypeScript driver's, copied here deliberately: if
//! one side drifts, this fails.
//!
//! Skipped rather than failed when the fixtures are absent, the same rule the
//! other suites follow — they live in the gitignored `demo/data`.

use std::path::{Path, PathBuf};

use voxelkloud_io::cloud::FormatId;
use voxelkloud_io::format::{open_path, Cloud};

fn data(rest: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../demo/data/_tiles")
        .join(rest)
}

fn skip(path: &Path) -> bool {
    if path.exists() {
        return false;
    }
    eprintln!("skipping: {} is not there", path.display());
    true
}

/// py3dtiles over a plain LAS of `lion_takanawa` — the fidelity oracle.
#[test]
fn reads_a_tileset_py3dtiles_wrote() {
    let dir = data("lion-las");
    if skip(&dir) {
        return;
    }
    let cloud = open_path(&dir).expect("open the tileset");
    let info = cloud.info();
    assert_eq!(info.format, FormatId::Tiles3D);
    assert_eq!(info.version.as_deref(), Some("1.0"));

    // The root bounding volume, read through the box-and-transform arithmetic,
    // has to land on the SOURCE cloud's extent — which laspy reports as
    // -4.99..-0.79, 1.04..6.72, -3.45..1.12. The two came through completely
    // different code, so agreement is evidence.
    assert!((info.bounds.min[0] - -4.99).abs() < 0.01, "{:?}", info.bounds);
    assert!((info.bounds.max[0] - -0.79).abs() < 0.01, "{:?}", info.bounds);
    assert!((info.bounds.min[1] - 1.04).abs() < 0.01, "{:?}", info.bounds);
    assert!((info.bounds.max[2] - 1.12).abs() < 0.01, "{:?}", info.bounds);

    // A tileset declares no point counts, and this says so rather than leaving
    // a zero that reads like a fact.
    assert_eq!(info.point_count, 0);
    assert!(info
        .warnings
        .iter()
        .any(|w| w.code == "point-count-unknown"));

    let Cloud::Tileset(t) = &cloud else {
        panic!("not a tileset")
    };
    assert!(!t.georeferenced, "py3dtiles writes a local tileset");
    assert_eq!(t.content_kinds, vec!["pnts".to_string()]);
    assert!(t.implicit.is_none());
}

#[test]
fn walks_an_explicit_tree_to_the_same_shape_the_driver_sees() {
    let dir = data("lion-las");
    if skip(&dir) {
        return;
    }
    let stats = open_path(&dir).unwrap().hierarchy().unwrap();
    // The TypeScript driver's numbers, to the tile.
    assert_eq!(stats.nodes, 67);
    assert_eq!(stats.depth, 4);
    assert_eq!(stats.hierarchy_requests, 1, "one document, one read");
}

#[test]
fn walks_an_implicit_quadtree() {
    let dir = data("cesium-samples/1.1/SparseImplicitQuadtree");
    if skip(&dir) {
        return;
    }
    let cloud = open_path(&dir).unwrap();
    let Cloud::Tileset(t) = &cloud else {
        panic!("not a tileset")
    };
    let implicit = t.implicit.as_ref().expect("an implicit rule");
    assert_eq!(implicit.scheme, "QUADTREE");
    assert_eq!(implicit.subtree_levels, 3);
    assert_eq!(implicit.available_levels, 6);

    let stats = cloud.hierarchy().unwrap();
    // 63 tiles, and the driver counts the same 63. The level table is the part
    // worth checking: a reader that forgets a subtree's OWN root leaves a hole
    // at every subtree boundary — 55 tiles and nothing at level 3, which is how
    // that bug was found.
    assert_eq!(stats.nodes, 63);
    assert_eq!(stats.depth, 5);
    assert_eq!(stats.nodes_by_level, vec![1, 2, 4, 8, 16, 32]);
    // The document, plus the root subtree, plus one per available child.
    assert_eq!(stats.hierarchy_requests, 10);
}

#[test]
fn walks_an_implicit_octree() {
    let dir = data("cesium-samples/1.1/SparseImplicitOctree");
    if skip(&dir) {
        return;
    }
    let cloud = open_path(&dir).unwrap();
    let Cloud::Tileset(t) = &cloud else {
        panic!("not a tileset")
    };
    assert_eq!(t.implicit.as_ref().unwrap().scheme, "OCTREE");
    let stats = cloud.hierarchy().unwrap();
    assert!(stats.nodes > 1, "the subtrees turned some tiles on");
    assert!(stats.depth >= 4);
    // No hole: every level between the root and the deepest has tiles.
    for (level, count) in stats.nodes_by_level.iter().enumerate() {
        assert!(*count > 0, "level {level} is empty");
    }
}

#[test]
fn follows_an_external_tileset_and_resolves_against_the_right_document() {
    // The bug this format is famous for: a nested tileset changes the base, and
    // a URI resolved against the top-level document lands nowhere.
    let dir = data("cesium-samples/1.0/TilesetWithRequestVolume");
    if skip(&dir) {
        return;
    }
    let stats = open_path(&dir).unwrap().hierarchy().unwrap();
    assert!(stats.hierarchy_requests >= 2, "the external one was read too");
    assert!(stats.nodes >= 6);
    assert!(
        !stats
            .warnings
            .iter()
            .any(|w| w.code == "external-tileset-missing"),
        "every named document was found: {:?}",
        stats.warnings
    );
}

#[test]
fn a_georeferenced_tileset_declares_ecef() {
    // And it does so through a LOCAL box under an ECEF root transform, not
    // through a region — which is the common shape and the one a reader that
    // only looks for `region` calls local.
    let dir = data("cesium-samples/1.0/TilesetWithDiscreteLOD");
    if skip(&dir) {
        return;
    }
    let cloud = open_path(&dir).unwrap();
    let Cloud::Tileset(t) = &cloud else {
        panic!("not a tileset")
    };
    assert!(t.georeferenced);
    assert_eq!(cloud.info().crs.as_ref().and_then(|c| c.epsg), Some(4978));
}

#[test]
fn refuses_a_document_that_is_not_a_tileset() {
    // A glTF also has an `asset`. Claiming it would take the load away from
    // whoever should get it.
    let dir = data("cesium-samples/glTF/EXT_structural_metadata/PropertyAttributesPointCloud");
    if skip(&dir) {
        return;
    }
    // Asked of the SNIFF directly rather than of `open_path`, which would find
    // the sibling `tileset.json` and be right to.
    let store: std::sync::Arc<dyn voxelkloud_io::source::Store> =
        std::sync::Arc::new(voxelkloud_io::source::FileStore::new(&dir));
    let result = voxelkloud_io::format::tileset::open_manifest(
        store,
        "PropertyAttributesPointCloudTree.gltf",
        "gltf",
    );
    assert!(result.is_err(), "a glTF is not a tileset");
}
