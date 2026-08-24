//! The Extra Bytes VLR: `LASF_Spec` record 4.
//!
//! A LAS record may carry dimensions the spec never named — an origin id, a
//! per-point return-path metric, a scanner temperature — and this 192-byte
//! descriptor per dimension is how the file says what they are. Without it the
//! bytes are there and unreadable, which is how the COPC demo file's
//! `OriginId` would look.

use crate::attribute::AttributeType;

/// Bytes of one descriptor. Fixed by the spec, and the record's length / 192.
const DESCRIPTOR_SIZE: usize = 192;

const HAS_MIN: u8 = 0b0_0010;
const HAS_MAX: u8 = 0b0_0100;
const HAS_SCALE: u8 = 0b0_1000;
const HAS_OFFSET: u8 = 0b1_0000;

/// One extra dimension, as the VLR describes it.
#[derive(Debug, Clone)]
pub struct ExtraByteField {
    pub name: String,
    pub description: String,
    /// `None` for `data_type` 0: raw bytes with no interpretation.
    pub kind: Option<AttributeType>,
    pub num_elements: usize,
    /// Bytes this field occupies in the record.
    pub byte_size: usize,
    /// Offset within one record.
    pub byte_offset: usize,
    pub min: Option<Vec<f64>>,
    pub max: Option<Vec<f64>>,
    pub scale: Option<Vec<f64>>,
    pub offset: Option<Vec<f64>>,
}

/// `data_type` to a type and a width.
///
/// Values 11-30 are the deprecated 2- and 3-element variants, removed in LAS
/// 1.4 R15. They are mapped rather than rejected because files written against
/// the older spec exist, and reading one dimension of the pair is better than
/// refusing the file.
fn base_type(data_type: u8) -> Option<AttributeType> {
    Some(match data_type {
        1 => AttributeType::Uint8,
        2 => AttributeType::Int8,
        3 => AttributeType::Uint16,
        4 => AttributeType::Int16,
        5 => AttributeType::Uint32,
        6 => AttributeType::Int32,
        7 => AttributeType::Uint64,
        8 => AttributeType::Int64,
        9 => AttributeType::Float,
        10 => AttributeType::Double,
        _ => return None,
    })
}

fn fixed_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim_end().to_string()
}

fn f64_at(b: &[u8], at: usize) -> f64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    f64::from_le_bytes(v)
}

/// Parse an Extra Bytes VLR payload into fields, in record order.
///
/// `first_offset` is the byte offset of the first extra field within a record,
/// which is the point format's base size.
pub fn parse_extra_bytes(record: &[u8], first_offset: usize) -> Vec<ExtraByteField> {
    let count = record.len() / DESCRIPTOR_SIZE;
    let mut out = Vec::with_capacity(count);
    let mut at = first_offset;

    for i in 0..count {
        let d = &record[i * DESCRIPTOR_SIZE..(i + 1) * DESCRIPTOR_SIZE];
        let data_type = d[2];
        let options = d[3];
        let name = fixed_string(&d[4..36]);
        let description = fixed_string(&d[160..192]);
        let name = if name.is_empty() {
            format!("extra {i}")
        } else {
            name
        };

        // `data_type` 0 means "undocumented bytes", and then `options` IS the
        // byte count rather than a flag word. Nothing can interpret them, but
        // they still occupy the record, so they must be counted or every later
        // field shifts.
        if data_type == 0 {
            let byte_size = options as usize;
            out.push(ExtraByteField {
                name,
                description,
                kind: None,
                num_elements: 1,
                byte_size,
                byte_offset: at,
                min: None,
                max: None,
                scale: None,
                offset: None,
            });
            at += byte_size;
            continue;
        }

        let elements = if data_type > 20 {
            3
        } else if data_type > 10 {
            2
        } else {
            1
        };
        let kind = base_type(if data_type > 10 {
            (data_type - 11) % 10 + 1
        } else {
            data_type
        })
        .unwrap_or(AttributeType::Uint8);
        let size = kind.size() * elements;

        // The optional triples are stored as three 8-byte slots whatever the
        // dimension's own width, so they are read as doubles for the integer
        // types too — which is what every writer does, and what the spec's
        // "anytype" means.
        let triple = |offset: usize| -> Vec<f64> {
            (0..elements).map(|k| f64_at(d, offset + k * 8)).collect()
        };

        out.push(ExtraByteField {
            name,
            description,
            kind: Some(kind),
            num_elements: elements,
            byte_size: size,
            byte_offset: at,
            min: (options & HAS_MIN != 0).then(|| triple(64)),
            max: (options & HAS_MAX != 0).then(|| triple(88)),
            scale: (options & HAS_SCALE != 0).then(|| triple(112)),
            offset: (options & HAS_OFFSET != 0).then(|| triple(136)),
            // `no_data` at offset 40 is deliberately unread: nothing
            // downstream can act on a sentinel without a per-point branch in
            // the hot loop.
        });
        at += size;
    }

    out
}
