//! Converting: read every input, build one octree, write one cloud.
//!
//! The whole pipeline in one place, because the interesting decisions are the
//! ones between the stages and they are easy to lose. In order: what quantum to
//! write at, what cube to index in, whether the points fit in memory, and what
//! to do when two inputs disagree.
//!
//! **Never in place, and never silently lossy.** Every narrowing this performs
//! — a dropped extra dimension, a coarsened scale, a scan angle rounded to fit
//! Potree's byte — comes back as a warning naming what was lost and why.

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::bounds::Bounds;
use crate::build::{build_subtree, chunked::ChunkedBuild, indexing_cube, BuildOptions, NodeSink};
use crate::cloud::FormatId;
use crate::crs::Crs;
use crate::error::{Error, Result};
use crate::octree::OctreeKey;
use crate::read::las_points::LasPointSource;
use crate::read::PointSource;
use crate::record::{output_format, RecordLayout};
use crate::warning::Warning;
use crate::write::copc::CopcWriter;
use crate::write::ept::{EptEncoding, EptWriter};
use crate::write::potree::{PotreeEncoding, PotreeWriter};
use crate::write::tileset::TilesetWriter;
use crate::write::{WriteOptions, WriteReport};

/// What to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Copc,
    PotreeV2(PotreeEncoding),
    Ept(EptEncoding),
    /// 3D Tiles 1.1, explicit tree, glTF content.
    Tiles3D,
}

impl OutputFormat {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "copc" => Self::Copc,
            "potree" | "potree-v2" => Self::PotreeV2(PotreeEncoding::Default),
            "potree-brotli" => Self::PotreeV2(PotreeEncoding::Brotli),
            "ept" | "ept-laszip" => Self::Ept(EptEncoding::Laszip),
            "ept-binary" => Self::Ept(EptEncoding::Binary),
            "3dtiles" | "3d-tiles" | "tileset" => Self::Tiles3D,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Copc => "copc",
            Self::PotreeV2(PotreeEncoding::Default) => "potree-v2",
            Self::PotreeV2(PotreeEncoding::Brotli) => "potree-v2-brotli",
            Self::Ept(EptEncoding::Laszip) => "ept-laszip",
            Self::Ept(EptEncoding::Binary) => "ept-binary",
            Self::Tiles3D => "3d-tiles",
        }
    }

    /// Whether the output is one file or a directory of them.
    pub fn is_file(self) -> bool {
        self == Self::Copc
    }
}

#[derive(Debug, Clone)]
pub struct ConvertOptions {
    pub inputs: Vec<PathBuf>,
    pub output: PathBuf,
    pub format: OutputFormat,
    /// Points across a node's edge.
    pub span: u32,
    /// Points a node may hold without being subdivided.
    pub leaf_points: usize,
    /// Position quantum. `None` takes the finest the inputs use.
    pub scale: Option<[f64; 3]>,
    /// How much of the cloud may be held in memory before it spills to disk.
    pub memory_budget: usize,
    /// Where spilled points go. `None` uses a directory beside the output.
    pub scratch: Option<PathBuf>,
    pub generator: String,
}

impl ConvertOptions {
    pub fn new(inputs: Vec<PathBuf>, output: PathBuf, format: OutputFormat) -> Self {
        Self {
            inputs,
            output,
            format,
            span: crate::build::DEFAULT_SPAN,
            leaf_points: crate::build::DEFAULT_LEAF_POINTS,
            scale: None,
            // A gigabyte of canonical records is about 28 million points. Above
            // that the build goes through disk, which costs a pass and makes
            // the ceiling the disk's rather than the machine's.
            memory_budget: 1 << 30,
            scratch: None,
            generator: format!("voxelkloud {}", crate::VERSION),
        }
    }
}

/// What the inputs turned out to be, before a point is read.
#[derive(Debug, Clone)]
pub struct Scan {
    pub point_count: u64,
    pub extent: Bounds,
    pub scale: [f64; 3],
    pub offset: [f64; 3],
    pub crs: Option<Crs>,
    pub any_color: bool,
    pub any_nir: bool,
    /// Whether every input used a legacy point format (0-5).
    ///
    /// It decides how the output names two fields. PotreeConverter writes
    /// `"scan angle rank"` as a signed byte of whole degrees for a legacy
    /// source and `"scan angle"` as an `int16` of 0.006° steps for a 1.4 one,
    /// and adds `"classification flags"` only for the second. Following it is
    /// what keeps a converted cloud readable by everything written against
    /// Potree — and, for a 1.4 source, stops the angle being rounded at all.
    pub all_legacy: bool,
    /// Whether any input carried GPS time.
    ///
    /// The canonical record always has the field — point formats 6, 7 and 8 all
    /// do — so this is the only place the answer survives. Potree and EPT
    /// declare their own attribute lists and can leave it out, which on a scan
    /// with no time is eight bytes of zeros per point, or a fifth of the file.
    pub any_gps_time: bool,
    pub extra: usize,
    pub extra_vlr: Vec<u8>,
    /// The `LASF_Projection` records the first input that declared any carried,
    /// verbatim.
    ///
    /// Copied rather than re-derived because this crate cannot synthesise WKT:
    /// turning `EPSG:26912` into a projection string needs the EPSG table, and
    /// that table is a separate opt-in package for the browser and nothing at
    /// all here. A file that declared its CRS in GeoTIFF keys would otherwise
    /// come out of the converter declaring nothing a machine can read —
    /// which is precisely the thing this project criticises PotreeConverter for.
    pub projection_vlrs: Vec<(u16, Vec<u8>)>,
    pub warnings: Vec<Warning>,
}

pub struct ConvertReport {
    pub scan: Scan,
    pub write: WriteReport,
    pub format: OutputFormat,
    pub warnings: Vec<Warning>,
    pub spilled: bool,
    pub seconds: f64,
}

/// Read every input's header.
///
/// Cheap — a few kilobytes per file — and it decides everything: the record to
/// write, the quantum, the cube, and whether the build fits in memory.
pub fn scan(inputs: &[PathBuf]) -> Result<Scan> {
    if inputs.is_empty() {
        return Err(Error::Unsupported("no input files".to_string()));
    }

    let mut out = Scan {
        point_count: 0,
        extent: Bounds::EMPTY,
        scale: [f64::INFINITY; 3],
        offset: [0.0; 3],
        crs: None,
        any_color: false,
        any_nir: false,
        any_gps_time: false,
        all_legacy: true,
        extra: 0,
        extra_vlr: Vec::new(),
        projection_vlrs: Vec::new(),
        warnings: Vec::new(),
    };
    let mut first_extra: Option<(usize, Vec<u8>)> = None;
    let mut extras_agree = true;

    for path in inputs {
        let cloud = crate::format::open_path(path)?;
        let info = cloud.info();

        // E57 has no LAS header to read facts off, and the facts it does
        // declare are not enough: a posed scan's bounds are not where its
        // points are, and its record count includes points that carry no
        // position. Both are settled by reading it, which is the cost of
        // converting a format that indexed nothing.
        #[cfg(feature = "e57")]
        if info.format == FormatId::E57 {
            let e57 = match &cloud {
                crate::format::Cloud::E57(c) => &c.e57,
                _ => unreachable!("the format was checked above"),
            };
            out.warnings.extend(e57.warnings.iter().cloned());
            let measured = crate::read::e57_points::measure(path)?;
            if measured.report.dropped > 0 {
                out.warnings.push(Warning::new(
                    "e57-invalid-points",
                    path.display().to_string(),
                    format!(
                        "{} of this file's {} points carry no position — a no-return in the scan \
                         grid — and are not converted.",
                        measured.report.dropped,
                        measured.report.dropped + measured.report.points
                    ),
                ));
            }
            out.point_count += measured.report.points;
            out.extent = out.extent.union(&measured.extent);
            // No quantum is declared: E57 stores floats, or integers with a
            // per-attribute scale. `choose_quantum` floors an unstated scale at
            // a millimetre, which is what this leaves it to do.
            out.any_color |= e57.has_color;
            // A scanner's E57 is not a legacy LAS file, so the output is
            // written with 1.4 field names rather than the 1.2 spellings.
            out.all_legacy = false;
            if out.crs.is_none() {
                out.crs = info.crs.clone();
            }
            continue;
        }

        if !matches!(info.format, FormatId::Las | FormatId::Copc) {
            return Err(Error::Unsupported(format!(
                "{} is {}. The converter reads LAS, LAZ, COPC and E57; a Potree or EPT cloud is \
                 already indexed, and re-indexing one is not implemented",
                path.display(),
                info.format.title()
            )));
        }
        let header = match &cloud {
            crate::format::Cloud::Las(las) => las.header.clone(),
            crate::format::Cloud::Copc(copc) => copc.header.clone(),
            _ => unreachable!("the format was checked above"),
        };

        out.point_count += header.point_count;
        out.extent = out.extent.union(&Bounds::new(header.min, header.max));
        for axis in 0..3 {
            if header.scale[axis] > 0.0 {
                out.scale[axis] = out.scale[axis].min(header.scale[axis]);
            }
        }
        out.any_color |= crate::las::point_format::las_format_has_color(header.point_format);
        out.any_nir |= matches!(header.point_format, 8 | 10);
        out.any_gps_time |=
            crate::las::point_format::las_format_has_gps_time(header.point_format);
        out.all_legacy &= header.point_format <= 5;

        if out.crs.is_none() {
            out.crs = info.crs.clone();
        } else if let Some(crs) = &info.crs {
            let same = out.crs.as_ref().map(|c| c.raw.as_str()) == Some(crs.raw.as_str());
            if !same {
                out.warnings.push(Warning::new(
                    "mixed-crs",
                    path.display().to_string(),
                    format!(
                        "This file declares {} and an earlier input declared {}. The output \
                         keeps the first, and the points are merged without reprojection.",
                        crs.label(),
                        out.crs.as_ref().map(Crs::label).unwrap_or_default()
                    ),
                ));
            }
        }

        if out.projection_vlrs.is_empty() {
            for record_id in [
                crate::las::crs::WKT_RECORD_ID,
                crate::las::crs::GEOKEY_DIRECTORY_RECORD_ID,
                crate::las::crs::GEOKEY_DOUBLE_RECORD_ID,
                crate::las::crs::GEOKEY_ASCII_RECORD_ID,
            ] {
                if let Some(vlr) = header
                    .vlrs
                    .iter()
                    .find(|v| v.is(crate::las::crs::PROJECTION_USER_ID, record_id))
                {
                    out.projection_vlrs.push((record_id, vlr.data.clone()));
                }
            }
        }

        // Extra dimensions travel as opaque bytes, so they can only be carried
        // when every input describes exactly the same ones.
        let base = crate::las::point_format::las_base_size(header.point_format)?;
        let extra_len = (header.point_size as usize).saturating_sub(base);
        let vlr = header
            .vlrs
            .iter()
            .find(|v| v.is("LASF_Spec", 4))
            .map(|v| v.data.clone())
            .unwrap_or_default();
        match &first_extra {
            None => first_extra = Some((extra_len, vlr)),
            Some((len, first)) => {
                if *len != extra_len || *first != vlr {
                    extras_agree = false;
                }
            }
        }
    }

    if let Some((len, vlr)) = first_extra {
        if extras_agree {
            out.extra = len;
            out.extra_vlr = vlr;
        } else if len > 0 {
            out.warnings.push(Warning::new(
                "extra-bytes-dropped",
                "inputs",
                "The inputs describe different extra dimensions, which cannot be merged \
                 into one record. They are dropped; every standard LAS field is kept."
                    .to_string(),
            ));
        }
    }

    if out.extent.is_empty() {
        return Err(Error::Unsupported(
            "the inputs declare no extent, so there is nothing to index".to_string(),
        ));
    }
    for axis in 0..3 {
        if !out.scale[axis].is_finite() || out.scale[axis] <= 0.0 {
            out.scale[axis] = 0.001;
        }
    }
    Ok(out)
}

/// Choose the quantum and origin the output stores positions at.
///
/// The quantum is the finest any input used: writing coarser would throw away
/// precision that is in the file, and finer would invent it.
///
/// The origin is the extent's minimum **verbatim**, which is also
/// PotreeConverter's choice and is the exact one. The minimum is itself a point
/// out of a source file, so every coordinate differs from it by a whole number
/// of that file's quanta and the division is exact. Snapping the origin to a
/// round multiple of the quantum instead — which looks tidier — shifts the
/// output grid by a fraction of a step and rounds every point in the cloud by
/// up to half of one.
///
/// The one thing that can force a change: an `i32` holds about 2.1 billion, so
/// an extent divided by the quantum has to fit. A 4 km survey at a micrometre
/// quantum does not, and the only honest options are to refuse or to coarsen —
/// this coarsens, and says so.
fn choose_quantum(scan: &Scan, requested: Option<[f64; 3]>) -> ([f64; 3], [f64; 3], Vec<Warning>) {
    let mut warnings = Vec::new();
    let mut scale = requested.unwrap_or(scan.scale);
    let size = scan.extent.size();

    for axis in 0..3 {
        if !scale[axis].is_finite() || scale[axis] <= 0.0 {
            scale[axis] = 0.001;
        }
        let needed = size[axis] / scale[axis];
        if needed > i32::MAX as f64 {
            let coarser = size[axis] / (i32::MAX as f64 * 0.9);
            warnings.push(Warning::new(
                "scale-coarsened",
                format!("scale[{axis}]"),
                format!(
                    "A quantum of {} over an extent of {:.1} needs {:.0} steps, past what a \
                     32-bit position holds. Coarsened to {coarser:.3e}.",
                    scale[axis], size[axis], needed
                ),
            ));
            scale[axis] = coarser;
        }
    }

    (scale, scan.extent.min, warnings)
}

/// Run a conversion.
///
/// `progress` is called with points read so far and the total the headers
/// promised.
pub fn convert(
    options: &ConvertOptions,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<ConvertReport> {
    let started = Instant::now();
    let scan = scan(&options.inputs)?;
    let (scale, offset, mut warnings) = choose_quantum(&scan, options.scale);
    warnings.extend(scan.warnings.clone());

    let format = output_format(scan.any_color, scan.any_nir);
    let extra_fields = if scan.extra_vlr.is_empty() {
        Vec::new()
    } else {
        // Parsed against the CANONICAL base, not the source's: the descriptors
        // describe the same dimensions, and their offsets have to point into
        // the record this pipeline actually moves.
        crate::las::extra_bytes::parse_extra_bytes(
            &scan.extra_vlr,
            crate::las::point_format::las_base_size(format)?,
        )
    };
    let layout = RecordLayout::new(format, scan.extra, scan.extra_vlr.clone(), extra_fields)?;
    let stride = layout.stride();

    let cube = indexing_cube(&scan.extent);
    let mut build = BuildOptions::new(cube, scale, offset);
    build.span = options.span.max(2);
    build.leaf_points = options.leaf_points.max(1);

    let write_options = WriteOptions {
        layout: layout.clone(),
        cube,
        extent: scan.extent,
        scale,
        offset,
        spacing: build.root_spacing(),
        span: build.span,
        has_gps_time: scan.any_gps_time,
        legacy_fields: scan.all_legacy,
        crs: scan.crs.clone(),
        projection_vlrs: scan.projection_vlrs.clone(),
        generator: options.generator.clone(),
        creation: crate::write::creation_today(),
    };

    // Whether the whole cloud fits. The estimate is exact for LAS input: the
    // headers state the count and the record is fixed width.
    let expected_bytes = scan.point_count.saturating_mul(stride as u64);
    let spilled = expected_bytes > options.memory_budget as u64;

    let mut writer = Writer::create(&options.output, options.format, write_options)?;

    if spilled {
        let scratch = options.scratch.clone().unwrap_or_else(|| {
            let parent = options
                .output
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            parent.join(".voxelkloud-convert")
        });
        let mut chunked = ChunkedBuild::new(
            build.clone(),
            stride,
            &scratch,
            expected_bytes,
            options.memory_budget,
        )?;
        stream(
            &options.inputs,
            &layout,
            scale,
            offset,
            scan.point_count,
            progress,
            &mut |records| chunked.distribute(records),
        )?;
        chunked.finish(&mut writer)?;
    } else {
        let mut all = Vec::with_capacity(expected_bytes as usize);
        stream(
            &options.inputs,
            &layout,
            scale,
            offset,
            scan.point_count,
            progress,
            &mut |records| {
                all.extend_from_slice(records);
                Ok(())
            },
        )?;
        build_subtree(all, OctreeKey::ROOT, stride, &build, &mut writer)?;
    }

    let (write, writer_warnings) = writer.finish()?;
    warnings.extend(writer_warnings);

    Ok(ConvertReport {
        scan,
        write,
        format: options.format,
        warnings,
        spilled,
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// Read every input, in canonical records, one batch at a time.
fn stream(
    inputs: &[PathBuf],
    layout: &RecordLayout,
    scale: [f64; 3],
    offset: [f64; 3],
    total: u64,
    progress: &mut dyn FnMut(u64, u64),
    sink: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<u64> {
    let mut done = 0u64;
    let mut buffer = Vec::new();

    for path in inputs {
        let mut source = open_source(path, layout.clone(), scale, offset)?;
        loop {
            buffer.clear();
            let got = source.next_batch(1 << 20, &mut buffer)?;
            if got == 0 {
                break;
            }
            sink(&buffer)?;
            done += got as u64;
            progress(done, total);
        }
    }
    Ok(done)
}

/// The reader for one input, decided by what the file is rather than by what
/// it is called.
///
/// A `.e57` renamed to `.bin` is still an E57, and the eight bytes that say so
/// cost one read. The LAS path is the fallback because LAS is the format that
/// declares itself furthest in — the `LASF` at byte 0 is checked by the header
/// reader either way.
fn open_source(
    path: &Path,
    layout: RecordLayout,
    scale: [f64; 3],
    offset: [f64; 3],
) -> Result<Box<dyn PointSource>> {
    #[cfg(feature = "e57")]
    {
        let mut head = [0u8; 8];
        use std::io::Read;
        let read = std::fs::File::open(path)?.read(&mut head)?;
        if crate::e57::is_e57(&head[..read]) {
            return Ok(Box::new(crate::read::e57_points::E57PointSource::open(
                path, layout, scale, offset,
            )?));
        }
    }
    Ok(Box::new(LasPointSource::open(path, layout, scale, offset)?))
}

/// The three writers behind one door, so the pipeline above has one shape.
enum Writer {
    Copc(Box<CopcWriter<std::io::BufWriter<std::fs::File>>>),
    Potree(Box<PotreeWriter>),
    Ept(Box<EptWriter>),
    Tileset(Box<TilesetWriter>),
}

impl Writer {
    fn create(path: &Path, format: OutputFormat, options: WriteOptions) -> Result<Self> {
        Ok(match format {
            OutputFormat::Copc => Self::Copc(Box::new(CopcWriter::create(path, options)?)),
            OutputFormat::PotreeV2(encoding) => Self::Potree(Box::new(
                PotreeWriter::create_with(path, options, encoding)?,
            )),
            OutputFormat::Ept(encoding) => {
                Self::Ept(Box::new(EptWriter::create(path, options, encoding)?))
            }
            OutputFormat::Tiles3D => Self::Tileset(Box::new(TilesetWriter::create(path, options)?)),
        })
    }

    fn finish(self) -> Result<(WriteReport, Vec<Warning>)> {
        Ok(match self {
            Self::Copc(writer) => (writer.finish()?.0, Vec::new()),
            Self::Potree(writer) => writer.finish()?,
            Self::Ept(writer) => (writer.finish()?, Vec::new()),
            Self::Tileset(writer) => (writer.finish()?, Vec::new()),
        })
    }
}

impl NodeSink for Writer {
    fn node(&mut self, node: crate::build::BuiltNode) -> Result<()> {
        match self {
            Self::Copc(writer) => writer.node(node),
            Self::Potree(writer) => writer.node(node),
            Self::Ept(writer) => writer.node(node),
            Self::Tileset(writer) => writer.node(node),
        }
    }
}
