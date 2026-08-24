//! `optimize`, against a cloud somebody else's converter wrote.
//!
//! The claim is narrow and checkable: the tree is untouched and only the bytes
//! inside the nodes change. So the tests are about identity — the same nodes,
//! the same points, and, for the encodings, the same bytes back.

use std::path::{Path, PathBuf};

use voxelkloud_io::convert::{convert, ConvertOptions, OutputFormat};
use voxelkloud_io::format::{open_path, Cloud};
use voxelkloud_io::optimize::{optimize, OptimizeOptions};
use voxelkloud_io::write::potree::PotreeEncoding;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the crate sits two levels under the repo root")
}

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

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir()
            .join(format!("voxelkloud-optimize-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("scratch");
        Self(dir)
    }
    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Every node of a Potree cloud, as (key, points), sorted.
fn nodes(path: &Path) -> Vec<(String, u64)> {
    let cloud = open_path(path).expect("opens");
    let Cloud::Potree(potree) = &cloud else {
        panic!("expected a Potree cloud");
    };
    let mut out = Vec::new();
    potree
        .for_each_node(&mut |node| out.push((node.key.potree_name(), u64::from(node.point_count))))
        .expect("walks");
    out.sort();
    out
}

#[test]
fn the_two_encodings_are_exact_inverses() {
    // DEFAULT to BROTLI and back has to return the same bytes. Morton coding
    // is a bit permutation and the planar transform is a transpose — both
    // lossless — so anything less than byte equality is a bug, and this is the
    // cheapest possible way to find one.
    let source = need!("demo/potree/pointclouds/lion_takanawa.copc.laz");
    let scratch = Scratch::new("inverse");

    let start = scratch.join("start");
    let options = ConvertOptions::new(
        vec![source],
        start.clone(),
        OutputFormat::PotreeV2(PotreeEncoding::Default),
    );
    convert(&options, &mut |_, _| {}).expect("converts");

    let compressed = scratch.join("brotli");
    let mut opts = OptimizeOptions::new(start.clone(), compressed.clone());
    opts.encoding = Some(PotreeEncoding::Brotli);
    let to_brotli = optimize(&opts, &mut |_, _| {}).expect("re-encodes");

    let back = scratch.join("back");
    let mut opts = OptimizeOptions::new(compressed.clone(), back.clone());
    opts.encoding = Some(PotreeEncoding::Default);
    optimize(&opts, &mut |_, _| {}).expect("re-encodes");

    let before = std::fs::read(start.join("octree.bin")).expect("reads");
    let after = std::fs::read(back.join("octree.bin")).expect("reads");
    assert_eq!(before.len(), after.len(), "the round trip changed the size");
    assert!(before == after, "the round trip changed the bytes");

    // And the compression was worth doing, which is the reason the feature
    // exists at all.
    assert!(
        to_brotli.bytes_after * 2 < to_brotli.bytes_before,
        "BROTLI came out at {} against {}",
        to_brotli.bytes_after,
        to_brotli.bytes_before
    );
}

#[test]
fn the_tree_is_untouched() {
    let source = need!("demo/data/brotli");
    let scratch = Scratch::new("tree");
    let out = scratch.join("out");

    let mut opts = OptimizeOptions::new(source.clone(), out.clone());
    opts.encoding = Some(PotreeEncoding::Default);
    let report = optimize(&opts, &mut |_, _| {}).expect("re-encodes");

    // Same nodes, same keys, same counts — read back through the driver, not
    // from the report, so a hierarchy that was written wrong is caught here
    // rather than believed.
    assert_eq!(nodes(&source), nodes(&out));
    assert_eq!(report.points, 341_989);

    let after = open_path(&out).expect("opens");
    assert_eq!(after.info().point_count, 341_989);
    assert_eq!(
        after.hierarchy().expect("walks").total_points(),
        341_989,
        "the rewritten hierarchy lost points"
    );
}

#[test]
fn dropping_an_attribute_removes_exactly_that_attribute() {
    let source = need!("demo/data/brotli");
    let scratch = Scratch::new("drop");
    let out = scratch.join("out");

    let before = open_path(&source).expect("opens");
    let gps = before
        .info()
        .attribute("gps-time")
        .expect("the fixture carries gps-time")
        .byte_size();

    let mut opts = OptimizeOptions::new(source.clone(), out.clone());
    opts.encoding = Some(PotreeEncoding::Default);
    opts.drop = vec!["gps-time".to_string()];
    let report = optimize(&opts, &mut |_, _| {}).expect("re-encodes");

    assert_eq!(report.dropped, vec!["gps-time".to_string()]);
    assert_eq!(report.record_before - report.record_after, gps);

    let after = open_path(&out).expect("opens");
    assert!(after.info().attribute("gps-time").is_none());
    assert_eq!(
        after.info().attributes.len(),
        before.info().attributes.len() - 1,
        "exactly one attribute left"
    );
    // Every other attribute keeps its name, type and order.
    let kept: Vec<&str> = before
        .info()
        .attributes
        .iter()
        .map(|a| a.name.as_str())
        .filter(|name| *name != "gps-time")
        .collect();
    let got: Vec<&str> = after.info().attributes.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(kept, got);
    assert_eq!(nodes(&source), nodes(&out), "dropping a field moved a node");
}

#[test]
fn position_survives_a_request_to_drop_it() {
    // Refusing beats obeying: a cloud with no position attribute is not a
    // cloud, and the file would parse and render nothing.
    let source = need!("demo/data/brotli");
    let scratch = Scratch::new("position");
    let out = scratch.join("out");

    let mut opts = OptimizeOptions::new(source, out.clone());
    opts.drop = vec!["position".to_string()];
    let report = optimize(&opts, &mut |_, _| {}).expect("re-encodes");

    assert!(report.dropped.is_empty());
    assert!(report.warnings.iter().any(|w| w.code == "position-kept"));
    assert!(open_path(&out).expect("opens").info().attribute("position").is_some());
}


/// Every proxy record names the count of the node it stands for.
///
/// A `hierarchy.bin` deeper than one chunk carries proxy records — type 2,
/// pointing at another chunk — and a reader meets one before it has fetched
/// what is behind it. Writing zero there produced a file whose totals were
/// short by every node at a chunk boundary: 1.8M of autzen's 10.6M points,
/// invisible until a reader that had not expanded the whole tree added them up.
///
/// The check is structural and needs no oracle: for each proxy, follow its
/// offset and read record 0 of the chunk it names, which is the same node.
#[test]
fn proxy_records_carry_the_point_count_of_the_node_they_stand_for() {
    let source = need!("demo/data/real");
    let scratch = Scratch::new("proxies");
    let out = scratch.join("out");

    // autzen is seven levels deep, which is what makes it produce proxies at
    // all. A shallow cloud writes one chunk and never exercises this.
    let mut opts = OptimizeOptions::new(source, out.clone());
    opts.encoding = Some(PotreeEncoding::Default);
    optimize(&opts, &mut |_, _| {}).expect("re-encodes");

    let bytes = std::fs::read(out.join("hierarchy.bin")).expect("reads");
    const RECORD: usize = 22;
    let count_at = |at: usize| u32::from_le_bytes(bytes[at + 2..at + 6].try_into().unwrap());
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());

    let mut proxies = 0;
    for at in (0..bytes.len() - RECORD + 1).step_by(RECORD) {
        if bytes[at] != 2 {
            continue;
        }
        proxies += 1;
        let target = u64_at(at + 6) as usize;
        assert!(target + RECORD <= bytes.len(), "a proxy points past the file");
        assert_eq!(
            count_at(at),
            count_at(target),
            "the proxy at byte {at} says {} points and its chunk says {}",
            count_at(at),
            count_at(target)
        );
        assert!(count_at(at) > 0, "a proxy for an empty node is not written");
    }
    assert!(proxies > 0, "autzen is deep enough to have proxies");
}
