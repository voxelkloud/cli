//! From a LAS record layout to the neutral attribute vocabulary.
//!
//! The names are PotreeConverter's for the same fields, on purpose: a colour
//! mode that keys off `"classification"` has to work on a COPC cloud and a
//! Potree cloud without the renderer knowing which it got.
//!
//! Never fails. A record that does not add up produces a warning and a layout
//! that is still self-consistent, because refusing a file over a mislabelled
//! extra dimension would be worse than showing it without that dimension.

use crate::attribute::Attribute;
use crate::bounds::Bounds;
use crate::error::Result;
use crate::warning::Warning;

use super::extra_bytes::parse_extra_bytes;
use super::point_format::{las_base_size, las_dimensions, LasAccess};

/// One attribute plus how to read it out of a record.
#[derive(Debug, Clone)]
pub struct LasAttribute {
    pub attribute: Attribute,
    pub access: LasAccess,
}

#[derive(Debug, Clone)]
pub struct LasLayoutOptions<'a> {
    pub format: u8,
    /// `point_data_record_length` from the header, extra bytes included.
    pub point_size: usize,
    /// Payload of the Extra Bytes VLR (`LASF_Spec`, record 4), when present.
    pub extra_bytes: Option<&'a [u8]>,
    /// Position's true domain, in absolute CRS units.
    pub bounds: Bounds,
    /// GPS time's true domain, when the container knows it. COPC's info VLR does.
    pub gps_time_range: Option<[f64; 2]>,
}

#[derive(Debug, Clone)]
pub struct LasLayout {
    pub attributes: Vec<LasAttribute>,
    /// Bytes per record. Equal to the header's `point_data_record_length`.
    pub stride: usize,
    pub warnings: Vec<Warning>,
}

impl LasLayout {
    pub fn find(&self, name: &str) -> Option<&LasAttribute> {
        self.attributes.iter().find(|a| a.attribute.name == name)
    }

    /// The neutral attributes alone, which is what a [`CloudInfo`] carries.
    ///
    /// [`CloudInfo`]: crate::cloud::CloudInfo
    pub fn plain(&self) -> Vec<Attribute> {
        self.attributes.iter().map(|a| a.attribute.clone()).collect()
    }
}

/// Build the attribute list for a LAS record.
pub fn las_layout(options: &LasLayoutOptions<'_>) -> Result<LasLayout> {
    let mut warnings: Vec<Warning> = Vec::new();
    let mut seen_codes: Vec<&'static str> = Vec::new();
    let mut warn = |code: &'static str, path: &str, message: String| {
        if seen_codes.contains(&code) {
            return;
        }
        seen_codes.push(code);
        warnings.push(Warning::new(code, path, message));
    };

    let base = las_base_size(options.format)?;
    let declared_extra = options.point_size.saturating_sub(base);

    let mut extras = Vec::new();
    if let Some(record) = options.extra_bytes.filter(|r| !r.is_empty()) {
        extras = parse_extra_bytes(record, base);
        let described: usize = extras.iter().map(|f| f.byte_size).sum();
        if described != declared_extra {
            warn(
                "extra-bytes-mismatch",
                "extraBytes",
                format!(
                    "The Extra Bytes VLR describes {described} bytes past the {base}-byte \
                     format {} record, but the header declares a {}-byte record \
                     ({declared_extra} extra). Fields past the declared end are dropped.",
                    options.format, options.point_size
                ),
            );
            extras.retain(|f| f.byte_offset + f.byte_size <= options.point_size);
        }
    }

    let mut attributes: Vec<LasAttribute> = Vec::new();

    for dim in las_dimensions(options.format)? {
        let mut min = dim.min.clone();
        let mut max = dim.max.clone();
        if dim.name == "position" {
            // Absolute CRS, post scale and offset — the same convention
            // Potree's manifest uses for its position attribute.
            min = options.bounds.min.to_vec();
            max = options.bounds.max.to_vec();
        } else if dim.name == "gps-time" {
            if let Some(range) = options.gps_time_range {
                min = vec![range[0]];
                max = vec![range[1]];
            }
        }

        attributes.push(LasAttribute {
            attribute: Attribute {
                name: dim.name.to_string(),
                description: dim.description.to_string(),
                kind: dim.kind,
                num_elements: dim.num_elements,
                // For a bit run this is the byte the run lives in. Nothing
                // reads it as an addressable offset — `access` is what a
                // decoder uses — but it is the honest answer to "where in the
                // record is this".
                byte_offset: match dim.access {
                    LasAccess::Scalar { at } => at,
                    LasAccess::Bits { at, .. } => at,
                },
                scale: vec![1.0; dim.num_elements],
                offset: vec![0.0; dim.num_elements],
                min,
                max,
                histogram: None,
            },
            access: dim.access,
        });
    }

    for field in &extras {
        let Some(kind) = field.kind else {
            warn(
                "undecodable-attribute",
                &field.name,
                format!(
                    "Extra dimension {:?} is declared as {} undocumented bytes (data_type 0), \
                     which carry no interpretation. It is skipped; the record stride still \
                     counts it.",
                    field.name, field.byte_size
                ),
            );
            continue;
        };
        attributes.push(LasAttribute {
            attribute: Attribute {
                name: field.name.clone(),
                description: field.description.clone(),
                kind,
                num_elements: field.num_elements,
                byte_offset: field.byte_offset,
                min: field.min.clone().unwrap_or_else(|| vec![0.0; field.num_elements]),
                max: field.max.clone().unwrap_or_else(|| vec![0.0; field.num_elements]),
                scale: field.scale.clone().unwrap_or_else(|| vec![1.0; field.num_elements]),
                offset: field.offset.clone().unwrap_or_else(|| vec![0.0; field.num_elements]),
                histogram: None,
            },
            access: LasAccess::Scalar {
                at: field.byte_offset,
            },
        });
    }

    // A duplicate name is tolerated: both attributes keep their own offsets,
    // and a lookup resolves to the first. An extra dimension called
    // "intensity" is the realistic way this happens.
    let mut duplicate: Option<String> = None;
    for (i, a) in attributes.iter().enumerate() {
        if attributes[..i].iter().any(|b| b.attribute.name == a.attribute.name) {
            duplicate = Some(a.attribute.name.clone());
            break;
        }
    }
    if let Some(name) = duplicate {
        warn(
            "duplicate-attribute-name",
            &name,
            format!(
                "Attribute name {name:?} appears more than once. Lookups by name resolve to \
                 the first occurrence; both keep their own byte offsets."
            ),
        );
    }

    Ok(LasLayout {
        attributes,
        stride: options.point_size,
        warnings,
    })
}
