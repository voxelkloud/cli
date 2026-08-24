//! The record the converter moves.
//!
//! Everything between reading a point and writing one happens in **LAS 1.4
//! record space**: point format 6, 7 or 8, positions as `i32` in the *output's*
//! scale and offset, extra bytes carried through verbatim. Not a struct of
//! fields, and not a columnar batch.
//!
//! That choice is the one that makes the rest of the converter small. A COPC
//! node is a laszip chunk of exactly these records, so the writer copies them
//! and compresses; the octree builder only needs the three integers at offset
//! 0, so it moves 36-byte blocks without knowing what is in them; and an input
//! in a legacy point format is converted once, on the way in, by the only code
//! that has to know the legacy layout at all.
//!
//! The alternative — a `Point` struct with an `Option<[u16; 3]>` for colour —
//! costs a branch per point per stage and cannot carry an extra dimension it
//! was not compiled to know about.

use crate::attribute::AttributeType;
use crate::error::{Error, Result};
use crate::las::point_format::{las_base_size, LasAccess};
use crate::las::{extra_bytes::ExtraByteField, LasHeader};

/// Where each field of a format 6/7/8 record sits. Fixed by the spec.
pub mod at {
    pub const X: usize = 0;
    pub const Y: usize = 4;
    pub const Z: usize = 8;
    pub const INTENSITY: usize = 12;
    /// Return number in the low four bits, number of returns in the high four.
    pub const RETURNS: usize = 14;
    /// Classification flags (4), scanner channel (2), scan direction, edge.
    pub const FLAGS: usize = 15;
    pub const CLASSIFICATION: usize = 16;
    pub const USER_DATA: usize = 17;
    /// Signed, in 0.006 degree increments.
    pub const SCAN_ANGLE: usize = 18;
    pub const POINT_SOURCE_ID: usize = 20;
    pub const GPS_TIME: usize = 22;
    pub const RGB: usize = 30;
    pub const NIR: usize = 36;
}

/// A legacy scan angle rank is whole degrees; the modern field counts 0.006 of
/// one. The spec fixes the conversion, and getting it wrong tilts every angle
/// by a factor of 167 without changing anything visible.
pub const SCAN_ANGLE_PER_DEGREE: f64 = 1.0 / 0.006;

/// The shape of one canonical record.
#[derive(Debug, Clone)]
pub struct RecordLayout {
    /// 6, 7 or 8.
    pub format: u8,
    /// Bytes before the extra dimensions.
    pub base: usize,
    /// Bytes of extra dimensions, carried through untouched.
    pub extra: usize,
    /// The Extra Bytes VLR describing them, to be written out verbatim.
    pub extra_vlr: Vec<u8>,
    pub extra_fields: Vec<ExtraByteField>,
}

impl RecordLayout {
    pub fn new(format: u8, extra: usize, extra_vlr: Vec<u8>, extra_fields: Vec<ExtraByteField>) -> Result<Self> {
        if !(6..=8).contains(&format) {
            return Err(Error::Unsupported(format!(
                "the converter writes point formats 6, 7 and 8; {format} was asked for"
            )));
        }
        Ok(Self {
            format,
            base: las_base_size(format)?,
            extra,
            extra_vlr,
            extra_fields,
        })
    }

    pub fn stride(&self) -> usize {
        self.base + self.extra
    }

    pub fn has_color(&self) -> bool {
        self.format == 7 || self.format == 8
    }

    pub fn has_nir(&self) -> bool {
        self.format == 8
    }
}

/// Read the three position integers.
#[inline]
pub fn position(record: &[u8]) -> [i32; 3] {
    [
        i32::from_le_bytes(record[at::X..at::X + 4].try_into().unwrap()),
        i32::from_le_bytes(record[at::Y..at::Y + 4].try_into().unwrap()),
        i32::from_le_bytes(record[at::Z..at::Z + 4].try_into().unwrap()),
    ]
}

/// Write the three position integers.
#[inline]
pub fn set_position(record: &mut [u8], value: [i32; 3]) {
    record[at::X..at::X + 4].copy_from_slice(&value[0].to_le_bytes());
    record[at::Y..at::Y + 4].copy_from_slice(&value[1].to_le_bytes());
    record[at::Z..at::Z + 4].copy_from_slice(&value[2].to_le_bytes());
}

/// Quantize an absolute coordinate to a record integer.
///
/// Rounds rather than truncates. Truncation biases every coordinate toward
/// zero by up to half a quantum, which on a 0.01 m scale is 5 mm of systematic
/// shift — small, uniform, and exactly the kind of thing that never gets
/// noticed and never stops being wrong.
#[inline]
pub fn quantize(value: f64, scale: f64, offset: f64) -> i32 {
    let raw = ((value - offset) / scale).round();
    raw.clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

#[inline]
pub fn dequantize(value: i32, scale: f64, offset: f64) -> f64 {
    f64::from(value) * scale + offset
}

/// How to turn one source record into a canonical one.
///
/// Built once per input file. The work it captures is the legacy-to-1.4 field
/// mapping, which is four rules and every one of them is a silent failure if it
/// is missed.
pub struct RecordConverter {
    source_format: u8,
    source_stride: usize,
    /// Where the source keeps its extra dimensions, and how many bytes.
    source_extra: (usize, usize),
    source_scale: [f64; 3],
    source_offset: [f64; 3],
    out_scale: [f64; 3],
    out_offset: [f64; 3],
    layout: RecordLayout,
    /// Field offsets in the source, resolved once.
    src: SourceOffsets,
}

struct SourceOffsets {
    legacy: bool,
    gps_time: Option<usize>,
    rgb: Option<usize>,
    nir: Option<usize>,
}

impl RecordConverter {
    pub fn new(
        header: &LasHeader,
        layout: RecordLayout,
        out_scale: [f64; 3],
        out_offset: [f64; 3],
    ) -> Result<Self> {
        Self::from_parts(
            header.point_format,
            header.point_size as usize,
            header.scale,
            header.offset,
            layout,
            out_scale,
            out_offset,
        )
    }

    /// The same, without a header.
    ///
    /// A browser build holds the records and the numbers that describe them but
    /// not the file they came out of — the header was parsed, used and dropped
    /// before the octree existed.
    pub fn from_parts(
        format: u8,
        stride: usize,
        source_scale: [f64; 3],
        source_offset: [f64; 3],
        layout: RecordLayout,
        out_scale: [f64; 3],
        out_offset: [f64; 3],
    ) -> Result<Self> {
        let base = las_base_size(format)?;
        if stride < base {
            return Err(Error::not_format(
                "a LAS file",
                format!(
                    "the header declares a {stride}-byte record for point format {format}, \
                     which needs {base}"
                ),
            ));
        }
        let legacy = format <= 5;
        let dimensions = crate::las::point_format::las_dimensions(format)?;
        let find = |name: &str| {
            dimensions.iter().find(|d| d.name == name).map(|d| match d.access {
                LasAccess::Scalar { at } => at,
                LasAccess::Bits { at, .. } => at,
            })
        };

        Ok(Self {
            source_format: format,
            source_stride: stride,
            source_extra: (base, stride - base),
            source_scale,
            source_offset,
            out_scale,
            out_offset,
            layout,
            src: SourceOffsets {
                legacy,
                gps_time: find("gps-time"),
                rgb: find("rgb"),
                nir: find("nir"),
            },
        })
    }

    pub fn source_stride(&self) -> usize {
        self.source_stride
    }

    pub fn layout(&self) -> &RecordLayout {
        &self.layout
    }

    /// Convert `count` source records into canonical ones, appended to `out`.
    pub fn convert_many(&self, source: &[u8], out: &mut Vec<u8>) {
        let stride = self.layout.stride();
        for record in source.chunks_exact(self.source_stride) {
            let start = out.len();
            out.resize(start + stride, 0);
            self.convert_one(record, &mut out[start..start + stride]);
        }
    }

    fn convert_one(&self, source: &[u8], out: &mut [u8]) {
        // Position: through absolute coordinates, because the input's quantum
        // and origin are its own and the output's are the output's. Going
        // integer-to-integer would only be exact when the two agree, and would
        // be wrong by a scale factor when they do not.
        let raw = position(source);
        set_position(
            out,
            [
                quantize(
                    dequantize(raw[0], self.source_scale[0], self.source_offset[0]),
                    self.out_scale[0],
                    self.out_offset[0],
                ),
                quantize(
                    dequantize(raw[1], self.source_scale[1], self.source_offset[1]),
                    self.out_scale[1],
                    self.out_offset[1],
                ),
                quantize(
                    dequantize(raw[2], self.source_scale[2], self.source_offset[2]),
                    self.out_scale[2],
                    self.out_offset[2],
                ),
            ],
        );

        out[at::INTENSITY..at::INTENSITY + 2].copy_from_slice(&source[12..14]);

        if self.src.legacy {
            // Legacy packs three bits of return number and three of return
            // count; 1.4 gives each four. Copying the byte would put the
            // return count in the classification flags.
            let bits = source[14];
            let return_number = bits & 0b111;
            let number_of_returns = (bits >> 3) & 0b111;
            out[at::RETURNS] = return_number | (number_of_returns << 4);

            // Legacy keeps the classification in the low five bits of a byte
            // whose top three are the synthetic, key-point and withheld flags.
            // 1.4 gives classification the whole byte and moves the flags. A
            // straight copy turns a withheld ground point (class 2, bit 7) into
            // class 130.
            let classification = source[15];
            out[at::CLASSIFICATION] = classification & 0b0001_1111;
            let scan_direction = (bits >> 6) & 1;
            let edge = (bits >> 7) & 1;
            out[at::FLAGS] = ((classification >> 5) & 0b111) | (scan_direction << 6) | (edge << 7);

            // Whole degrees to 0.006-degree increments.
            let rank = source[16] as i8;
            let angle = (f64::from(rank) * SCAN_ANGLE_PER_DEGREE).round() as i16;
            out[at::SCAN_ANGLE..at::SCAN_ANGLE + 2].copy_from_slice(&angle.to_le_bytes());

            out[at::USER_DATA] = source[17];
            out[at::POINT_SOURCE_ID..at::POINT_SOURCE_ID + 2].copy_from_slice(&source[18..20]);
        } else {
            out[at::RETURNS] = source[14];
            out[at::FLAGS] = source[15];
            out[at::CLASSIFICATION] = source[16];
            out[at::USER_DATA] = source[17];
            out[at::SCAN_ANGLE..at::SCAN_ANGLE + 2].copy_from_slice(&source[18..20]);
            out[at::POINT_SOURCE_ID..at::POINT_SOURCE_ID + 2].copy_from_slice(&source[20..22]);
        }

        // Formats 0 and 2 carry no time. Zero is what a file with no GPS time
        // says, and every reader treats it as such.
        if let Some(at_gps) = self.src.gps_time {
            out[at::GPS_TIME..at::GPS_TIME + 8].copy_from_slice(&source[at_gps..at_gps + 8]);
        }

        if self.layout.has_color() {
            match self.src.rgb {
                Some(at_rgb) => out[at::RGB..at::RGB + 6].copy_from_slice(&source[at_rgb..at_rgb + 6]),
                // An input with no colour, merged with one that has it. White
                // rather than black: a black point reads as a shadow, and the
                // difference between "no colour here" and "this is dark" would
                // be invisible.
                None => {
                    for i in 0..3 {
                        out[at::RGB + i * 2..at::RGB + i * 2 + 2]
                            .copy_from_slice(&u16::MAX.to_le_bytes());
                    }
                }
            }
        }
        if self.layout.has_nir() {
            if let Some(at_nir) = self.src.nir {
                out[at::NIR..at::NIR + 2].copy_from_slice(&source[at_nir..at_nir + 2]);
            }
        }

        // Extra dimensions are bytes whose meaning lives in a VLR this code
        // never interprets. Copying them is the only correct thing to do —
        // and only when the two files describe the same ones, which the
        // caller has already checked.
        let (src_at, src_len) = self.source_extra;
        let copy = src_len.min(self.layout.extra);
        if copy > 0 {
            out[self.layout.base..self.layout.base + copy]
                .copy_from_slice(&source[src_at..src_at + copy]);
        }
    }

    pub fn source_format(&self) -> u8 {
        self.source_format
    }
}

/// The point format that can hold everything the inputs carry.
///
/// Always 6 or above: the legacy formats cap the return number at three bits
/// and the classification at five, so writing one would silently narrow data
/// that arrived wider.
pub fn output_format(any_color: bool, any_nir: bool) -> u8 {
    match (any_color, any_nir) {
        (_, true) => 8,
        (true, false) => 7,
        (false, false) => 6,
    }
}

/// The neutral attribute list for a canonical record, for a manifest to state.
pub fn layout_attributes(layout: &RecordLayout, bounds: crate::bounds::Bounds) -> Vec<crate::attribute::Attribute> {
    let options = crate::las::layout::LasLayoutOptions {
        format: layout.format,
        point_size: layout.stride(),
        extra_bytes: (!layout.extra_vlr.is_empty()).then_some(layout.extra_vlr.as_slice()),
        bounds,
        gps_time_range: None,
    };
    crate::las::layout::las_layout(&options)
        .map(|l| l.plain())
        .unwrap_or_default()
}

/// The width one attribute type occupies, for a writer laying out a record.
pub fn type_size(kind: AttributeType) -> usize {
    kind.size()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_rounds_rather_than_truncates() {
        // 0.014 at a 0.01 quantum is one and a bit units. Truncation gives 1
        // and rounding gives 1; the difference shows at 0.016, where
        // truncation still gives 1 and rounding gives 2.
        assert_eq!(quantize(0.016, 0.01, 0.0), 2);
        assert_eq!(quantize(-0.016, 0.01, 0.0), -2);
        assert_eq!(quantize(100.0, 0.01, 50.0), 5000);
    }

    #[test]
    fn quantize_round_trips_within_half_a_quantum() {
        for value in [0.0, 1.234, -987.654, 1e6 + 0.005] {
            let q = quantize(value, 0.001, 0.0);
            let back = dequantize(q, 0.001, 0.0);
            assert!((back - value).abs() <= 0.0005 + 1e-9, "{value} -> {q} -> {back}");
        }
    }

    #[test]
    fn the_output_format_widens_and_never_narrows() {
        assert_eq!(output_format(false, false), 6);
        assert_eq!(output_format(true, false), 7);
        assert_eq!(output_format(true, true), 8);
        assert_eq!(output_format(false, true), 8);
    }
}
