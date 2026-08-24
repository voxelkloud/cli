//! The converter, end to end, against files this repo did not write.
//!
//! The property that matters is conservation: every point that goes in comes
//! out, once, with its fields intact and its position within one quantum of
//! where it was. Everything else the converter does — the tree, the pages, the
//! compression — is machinery in service of that, and a test that only checked
//! the point *count* would pass on a converter that wrote the same point
//! 342,000 times.

use std::path::{Path, PathBuf};

use voxelkloud_io::convert::{convert, ConvertOptions, OutputFormat};
use voxelkloud_io::format::{open_path, Cloud};
use voxelkloud_io::read::las_points::LasPointSource;
use voxelkloud_io::read::PointSource;
use voxelkloud_io::record::{at, dequantize, position, RecordLayout};
use std::io::Cursor;

use voxelkloud_io::bounds::Bounds;
use voxelkloud_io::build::{build_subtree, indexing_cube, BuildOptions, BuiltNode, NodeSink};
use voxelkloud_io::las::point_format::{las_base_size, las_format_has_color, las_format_has_gps_time};
use voxelkloud_io::las::LasHeader;
use voxelkloud_io::octree::OctreeKey;
use voxelkloud_io::record::{output_format, RecordConverter};
use voxelkloud_io::write::copc::CopcWriter;
use voxelkloud_io::write::WriteOptions;


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

/// A scratch directory that cleans up after itself.
///
/// Named after the process as well as the test: two runs of this suite at once
/// — `cargo test` in one terminal while another is building, which is exactly
/// how this was found — would otherwise share a directory and delete each
/// other's output half way through.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir()
            .join(format!("voxelkloud-convert-{name}-{}", std::process::id()));
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

/// One point, reduced to what survives a conversion.
#[derive(PartialEq, PartialOrd, Debug, Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
    z: f64,
    intensity: u16,
    classification: u8,
    rgb: [u16; 3],
}

/// Read every point of a LAS, LAZ or COPC file, in absolute coordinates.
fn read_points(path: &Path) -> Vec<Point> {
    let layout = RecordLayout::new(7, 0, Vec::new(), Vec::new()).expect("format 7");
    let stride = layout.stride();
    // Quantized at a millimetre against a zero origin, so both sides of a
    // comparison land on the same grid whatever the files themselves used.
    let scale = [0.001; 3];
    let offset = [0.0; 3];
    let mut source = LasPointSource::open(path, layout, scale, offset).expect("opens");

    let mut out = Vec::new();
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let got = source.next_batch(1 << 18, &mut buffer).expect("reads");
        if got == 0 {
            break;
        }
        for record in buffer.chunks_exact(stride) {
            let p = position(record);
            out.push(Point {
                x: dequantize(p[0], scale[0], offset[0]),
                y: dequantize(p[1], scale[1], offset[1]),
                z: dequantize(p[2], scale[2], offset[2]),
                intensity: u16::from_le_bytes(
                    record[at::INTENSITY..at::INTENSITY + 2].try_into().unwrap(),
                ),
                classification: record[at::CLASSIFICATION],
                rgb: [
                    u16::from_le_bytes(record[at::RGB..at::RGB + 2].try_into().unwrap()),
                    u16::from_le_bytes(record[at::RGB + 2..at::RGB + 4].try_into().unwrap()),
                    u16::from_le_bytes(record[at::RGB + 4..at::RGB + 6].try_into().unwrap()),
                ],
            });
        }
    }
    out
}

fn sorted(mut points: Vec<Point>) -> Vec<Point> {
    points.sort_by(|a, b| {
        (a.x, a.y, a.z, a.intensity)
            .partial_cmp(&(b.x, b.y, b.z, b.intensity))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    points
}

#[test]
fn a_converted_cloud_holds_exactly_the_points_that_went_in() {
    // PotreeConverter wrote this node; it is LAS 1.2, point format 2, with
    // colour and no GPS time — so the conversion also exercises the legacy
    // field mapping into a 1.4 record.
    let input = need!("demo/potree/pointclouds/lion_takanawa_las/data/r.las");
    let scratch = Scratch::new("roundtrip");
    let output = scratch.join("out.copc.laz");

    let options = ConvertOptions::new(vec![input.clone()], output.clone(), OutputFormat::Copc);
    let report = convert(&options, &mut |_, _| {}).expect("converts");

    let before = sorted(read_points(&input));
    let after = sorted(read_points(&output));

    assert_eq!(report.write.points as usize, before.len());
    assert_eq!(after.len(), before.len(), "the cloud changed size");

    // Every field, point for point. The positions are compared on the
    // millimetre grid both sides were quantized onto: the converter rounds to
    // its own quantum, so equality here is equality within half of one.
    for (i, (a, b)) in before.iter().zip(after.iter()).enumerate() {
        let close = (a.x - b.x).abs() <= 0.0011
            && (a.y - b.y).abs() <= 0.0011
            && (a.z - b.z).abs() <= 0.0011;
        assert!(close, "point {i}: {a:?} became {b:?}");
        assert_eq!(a.intensity, b.intensity, "point {i} intensity");
        assert_eq!(a.classification, b.classification, "point {i} classification");
        assert_eq!(a.rgb, b.rgb, "point {i} colour");
    }
}

#[test]
fn the_index_accounts_for_every_point_in_all_three_formats() {
    let input = need!("demo/data/copc/lion_takanawa.copc.laz");
    let scratch = Scratch::new("formats");

    for (format, name) in [
        (OutputFormat::Copc, "out.copc.laz"),
        (
            OutputFormat::PotreeV2(voxelkloud_io::write::potree::PotreeEncoding::Default),
            "potree",
        ),
        (
            OutputFormat::PotreeV2(voxelkloud_io::write::potree::PotreeEncoding::Brotli),
            "potree-brotli",
        ),
        (OutputFormat::Ept(voxelkloud_io::write::ept::EptEncoding::Binary), "ept"),
    ] {
        let output = scratch.join(name);
        let options = ConvertOptions::new(vec![input.clone()], output.clone(), format);
        let report = convert(&options, &mut |_, _| {}).expect("converts");
        assert_eq!(report.write.points, 341_989, "{name}: point count");

        // Read it back with the driver for its own format — the same code path
        // the browser uses — and make the hierarchy account for every point.
        let cloud = open_path(&output).expect("the output opens");
        assert_eq!(cloud.info().point_count, 341_989, "{name}: manifest count");
        let stats = cloud.hierarchy().expect("the hierarchy walks");
        assert_eq!(
            stats.total_points(),
            341_989,
            "{name}: the hierarchy lost or duplicated points"
        );
        assert_eq!(stats.nodes, report.write.nodes, "{name}: node count");
        assert!(stats.warnings.is_empty(), "{name}: {:?}", stats.warnings);
    }
}

#[test]
fn the_tree_is_shaped_for_streaming_rather_than_for_the_grid() {
    // The leaf rule, measured. Without it the recursion runs until every cell
    // of every grid holds one point: for this cloud that was 832 nodes of 3 KB,
    // which is 832 requests to open a 2.5 MB file.
    let input = need!("demo/data/copc/lion_takanawa.copc.laz");
    let scratch = Scratch::new("shape");
    let output = scratch.join("out.copc.laz");

    let options = ConvertOptions::new(vec![input], output.clone(), OutputFormat::Copc);
    let report = convert(&options, &mut |_, _| {}).expect("converts");

    assert!(
        report.write.nodes < 64,
        "342k points became {} nodes; the leaf rule is not doing its job",
        report.write.nodes
    );
    let cloud = open_path(&output).expect("opens");
    let stats = cloud.hierarchy().expect("walks");
    // One read of the hierarchy. A page per subtree would be correct and slow.
    assert_eq!(stats.hierarchy_requests, 1);

    let Cloud::Copc(copc) = &cloud else {
        panic!("expected COPC");
    };
    // The spacing the file states has to be the one the tree was built with,
    // or the renderer schedules against a number nothing produced.
    let expected = copc.info.bounds.longest_edge() / f64::from(options.span);
    assert!(
        (copc.copc.spacing - expected).abs() < 1e-9,
        "stated spacing {} against {expected}",
        copc.copc.spacing
    );
}

#[test]
fn several_inputs_become_one_cloud() {
    // The case the repo's own `fetch-large.sh` exists to work around: a survey
    // ships as tiles and a viewer wants a cloud.
    let dir = need!("demo/potree/pointclouds/lion_takanawa_las/data");
    let mut inputs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("reads")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "las").unwrap_or(false))
        .collect();
    inputs.sort();
    inputs.truncate(8);
    assert!(inputs.len() > 1, "need several files to merge");

    let expected: u64 = inputs
        .iter()
        .map(|p| open_path(p).expect("opens").info().point_count)
        .sum();

    let scratch = Scratch::new("merge");
    let output = scratch.join("merged.copc.laz");
    let options = ConvertOptions::new(inputs, output.clone(), OutputFormat::Copc);
    let report = convert(&options, &mut |_, _| {}).expect("converts");

    assert_eq!(report.write.points, expected);
    let cloud = open_path(&output).expect("opens");
    assert_eq!(cloud.hierarchy().expect("walks").total_points(), expected);
}

#[test]
fn a_spilled_build_writes_a_hierarchy_that_reaches_every_point() {
    // The points were never the problem. A COPC's chunks can all be present,
    // correct and readable while its HIERARCHY describes a fraction of them —
    // and then a viewer shows that fraction and nothing looks broken enough to
    // investigate. On a 241M-point cloud this wrote 181 entries out of 18,545
    // and reached 0.98% of the points.
    //
    // Two things had to be wrong at once, which is why the existing spill test
    // did not catch it. The page tree was rooted at the FIRST node written,
    // true in core where a parent precedes its children and false out of core,
    // where each cell's subtree is built before the levels above it. And a
    // sampled coarse pass can miss a cell that turns out to hold points, so
    // the chain down to that cell's subtree has a hole in it.
    //
    // `--leaf 500 --span 16` is what makes the tree deeper than the level the
    // spill cuts at. With the defaults, 342k points build a depth-2 tree that
    // fits entirely in the coarse pass and the bug cannot appear.
    let input = need!("demo/data/copc/lion_takanawa.copc.laz");
    let expected = open_path(&input).expect("opens").info().point_count;
    let scratch = Scratch::new("spill-hierarchy");

    let output = scratch.join("deep.copc.laz");
    let mut options =
        ConvertOptions::new(vec![input.clone()], output.clone(), OutputFormat::Copc);
    options.leaf_points = 500;
    options.span = 16;
    options.memory_budget = 1 << 20;
    options.scratch = Some(scratch.join("spill"));
    let report = convert(&options, &mut |_, _| {}).expect("converts");
    assert!(report.spilled, "the budget should have forced a spill");
    assert!(report.write.depth > 2, "the tree has to outgrow the coarse pass");

    let stats = open_path(&output)
        .expect("opens")
        .hierarchy()
        .expect("walks");
    assert_eq!(
        stats.total_points(),
        expected,
        "the hierarchy reaches {} of {expected} points",
        stats.total_points(),
    );
    // Every node the writer recorded is reachable from the root, plus however
    // many empty entries it had to invent to bridge the gaps.
    assert!(stats.nodes >= report.write.nodes);
    assert_eq!(stats.nodes_by_level[0], 1, "there must be a root node");
}

#[test]
fn spilling_to_disk_gives_the_same_cloud_as_holding_it_in_memory() {
    // The out-of-core path is a different algorithm — distribute, build each
    // cell, then fill in the levels above from the survivors — and the only
    // way to know it agrees with the simple one is to run both.
    let input = need!("demo/data/copc/lion_takanawa.copc.laz");
    let scratch = Scratch::new("spill");

    let in_memory = scratch.join("memory.copc.laz");
    let mut options = ConvertOptions::new(vec![input.clone()], in_memory.clone(), OutputFormat::Copc);
    let a = convert(&options, &mut |_, _| {}).expect("converts");
    assert!(!a.spilled);

    let on_disk = scratch.join("disk.copc.laz");
    options.output = on_disk.clone();
    // Small enough to force several cells out of a 342k-point cloud.
    options.memory_budget = 1 << 20;
    options.scratch = Some(scratch.join("spill"));
    let b = convert(&options, &mut |_, _| {}).expect("converts");
    assert!(b.spilled, "the budget should have forced a spill");

    assert_eq!(a.write.points, b.write.points, "the spilled build lost points");

    let mut before = sorted(read_points(&in_memory));
    let mut after = sorted(read_points(&on_disk));
    assert_eq!(before.len(), after.len());
    // The two builds may put a point at a different LEVEL — the sampling sees
    // the points in a different order — but the set of points is the same one.
    before.dedup_by(|a, b| a == b);
    after.dedup_by(|a, b| a == b);
    assert_eq!(before.len(), after.len(), "the two builds hold different points");
    for (a, b) in before.iter().zip(after.iter()) {
        assert_eq!(a, b);
    }
}


/// A minimal LAS file with a projection declared the older way.
///
/// Written by hand rather than taken from `demo/data` because the point is the
/// GeoTIFF key directory, and the files in this repo that carry one are 134 MB
/// each. Three points is enough: what is being tested is what happens to the
/// VLRs, not to the points.
fn write_las_with_geotiff(path: &Path, epsg: u16) {
    use voxelkloud_io::write::las_write::{write_vlr, OutHeader, OutVlr, HEADER_SIZE};

    // Four `u16` of directory header — version, revision, minor, key count —
    // then one four-`u16` key: ProjectedCSType, held inline, one value.
    let mut keys: Vec<u8> = Vec::new();
    for value in [1u16, 1, 0, 1, 3072, 0, 1, epsg] {
        keys.extend_from_slice(&value.to_le_bytes());
    }
    let vlr = OutVlr::new("LASF_Projection", 34735, "GeoTIFF key directory", keys);
    let point_size = 20u16; // point format 0

    let header = OutHeader {
        point_format: 0,
        point_size,
        compressed: false,
        point_count: 3,
        scale: [0.01; 3],
        offset: [0.0; 3],
        min: [0.0, 0.0, 0.0],
        max: [2.0, 2.0, 2.0],
        offset_to_point_data: (HEADER_SIZE + vlr.size()) as u32,
        vlr_count: 1,
        evlr_offset: 0,
        evlr_count: 0,
        generator: "test".to_string(),
        wkt: false,
        creation: (1, 2026),
        points_by_return: [0; 15],
    };

    let mut bytes = header.to_bytes();
    write_vlr(&mut bytes, &vlr).expect("in-memory write");
    for i in 0..3i32 {
        let mut record = vec![0u8; point_size as usize];
        for (axis, value) in [i * 100, i * 100, i * 100].iter().enumerate() {
            record[axis * 4..axis * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&record);
    }
    std::fs::write(path, bytes).expect("writes");
}

#[test]
fn a_projection_declared_in_geotiff_keys_survives_the_conversion() {
    // The failure this catches is the one this project criticises other
    // converters for: a cloud that went in with a resolvable CRS and came out
    // declaring nothing a machine can read. The records cannot be re-derived —
    // turning a code into WKT needs the EPSG table — so they have to be copied.
    let scratch = Scratch::new("crs");
    let input = scratch.join("in.las");
    write_las_with_geotiff(&input, 26912);

    let before = open_path(&input).expect("opens");
    assert_eq!(
        before.info().crs.as_ref().and_then(|c| c.epsg),
        Some(26912),
        "the hand-written input declares the CRS"
    );

    let output = scratch.join("out.copc.laz");
    let options = ConvertOptions::new(vec![input], output.clone(), OutputFormat::Copc);
    convert(&options, &mut |_, _| {}).expect("converts");

    let after = open_path(&output).expect("the output opens");
    assert_eq!(
        after.info().crs.as_ref().and_then(|c| c.epsg),
        Some(26912),
        "the converted cloud kept its projection"
    );
    assert_eq!(after.info().point_count, 3);
}

/// The COPC writer, into memory rather than into a file.
///
/// The path a browser takes: there is no filesystem in a tab, so the writer is
/// generic over its sink and the wasm build hands it a `Cursor<Vec<u8>>`. This
/// is that path, run natively — where a panic has a backtrace and a wasm trap
/// does not.
#[test]
fn a_copc_can_be_written_into_memory() {
    let path = need!("demo/potree/pointclouds/lion_takanawa_las/data/r.las");
    let bytes = std::fs::read(&path).expect("reads");
    let header = LasHeader::read(&bytes).expect("ok");
    let stride = header.point_size as usize;
    let start = header.offset_to_point_data as usize;
    let records = bytes[start..start + header.point_count as usize * stride].to_vec();

    let extent = Bounds { min: header.min, max: header.max };
    let cube = indexing_cube(&extent);
    let options = BuildOptions::new(cube, header.scale, header.offset);

    struct Collect(Vec<BuiltNode>);
    impl NodeSink for Collect {
        fn node(&mut self, node: BuiltNode) -> voxelkloud_io::error::Result<()> { self.0.push(node); Ok(()) }
    }
    let mut sink = Collect(Vec::new());
    build_subtree(records, OctreeKey::ROOT, stride, &options, &mut sink).expect("ok");

    let base = las_base_size(header.point_format).expect("ok");
    let extra = stride - base;
    let out_format = output_format(las_format_has_color(header.point_format), false);
    let layout = RecordLayout::new(out_format, extra, Vec::new(), Vec::new()).expect("ok");
    let converter = RecordConverter::from_parts(header.point_format, stride, header.scale, header.offset, layout.clone(), header.scale, header.offset).expect("ok");

    let write_options = WriteOptions {
        layout,
        cube,
        extent,
        scale: header.scale,
        offset: header.offset,
        spacing: cube.longest_edge() / 128.0,
        span: 128,
        has_gps_time: las_format_has_gps_time(header.point_format),
        legacy_fields: header.point_format <= 5,
        crs: None,
        projection_vlrs: Vec::new(),
        generator: "test".into(),
        creation: (1, 2026),
    };
    let mut writer = CopcWriter::new(Cursor::new(Vec::new()), write_options, "memory".into()).expect("ok");
    let mut converted = Vec::new();
    for node in sink.0 {
        converted.clear();
        converter.convert_many(&node.records, &mut converted);
        writer.write_node(&BuiltNode { key: node.key, records: std::mem::take(&mut converted) }).expect("ok");
        converted = Vec::new();
    }
    let (report, sink) = writer.finish().expect("ok");
    let out = sink.into_inner();
    assert_eq!(&out[0..4], b"LASF");
    assert_eq!(report.points, header.point_count);
    assert!(out.len() > 1000, "a file with 5,202 points is not 1 KB");
}

/// Reconstruct every point a 3D Tiles output holds, in absolute coordinates.
///
/// Reads the GLBs directly rather than through the TypeScript driver, so the
/// writer is checked against the SPEC and not against its own reader. Applies
/// the two conventions the writer states: Y-up to Z-up, then the tile
/// transform.
fn read_tileset_points(dir: &Path) -> Vec<(i64, i64, i64)> {
    fn walk(dir: &Path, tile: &serde_json::Value, out: &mut Vec<(i64, i64, i64)>) {
        let m = tile
            .get("transform")
            .and_then(|t| t.as_array())
            .expect("every tile this writer emits carries a transform");
        let c = [
            m[12].as_f64().unwrap(),
            m[13].as_f64().unwrap(),
            m[14].as_f64().unwrap(),
        ];
        if let Some(uri) = tile.get("content").and_then(|c| c.get("uri")).and_then(|u| u.as_str()) {
            let bytes = std::fs::read(dir.join(uri)).expect("the content file exists");
            let json_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
            let json: serde_json::Value =
                serde_json::from_slice(&bytes[20..20 + json_len]).expect("the JSON chunk");
            let binary = &bytes[20 + json_len + 8..];
            let accessor = &json["accessors"][0];
            let view = &json["bufferViews"][accessor["bufferView"].as_u64().unwrap() as usize];
            let offset = view["byteOffset"].as_u64().unwrap_or(0) as usize;
            let count = accessor["count"].as_u64().unwrap() as usize;
            for i in 0..count {
                let at = offset + i * 12;
                let gx = f32::from_le_bytes(binary[at..at + 4].try_into().unwrap()) as f64;
                let gy = f32::from_le_bytes(binary[at + 4..at + 8].try_into().unwrap()) as f64;
                let gz = f32::from_le_bytes(binary[at + 8..at + 12].try_into().unwrap()) as f64;
                // Y-up to Z-up: (x, y, z)_gltf is (x, -z, y) in the tile frame.
                out.push((
                    ((gx + c[0]) / 0.01).round() as i64,
                    ((-gz + c[1]) / 0.01).round() as i64,
                    ((gy + c[2]) / 0.01).round() as i64,
                ));
            }
        }
        for child in tile.get("children").and_then(|c| c.as_array()).unwrap_or(&Vec::new()) {
            walk(dir, child, out);
        }
    }

    let doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("tileset.json")).expect("tileset.json"))
            .expect("valid JSON");
    let mut out = Vec::new();
    walk(dir, &doc["root"], &mut out);
    out
}

#[test]
fn writes_3d_tiles_that_round_trip_to_the_source_points() {
    // THE round trip: convert, then read the written GLBs back through the
    // spec's own arithmetic and land on the source cloud exactly.
    //
    // Counting records would not be enough — a writer that emitted the right
    // NUMBER of wrong points passes that. What is compared is the SET of
    // quantised positions, at the file's own 0.01 m quantum.
    let input = need!("demo/data/_tiles/lion.las");
    let scratch = Scratch::new("3dtiles");
    let out = scratch.join("tileset");

    let options = ConvertOptions::new(vec![input.clone()], out.clone(), OutputFormat::Tiles3D);
    let report = convert(&options, &mut |_, _| {}).expect("convert to 3D Tiles");
    assert_eq!(report.write.points, 341_989);
    assert!(report.write.nodes > 1, "the cloud was actually subdivided");

    let source: std::collections::HashSet<(i64, i64, i64)> = read_points(&input)
        .into_iter()
        .map(|p| {
            (
                (p.x / 0.01).round() as i64,
                (p.y / 0.01).round() as i64,
                (p.z / 0.01).round() as i64,
            )
        })
        .collect();

    let written = read_tileset_points(&out);
    assert_eq!(written.len(), 341_989, "every point was written once");
    let distinct: std::collections::HashSet<_> = written.iter().copied().collect();
    // The source has 341,989 records at 275,855 distinct positions: a scan
    // quantised to 0.01 m has coincident points, and that is normal.
    assert_eq!(distinct.len(), source.len());
    let strays = written.iter().filter(|p| !source.contains(p)).count();
    assert_eq!(strays, 0, "no point moved");
}

#[test]
fn the_tileset_it_writes_reads_back_as_a_tileset() {
    let input = need!("demo/data/_tiles/lion.las");
    let scratch = Scratch::new("3dtiles-reopen");
    let out = scratch.join("tileset");
    let options = ConvertOptions::new(vec![input], out.clone(), OutputFormat::Tiles3D);
    convert(&options, &mut |_, _| {}).expect("convert");

    let cloud = open_path(&out).expect("reopen what we wrote");
    let Cloud::Tileset(t) = &cloud else {
        panic!("not read back as a tileset")
    };
    assert_eq!(t.version, "1.1");
    assert_eq!(t.content_kinds, vec!["gltf".to_string()]);
    let stats = cloud.hierarchy().expect("walk it");
    assert!(stats.nodes > 1);
    // Every leaf carries geometric error 0, which is what the format means by
    // "nothing finer exists" — and what DEC-T2's shift exists to survive.
    let doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("tileset.json")).unwrap()).unwrap();
    assert_eq!(doc["asset"]["version"], "1.1");
    assert_eq!(doc["root"]["refine"], "ADD");
}
