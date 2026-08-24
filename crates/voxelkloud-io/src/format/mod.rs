//! The five formats, behind one door.
//!
//! Each module reads its own manifest and produces a [`CloudInfo`]. What they
//! have in common is expressed by [`Cloud`], which is what the CLI holds: it
//! knows which format answered, and beyond that it asks the same three
//! questions of all of them.
//!
//! [`CloudInfo`]: crate::cloud::CloudInfo

pub mod copc;
#[cfg(feature = "e57")]
pub mod e57_file;
pub mod ept;
pub mod las_file;
pub mod potree;
pub mod tileset;

use std::sync::Arc;

use crate::cloud::{CloudInfo, HierarchyStats};
use crate::error::{Error, Result};
use crate::source::{ByteSource, FileStore, Store};

/// An opened cloud, whichever format it turned out to be.
pub enum Cloud {
    Potree(potree::PotreeCloud),
    Copc(copc::CopcCloud),
    Ept(ept::EptCloud),
    Las(las_file::LasCloud),
    #[cfg(feature = "e57")]
    E57(e57_file::E57Cloud),
    Tileset(tileset::TilesetCloud),
}

impl Cloud {
    pub fn info(&self) -> &CloudInfo {
        match self {
            Self::Potree(c) => &c.info,
            Self::Copc(c) => &c.info,
            Self::Ept(c) => &c.info,
            Self::Las(c) => &c.info,
            #[cfg(feature = "e57")]
            Self::E57(c) => &c.info,
            Self::Tileset(c) => &c.info,
        }
    }

    /// Walk the index and count what is in it.
    ///
    /// Costs whatever the format's hierarchy costs — one file for Potree, one
    /// EVLR plus a page per subtree for COPC, one JSON per page for EPT — so it
    /// is a separate call rather than part of opening. A bare LAS file has no
    /// index and says so by returning a single node.
    pub fn hierarchy(&self) -> Result<HierarchyStats> {
        match self {
            Self::Potree(c) => c.hierarchy(),
            Self::Copc(c) => c.hierarchy(),
            Self::Ept(c) => c.hierarchy(),
            Self::Las(c) => Ok(c.hierarchy()),
            #[cfg(feature = "e57")]
            Self::E57(c) => Ok(c.hierarchy()),
            Self::Tileset(c) => c.hierarchy(),
        }
    }
}

/// What a target looks like before anything has been read.
///
/// Sniffing is by document, not by name — the same rule `@voxelkloud/loader`
/// settled on. A name only decides what to *look at first*: a directory could
/// hold a Potree cloud or an EPT one, and only its contents can say which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A directory or a URL prefix.
    Prefix,
    /// A specific file: a manifest, or a `.las`/`.laz`/`.copc.laz`.
    File,
}

/// Open whatever is at `target`, deciding the format from what is there.
///
/// The store is the directory the target lives in and `name` is the target
/// within it — empty for a directory. Splitting them is the caller's job
/// because only the caller knows whether a trailing path segment is a file or a
/// prefix on its transport.
pub fn open(store: Arc<dyn Store>, name: &str, label: &str) -> Result<Cloud> {
    // A named manifest is unambiguous: whoever typed it said which format.
    match name {
        "metadata.json" => return Ok(Cloud::Potree(potree::open(store.clone(), label)?)),
        "ept.json" => return Ok(Cloud::Ept(ept::open(store.clone(), label)?)),
        "tileset.json" => return Ok(Cloud::Tileset(tileset::open(store.clone(), label)?)),
        _ => {}
    }

    if !name.is_empty() {
        let lower = name.to_ascii_lowercase();
        #[cfg(feature = "e57")]
        if lower.ends_with(".e57") {
            let source = store.open(name)?;
            return Ok(Cloud::E57(e57_file::open(source, label)?));
        }
        if lower.ends_with(".las") || lower.ends_with(".laz") {
            let source = store.open(name)?;
            // `.copc.laz` is a naming convention, not a guarantee, and a plain
            // `.laz` may well be COPC. The VLR decides — it is the first one in
            // the file, so this costs no extra request.
            return open_las_like(source, label);
        }
        if lower.ends_with(".json") {
            // Some other manifest. Read it and let the two JSON formats say
            // whether they recognise it.
            if let Ok(cloud) = potree::open_manifest(store.clone(), name, label) {
                return Ok(Cloud::Potree(cloud));
            }
            if let Ok(cloud) = tileset::open_manifest(store.clone(), name, label) {
                return Ok(Cloud::Tileset(cloud));
            }
            return Ok(Cloud::Ept(ept::open_manifest(store.clone(), name, label)?));
        }
    }

    // A prefix. Three probes, cheapest first, and no guessing beyond them.
    if store.exists("metadata.json") {
        return Ok(Cloud::Potree(potree::open(store.clone(), label)?));
    }
    if store.exists("ept.json") {
        return Ok(Cloud::Ept(ept::open(store.clone(), label)?));
    }
    if store.exists("tileset.json") {
        return Ok(Cloud::Tileset(tileset::open(store.clone(), label)?));
    }
    Err(Error::not_format(
        "a point cloud",
        format!(
            "{label} holds no metadata.json, ept.json or tileset.json, and is not \
             a .las, .laz, .copc.laz or .e57 file"
        ),
    ))
}

/// Decide between COPC and a bare LAS/LAZ by reading the VLR directory.
pub fn open_las_like(source: Arc<dyn ByteSource>, label: &str) -> Result<Cloud> {
    match copc::open(source.clone(), label) {
        Ok(cloud) => Ok(Cloud::Copc(cloud)),
        Err(Error::NotFormat { .. }) => Ok(Cloud::Las(las_file::open(source, label)?)),
        Err(other) => Err(other),
    }
}

/// Open a local path, whether it names a directory, a manifest or a file.
pub fn open_path(path: &std::path::Path) -> Result<Cloud> {
    let label = path.display().to_string();
    if path.is_dir() {
        return open(Arc::new(FileStore::new(path)), "", &label);
    }
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Source(format!("{label}: not a readable path")))?;
    open(Arc::new(FileStore::new(parent)), name, &label)
}
