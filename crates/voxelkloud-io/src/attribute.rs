//! The neutral attribute vocabulary.
//!
//! Names are whatever the source format declared, verbatim: `"scan angle
//! rank"`, `"gps-time"`, `"rgb"`. Not slugified, not camelCased, not
//! normalised — the name is the on-disk identity, and a viewer looking up
//! `"classification"` has to find it whichever writer produced the cloud.
//!
//! The one derived thing is [`AttributeRole`], which is how a renderer finds
//! position and colour without knowing the format. It is a closed two-member
//! set on purpose: everything else is found by name.

use std::fmt;

/// The element types a point attribute can have.
///
/// The same ten `@voxelkloud/core` declares, spelled the same way, because
/// these strings are written into a Potree v2 `metadata.json` and read back by
/// the TypeScript. A rename here is a file-format change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttributeType {
    Int8,
    Uint8,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Int64,
    Uint64,
    Float,
    Double,
}

impl AttributeType {
    /// Bytes of one element. The canonical width — a manifest's own
    /// `elementSize` is cross-checked against this and then discarded.
    pub fn size(self) -> usize {
        match self {
            Self::Int8 | Self::Uint8 => 1,
            Self::Int16 | Self::Uint16 => 2,
            Self::Int32 | Self::Uint32 | Self::Float => 4,
            Self::Int64 | Self::Uint64 | Self::Double => 8,
        }
    }

    /// The manifest spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Int8 => "int8",
            Self::Uint8 => "uint8",
            Self::Int16 => "int16",
            Self::Uint16 => "uint16",
            Self::Int32 => "int32",
            Self::Uint32 => "uint32",
            Self::Int64 => "int64",
            Self::Uint64 => "uint64",
            Self::Float => "float",
            Self::Double => "double",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "int8" => Self::Int8,
            "uint8" => Self::Uint8,
            "int16" => Self::Int16,
            "uint16" => Self::Uint16,
            "int32" => Self::Int32,
            "uint32" => Self::Uint32,
            "int64" => Self::Int64,
            "uint64" => Self::Uint64,
            "float" => Self::Float,
            "double" => Self::Double,
            _ => return None,
        })
    }

    /// True for the two widths nothing downstream can decode without loss.
    ///
    /// Legal in a manifest, and the widths are known, so the record stride
    /// stays right — which is why this is a warning at read time rather than a
    /// parse failure.
    pub fn is_undecodable(self) -> bool {
        matches!(self, Self::Int64 | Self::Uint64)
    }
}

impl fmt::Display for AttributeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The two attributes a renderer must find without knowing the format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeRole {
    Position,
    Color,
}

impl AttributeRole {
    /// Assigned by exact-name membership, collapsing the aliases the
    /// PotreeConverter and the reference client scatter across two files.
    pub fn of(name: &str) -> Option<Self> {
        match name {
            "position" | "POSITION_CARTESIAN" => Some(Self::Position),
            "rgb" | "rgba" | "RGBA" => Some(Self::Color),
            _ => None,
        }
    }
}

/// One attribute of a point record.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    /// Verbatim from the source. Spaces and hyphens included.
    pub name: String,
    /// `""` when the source omits it, which is also what PotreeConverter emits.
    pub description: String,
    pub kind: AttributeType,
    pub num_elements: usize,
    /// Byte offset within one record, for the record-oriented encodings.
    ///
    /// Meaningless for a planar encoding (Potree's BROTLI blocks), which is why
    /// a reader of one of those must not be handed this and asked to seek.
    pub byte_offset: usize,
    /// Semantic bounds in the attribute's own domain, length `num_elements`.
    ///
    /// Copied verbatim and never validated against `kind`: stock
    /// PotreeConverter output has `"scan angle rank"` as `uint8` with
    /// `min: [-21]`, because LAS keeps a signed rank in a raw byte. Inverted
    /// and degenerate ranges are both tolerated.
    pub min: Vec<f64>,
    pub max: Vec<f64>,
    /// Per-element affine transform. All-ones and all-zeros when unstated.
    ///
    /// Not the position quantization — that is the cloud's `scale`/`offset`.
    /// `position` carries `scale: [1,1,1]` here while the real quantum is
    /// `[0.01, 0.01, 0.01]` at the top level.
    pub scale: Vec<f64>,
    pub offset: Vec<f64>,
    /// 256-bucket value histogram, when the source carried one.
    pub histogram: Option<Vec<u64>>,
}

impl Attribute {
    /// `num_elements * kind.size()` — this attribute's stride contribution.
    pub fn byte_size(&self) -> usize {
        self.num_elements * self.kind.size()
    }

    pub fn role(&self) -> Option<AttributeRole> {
        AttributeRole::of(&self.name)
    }

    /// An attribute with identity transforms and no stated domain.
    pub fn new(name: impl Into<String>, kind: AttributeType, num_elements: usize) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            kind,
            num_elements,
            byte_offset: 0,
            min: vec![0.0; num_elements],
            max: vec![0.0; num_elements],
            scale: vec![1.0; num_elements],
            offset: vec![0.0; num_elements],
            histogram: None,
        }
    }
}

/// Assign `byte_offset` down a record in declaration order.
///
/// The record layout is the declaration order and nothing else: no alignment,
/// no padding. Every format here agrees on that, and a writer that padded would
/// produce files the reference client reads as garbage.
pub fn lay_out(attributes: &mut [Attribute]) -> usize {
    let mut at = 0usize;
    for a in attributes.iter_mut() {
        a.byte_offset = at;
        at += a.byte_size();
    }
    at
}
