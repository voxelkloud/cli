//! What a file says about its coordinate reference system.
//!
//! Declaration, not projection — the same split `@voxelkloud/core` makes, and
//! for the same reason: reading a CRS out of a file is a few hundred bytes of
//! parsing that every driver does, and projecting through one needs an EPSG
//! table and a projection engine. The CLI reports what it read; placing two
//! clouds against each other is a different job with a different cost.
//!
//! This is a port of `packages/core/src/crs.ts`, traps included. Where the two
//! disagree, one of them has a bug.

/// How a file spelled its CRS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrsFormat {
    /// An EPSG code, from a GeoTIFF key or an `srs.authority`/`horizontal` pair.
    Epsg,
    /// OGC Well-Known Text, WKT1 or WKT2.
    Wkt,
    /// A proj4 string: `"+proj=utm +zone=12 +datum=NAD83"`.
    Proj4,
    /// Something was declared and none of the above recognised it.
    Unknown,
}

impl CrsFormat {
    pub fn name(self) -> &'static str {
        match self {
            Self::Epsg => "epsg",
            Self::Wkt => "wkt",
            Self::Proj4 => "proj4",
            Self::Unknown => "unknown",
        }
    }
}

/// A cloud's coordinate reference system, as declared.
///
/// `None` on a cloud means the file said *nothing*, which is common and not an
/// error: a photogrammetry scan sits in an arbitrary local frame, and
/// PotreeConverter drops the projection of everything it converts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crs {
    pub format: CrsFormat,
    /// Verbatim, whatever the file held. The only lossless field here.
    pub raw: String,
    /// EPSG code of the horizontal system, when it could be determined.
    pub epsg: Option<u32>,
    /// EPSG code of the vertical system, separately, when one was declared.
    ///
    /// Kept apart from `epsg` because nothing in this project shifts heights
    /// between vertical datums, and a compound code standing in for a
    /// horizontal one projects a cloud wrong by the datum separation — tens of
    /// metres, silently.
    pub vertical_epsg: Option<u32>,
    /// The human name the file gave, when it gave one.
    pub name: Option<String>,
}

impl Crs {
    /// Build a declaration from an OGC WKT string.
    pub fn from_wkt(wkt: &str) -> Self {
        Self {
            format: CrsFormat::Wkt,
            raw: wkt.to_string(),
            epsg: horizontal_epsg(wkt),
            vertical_epsg: vertical_epsg(wkt),
            name: wkt_name(wkt),
        }
    }

    /// Build a declaration from an EPSG code.
    pub fn from_epsg(epsg: u32) -> Self {
        Self {
            format: CrsFormat::Epsg,
            raw: format!("EPSG:{epsg}"),
            epsg: Some(epsg),
            vertical_epsg: None,
            name: None,
        }
    }

    /// Build a declaration from whatever a manifest field held.
    ///
    /// Potree's `projection` and EPT's `srs.wkt` are both "a string, and the
    /// writer decided what kind", so the kind is sniffed rather than assumed.
    /// `None` for an empty string — which is what PotreeConverter writes for
    /// every cloud it converts, and is not a declaration at all.
    pub fn from_string(text: &str) -> Option<Self> {
        let raw = text.trim();
        if raw.is_empty() {
            return None;
        }
        if raw.starts_with('+') {
            return Some(Self {
                format: CrsFormat::Proj4,
                raw: raw.to_string(),
                epsg: None,
                vertical_epsg: None,
                name: None,
            });
        }
        if let Some(code) = epsg_only(raw) {
            return Some(Self::from_epsg(code));
        }
        if looks_like_wkt(raw) {
            return Some(Self::from_wkt(raw));
        }
        Some(Self {
            format: CrsFormat::Unknown,
            raw: raw.to_string(),
            epsg: None,
            vertical_epsg: None,
            name: None,
        })
    }

    /// What to show a human in one line.
    pub fn label(&self) -> String {
        match (&self.name, self.epsg) {
            (Some(name), Some(code)) => format!("{name} (EPSG:{code})"),
            (Some(name), None) => name.clone(),
            (None, Some(code)) => format!("EPSG:{code}"),
            (None, None) => format!("{} (unresolved)", self.format.name()),
        }
    }
}

/// `EPSG:1234` and nothing else.
fn epsg_only(raw: &str) -> Option<u32> {
    let rest = raw.strip_prefix("EPSG:").or_else(|| raw.strip_prefix("epsg:"))?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// A leading `KEYWORD[`, allowing whitespace.
fn looks_like_wkt(raw: &str) -> bool {
    let rest = raw.trim_start();
    let keyword: String = rest
        .chars()
        .take_while(|c| c.is_ascii_uppercase() || *c == '_')
        .collect();
    !keyword.is_empty() && rest[keyword.len()..].trim_start().starts_with('[')
}

/// The EPSG code of a WKT's horizontal system.
///
/// Not the last `AUTHORITY` in the string, which is the trap a real file found:
/// a compound WKT ends with the vertical system's code and then the compound's
/// own, and projecting through either places the cloud nowhere. So: the first
/// `PROJCS` (or `GEOGCS`, when there is no projected system), brackets matched,
/// and the authority that closes *that* node.
pub fn horizontal_epsg(wkt: &str) -> Option<u32> {
    let node = first_node(wkt, &["PROJCS", "PROJCRS"]).or_else(|| first_node(wkt, &["GEOGCS", "GEOGCRS"]))?;
    direct_authority_in(node)
}

/// The EPSG code of a WKT's vertical system, when it declares one.
pub fn vertical_epsg(wkt: &str) -> Option<u32> {
    let node = first_node(wkt, &["VERT_CS", "VERTCRS"])?;
    direct_authority_in(node)
}

/// The name the outermost node carries.
pub fn wkt_name(wkt: &str) -> Option<String> {
    let open = wkt.find('[')?;
    let head = wkt[..open].trim();
    if head.is_empty() || !head.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
        return None;
    }
    let rest = wkt[open + 1..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The text of the first node with one of these keywords, brackets balanced.
///
/// Bracket matching rather than a pattern, because a WKT nests six levels deep
/// and stopping at the first `]` lands inside a SPHEROID. Quoted text is
/// skipped: real citations contain brackets, as in
/// `PROJCS["NAD83 / UTM zone 12N [deprecated]"]`.
fn first_node<'a>(wkt: &'a str, keywords: &[&str]) -> Option<&'a str> {
    for keyword in keywords {
        let pattern = format!("{keyword}[");
        let Some(start) = wkt.find(&pattern) else {
            continue;
        };
        let bytes = wkt.as_bytes();
        let mut depth = 0i32;
        let mut in_string = false;
        for i in start + keyword.len()..bytes.len() {
            match bytes[i] {
                b'"' => in_string = !in_string,
                b'[' if !in_string => depth += 1,
                b']' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&wkt[start..=i]);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// The `AUTHORITY["EPSG",n]` / `ID["EPSG",n]` that is a *direct child* of a node.
///
/// Not the last one anywhere in its text, which is the second trap: a `VERT_CS`
/// with no authority of its own still contains
/// `UNIT["Meter",1,AUTHORITY["EPSG","9001"]]`, and 9001 is EPSG's code for the
/// metre. Reported as a CRS it is nonsense; used as the horizontal system it is
/// a projection through a unit, which fails or — worse — does not.
fn direct_authority_in(node: &str) -> Option<u32> {
    let bytes = node.as_bytes();
    let open = node.find('[')?;
    let mut depth = 1i32;
    let mut in_string = false;
    let mut code = None;

    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_string = !in_string,
            _ if in_string => {}
            b'[' => {
                if depth == 1 {
                    let keyword = keyword_ending_at(node, i);
                    if keyword == "AUTHORITY" || keyword == "ID" {
                        if let Some(value) = epsg_argument(&node[i + 1..]) {
                            // 0 is what a writer emits for "there isn't one" —
                            // a real file declares
                            // `VERT_DATUM[..., AUTHORITY["EPSG","0"]]`.
                            if value > 0 {
                                code = Some(value);
                            }
                        }
                    }
                }
                depth += 1;
            }
            b']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    code
}

/// `"EPSG","1234"]` or `EPSG,1234]`, with the quotes optional on either side.
fn epsg_argument(text: &str) -> Option<u32> {
    let rest = text.trim_start();
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    let rest = rest
        .get(..4)
        .filter(|head| head.eq_ignore_ascii_case("EPSG"))
        .map(|_| &rest[4..])?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('"').unwrap_or(rest).trim_start();
    let rest = rest.strip_prefix(',')?.trim_start();
    let rest = rest.strip_prefix('"').unwrap_or(rest);
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let after = rest[digits.len()..].trim_start();
    let after = after.strip_prefix('"').unwrap_or(after).trim_start();
    if !after.starts_with(']') {
        return None;
    }
    digits.parse().ok()
}

/// The bare keyword immediately before an opening bracket.
fn keyword_ending_at(text: &str, bracket: usize) -> String {
    let bytes = text.as_bytes();
    let mut start = bracket;
    while start > 0 && (bytes[start - 1].is_ascii_alphabetic() || bytes[start - 1] == b'_') {
        start -= 1;
    }
    text[start..bracket].to_ascii_uppercase()
}
