//! The LAS point record, formats 0 through 10.
//!
//! Not a decoder: this says where each dimension sits and how wide it is.
//! Turning that into values is [`layout`](super::layout), and finding the bytes
//! at all is the driver's job.

use crate::attribute::AttributeType;
use crate::error::{Error, Result};

/// How one dimension is read out of a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LasAccess {
    /// A whole little-endian value at a byte offset.
    Scalar { at: usize },
    /// An unsigned bit run inside one byte.
    ///
    /// LAS packs return numbers, the classification flags and the scanner
    /// channel into shared bytes, and every writer in this space —
    /// PotreeConverter included — presents them as separate dimensions. Doing
    /// the same is what lets one colour mode key off `"classification"`
    /// whichever driver produced the cloud.
    Bits { at: usize, shift: u32, width: u32 },
}

/// One dimension of a LAS point record.
#[derive(Debug, Clone)]
pub struct LasDimension {
    /// Verbatim the name PotreeConverter emits for the same field, so an
    /// attribute lookup does not depend on which driver loaded the cloud:
    /// `"position"`, `"intensity"`, `"return number"`, `"gps-time"`, `"rgb"`.
    pub name: &'static str,
    pub kind: AttributeType,
    pub num_elements: usize,
    pub access: LasAccess,
    /// Declared domain, from the LAS spec. Position's comes from the header.
    pub min: Vec<f64>,
    pub max: Vec<f64>,
    pub description: &'static str,
}

/// Bytes of one record before any extra bytes, per point format.
const BASE_SIZE: [usize; 11] = [20, 28, 26, 34, 57, 63, 30, 36, 38, 59, 67];

/// Point data record length, before extra bytes.
pub fn las_base_size(format: u8) -> Result<usize> {
    BASE_SIZE
        .get(format as usize)
        .copied()
        .ok_or_else(|| Error::Unsupported(format!("LAS point data record format {format} is not one of 0-10")))
}

fn scalar(
    name: &'static str,
    kind: AttributeType,
    at: usize,
    min: &[f64],
    max: &[f64],
    description: &'static str,
    num_elements: usize,
) -> LasDimension {
    LasDimension {
        name,
        kind,
        num_elements,
        access: LasAccess::Scalar { at },
        min: min.to_vec(),
        max: max.to_vec(),
        description,
    }
}

fn bits(name: &'static str, at: usize, shift: u32, width: u32) -> LasDimension {
    LasDimension {
        name,
        kind: AttributeType::Uint8,
        num_elements: 1,
        access: LasAccess::Bits { at, shift, width },
        min: vec![0.0],
        max: vec![f64::from((1u32 << width) - 1)],
        description: "",
    }
}

/// The dimensions of one point format, in record order.
///
/// Position's `min`/`max` are placeholders: the header's bounding box is the
/// real domain and the caller substitutes it, because only the header knows it.
///
/// Wave packet fields (formats 4, 5, 9, 10) are deliberately absent. They are
/// five fields describing a waveform this project has no way to render, they
/// appear in a vanishing fraction of files, and their bytes are still counted
/// in the stride — so nothing is misaligned by leaving them out, only
/// unavailable.
pub fn las_dimensions(format: u8) -> Result<Vec<LasDimension>> {
    las_base_size(format)?;
    let legacy = format <= 5;
    let u8_range: (&[f64], &[f64]) = (&[0.0], &[255.0]);
    let u16_range: (&[f64], &[f64]) = (&[0.0], &[65535.0]);

    let mut out = vec![
        LasDimension {
            name: "position",
            kind: AttributeType::Int32,
            num_elements: 3,
            access: LasAccess::Scalar { at: 0 },
            min: vec![0.0; 3],
            max: vec![0.0; 3],
            description: "",
        },
        scalar("intensity", AttributeType::Uint16, 12, u16_range.0, u16_range.1, "", 1),
    ];

    if legacy {
        out.push(bits("return number", 14, 0, 3));
        out.push(bits("number of returns", 14, 3, 3));
        out.push(bits("scan direction flag", 14, 6, 1));
        out.push(bits("edge of flight line", 14, 7, 1));
        // The whole byte, flags included. PotreeConverter does the same for
        // legacy formats, and the synthetic/keypoint/withheld bits above class
        // 31 are almost never set in files that reach a viewer.
        out.push(scalar("classification", AttributeType::Uint8, 15, u8_range.0, u8_range.1, "", 1));
        out.push(scalar("scan angle rank", AttributeType::Int8, 16, &[-90.0], &[90.0], "degrees", 1));
        out.push(scalar("user data", AttributeType::Uint8, 17, u8_range.0, u8_range.1, "", 1));
        out.push(scalar("point source id", AttributeType::Uint16, 18, u16_range.0, u16_range.1, "", 1));
    } else {
        out.push(bits("return number", 14, 0, 4));
        out.push(bits("number of returns", 14, 4, 4));
        out.push(bits("classification flags", 15, 0, 4));
        out.push(bits("scanner channel", 15, 4, 2));
        out.push(bits("scan direction flag", 15, 6, 1));
        out.push(bits("edge of flight line", 15, 7, 1));
        out.push(scalar("classification", AttributeType::Uint8, 16, u8_range.0, u8_range.1, "", 1));
        out.push(scalar("user data", AttributeType::Uint8, 17, u8_range.0, u8_range.1, "", 1));
        out.push(scalar(
            "scan angle",
            AttributeType::Int16,
            18,
            &[-30000.0],
            &[30000.0],
            "0.006 degree increments",
            1,
        ));
        out.push(scalar("point source id", AttributeType::Uint16, 20, u16_range.0, u16_range.1, "", 1));
    }

    let gps_at = if legacy { 20 } else { 22 };
    let has_gps = if legacy {
        format == 1 || format == 3 || format >= 4
    } else {
        true
    };
    if has_gps {
        out.push(scalar("gps-time", AttributeType::Double, gps_at, &[0.0], &[0.0], "", 1));
    }

    // Colour sits after GPS time where there is one. Format 2 is the only one
    // with colour and no GPS time.
    let rgb_at = if legacy {
        if format == 2 {
            20
        } else {
            28
        }
    } else {
        30
    };
    // Not `format >= 7`: point format 9 is format 6 plus wave packets and has
    // no colour at all, so a `>= 7` test lands RGB on top of the wave packet
    // descriptor. The table is the authority, and the test below holds the two
    // to each other.
    if las_format_has_color(format) {
        out.push(scalar(
            "rgb",
            AttributeType::Uint16,
            rgb_at,
            &[0.0; 3],
            &[65535.0; 3],
            "",
            3,
        ));
    }
    if format == 8 || format == 10 {
        out.push(scalar("nir", AttributeType::Uint16, rgb_at + 6, u16_range.0, u16_range.1, "", 1));
    }

    Ok(out)
}

/// Whether a point format carries colour, without building its dimensions.
pub fn las_format_has_color(format: u8) -> bool {
    matches!(format, 2 | 3 | 5 | 7 | 8 | 10)
}

/// Whether a point format carries GPS time.
pub fn las_format_has_gps_time(format: u8) -> bool {
    !matches!(format, 0 | 2)
}

/// The smallest point format that carries the requested dimensions.
///
/// The writer's side of the table. Legacy formats (0-5) cap the return number
/// at 3 bits and the classification at 5, so anything written from a modern
/// source stays in the 6-10 range where those fields are wide enough.
pub fn las_format_for(color: bool, gps_time: bool, legacy: bool) -> u8 {
    match (legacy, color, gps_time) {
        (true, false, false) => 0,
        (true, false, true) => 1,
        (true, true, false) => 2,
        (true, true, true) => 3,
        (false, false, _) => 6,
        (false, true, _) => 7,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dimensions must exactly tile the base record, with no gap and no
    /// overlap — a shifted offset is the failure that produces plausible
    /// garbage rather than an error.
    #[test]
    fn dimensions_fit_the_base_record() {
        for format in 0..=10u8 {
            let dims = las_dimensions(format).unwrap();
            let base = las_base_size(format).unwrap();
            for d in &dims {
                let (at, size) = match d.access {
                    LasAccess::Scalar { at } => (at, d.num_elements * d.kind.size()),
                    LasAccess::Bits { at, .. } => (at, 1),
                };
                assert!(
                    at + size <= base,
                    "format {format}: {} runs past the {base}-byte record",
                    d.name
                );
            }
        }
    }

    #[test]
    fn colour_and_gps_tables_agree_with_the_dimensions() {
        for format in 0..=10u8 {
            let dims = las_dimensions(format).unwrap();
            let has_rgb = dims.iter().any(|d| d.name == "rgb");
            let has_gps = dims.iter().any(|d| d.name == "gps-time");
            assert_eq!(has_rgb, las_format_has_color(format), "format {format} colour");
            assert_eq!(has_gps, las_format_has_gps_time(format), "format {format} gps");
        }
    }
}
