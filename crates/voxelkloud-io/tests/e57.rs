//! E57, against files written for the purpose.
//!
//! Every fixture here is written by the `e57` crate's own writer, which makes
//! them synthetic — and deliberately so. What is being tested is not whether
//! the bitpacking decodes; the dependency's own suite covers that. It is the
//! five decisions this repo makes on top of it, each of which is invisible in a
//! point count and obvious in a viewer: the pose, the spherical conversion, the
//! points that carry no position, intensity not becoming colour, and which
//! scan a point came from.
//!
//! Fixtures are written rather than downloaded so the suite runs on a clone
//! with no network. Reading what a real scanner writes is a different claim,
//! and `real_files.rs` is where it belongs.

use std::path::{Path, PathBuf};

use e57::{
    E57Writer, Quaternion, Record, RecordDataType, RecordValue, Transform, Translation,
};
use voxelkloud_io::cloud::FormatId;
use voxelkloud_io::e57::E57Points;
use voxelkloud_io::convert::{convert, scan, ConvertOptions, OutputFormat};
use voxelkloud_io::format::{open_path, Cloud};
use voxelkloud_io::read::las_points::LasPointSource;
use voxelkloud_io::read::PointSource;
use voxelkloud_io::record::{at, dequantize, position, RecordLayout};

/// A scratch directory that cleans up after itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("voxelkloud-e57-{name}-{}", std::process::id()));
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
#[derive(PartialEq, Debug, Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
    z: f64,
    intensity: u16,
    rgb: [u16; 3],
    source_id: u16,
}

/// Read every point of a converted file back, in absolute coordinates.
///
/// Re-quantised against the file's OWN origin rather than against zero: these
/// fixtures sit at surveyed coordinates, and 4,300,000 m at a millimetre is
/// past what an `i32` position holds. Reading them against zero clamps, which
/// is a bug in the reading and looks exactly like a bug in the writing.
fn read_points(path: &Path) -> Vec<Point> {
    let layout = RecordLayout::new(7, 0, Vec::new(), Vec::new()).expect("format 7");
    let stride = layout.stride();
    let (scale, offset) = {
        let cloud = open_path(path).expect("opens the converted file");
        let info = cloud.info();
        (info.scale, info.offset)
    };
    let mut source = LasPointSource::open(path, layout, scale, offset).expect("opens");

    let mut out = Vec::new();
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        if source.next_batch(1 << 16, &mut buffer).expect("reads") == 0 {
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
                rgb: [
                    u16::from_le_bytes(record[at::RGB..at::RGB + 2].try_into().unwrap()),
                    u16::from_le_bytes(record[at::RGB + 2..at::RGB + 4].try_into().unwrap()),
                    u16::from_le_bytes(record[at::RGB + 4..at::RGB + 6].try_into().unwrap()),
                ],
                source_id: u16::from_le_bytes(
                    record[at::POINT_SOURCE_ID..at::POINT_SOURCE_ID + 2]
                        .try_into()
                        .unwrap(),
                ),
            });
        }
    }
    out
}

fn sorted(mut points: Vec<Point>) -> Vec<Point> {
    points.sort_by(|a, b| {
        (a.x, a.y, a.z)
            .partial_cmp(&(b.x, b.y, b.z))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    points
}

fn cartesian_f64() -> Vec<Record> {
    vec![
        Record::CARTESIAN_X_F64,
        Record::CARTESIAN_Y_F64,
        Record::CARTESIAN_Z_F64,
    ]
}

fn colour_records() -> Vec<Record> {
    vec![
        Record {
            name: e57::RecordName::ColorRed,
            data_type: RecordDataType::U8,
        },
        Record {
            name: e57::RecordName::ColorGreen,
            data_type: RecordDataType::U8,
        },
        Record {
            name: e57::RecordName::ColorBlue,
            data_type: RecordDataType::U8,
        },
    ]
}

fn xyz(x: f64, y: f64, z: f64) -> Vec<RecordValue> {
    vec![
        RecordValue::Double(x),
        RecordValue::Double(y),
        RecordValue::Double(z),
    ]
}

/// What an 8-bit colour channel becomes after normalisation and widening.
///
/// The same value `widen_8_to_16` produces in the browser tier: `v * 257`, so
/// 255 lands on 65535 exactly. Written out the long way here on purpose — if
/// the two ever disagree, this is the test that says which one moved.
fn widened(channel: u8) -> u16 {
    (f64::from(channel) / 255.0 * 65535.0).round() as u16
}

/// One scan of a lattice, with colour, at a surveyed-looking origin.
fn write_lattice(path: &Path) -> Vec<Point> {
    let mut writer = E57Writer::from_file(path, "test-file-guid").expect("creates");
    let mut prototype = cartesian_f64();
    prototype.extend(colour_records());
    let mut cloud = writer
        .add_pointcloud("scan-guid", prototype)
        .expect("adds a scan");

    let mut expected = Vec::new();
    for i in 0..10 {
        for j in 0..10 {
            let (x, y, z) = (
                500_000.0 + f64::from(i),
                4_300_000.0 + f64::from(j),
                100.0 + f64::from(i * j) / 10.0,
            );
            let (r, g, b) = ((i * 25) as u8, (j * 25) as u8, 255u8);
            let mut values = xyz(x, y, z);
            values.push(RecordValue::Integer(i64::from(r)));
            values.push(RecordValue::Integer(i64::from(g)));
            values.push(RecordValue::Integer(i64::from(b)));
            cloud.add_point(values).expect("writes a point");
            expected.push(Point {
                x,
                y,
                z,
                intensity: 0,
                rgb: [widened(r), widened(g), widened(b)],
                source_id: 1,
            });
        }
    }
    cloud.finalize().expect("finalises the scan");
    writer.finalize().expect("finalises the file");
    expected
}

fn to_copc(scratch: &Scratch, input: &Path, name: &str) -> PathBuf {
    let output = scratch.join(name);
    let options = ConvertOptions::new(
        vec![input.to_path_buf()],
        output.clone(),
        OutputFormat::Copc,
    );
    convert(&options, &mut |_, _| {}).expect("converts");
    output
}

#[test]
fn a_scan_converts_point_for_point() {
    let scratch = Scratch::new("lattice");
    let input = scratch.join("lattice.e57");
    let expected = write_lattice(&input);

    let output = to_copc(&scratch, &input, "lattice.copc.laz");
    let got = read_points(&output);

    assert_eq!(got.len(), expected.len());
    for (got, want) in sorted(got).into_iter().zip(sorted(expected)) {
        // A millimetre is the quantum the converter picks when the input
        // declares none, so that is the tolerance the position has.
        assert!((got.x - want.x).abs() <= 0.001, "{got:?} vs {want:?}");
        assert!((got.y - want.y).abs() <= 0.001, "{got:?} vs {want:?}");
        assert!((got.z - want.z).abs() <= 0.001, "{got:?} vs {want:?}");
        assert_eq!(got.rgb, want.rgb, "colour");
        assert_eq!(got.source_id, 1, "the first scan is source 1");
    }
}

#[test]
fn the_file_opens_as_an_e57_and_says_what_is_in_it() {
    let scratch = Scratch::new("inspect");
    let input = scratch.join("lattice.e57");
    write_lattice(&input);

    let cloud = open_path(&input).expect("opens");
    let info = cloud.info();
    assert_eq!(info.format, FormatId::E57);
    assert!(!info.format.is_indexed(), "an E57 carries no index");
    assert_eq!(info.point_count, 100);
    // The prototype is reported in the file's own spelling, not translated
    // into LAS names it does not have.
    let names: Vec<&str> = info.attributes.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        ["cartesianX", "cartesianY", "cartesianZ", "colorRed", "colorGreen", "colorBlue"]
    );

    let Cloud::E57(e57) = &cloud else {
        panic!("opened as something else");
    };
    assert_eq!(e57.e57.scans.len(), 1);
    assert!(e57.e57.has_color);
    assert!(!e57.e57.has_intensity);
    // No pose, so the file's own bounds describe its own points.
    assert!(e57.e57.extent_exact);
    let extent = e57.e57.extent.expect("declared bounds");
    assert!((extent.min[0] - 500_000.0).abs() < 1e-6);
    assert!((extent.max[0] - 500_009.0).abs() < 1e-6);
}

#[test]
fn a_pose_moves_the_points_and_the_declared_bounds_stop_being_exact() {
    let scratch = Scratch::new("pose");
    let input = scratch.join("posed.e57");

    // A quarter turn about Z, then a hundred metres east.
    let half = std::f64::consts::FRAC_PI_4;
    let mut writer = E57Writer::from_file(&input, "posed").expect("creates");
    let mut cloud = writer
        .add_pointcloud("scan", cartesian_f64())
        .expect("adds a scan");
    cloud.set_transform(Some(Transform {
        rotation: Quaternion {
            w: half.cos(),
            x: 0.0,
            y: 0.0,
            z: half.sin(),
        },
        translation: Translation {
            x: 100.0,
            y: 0.0,
            z: 0.0,
        },
    }));
    for (x, y, z) in [(1.0, 0.0, 0.0), (0.0, 2.0, 0.0), (0.0, 0.0, 3.0)] {
        cloud.add_point(xyz(x, y, z)).expect("writes a point");
    }
    cloud.finalize().expect("finalises");
    writer.finalize().expect("finalises");

    let cloud = open_path(&input).expect("opens");
    let Cloud::E57(e57) = &cloud else {
        panic!("opened as something else");
    };
    assert!(
        !e57.e57.extent_exact,
        "a rotated scan's own bounds are not the box its points occupy"
    );
    assert!(
        e57.e57
            .warnings
            .iter()
            .chain(e57.info.warnings.iter())
            .any(|w| w.code == "e57-bounds-posed"),
        "and the file says so"
    );

    let output = to_copc(&scratch, &input, "posed.copc.laz");
    let got = sorted(read_points(&output));
    assert_eq!(got.len(), 3);
    // (1,0,0) rotated a quarter turn about Z is (0,1,0); the translation puts
    // it at (100,1,0). If the pose were dropped it would still be at (1,0,0).
    let moved = got
        .iter()
        .find(|p| (p.z - 0.0).abs() < 0.01 && (p.y - 1.0).abs() < 0.01)
        .expect("the rotated point");
    assert!((moved.x - 100.0).abs() < 0.01, "{moved:?}");
    // And (0,2,0) becomes (-2,0,0), which the translation puts at 98.
    let second = got
        .iter()
        .find(|p| (p.x - 98.0).abs() < 0.01)
        .expect("the second rotated point");
    assert!(second.y.abs() < 0.01, "{second:?}");
}

#[test]
fn spherical_records_arrive_cartesian() {
    let scratch = Scratch::new("spherical");
    let input = scratch.join("spherical.e57");

    let mut writer = E57Writer::from_file(&input, "spherical").expect("creates");
    let prototype = vec![
        Record::SPHERICAL_RANGE_F64,
        Record::SPHERICAL_AZIMUTH_F64,
        Record::SPHERICAL_ELEVATION_F64,
    ];
    let mut cloud = writer.add_pointcloud("scan", prototype).expect("adds");
    // Range 10 straight ahead, a quarter turn round, and straight up.
    for (range, azimuth, elevation) in [
        (10.0, 0.0, 0.0),
        (10.0, std::f64::consts::FRAC_PI_2, 0.0),
        (10.0, 0.0, std::f64::consts::FRAC_PI_2),
    ] {
        cloud
            .add_point(vec![
                RecordValue::Double(range),
                RecordValue::Double(azimuth),
                RecordValue::Double(elevation),
            ])
            .expect("writes a point");
    }
    cloud.finalize().expect("finalises");
    writer.finalize().expect("finalises");

    let output = to_copc(&scratch, &input, "spherical.copc.laz");
    let got = read_points(&output);
    assert_eq!(got.len(), 3);
    let near = |p: &&Point, x: f64, y: f64, z: f64| {
        (p.x - x).abs() < 0.01 && (p.y - y).abs() < 0.01 && (p.z - z).abs() < 0.01
    };
    assert!(got.iter().any(|p| near(&p, 10.0, 0.0, 0.0)), "{got:?}");
    assert!(got.iter().any(|p| near(&p, 0.0, 10.0, 0.0)), "{got:?}");
    assert!(got.iter().any(|p| near(&p, 0.0, 0.0, 10.0)), "{got:?}");
}

#[test]
fn points_with_no_position_are_dropped_rather_than_written_at_the_origin() {
    let scratch = Scratch::new("invalid");
    let input = scratch.join("invalid.e57");

    let mut writer = E57Writer::from_file(&input, "invalid").expect("creates");
    let mut prototype = cartesian_f64();
    prototype.push(Record::CARTESIAN_INVALID_STATE);
    let mut cloud = writer.add_pointcloud("scan", prototype).expect("adds");
    for i in 0..10 {
        // Every third point is a no-return: state 2 means the coordinate has
        // no meaning, whatever bytes are sitting in it.
        let state = i64::from(i % 3 == 0) * 2;
        let mut values = xyz(f64::from(i), 0.0, 0.0);
        values.push(RecordValue::Integer(state));
        cloud.add_point(values).expect("writes a point");
    }
    cloud.finalize().expect("finalises");
    writer.finalize().expect("finalises");

    let report = scan(std::slice::from_ref(&input)).expect("scans");
    assert_eq!(report.point_count, 6, "four of the ten carry no position");
    assert!(
        report.warnings.iter().any(|w| w.code == "e57-invalid-points"),
        "and the conversion says so: {:?}",
        report.warnings
    );

    let output = to_copc(&scratch, &input, "invalid.copc.laz");
    let got = read_points(&output);
    assert_eq!(got.len(), 6);
    assert!(
        !got.iter().any(|p| p.x == 0.0 && p.y == 0.0 && p.z == 0.0),
        "a dropped point must not reappear at the origin: {got:?}"
    );
}

#[test]
fn each_scan_keeps_its_place_as_a_point_source_id() {
    let scratch = Scratch::new("scans");
    let input = scratch.join("two-scans.e57");

    let mut writer = E57Writer::from_file(&input, "two").expect("creates");
    for (index, offset) in [(0, 0.0), (1, 50.0)] {
        let mut cloud = writer
            .add_pointcloud(&format!("scan-{index}"), cartesian_f64())
            .expect("adds");
        for i in 0..5 {
            cloud
                .add_point(xyz(offset + f64::from(i), 0.0, 0.0))
                .expect("writes");
        }
        cloud.finalize().expect("finalises");
    }
    writer.finalize().expect("finalises");

    let output = to_copc(&scratch, &input, "two-scans.copc.laz");
    let got = read_points(&output);
    assert_eq!(got.len(), 10);
    for point in &got {
        let expected = if point.x < 50.0 { 1 } else { 2 };
        assert_eq!(point.source_id, expected, "{point:?}");
    }
}

#[test]
fn intensity_stays_intensity() {
    let scratch = Scratch::new("intensity");
    let input = scratch.join("intensity.e57");

    let mut writer = E57Writer::from_file(&input, "intensity").expect("creates");
    let mut prototype = cartesian_f64();
    prototype.push(Record {
        name: e57::RecordName::Intensity,
        data_type: RecordDataType::UNIT_F32,
    });
    let mut cloud = writer.add_pointcloud("scan", prototype).expect("adds");
    for i in 0..5 {
        let mut values = xyz(f64::from(i), 0.0, 0.0);
        values.push(RecordValue::Single(i as f32 / 4.0));
        cloud.add_point(values).expect("writes");
    }
    cloud.finalize().expect("finalises");
    writer.finalize().expect("finalises");

    let report = scan(std::slice::from_ref(&input)).expect("scans");
    assert!(
        !report.any_color,
        "a scan with intensity and no colour must not arrive pre-greyed"
    );

    let output = to_copc(&scratch, &input, "intensity.copc.laz");
    // The written file has no colour lane at all. Asserting on the bytes a
    // reader hands back would prove nothing: reading a format 6 file as a
    // format 7 one fills the lane with white, deliberately, in
    // `RecordConverter`.
    let converted = open_path(&output).expect("opens");
    let Cloud::Copc(copc) = &converted else {
        panic!("converted to something else");
    };
    assert_eq!(copc.header.point_format, 6, "no colour lane was invented");

    let got = sorted(read_points(&output));
    assert_eq!(got.len(), 5);
    // Normalised against the file's own limits and widened: 0 to 65535 across
    // the five points.
    assert_eq!(got.first().expect("first").intensity, 0);
    assert_eq!(got.last().expect("last").intensity, 65535);
}

/// `None` when the dataset is not on this machine. Same convention as
/// `real_files.rs`: the E57 samples are gitignored, so this skips on a clone
/// that never downloaded them.
fn sample(name: &str) -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../demo/data/e57")
        .join(name);
    path.exists().then_some(path)
}

/// The same scan, stored two ways, has to arrive as the same cloud.
///
/// `pumpACartesian` and `pumpASpherical` are the libE57 project's own sample:
/// one real terrestrial scan of a pump, published once as x/y/z and once as
/// range/azimuth/elevation. Nothing this repo wrote is in either of them, and
/// the only way the two conversions agree is if the spherical reconstruction is
/// right.
///
/// It is also the file that proves the no-return case is not academic: 215,329
/// of its 370,530 points carry no position. Writing those at the origin — which
/// is what dropping the invalid-state check does — would put a 215k-point spike
/// in the middle of the scan.
#[test]
fn one_real_scan_stored_two_ways_reads_as_one_cloud() {
    let (Some(cartesian), Some(spherical)) = (
        sample("pumpACartesian.e57"),
        sample("pumpASpherical.e57"),
    ) else {
        eprintln!("skipping: demo/data/e57/pump*.e57 are not present");
        return;
    };

    // Compared in FILE ORDER, straight out of the reader, rather than after a
    // conversion. The octree reorders points, and two clouds that agree to
    // within a millimetre do not sort into the same sequence — comparing them
    // sorted pairs each point with its neighbour and calls that a colour bug.
    let from_cartesian = read_e57(&cartesian);
    let from_spherical = read_e57(&spherical);

    assert_eq!(
        from_cartesian.len(),
        155_201,
        "the points that have a position; the file declares 370,530"
    );
    assert_eq!(from_spherical.len(), from_cartesian.len());

    let mut worst = 0.0f64;
    for (index, (a, b)) in from_cartesian.iter().zip(from_spherical.iter()).enumerate() {
        let delta = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2) + (a.z - b.z).powi(2)).sqrt();
        worst = worst.max(delta);
        assert_eq!(a.rgb, b.rgb, "colour, at point {index}");
        assert_eq!(a.intensity, b.intensity, "intensity, at point {index}");
    }
    // MEASURED, not guessed: the spherical file reconstructs every position
    // from three scaled integers through two trigonometric functions, so the
    // two never land on the same bit pattern.
    assert!(worst <= 0.001, "worst disagreement {worst} m");
}

/// Every point of an E57, in the file's own order.
fn read_e57(path: &Path) -> Vec<Point> {
    let file = std::io::BufReader::new(std::fs::File::open(path).expect("opens"));
    let mut reader = E57Points::open(file).expect("reads the header");
    let mut out = Vec::new();
    reader
        .read(1 << 16, &mut |batch| {
            for i in 0..batch.len() {
                out.push(Point {
                    x: batch.x[i],
                    y: batch.y[i],
                    z: batch.z[i],
                    intensity: batch.intensity.get(i).copied().unwrap_or(0),
                    rgb: batch.rgb.get(i).copied().unwrap_or([0; 3]),
                    source_id: batch.source_id[i],
                });
            }
            Ok(())
        })
        .expect("reads the points");
    out
}

/// An E57 and a LAS-family file, merged into one cloud.
///
/// The path most likely to break quietly: the two sources agree on nothing —
/// one declares a quantum and the other does not, one has colour and the other
/// may not, one is legacy LAS and the other is not LAS at all — and the
/// converter has to pick one record, one cube and one origin for both.
#[test]
fn an_e57_and_a_las_file_merge_into_one_cloud() {
    let scratch = Scratch::new("merge");
    let e57 = scratch.join("lattice.e57");
    let expected = write_lattice(&e57);

    // The LAS half comes from converting a second E57 that sits somewhere else
    // entirely, so the merged extent has to cover both.
    let far = scratch.join("far.e57");
    {
        let mut writer = E57Writer::from_file(&far, "far").expect("creates");
        let mut cloud = writer
            .add_pointcloud("scan", cartesian_f64())
            .expect("adds");
        for i in 0..10 {
            cloud
                .add_point(xyz(500_100.0 + f64::from(i), 4_300_100.0, 150.0))
                .expect("writes");
        }
        cloud.finalize().expect("finalises");
        writer.finalize().expect("finalises");
    }
    let far_copc = to_copc(&scratch, &far, "far.copc.laz");

    let output = scratch.join("merged.copc.laz");
    let options = ConvertOptions::new(
        vec![e57.clone(), far_copc.clone()],
        output.clone(),
        OutputFormat::Copc,
    );
    convert(&options, &mut |_, _| {}).expect("converts both");

    let got = read_points(&output);
    assert_eq!(got.len(), expected.len() + 10);

    let merged = open_path(&output).expect("opens");
    let bounds = merged.info().tight_bounds;
    // The lattice starts at 500,000 and the far file ends at 500,109: one cloud
    // covering both, not one of them silently dropped.
    assert!((bounds.min[0] - 500_000.0).abs() < 0.01, "{bounds:?}");
    assert!((bounds.max[0] - 500_109.0).abs() < 0.01, "{bounds:?}");
    // The E57 has colour and the converted file does not, so the output keeps
    // the colour lane and the colourless half arrives white.
    assert!(got.iter().any(|p| p.rgb == [u16::MAX; 3]), "the colourless half");
    assert!(got.iter().any(|p| p.rgb[2] == 65535 && p.rgb[0] < 65535), "the lattice");
}
