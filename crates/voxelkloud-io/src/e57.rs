//! E57 — what the terrestrial scanners write.
//!
//! Structurally unlike anything else here: a 48-byte header, an XML section at
//! the *end* of the file describing what is in it, and `CompressedVector`
//! sections holding bitpacked records. It carries no index, so it belongs to
//! the same tier as a bare LAS — read it whole, index it here.
//!
//! WHAT THIS MODULE OWNS is the semantics, not the decoding. The `e57` crate
//! (pure Rust, `forbid(unsafe_code)`, one dependency) does the bitpacking, the
//! per-page CRC and the XML. What is left is what a point cloud library has to
//! decide, and every one of these decisions is visible in what a viewer draws:
//!
//! - **Pose.** A scan stores its points in the scanner's own frame plus a
//!   rotation and translation into the file's frame. Two scans of one building
//!   are two clouds sitting on top of each other until the pose is applied.
//! - **Spherical coordinates.** Some scanners store range/azimuth/elevation
//!   rather than x/y/z. Converted, because everything downstream is Cartesian.
//! - **Invalid points.** E57 says a point may carry no position at all — a
//!   no-return in the scan grid. They are dropped and counted, not written at
//!   the origin, which is where they would otherwise land as a spike.
//! - **Intensity is not colour.** The reader will happily synthesise grey RGB
//!   from intensity; that is turned OFF here. A cloud with intensity and no
//!   colour should reach the viewer's intensity ramp, not arrive pre-greyed
//!   with nothing left to ramp.
//! - **Which scan a point came from** survives as the LAS point source id, so
//!   a merged multi-scan file can still be told apart by where it was measured.
//!
//! PUSH, NOT PULL, and that is forced rather than chosen:
//! `e57::PointCloudReaderSimple` borrows the file reader for as long as it
//! lives, so a struct holding both would be self-referential. Rather than pull
//! in a crate to make that safe, the traversal is inverted — [`E57Points::read`]
//! drives and hands out batches. The converter, which wants a pull, gets one by
//! running this on a thread and posting batches through a bounded channel (see
//! [`crate::read::e57_points`]); the browser tier accumulates them, which is
//! what it does with every other single-file format anyway.

use std::io::{Read, Seek};

use ::e57::{CartesianCoordinate, E57Reader, PointCloud, Record, RecordDataType, RecordName};

use crate::attribute::AttributeType;
use crate::bounds::Bounds;
use crate::error::{Error, Result};
use crate::warning::Warning;

/// The eight bytes every E57 file starts with.
pub const SIGNATURE: &[u8; 8] = b"ASTM-E57";

/// Points per batch handed to a sink.
///
/// A million points is 24 MB of f64 coordinates plus 6 MB of colour — large
/// enough that the per-call overhead disappears, small enough to stay a
/// bounded allocation on the way to a converter that is out-of-core by design.
pub const BATCH: usize = 1 << 20;

/// Whether a pose rotation is close enough to identity that a box under it is
/// still an axis-aligned box.
///
/// Only exact identity would be safe to assume, but files written by round
/// trips through float32 carry `w = 0.9999999999999999`, and refusing those
/// would send every one of them down the measuring path for nothing.
const IDENTITY_EPSILON: f64 = 1e-9;

/// One field of a scan's record prototype, in this crate's vocabulary.
///
/// E57 has no fixed record: every scan declares its own list of fields and the
/// width of each. A `ScaledInteger` field is reported at the width it is stored
/// in and with the domain it decodes to, because those are two different facts
/// and collapsing them is how a reader ends up describing millimetres as
/// metres.
#[derive(Debug, Clone)]
pub struct PrototypeField {
    /// The E57 XML spelling: `cartesianX`, `colorRed`, `intensity`.
    pub name: String,
    /// The width the field DECODES to, which is what a consumer of this crate
    /// sees. Not what it costs in the file — see [`Self::bits`].
    pub kind: AttributeType,
    /// Bits the field actually occupies in the file.
    ///
    /// E57 packs integers to the width their declared range needs, so a
    /// `cartesianInvalidState` bounded 0..2 is TWO BITS and not the eight bytes
    /// its decoded type suggests. Reporting the decoded widths as a record size
    /// overstates a bunny by a third.
    pub bits: usize,
    /// The domain the field decodes to, when it declares one. Zero to zero
    /// when it does not, which is what "unstated" looks like everywhere else
    /// in this crate.
    pub min: f64,
    pub max: f64,
}

/// What one scan inside the file declares about itself.
#[derive(Debug, Clone)]
pub struct ScanInfo {
    pub name: Option<String>,
    pub guid: Option<String>,
    /// As the XML declares it. Invalid points are included in this count and
    /// are not written, so what a conversion produces is smaller.
    pub point_count: u64,
    pub has_color: bool,
    pub has_intensity: bool,
    /// Whether the records hold x/y/z at all. False means range/azimuth/
    /// elevation, converted on the way out.
    pub has_cartesian: bool,
    /// The scan's own `cartesianBounds`, in the scan's frame — BEFORE the pose.
    pub declared_bounds: Option<Bounds>,
    /// Whether this scan carries a rotation that is not the identity.
    pub rotated: bool,
    /// Vendor and model, when the file says.
    pub sensor: Option<String>,
}

/// What the XML section says, before a single point has been read.
#[derive(Debug, Clone)]
pub struct E57Info {
    /// Sum of every scan's declared record count.
    pub point_count: u64,
    /// Union of the scans' own bounds, with each scan's pose applied.
    ///
    /// `None` when no scan declares any — a spherical-only file usually does
    /// not. See [`Self::extent_exact`] before trusting it.
    pub extent: Option<Bounds>,
    /// Whether [`Self::extent`] is the file's own claim rather than a derived
    /// one.
    ///
    /// False when a scan is posed with a real rotation: the corners of a
    /// rotated box do not make a box, and the axis-aligned hull of them is
    /// larger than the points. Good enough to look at, not good enough to write
    /// into a LAS header, which is why the converter measures instead.
    pub extent_exact: bool,
    pub has_color: bool,
    pub has_intensity: bool,
    pub scans: Vec<ScanInfo>,
    pub guid: String,
    /// The writing library's own version string, when it left one.
    pub library_version: Option<String>,
    /// The file's free-text CRS field. E57 has no structured place for a
    /// projection: this is where a writer puts WKT when it bothers at all.
    pub coordinate_metadata: Option<String>,
    /// The FIRST scan's record prototype. Empty when the file holds no scans.
    ///
    /// The first, not a union: a file whose scans declare different prototypes
    /// is legal, and a merged list would describe a record no scan has.
    pub prototype: Vec<PrototypeField>,
    pub warnings: Vec<Warning>,
}

/// One batch of points, in the file's own units and frame.
///
/// Columnar rather than a `Vec<Point>`: both consumers want the columns — the
/// converter quantises positions into a record, the browser tier hands them to
/// the same partitioner the CLI runs — and a struct per point would allocate
/// an `Option` per field per point to say what the file already said once.
#[derive(Debug, Default)]
pub struct PointBatch {
    pub x: Vec<f64>,
    pub y: Vec<f64>,
    pub z: Vec<f64>,
    /// Empty when the file carries no colour. 16-bit, widened from whatever
    /// the file stored, normalised against the scan's own colour limits.
    pub rgb: Vec<[u16; 3]>,
    /// Empty when the file carries no intensity. Normalised against the scan's
    /// own intensity limits, which is the only scale that means anything: E57
    /// intensity is a float with file-defined bounds, not a LAS-style u16.
    pub intensity: Vec<u16>,
    /// Which scan each point came from, 1-based, saturating at `u16::MAX`.
    pub source_id: Vec<u16>,
}

impl PointBatch {
    pub fn len(&self) -> usize {
        self.x.len()
    }

    pub fn is_empty(&self) -> bool {
        self.x.is_empty()
    }

    fn clear(&mut self) {
        self.x.clear();
        self.y.clear();
        self.z.clear();
        self.rgb.clear();
        self.intensity.clear();
        self.source_id.clear();
    }
}

/// What a full pass produced.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadReport {
    /// Points handed to the sink.
    pub points: u64,
    /// Points the file declared and that carried no position.
    pub dropped: u64,
    /// Points whose colour was marked invalid while the file has colour. The
    /// record has a colour lane either way, and they are written white for the
    /// reason `RecordConverter` writes white: a black point reads as a shadow,
    /// so "no colour here" would be indistinguishable from "this is dark".
    pub colourless: u64,
}

/// A measured extent, and the count that goes with it.
#[derive(Debug, Clone, Copy)]
pub struct Measured {
    pub extent: Bounds,
    pub report: ReadReport,
}

/// An open E57 file.
pub struct E57Points<T: Read + Seek> {
    reader: E57Reader<T>,
    clouds: Vec<PointCloud>,
    info: E57Info,
}

impl<T: Read + Seek> E57Points<T> {
    /// Read the header and the XML section. No points.
    pub fn open(inner: T) -> Result<Self> {
        let reader = E57Reader::new(inner).map_err(open_err)?;
        let clouds = reader.pointclouds();
        let info = describe(&reader, &clouds);
        Ok(Self {
            reader,
            clouds,
            info,
        })
    }

    pub fn info(&self) -> &E57Info {
        &self.info
    }

    /// Push every point of every scan through `sink`, in batches.
    ///
    /// The sink may fail, and its error is returned unchanged — which is what
    /// lets the converter stop a 40-minute read the moment its output disk
    /// fills.
    /// The sink is handed the batch by `&mut` so it can take the buffers
    /// rather than copy them — the converter posts them to another thread, and
    /// a 30 MB memcpy per million points to hand over memory that is about to
    /// be dropped anyway is a cost with nothing on the other side of it. What
    /// it leaves behind is cleared and refilled either way.
    pub fn read(
        &mut self,
        batch: usize,
        sink: &mut dyn FnMut(&mut PointBatch) -> Result<()>,
    ) -> Result<ReadReport> {
        let batch = batch.max(1);
        let mut out = PointBatch::default();
        let mut report = ReadReport::default();
        let want_color = self.info.has_color;
        let want_intensity = self.info.has_intensity;

        for (index, cloud) in self.clouds.iter().enumerate() {
            // 1-based: a LAS point source id of 0 means "unknown", and a scan
            // that exists is not unknown.
            let source_id = u16::try_from(index + 1).unwrap_or(u16::MAX);

            let mut points = self
                .reader
                .pointcloud_simple(cloud)
                .map_err(|e| Error::Codec(format!("scan {}: {e}", index + 1)))?;
            points.apply_pose(true);
            points.spherical_to_cartesian(true);
            points.normalize_color(true);
            points.normalize_intensity(true);
            // See the module docs: intensity reaches the viewer as intensity.
            points.intensity_to_color(false);

            for point in points {
                let point = point.map_err(|e| Error::Codec(format!("scan {}: {e}", index + 1)))?;
                let CartesianCoordinate::Valid { x, y, z } = point.cartesian else {
                    // Invalid, or a bare direction with no range. Neither is a
                    // place.
                    report.dropped += 1;
                    continue;
                };

                out.x.push(x);
                out.y.push(y);
                out.z.push(z);
                out.source_id.push(source_id);
                if want_color {
                    match point.color {
                        Some(c) => out.rgb.push([
                            to_u16(f64::from(c.red)),
                            to_u16(f64::from(c.green)),
                            to_u16(f64::from(c.blue)),
                        ]),
                        None => {
                            report.colourless += 1;
                            out.rgb.push([u16::MAX; 3]);
                        }
                    }
                }
                if want_intensity {
                    out.intensity
                        .push(point.intensity.map_or(0, |i| to_u16(f64::from(i))));
                }

                if out.len() >= batch {
                    report.points += out.len() as u64;
                    sink(&mut out)?;
                    out.clear();
                }
            }
        }

        if !out.is_empty() {
            report.points += out.len() as u64;
            sink(&mut out)?;
        }
        Ok(report)
    }

    /// One pass, for the extent and the count that survive it.
    ///
    /// The converter cannot use the declared bounds when a scan is posed —
    /// see [`E57Info::extent_exact`] — and it cannot use the declared count
    /// either, because invalid points are declared and not written. Both facts
    /// come out of the same read.
    pub fn measure(&mut self) -> Result<Measured> {
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        let report = self.read(BATCH, &mut |b| {
            for i in 0..b.len() {
                for (axis, value) in [b.x[i], b.y[i], b.z[i]].into_iter().enumerate() {
                    if value < min[axis] {
                        min[axis] = value;
                    }
                    if value > max[axis] {
                        max[axis] = value;
                    }
                }
            }
            Ok(())
        })?;

        if report.points == 0 {
            return Err(Error::Unsupported(
                "this E57 declares points and none of them carried a position".to_string(),
            ));
        }
        Ok(Measured {
            extent: Bounds::new(min, max),
            report,
        })
    }
}

/// Normalised 0..1 to a 16-bit channel.
///
/// `* 65535` rather than `* 65536`, so a fully saturated channel lands on
/// `u16::MAX` exactly instead of wrapping to zero.
fn to_u16(value: f64) -> u16 {
    (value.clamp(0.0, 1.0) * 65535.0).round() as u16
}

/// Everything the XML says, folded into one description.
fn describe<T: Read + Seek>(reader: &E57Reader<T>, clouds: &[PointCloud]) -> E57Info {
    let mut info = E57Info {
        point_count: 0,
        extent: None,
        extent_exact: true,
        has_color: false,
        has_intensity: false,
        scans: Vec::with_capacity(clouds.len()),
        guid: reader.guid().to_string(),
        library_version: reader.library_version().map(str::to_string),
        coordinate_metadata: reader.coordinate_metadata().map(str::to_string),
        prototype: clouds.first().map(|c| prototype(&c.prototype)).unwrap_or_default(),
        warnings: Vec::new(),
    };

    let mut extent: Option<Bounds> = None;

    for (index, cloud) in clouds.iter().enumerate() {
        let rotated = cloud.transform.as_ref().is_some_and(|t| {
            let q = &t.rotation;
            (q.w.abs() - 1.0).abs() > IDENTITY_EPSILON
                || q.x.abs() > IDENTITY_EPSILON
                || q.y.abs() > IDENTITY_EPSILON
                || q.z.abs() > IDENTITY_EPSILON
        });
        let declared = cloud.cartesian_bounds.as_ref().and_then(|b| {
            match (b.x_min, b.y_min, b.z_min, b.x_max, b.y_max, b.z_max) {
                (Some(x0), Some(y0), Some(z0), Some(x1), Some(y1), Some(z1)) => {
                    Some(Bounds::new([x0, y0, z0], [x1, y1, z1]))
                }
                _ => None,
            }
        });

        let scan = ScanInfo {
            name: cloud.name.clone(),
            guid: cloud.guid.clone(),
            point_count: cloud.records,
            has_color: cloud.has_color(),
            has_intensity: cloud.has_intensity(),
            has_cartesian: cloud.has_cartesian(),
            declared_bounds: declared,
            rotated,
            sensor: match (&cloud.sensor_vendor, &cloud.sensor_model) {
                (Some(vendor), Some(model)) => Some(format!("{vendor} {model}")),
                (Some(vendor), None) => Some(vendor.clone()),
                (None, Some(model)) => Some(model.clone()),
                (None, None) => None,
            },
        };

        info.point_count += scan.point_count;
        info.has_color |= scan.has_color;
        info.has_intensity |= scan.has_intensity;

        match &scan.declared_bounds {
            Some(bounds) => {
                let posed = pose_box(bounds, cloud);
                extent = Some(match extent {
                    Some(current) => current.union(&posed),
                    None => posed,
                });
                if rotated {
                    info.extent_exact = false;
                }
            }
            None => {
                info.extent_exact = false;
                info.warnings.push(Warning::new(
                    "e57-bounds-undeclared",
                    format!("data3D[{index}]"),
                    "This scan declares no cartesianBounds, so where its points are can only be \
                     learnt by reading them."
                        .to_string(),
                ));
            }
        }

        if !scan.has_cartesian {
            info.warnings.push(Warning::new(
                "e57-spherical",
                format!("data3D[{index}]"),
                "This scan stores range, azimuth and elevation rather than x/y/z. The points are \
                 converted to Cartesian on the way out."
                    .to_string(),
            ));
        }

        info.scans.push(scan);
    }

    if clouds.len() > usize::from(u16::MAX) {
        info.warnings.push(Warning::new(
            "e57-scans-overflow",
            "data3D",
            format!(
                "This file holds {} scans and a LAS point source id is 16 bits, so every scan past \
                 the 65,535th is written with the same id.",
                clouds.len()
            ),
        ));
    }

    if info.extent_exact && extent.is_some() {
        // Nothing to say: the file's own bounds describe the file's own points.
    } else if extent.is_some() {
        info.warnings.push(Warning::new(
            "e57-bounds-posed",
            "data3D",
            "At least one scan is stored rotated relative to the file's frame, so the bounds it \
             declares are not the box its points occupy. What is reported here is the hull of the \
             rotated box, which is larger."
                .to_string(),
        ));
    }

    info.extent = extent;
    info
}

/// A scan's prototype, field by field.
fn prototype(records: &[Record]) -> Vec<PrototypeField> {
    records
        .iter()
        .map(|record| {
            let (kind, min, max) = match &record.data_type {
                RecordDataType::Single { min, max } => (
                    AttributeType::Float,
                    f64::from(min.unwrap_or(0.0)),
                    f64::from(max.unwrap_or(0.0)),
                ),
                RecordDataType::Double { min, max } => {
                    (AttributeType::Double, min.unwrap_or(0.0), max.unwrap_or(0.0))
                }
                // Stored as an integer, read as what it decodes to. Both facts
                // survive: the width says how it sits in the file, the domain
                // says what it means.
                RecordDataType::ScaledInteger {
                    min,
                    max,
                    scale,
                    offset,
                } => (
                    AttributeType::Int64,
                    *min as f64 * scale + offset,
                    *max as f64 * scale + offset,
                ),
                RecordDataType::Integer { min, max } => {
                    (AttributeType::Int64, *min as f64, *max as f64)
                }
            };
            PrototypeField {
                name: field_name(&record.name),
                kind,
                bits: stored_bits(&record.data_type),
                min,
                max,
            }
        })
        .collect()
}

/// Bits one value of a field occupies in the file.
///
/// The rule is the spec's and the `e57` crate keeps its own copy private: a
/// float is its IEEE width, an integer is as many bits as its declared range
/// needs, and a range of zero — a field every point agrees on — is stored in no
/// bits at all.
fn stored_bits(kind: &RecordDataType) -> usize {
    match kind {
        RecordDataType::Single { .. } => 32,
        RecordDataType::Double { .. } => 64,
        RecordDataType::ScaledInteger { min, max, .. } | RecordDataType::Integer { min, max } => {
            let range = i128::from(*max) - i128::from(*min);
            if range > 0 {
                range.ilog2() as usize + 1
            } else {
                0
            }
        }
    }
}

/// The E57 XML spelling of a field.
///
/// Spelled out rather than derived from the debug format: the crate keeps its
/// own `tag_name` private, and a name printed into a report should not change
/// because a dependency renamed a variant.
fn field_name(name: &RecordName) -> String {
    match name {
        RecordName::CartesianX => "cartesianX",
        RecordName::CartesianY => "cartesianY",
        RecordName::CartesianZ => "cartesianZ",
        RecordName::CartesianInvalidState => "cartesianInvalidState",
        RecordName::SphericalRange => "sphericalRange",
        RecordName::SphericalAzimuth => "sphericalAzimuth",
        RecordName::SphericalElevation => "sphericalElevation",
        RecordName::SphericalInvalidState => "sphericalInvalidState",
        RecordName::Intensity => "intensity",
        RecordName::IsIntensityInvalid => "isIntensityInvalid",
        RecordName::ColorRed => "colorRed",
        RecordName::ColorGreen => "colorGreen",
        RecordName::ColorBlue => "colorBlue",
        RecordName::IsColorInvalid => "isColorInvalid",
        RecordName::RowIndex => "rowIndex",
        RecordName::ColumnIndex => "columnIndex",
        RecordName::ReturnCount => "returnCount",
        RecordName::ReturnIndex => "returnIndex",
        RecordName::TimeStamp => "timeStamp",
        RecordName::IsTimeStampInvalid => "isTimeStampInvalid",
        // An extension. The namespace is part of the name in the file and part
        // of it here, or two vendors' extensions read as one field.
        RecordName::Unknown { namespace, name } if namespace.is_empty() => return name.clone(),
        RecordName::Unknown { namespace, name } => return format!("{namespace}:{name}"),
    }
    .to_string()
}

/// A scan's declared box, moved into the file's frame.
///
/// The eight corners are transformed and re-bounded rather than the two
/// extremes, because a rotated box's extremes are not its corners' extremes.
/// The rotation is spelled exactly as `e57` spells it when it transforms a
/// point — a second convention here would put the box somewhere the points are
/// not.
fn pose_box(bounds: &Bounds, cloud: &PointCloud) -> Bounds {
    let Some(transform) = &cloud.transform else {
        return *bounds;
    };
    let q = &transform.rotation;
    let r = [
        q.w * q.w + q.x * q.x - q.y * q.y - q.z * q.z,
        2.0 * (q.x * q.y + q.w * q.z),
        2.0 * (q.x * q.z - q.w * q.y),
        2.0 * (q.x * q.y - q.w * q.z),
        q.w * q.w + q.y * q.y - q.x * q.x - q.z * q.z,
        2.0 * (q.y * q.z + q.w * q.x),
        2.0 * (q.x * q.z + q.w * q.y),
        2.0 * (q.y * q.z - q.w * q.x),
        q.w * q.w + q.z * q.z - q.x * q.x - q.y * q.y,
    ];
    let t = &transform.translation;

    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for corner in 0..8 {
        let x = if corner & 1 == 0 { bounds.min[0] } else { bounds.max[0] };
        let y = if corner & 2 == 0 { bounds.min[1] } else { bounds.max[1] };
        let z = if corner & 4 == 0 { bounds.min[2] } else { bounds.max[2] };
        let moved = [
            r[0] * x + r[3] * y + r[6] * z + t.x,
            r[1] * x + r[4] * y + r[7] * z + t.y,
            r[2] * x + r[5] * y + r[8] * z + t.z,
        ];
        for axis in 0..3 {
            if moved[axis] < min[axis] {
                min[axis] = moved[axis];
            }
            if moved[axis] > max[axis] {
                max[axis] = moved[axis];
            }
        }
    }
    Bounds::new(min, max)
}

/// An `e57` error from opening, in this crate's vocabulary.
///
/// The distinction that matters to a caller is "this is not an E57 file" —
/// which sniffing depends on — against "this is one and it is broken". The
/// crate does not type that difference, so the signature is checked before the
/// reader is built and anything that fails after it is a real failure.
fn open_err(error: ::e57::Error) -> Error {
    Error::NotFormat {
        format: "E57",
        detail: error.to_string(),
    }
}

/// Whether a buffer starts the way every E57 file starts.
pub fn is_e57(head: &[u8]) -> bool {
    head.len() >= SIGNATURE.len() && &head[..SIGNATURE.len()] == SIGNATURE
}
