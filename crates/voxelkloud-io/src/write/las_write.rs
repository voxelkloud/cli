//! The LAS 1.4 public header block and the record directories, written.
//!
//! Every offset here is fixed by the spec and every one of them is a silent
//! failure if it is wrong: a header that misplaces `offset_to_point_data` by
//! four bytes produces a file that opens, reports a plausible point count, and
//! decodes noise. The layout is stated once, as constants, and the writer and
//! `crate::las`'s reader are held to each other by a round-trip test.

use std::io::{Seek, SeekFrom, Write};

use crate::error::Result;

/// Bytes of a LAS 1.4 public header block.
pub const HEADER_SIZE: usize = 375;
/// Bytes of a VLR record header, before its payload.
pub const VLR_HEADER_SIZE: usize = 54;
/// Same, with a 64-bit length.
pub const EVLR_HEADER_SIZE: usize = 60;

/// Global encoding bit 4: the CRS is WKT rather than GeoTIFF keys.
///
/// Required for point formats 6 and up. A file that omits it is telling a
/// reader to look for a GeoTIFF key directory that a 1.4 writer never wrote.
pub const GLOBAL_ENCODING_WKT: u16 = 0b1_0000;

/// One record to be written, header and payload.
pub struct OutVlr {
    pub user_id: String,
    pub record_id: u16,
    pub description: String,
    pub data: Vec<u8>,
}

impl OutVlr {
    pub fn new(
        user_id: &str,
        record_id: u16,
        description: &str,
        data: Vec<u8>,
    ) -> Self {
        Self {
            user_id: user_id.to_string(),
            record_id,
            description: description.to_string(),
            data,
        }
    }

    pub fn size(&self) -> usize {
        VLR_HEADER_SIZE + self.data.len()
    }

    pub fn extended_size(&self) -> u64 {
        EVLR_HEADER_SIZE as u64 + self.data.len() as u64
    }
}

/// The header fields a writer decides.
#[derive(Debug, Clone)]
pub struct OutHeader {
    pub point_format: u8,
    pub point_size: u16,
    /// Set when the points are laszip-compressed.
    pub compressed: bool,
    pub point_count: u64,
    pub scale: [f64; 3],
    pub offset: [f64; 3],
    pub min: [f64; 3],
    pub max: [f64; 3],
    pub offset_to_point_data: u32,
    pub vlr_count: u32,
    pub evlr_offset: u64,
    pub evlr_count: u32,
    pub generator: String,
    /// Whether the file declares its CRS as WKT.
    ///
    /// Global encoding bit 4, which point formats 6 and up are supposed to set
    /// always. It is set here only when a WKT record is actually written: a
    /// file whose only projection is a GeoTIFF key directory — carried through
    /// from a LAS 1.2 source, because this crate cannot turn a code into WKT —
    /// would otherwise tell every reader to look for a record that is not
    /// there.
    pub wkt: bool,
    /// Day of year and year, as the spec spells a creation date.
    pub creation: (u16, u16),
    /// Points by return, 15 slots. Zeros when unknown.
    pub points_by_return: [u64; 15],
}

impl OutHeader {
    /// Serialize the 375-byte block.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = vec![0u8; HEADER_SIZE];
        b[0..4].copy_from_slice(b"LASF");
        write_u16(&mut b, 6, if self.wkt { GLOBAL_ENCODING_WKT } else { 0 });
        b[24] = 1;
        b[25] = 4;
        write_fixed(&mut b, 26, 32, "voxelkloud");
        write_fixed(&mut b, 58, 32, &self.generator);
        write_u16(&mut b, 90, self.creation.0);
        write_u16(&mut b, 92, self.creation.1);
        write_u16(&mut b, 94, HEADER_SIZE as u16);
        write_u32(&mut b, 96, self.offset_to_point_data);
        write_u32(&mut b, 100, self.vlr_count);
        // Bit 7 of the format byte is what makes a LAS file a LAZ one.
        b[104] = self.point_format | if self.compressed { 0x80 } else { 0 };
        write_u16(&mut b, 105, self.point_size);

        // The legacy 32-bit counts. The spec requires them to be zero for point
        // formats above 5, and a reader that trusts them over the 64-bit field
        // would otherwise see a truncated cloud.
        write_u32(&mut b, 107, 0);

        write_f64(&mut b, 131, self.scale[0]);
        write_f64(&mut b, 139, self.scale[1]);
        write_f64(&mut b, 147, self.scale[2]);
        write_f64(&mut b, 155, self.offset[0]);
        write_f64(&mut b, 163, self.offset[1]);
        write_f64(&mut b, 171, self.offset[2]);
        // Interleaved max/min, per axis. Not min-then-max.
        write_f64(&mut b, 179, self.max[0]);
        write_f64(&mut b, 187, self.min[0]);
        write_f64(&mut b, 195, self.max[1]);
        write_f64(&mut b, 203, self.min[1]);
        write_f64(&mut b, 211, self.max[2]);
        write_f64(&mut b, 219, self.min[2]);
        write_u64(&mut b, 235, self.evlr_offset);
        write_u32(&mut b, 243, self.evlr_count);
        write_u64(&mut b, 247, self.point_count);
        for (i, count) in self.points_by_return.iter().enumerate() {
            write_u64(&mut b, 255 + i * 8, *count);
        }
        b
    }
}

pub fn write_vlr<W: Write>(out: &mut W, vlr: &OutVlr) -> Result<()> {
    let mut header = vec![0u8; VLR_HEADER_SIZE];
    write_fixed(&mut header, 2, 16, &vlr.user_id);
    write_u16(&mut header, 18, vlr.record_id);
    write_u16(&mut header, 20, vlr.data.len() as u16);
    write_fixed(&mut header, 22, 32, &vlr.description);
    out.write_all(&header)?;
    out.write_all(&vlr.data)?;
    Ok(())
}

pub fn write_evlr<W: Write>(out: &mut W, vlr: &OutVlr) -> Result<()> {
    let mut header = vec![0u8; EVLR_HEADER_SIZE];
    write_fixed(&mut header, 2, 16, &vlr.user_id);
    write_u16(&mut header, 18, vlr.record_id);
    write_u64(&mut header, 20, vlr.data.len() as u64);
    write_fixed(&mut header, 28, 32, &vlr.description);
    out.write_all(&header)?;
    out.write_all(&vlr.data)?;
    Ok(())
}

/// Overwrite the header in place, at the start of the file.
pub fn patch_header<W: Write + Seek>(out: &mut W, header: &OutHeader) -> Result<()> {
    let here = out.stream_position()?;
    out.seek(SeekFrom::Start(0))?;
    out.write_all(&header.to_bytes())?;
    out.seek(SeekFrom::Start(here))?;
    Ok(())
}

/// A NUL-padded fixed-width ASCII field, truncated rather than overflowing.
fn write_fixed(buffer: &mut [u8], at: usize, width: usize, text: &str) {
    let bytes = text.as_bytes();
    let n = bytes.len().min(width);
    buffer[at..at + n].copy_from_slice(&bytes[..n]);
}

pub fn write_u16(buffer: &mut [u8], at: usize, value: u16) {
    buffer[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

pub fn write_u32(buffer: &mut [u8], at: usize, value: u32) {
    buffer[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn write_u64(buffer: &mut [u8], at: usize, value: u64) {
    buffer[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

pub fn write_i32(buffer: &mut [u8], at: usize, value: i32) {
    buffer[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn write_f64(buffer: &mut [u8], at: usize, value: f64) {
    buffer[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// Today, as LAS spells a date: day of the year, and the year.
///
/// Native only in practice — it is the one thing in this module that asks the
/// environment for anything, and on `wasm32-unknown-unknown` `SystemTime::now`
/// panics rather than failing. That is why the writers take the date as an
/// input and this is merely the convenient way to produce one.
pub fn creation_today() -> (u16, u16) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    creation_from_unix_seconds(now)
}

/// Civil date from a Unix timestamp, as (day of year, year).
///
/// The usual days-from-civil algorithm run backwards, shifted to March so the
/// leap day lands at the end of the cycle. Separate from the clock so it can be
/// tested, and so a caller with a timestamp from somewhere else — a browser's
/// `Date`, a build's `SOURCE_DATE_EPOCH` — can use it.
pub fn creation_from_unix_seconds(seconds: u64) -> (u16, u16) {
    let days = seconds / 86_400;
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    const CUMULATIVE: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let mut day_of_year = CUMULATIVE[(month - 1) as usize] + day;
    if leap && month > 2 {
        day_of_year += 1;
    }
    (day_of_year as u16, year as u16)
}

/// The projection records to write, carried from the source.
///
/// Verbatim, and that is the whole design. A CRS can be declared two ways — OGC
/// WKT in record 2112, or the GeoTIFF key directory in 34735/6/7 — and this
/// crate can read both and synthesise neither: turning `EPSG:26912` into WKT
/// needs the EPSG table, which is an opt-in package in the browser and nothing
/// at all here. Copying the records the input carried is therefore the only
/// lossless option, and losing the projection in conversion is exactly the
/// thing this project criticises other converters for.
///
/// Returns the records and whether one of them is WKT, which decides the global
/// encoding bit.
pub fn projection_vlrs(records: &[(u16, Vec<u8>)]) -> (Vec<OutVlr>, bool) {
    let mut out = Vec::with_capacity(records.len().max(1));
    let mut wkt = false;
    for (record_id, data) in records {
        if data.is_empty() {
            continue;
        }
        if *record_id == crate::las::crs::WKT_RECORD_ID {
            wkt = true;
        }
        out.push(OutVlr::new(
            crate::las::crs::PROJECTION_USER_ID,
            *record_id,
            match *record_id {
                crate::las::crs::WKT_RECORD_ID => "OGC coordinate system WKT",
                crate::las::crs::GEOKEY_DIRECTORY_RECORD_ID => "GeoTIFF key directory",
                crate::las::crs::GEOKEY_DOUBLE_RECORD_ID => "GeoTIFF double parameters",
                _ => "GeoTIFF ASCII parameters",
            },
            data.clone(),
        ));
    }

    if out.is_empty() {
        // No projection at all — which is common and not an error. An empty WKT
        // record says "none declared" in the words the format uses everywhere
        // else, and keeps the bit and the record consistent with each other.
        out.push(OutVlr::new(
            crate::las::crs::PROJECTION_USER_ID,
            crate::las::crs::WKT_RECORD_ID,
            "OGC coordinate system WKT",
            vec![0],
        ));
        wkt = true;
    }
    (out, wkt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::las::LasHeader;

    #[test]
    fn a_written_header_reads_back_the_same() {
        // The writer and the reader are the two halves of every offset in the
        // spec. Holding them to each other is what makes a wrong constant fail
        // here rather than in somebody's file.
        let header = OutHeader {
            point_format: 7,
            point_size: 36,
            compressed: true,
            point_count: 10_653_336,
            scale: [0.01, 0.01, 0.001],
            offset: [635577.79, 848882.15, 406.14],
            min: [1.0, 2.0, 3.0],
            max: [4.0, 5.0, 6.0],
            offset_to_point_data: 375 + 54 + 160,
            vlr_count: 3,
            evlr_offset: 900_000,
            evlr_count: 1,
            generator: "voxelkloud 0.1.0".to_string(),
            wkt: true,
            creation: (200, 2026),
            points_by_return: [0; 15],
        };
        let mut bytes = header.to_bytes();
        // A header alone is a legal prefix; the reader tolerates the VLR
        // directory being absent.
        bytes.resize(HEADER_SIZE, 0);

        let read = LasHeader::read(&bytes).expect("reads");
        assert_eq!(read.version_major, 1);
        assert_eq!(read.version_minor, 4);
        assert_eq!(read.point_format, 7);
        assert!(read.compressed);
        assert_eq!(read.point_size, 36);
        assert_eq!(read.point_count, 10_653_336);
        assert_eq!(read.scale, header.scale);
        assert_eq!(read.offset, header.offset);
        assert_eq!(read.min, header.min);
        assert_eq!(read.max, header.max);
        assert_eq!(read.offset_to_point_data, header.offset_to_point_data);
        assert_eq!(read.vlr_count, 3);
        assert_eq!(read.evlr_offset, 900_000);
        assert_eq!(read.evlr_count, 1);
    }

    #[test]
    fn the_legacy_point_count_is_zero_for_a_modern_format() {
        // Not a detail: a 1.4 file with a non-zero legacy count and a format-7
        // record is self-contradictory, and readers pick different halves.
        let header = OutHeader {
            point_format: 7,
            point_size: 36,
            compressed: false,
            point_count: 5,
            scale: [0.01; 3],
            offset: [0.0; 3],
            min: [0.0; 3],
            max: [1.0; 3],
            offset_to_point_data: 375,
            vlr_count: 0,
            evlr_offset: 0,
            evlr_count: 0,
            generator: String::new(),
            wkt: true,
            creation: (1, 2026),
            points_by_return: [0; 15],
        };
        let bytes = header.to_bytes();
        assert_eq!(u32::from_le_bytes(bytes[107..111].try_into().unwrap()), 0);
        assert_eq!(u64::from_le_bytes(bytes[247..255].try_into().unwrap()), 5);
    }
}
