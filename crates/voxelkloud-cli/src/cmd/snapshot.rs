//! A thumbnail of a cloud, with no GPU and no browser.
//!
//! Streams the points once through the same reader `convert` uses and
//! software-rasterises an orthographic isometric view into a PNG: z-buffer,
//! 2x2 splats, the file's own RGB when it has any and an elevation ramp when
//! it does not. Exists because every hosting UI needs a card image and
//! headless browsers cannot be relied on for WebGPU — this needs a file and a
//! CPU, nothing else.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use clap::Args as ClapArgs;
use voxelkloud_io::convert::{self};
use voxelkloud_io::error::{Error, Result};
use voxelkloud_io::read::las_points::LasPointSource;
use voxelkloud_io::read::PointSource;
use voxelkloud_io::record::{self, RecordLayout};

use crate::out::{count, Output};

#[derive(ClapArgs)]
pub struct Args {
    /// A LAS, LAZ, COPC or E57 file.
    pub input: PathBuf,

    /// Where to write the PNG.
    #[arg(long, short)]
    pub output: PathBuf,

    /// Image size in pixels (square).
    #[arg(long, default_value_t = 640)]
    pub size: u32,

    /// At most this many points are rasterised; denser inputs are decimated.
    #[arg(long, default_value_t = 1_500_000)]
    pub points: u64,

    /// View azimuth in degrees.
    #[arg(long, default_value_t = 225.0)]
    pub azimuth: f64,

    /// View elevation in degrees above the horizon.
    #[arg(long, default_value_t = 35.0)]
    pub elevation: f64,
}

/// Elevation ramp: deep blue -> teal -> warm yellow, two lerped segments.
fn ramp(t: f32) -> [u8; 3] {
    const STOPS: [[f32; 3]; 3] = [[45.0, 50.0, 110.0], [55.0, 180.0, 140.0], [242.0, 220.0, 90.0]];
    let t = t.clamp(0.0, 1.0) * 2.0;
    let (a, b, f) = if t < 1.0 { (STOPS[0], STOPS[1], t) } else { (STOPS[1], STOPS[2], t - 1.0) };
    [
        (a[0] + (b[0] - a[0]) * f) as u8,
        (a[1] + (b[1] - a[1]) * f) as u8,
        (a[2] + (b[2] - a[2]) * f) as u8,
    ]
}

/// How far to shift a source's u16 colour channels down to bytes.
///
/// ONE decision for the whole cloud, taken only once every point has been seen,
/// because it is not knowable per point: LAS declares RGB as u16 and files
/// disagree about what that means. A source whose widest channel fits in a byte
/// was never 16-bit, and shifting it throws the entire picture away — that is
/// not a subtle loss, it renders black. The elevation ramp already produces
/// bytes and lands in the `0` branch by construction.
fn narrow_shift(has_color: bool, max_channel: u16) -> u32 {
    if has_color && max_channel > 255 {
        8
    } else {
        0
    }
}

pub fn run(args: &Args, out: &Output) -> Result<bool> {
    let scan = convert::scan(std::slice::from_ref(&args.input))?;
    let size = args.size.clamp(16, 4096) as usize;

    // The canonical record: 7 carries RGB, 6 does not. Same choice convert
    // makes, minus NIR — a thumbnail has no use for it.
    let layout = RecordLayout::new(if scan.any_color { 7 } else { 6 }, 0, Vec::new(), Vec::new())?;
    let has_color = layout.has_color();
    let stride = layout.stride();

    let mut source: Box<dyn PointSource> = {
        // Same sniff as convert's open_source: an E57 renamed `.las` is
        // still an E57.
        let mut head = [0u8; 8];
        use std::io::Read;
        let read = File::open(&args.input)?.read(&mut head)?;
        if voxelkloud_io::e57::is_e57(&head[..read]) {
            Box::new(voxelkloud_io::read::e57_points::E57PointSource::open(
                &args.input,
                layout.clone(),
                scan.scale,
                scan.offset,
            )?)
        } else {
            Box::new(LasPointSource::open(&args.input, layout.clone(), scan.scale, scan.offset)?)
        }
    };

    // View basis: azimuth around Z (LAS is Z-up), then tilt by elevation.
    let az = args.azimuth.to_radians();
    let el = args.elevation.to_radians();
    let (sin_a, cos_a) = az.sin_cos();
    let (sin_e, cos_e) = el.sin_cos();
    let center = [
        (scan.extent.min[0] + scan.extent.max[0]) / 2.0,
        (scan.extent.min[1] + scan.extent.max[1]) / 2.0,
        (scan.extent.min[2] + scan.extent.max[2]) / 2.0,
    ];
    let project = |p: [f64; 3]| -> (f64, f64, f64) {
        let (x, y, z) = (p[0] - center[0], p[1] - center[1], p[2] - center[2]);
        let sx = -sin_a * x + cos_a * y;
        let sy = (-cos_a * x - sin_a * y) * sin_e + z * cos_e;
        let depth = (cos_a * x + sin_a * y) * cos_e + z * sin_e;
        (sx, sy, depth)
    };

    // Fit the projected extent corners into the image with a 5% margin.
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for cx in [scan.extent.min[0], scan.extent.max[0]] {
        for cy in [scan.extent.min[1], scan.extent.max[1]] {
            for cz in [scan.extent.min[2], scan.extent.max[2]] {
                let (sx, sy, _) = project([cx, cy, cz]);
                min_x = min_x.min(sx);
                max_x = max_x.max(sx);
                min_y = min_y.min(sy);
                max_y = max_y.max(sy);
            }
        }
    }
    let span = (max_x - min_x).max(max_y - min_y).max(1e-9);
    let px_per_unit = size as f64 * 0.9 / span;
    let to_px = |sx: f64, sy: f64| -> (i64, i64) {
        (
            ((sx - (min_x + max_x) / 2.0) * px_per_unit + size as f64 / 2.0) as i64,
            (-(sy - (min_y + max_y) / 2.0) * px_per_unit + size as f64 / 2.0) as i64,
        )
    };

    let z_min = scan.extent.min[2];
    let z_span = (scan.extent.max[2] - z_min).max(1e-9);
    let keep_every = (scan.point_count / args.points.max(1)).max(1);

    let mut zbuf = vec![f32::NEG_INFINITY; size * size];
    // Channels are kept at their SOURCE width until the whole cloud has been
    // seen, because how to narrow them is not knowable per point. See
    // `max_channel` below.
    let mut chan = vec![0u16; size * size * 3];
    let mut hit = vec![false; size * size];
    let mut max_channel: u16 = 0;
    let mut buffer: Vec<u8> = Vec::new();
    let mut seen: u64 = 0;
    let mut drawn: u64 = 0;
    loop {
        buffer.clear();
        let got = source.next_batch(65_536, &mut buffer)?;
        if got == 0 {
            break;
        }
        for i in 0..got {
            seen += 1;
            if keep_every > 1 && seen % keep_every != 0 {
                continue;
            }
            let rec = &buffer[i * stride..(i + 1) * stride];
            let q = record::position(rec);
            let world = [
                record::dequantize(q[0], scan.scale[0], scan.offset[0]),
                record::dequantize(q[1], scan.scale[1], scan.offset[1]),
                record::dequantize(q[2], scan.scale[2], scan.offset[2]),
            ];
            let (sx, sy, depth) = project(world);
            let (px, py) = to_px(sx, sy);

            let color: [u16; 3] = if has_color {
                // LAS declares RGB as three u16 channels, and files disagree
                // about what that means: some fill 0..65535, many write 0..255
                // into the same fields. Narrowing with a fixed `>> 8` renders
                // the second kind ENTIRELY BLACK — geometry correct, every
                // colour zero — which is what this produced on a 241M-point
                // Rotterdam survey before the widest channel decided it.
                let r = u16::from_le_bytes([rec[record::at::RGB], rec[record::at::RGB + 1]]);
                let g = u16::from_le_bytes([rec[record::at::RGB + 2], rec[record::at::RGB + 3]]);
                let b = u16::from_le_bytes([rec[record::at::RGB + 4], rec[record::at::RGB + 5]]);
                max_channel = max_channel.max(r).max(g).max(b);
                [r, g, b]
            } else {
                let c = ramp(((world[2] - z_min) / z_span) as f32);
                [c[0] as u16, c[1] as u16, c[2] as u16]
            };

            // 2x2 splat, z-buffered. Larger depth is closer to the camera.
            for dy in 0..2i64 {
                for dx in 0..2i64 {
                    let (x, y) = (px + dx, py + dy);
                    if x < 0 || y < 0 || x >= size as i64 || y >= size as i64 {
                        continue;
                    }
                    let at = y as usize * size + x as usize;
                    if (depth as f32) > zbuf[at] {
                        zbuf[at] = depth as f32;
                        let o = at * 3;
                        chan[o] = color[0];
                        chan[o + 1] = color[1];
                        chan[o + 2] = color[2];
                        hit[at] = true;
                    }
                }
            }
            drawn += 1;
        }
    }

    let shift = narrow_shift(has_color, max_channel);
    let mut rgba = vec![0u8; size * size * 4];
    for at in 0..size * size {
        if !hit[at] {
            continue;
        }
        let (o, q) = (at * 4, at * 3);
        rgba[o] = (chan[q] >> shift) as u8;
        rgba[o + 1] = (chan[q + 1] >> shift) as u8;
        rgba[o + 2] = (chan[q + 2] >> shift) as u8;
        rgba[o + 3] = 255;
    }

    let file = File::create(&args.output)?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), size as u32, size as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;
    writer
        .write_image_data(&rgba)
        .map_err(|e| Error::Io(std::io::Error::other(e)))?;

    out.heading(&format!("wrote {}", args.output.display()));
    out.field("points", count(drawn));
    out.field("size", &format!("{size} px"));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::narrow_shift;

    /// REGRESSION. A 241M-point Rotterdam survey stores 0..255 in the u16 RGB
    /// fields, and a fixed `>> 8` rendered every one of its points black: the
    /// silhouette was correct and the colour was gone. Nothing covered this
    /// command, which is how it shipped that far.
    #[test]
    fn byte_ranged_sources_are_not_shifted() {
        assert_eq!(narrow_shift(true, 255), 0);
        assert_eq!(narrow_shift(true, 200), 0);
        assert_eq!(narrow_shift(true, 1), 0);
    }

    #[test]
    fn full_range_sources_are_shifted_to_bytes() {
        assert_eq!(narrow_shift(true, 256), 8);
        assert_eq!(narrow_shift(true, 65_535), 8);
    }

    /// The ramp path writes bytes already, so it must never be narrowed no
    /// matter what the (unused) channel maximum happens to hold.
    #[test]
    fn the_elevation_ramp_is_never_shifted() {
        assert_eq!(narrow_shift(false, 0), 0);
        assert_eq!(narrow_shift(false, 65_535), 0);
    }

    /// An all-black cloud is legal and must not be mistaken for a byte-ranged
    /// one in a way that changes the answer: both give 0, and 0 is right.
    #[test]
    fn a_black_cloud_is_left_alone() {
        assert_eq!(narrow_shift(true, 0), 0);
    }
}
