//! The CRS a LAS file declares, from either of the two ways it can.
//!
//! LAS carries its projection in `LASF_Projection` VLRs, and which one depends
//! on the version. 1.4 requires OGC WKT in record 2112. 1.2 and earlier use the
//! GeoTIFF key directory — records 34735, 34736 and 34737 — a flat table of
//! numeric keys borrowed wholesale from the TIFF spec.
//!
//! Both appear in real files in this repo, so both are here. A 1.4 file may
//! also carry the GeoTIFF keys for compatibility, and then the WKT wins: it is
//! the form the version requires and the only one of the two that can express a
//! compound system.

use std::collections::HashMap;

use super::Vlr;
use crate::crs::Crs;

/// User id of every projection VLR.
pub const PROJECTION_USER_ID: &str = "LASF_Projection";
/// OGC coordinate system WKT. LAS 1.4's required form.
pub const WKT_RECORD_ID: u16 = 2112;
/// GeoTIFF key directory: the key table itself.
pub const GEOKEY_DIRECTORY_RECORD_ID: u16 = 34735;
/// GeoTIFF double parameters, referenced by key.
pub const GEOKEY_DOUBLE_RECORD_ID: u16 = 34736;
/// GeoTIFF ASCII parameters, referenced by key.
pub const GEOKEY_ASCII_RECORD_ID: u16 = 34737;

const GT_MODEL_TYPE: u16 = 1024;
const GEOGRAPHIC_TYPE: u16 = 2048;
const PROJECTED_CS_TYPE: u16 = 3072;
const PROJECTED_CITATION: u16 = 3073;
const VERTICAL_CS_TYPE: u16 = 4096;

const MODEL_PROJECTED: u16 = 1;
const MODEL_GEOGRAPHIC: u16 = 2;

/// "User-defined" and "undefined" in the GeoTIFF key space.
///
/// A file that sets `ProjectedCSType` to 32767 is saying "the code space cannot
/// name this one" — reading it as a code would resolve to nothing, or worse, to
/// something.
const USER_DEFINED: u16 = 32767;
const UNDEFINED: u16 = 0;

fn text(bytes: &[u8]) -> String {
    // LAS pads its strings with NULs, and a trailing one inside a WKT makes
    // every downstream parse fail on a character that is not there.
    String::from_utf8_lossy(bytes)
        .trim_end_matches('\0')
        .trim()
        .to_string()
}

/// Read the CRS out of a LAS file's projection VLRs.
///
/// Returns `None` when the file declares nothing this can resolve — common, and
/// not an error: most photogrammetry output has no CRS at all.
pub fn las_crs(vlrs: &[Vlr]) -> Option<Crs> {
    let find = |record_id: u16| {
        vlrs.iter()
            .find(|v| v.is(PROJECTION_USER_ID, record_id))
            .map(|v| v.data.as_slice())
    };

    if let Some(raw) = find(WKT_RECORD_ID) {
        let wkt = text(raw);
        if !wkt.is_empty() {
            return Some(Crs::from_wkt(&wkt));
        }
    }
    let directory = find(GEOKEY_DIRECTORY_RECORD_ID)?;
    geo_key_crs(directory, find(GEOKEY_ASCII_RECORD_ID))
}

#[derive(Clone, Copy)]
struct GeoKey {
    location: u16,
    count: u16,
    value: u16,
}

/// Read the GeoTIFF key directory.
///
/// Four `u16` of header — version, revision, minor revision, key count — then
/// one four-`u16` entry per key: id, the record the value lives in, how many
/// values, and either the value itself or an offset into that record.
/// `location == 0` means the value *is* the fourth field, which is the case for
/// every key that matters here.
fn geo_key_crs(directory: &[u8], ascii: Option<&[u8]>) -> Option<Crs> {
    if directory.len() < 8 {
        return None;
    }
    let u16_at = |at: usize| u16::from_le_bytes([directory[at], directory[at + 1]]);
    let count = u16_at(6) as usize;

    let mut keys: HashMap<u16, GeoKey> = HashMap::with_capacity(count);
    for i in 0..count {
        let at = 8 + i * 8;
        if at + 8 > directory.len() {
            break;
        }
        keys.insert(
            u16_at(at),
            GeoKey {
                location: u16_at(at + 2),
                count: u16_at(at + 4),
                value: u16_at(at + 6),
            },
        );
    }

    let code_of = |id: u16| -> Option<u32> {
        let key = keys.get(&id)?;
        if key.location != 0 || key.value == USER_DEFINED || key.value == UNDEFINED {
            return None;
        }
        Some(u32::from(key.value))
    };

    let model = keys.get(&GT_MODEL_TYPE).map(|k| k.value);
    // Prefer the projected code and fall back to the geographic one: a file
    // that declares itself geographic has no projected key at all, and one that
    // declares itself projected may still carry the geographic code of its
    // datum — which is NOT the system the coordinates are in.
    let projected = code_of(PROJECTED_CS_TYPE);
    let geographic = code_of(GEOGRAPHIC_TYPE);
    let epsg = if model == Some(MODEL_GEOGRAPHIC) {
        geographic
    } else {
        projected.or(if model == Some(MODEL_PROJECTED) {
            None
        } else {
            geographic
        })
    }?;

    let mut crs = Crs::from_epsg(epsg);
    crs.vertical_epsg = code_of(VERTICAL_CS_TYPE);
    crs.name = keys
        .get(&PROJECTED_CITATION)
        .and_then(|key| ascii_value(key, ascii?));
    Some(crs)
}

/// An ASCII-valued key, out of the 34737 record.
///
/// GeoTIFF concatenates every ASCII value into one string separated by `|`, and
/// a key's `value` is the offset into it while `count` is the length including
/// that separator.
fn ascii_value(key: &GeoKey, ascii: &[u8]) -> Option<String> {
    if key.location != GEOKEY_ASCII_RECORD_ID {
        return None;
    }
    let start = key.value as usize;
    let end = (start + key.count as usize).min(ascii.len());
    if start >= end {
        return None;
    }
    let value = String::from_utf8_lossy(&ascii[start..end])
        .trim_end_matches(['|', '\0'])
        .trim()
        .to_string();
    (!value.is_empty()).then_some(value)
}
