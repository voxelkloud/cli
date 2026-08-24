//! LAS: the framing, the record layout, and the projection it declares.
//!
//! Every format in this project that stores a LAS point shares this. COPC nodes
//! are LAS 1.4 records, EPT's `laszip` payload is a LAS file per node, and the
//! single-file tier is a `.las`/`.laz` outright — so the layout is stated once
//! here rather than three times with two of them subtly wrong.
//!
//! This module is the one `@voxelkloud/wasm-codecs` compiles into its wasm.
//! What it reads is *framing*: enough of the format to find the compressed
//! points and the records that describe them. Every reader takes a byte slice
//! rather than a stream and tolerates one that stops early — a driver reading a
//! remote file discovers the layout with one ranged `GET` of the first few
//! kilobytes, and [`LasHeader::vlrs_complete`] is how it learns that the prefix
//! was too short instead of guessing.

pub mod copc;
pub mod crs;
pub mod extra_bytes;
pub mod layout;
pub mod point_format;

pub use crs::las_crs;
pub use extra_bytes::{parse_extra_bytes, ExtraByteField};
pub use layout::{las_layout, LasAttribute, LasLayout, LasLayoutOptions};
pub use point_format::{las_base_size, las_dimensions, LasAccess, LasDimension};

use std::fmt;

/// The `LASF` magic every LAS and LAZ file opens with.
const SIGNATURE: &[u8; 4] = b"LASF";

/// Bytes of public header block that predate LAS 1.3 — through `min_z`.
const HEADER_SIZE_1_2: usize = 227;
/// LAS 1.3 adds the waveform offset.
const HEADER_SIZE_1_3: usize = 235;
/// LAS 1.4 adds the EVLR directory and the 64-bit point counts.
const HEADER_SIZE_1_4: usize = 375;

/// Fixed part of a VLR record: reserved, user id, record id, length, description.
const VLR_HEADER_SIZE: usize = 54;
/// Same, but the length field is 64-bit.
const EVLR_HEADER_SIZE: usize = 60;

/// Set in `point_data_record_format` when the point records are laszip-compressed.
const COMPRESSED_BIT: u8 = 0x80;

/// User id of the VLR that carries the laszip parameters.
pub const LASZIP_USER_ID: &str = "laszip encoded";
/// Record id of the same.
pub const LASZIP_RECORD_ID: u16 = 22204;

#[derive(Debug)]
pub enum LasError {
    /// The buffer does not begin with `LASF`.
    NotLas,
    /// A field was needed that the buffer does not reach.
    Truncated { need: usize, got: usize },
    /// `header_size` is smaller than the version's own minimum.
    ShortHeader { version: (u8, u8), header_size: u16 },
    /// `point_data_record_length` is zero, so a point count means nothing.
    ZeroPointSize,
}

impl fmt::Display for LasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLas => write!(f, "not a LAS or LAZ file: missing the LASF signature"),
            Self::Truncated { need, got } => write!(
                f,
                "truncated LAS header: need at least {need} bytes, got {got}"
            ),
            Self::ShortHeader {
                version: (major, minor),
                header_size,
            } => write!(
                f,
                "LAS {major}.{minor} declares a {header_size}-byte header, \
                 shorter than the version's own minimum"
            ),
            Self::ZeroPointSize => write!(f, "LAS header declares a point record length of 0"),
        }
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

fn f64_at(b: &[u8], at: usize) -> f64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    f64::from_le_bytes(v)
}

/// A fixed-width, NUL-padded ASCII field, trimmed the way the spec intends.
fn fixed_str(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim_end().to_string()
}

/// One variable length record, header fields plus its payload.
#[derive(Debug, Clone)]
pub struct Vlr {
    pub user_id: String,
    pub record_id: u16,
    pub description: String,
    pub data: Vec<u8>,
    /// True for records read from the EVLR directory at the end of the file.
    pub extended: bool,
}

impl Vlr {
    /// Whether this record is the one identified by `user_id` and `record_id`.
    pub fn is(&self, user_id: &str, record_id: u16) -> bool {
        self.record_id == record_id && self.user_id == user_id
    }
}

/// The public header block, plus whatever of the VLR directory the buffer held.
#[derive(Debug, Clone)]
pub struct LasHeader {
    pub version_major: u8,
    pub version_minor: u8,
    pub header_size: u16,
    pub offset_to_point_data: u32,
    /// Format id with the laszip compression bit already masked off.
    pub point_format: u8,
    /// Size of one *decompressed* point record, extra bytes included.
    pub point_size: u16,
    pub point_count: u64,
    /// Whether the point records are laszip-compressed.
    pub compressed: bool,
    pub scale: [f64; 3],
    pub offset: [f64; 3],
    pub min: [f64; 3],
    pub max: [f64; 3],
    /// Absolute offset of the first EVLR, or 0 when there are none.
    pub evlr_offset: u64,
    pub evlr_count: u32,
    /// How many VLRs the header claims.
    pub vlr_count: u32,
    /// The VLRs the buffer actually reached.
    pub vlrs: Vec<Vlr>,
    /// False when the buffer ended before the VLR directory did.
    pub vlrs_complete: bool,
}

impl LasHeader {
    /// Parse a LAS/LAZ file, or as much of its front as the buffer holds.
    pub fn read(bytes: &[u8]) -> Result<Self, LasError> {
        if bytes.len() < 4 {
            return Err(LasError::Truncated {
                need: 4,
                got: bytes.len(),
            });
        }
        if &bytes[..4] != SIGNATURE {
            return Err(LasError::NotLas);
        }
        if bytes.len() < HEADER_SIZE_1_2 {
            return Err(LasError::Truncated {
                need: HEADER_SIZE_1_2,
                got: bytes.len(),
            });
        }

        let version_major = bytes[24];
        let version_minor = bytes[25];
        let header_size = u16_at(bytes, 94);

        // A 1.4 file whose header is 1.2-sized has no 64-bit point count to
        // read; trusting the version over the size would read the first VLR as
        // if it were header fields.
        let min_header = match (version_major, version_minor) {
            (1, 0..=2) => HEADER_SIZE_1_2,
            (1, 3) => HEADER_SIZE_1_3,
            _ => HEADER_SIZE_1_4,
        };
        if (header_size as usize) < min_header {
            return Err(LasError::ShortHeader {
                version: (version_major, version_minor),
                header_size,
            });
        }

        let raw_format = bytes[104];
        let compressed = raw_format & COMPRESSED_BIT != 0;
        // Bit 6 is reserved and bit 7 is the laszip flag; the format id is the
        // low six.
        let point_format = raw_format & 0x3f;
        let point_size = u16_at(bytes, 105);
        if point_size == 0 {
            return Err(LasError::ZeroPointSize);
        }

        let offset_to_point_data = u32_at(bytes, 96);
        let vlr_count = u32_at(bytes, 100);
        let legacy_point_count = u32_at(bytes, 107) as u64;

        let is_1_4 = version_major > 1 || (version_major == 1 && version_minor >= 4);
        let have_1_4_fields = is_1_4 && bytes.len() >= HEADER_SIZE_1_4;
        let (evlr_offset, evlr_count, point_count) = if have_1_4_fields {
            let wide = u64_at(bytes, 247);
            // Files above 2^32 points zero the legacy field; files below it are
            // required to write both, and writers disagree about which they
            // fill. Prefer the wide field, fall back when it is zero.
            let count = if wide != 0 { wide } else { legacy_point_count };
            (u64_at(bytes, 235), u32_at(bytes, 243), count)
        } else {
            if is_1_4 {
                return Err(LasError::Truncated {
                    need: HEADER_SIZE_1_4,
                    got: bytes.len(),
                });
            }
            (0, 0, legacy_point_count)
        };

        let mut header = Self {
            version_major,
            version_minor,
            header_size,
            offset_to_point_data,
            point_format,
            point_size,
            point_count,
            compressed,
            scale: [f64_at(bytes, 131), f64_at(bytes, 139), f64_at(bytes, 147)],
            offset: [f64_at(bytes, 155), f64_at(bytes, 163), f64_at(bytes, 171)],
            // The header interleaves the bounds as max/min per axis.
            min: [f64_at(bytes, 187), f64_at(bytes, 203), f64_at(bytes, 219)],
            max: [f64_at(bytes, 179), f64_at(bytes, 195), f64_at(bytes, 211)],
            evlr_offset,
            evlr_count,
            vlr_count,
            vlrs: Vec::new(),
            vlrs_complete: vlr_count == 0,
        };

        // The VLR directory runs from the end of the header to the point data.
        // Both bounds are declared, and a corrupt file can invert them.
        let start = header_size as usize;
        let end = (offset_to_point_data as usize).min(bytes.len());
        if start < end {
            let (vlrs, complete) = read_records(&bytes[start..end], vlr_count as usize, false);
            header.vlrs_complete = complete;
            header.vlrs = vlrs;
        } else {
            header.vlrs_complete = vlr_count == 0;
        }

        Ok(header)
    }

    /// The laszip VLR's payload, which is what a decoder needs to be built.
    pub fn laszip_record(&self) -> Option<&[u8]> {
        self.vlrs
            .iter()
            .find(|v| v.is(LASZIP_USER_ID, LASZIP_RECORD_ID))
            .map(|v| v.data.as_slice())
    }
}

/// Parse an EVLR directory from a buffer that begins at the first record.
///
/// A driver reads these with a second ranged `GET` from
/// [`LasHeader::evlr_offset`] to the end of the file, so the buffer starts at
/// the record rather than at the file.
pub fn read_evlrs(bytes: &[u8], count: u32) -> (Vec<Vlr>, bool) {
    read_records(bytes, count as usize, true)
}

/// Shared walk over a record directory. Returns what fit, and whether all of
/// `count` records fit.
fn read_records(bytes: &[u8], count: usize, extended: bool) -> (Vec<Vlr>, bool) {
    let header_size = if extended {
        EVLR_HEADER_SIZE
    } else {
        VLR_HEADER_SIZE
    };
    let mut out = Vec::with_capacity(count.min(64));
    let mut at = 0usize;

    for _ in 0..count {
        if at.saturating_add(header_size) > bytes.len() {
            return (out, false);
        }
        let user_id = fixed_str(&bytes[at + 2..at + 18]);
        let record_id = u16_at(bytes, at + 18);
        let (len, desc_at) = if extended {
            // A length past what this target can address cannot be satisfied by
            // any buffer; saturating makes the bounds check below reject it.
            (
                usize::try_from(u64_at(bytes, at + 20)).unwrap_or(usize::MAX),
                at + 28,
            )
        } else {
            (u16_at(bytes, at + 20) as usize, at + 22)
        };
        let description = fixed_str(&bytes[desc_at..desc_at + 32]);

        let data_at = at + header_size;
        // `len` comes off disk as a u64 and this target is 32-bit, so a record
        // claiming four gigabytes must not wrap the bounds check it fails.
        match data_at.checked_add(len) {
            Some(end) if end <= bytes.len() => {}
            _ => return (out, false),
        }
        out.push(Vlr {
            user_id,
            record_id,
            description,
            data: bytes[data_at..data_at + len].to_vec(),
            extended,
        });
        at = data_at + len;
    }

    (out, true)
}

impl From<LasError> for crate::error::Error {
    fn from(err: LasError) -> Self {
        match err {
            LasError::Truncated { need, got } => Self::Truncated {
                need: need as u64,
                got: got as u64,
                what: "LAS header".to_string(),
            },
            other => Self::not_format("a LAS file", other.to_string()),
        }
    }
}
