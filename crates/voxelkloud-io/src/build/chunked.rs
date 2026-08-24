//! The out-of-core path.
//!
//! The in-memory build needs the whole cloud at once. 100 million format-7
//! records is 3.6 GB, and the machines people convert on do not reliably have
//! it — so above a budget the points go to disk first, in a shape that lets
//! each piece be built independently.
//!
//! **Distribute, then build, then fill in the top.** Every point is written to
//! the file for its cell at some level *K*, chosen so a cell fits the budget.
//! Each cell is then read back and subdivided on its own, which produces every
//! node at level *K* and below. What is left is the levels above *K*, and they
//! are built from the survivors of the level-*K* nodes: a point kept at level
//! *K* is exactly a point that owns a cell of that grid, and the coarser grids
//! above need no finer input than that.
//!
//! The subtlety is that a point promoted to level 2 must stop being in the
//! level-*K* node it was standing in, or the total comes out too high. That is
//! why the level-*K* nodes are not written during the per-cell build: they are
//! the *leftovers* of the top pass, and they are written by it.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::octree::{child_index, OctreeKey};
use crate::record::{dequantize, position};

use super::{build_subtree, partition, BuildOptions, BuiltNode, NodeSink};

/// Records held for one cell before they are appended to its file.
const SPILL_BYTES: usize = 4 << 20;

/// How much of the budget one cell is allowed to be.
///
/// Half, not all: the per-cell build partitions its input into what a node
/// keeps and eight buckets, so it holds roughly twice the cell at its peak.
const CELL_FRACTION: f64 = 0.5;

/// Cells at level K, at most. Each is a file, and a converter that opened
/// 32,768 of them would hit a limit that is not about point clouds.
const MAX_CELLS: u64 = 4096;

pub struct ChunkedBuild {
    options: BuildOptions,
    stride: usize,
    level: u32,
    dir: PathBuf,
    /// Buffered records per cell, flushed to its file when they grow.
    pending: BTreeMap<OctreeKey, Vec<u8>>,
    pending_bytes: usize,
    counts: BTreeMap<OctreeKey, u64>,
}

impl ChunkedBuild {
    /// `expected_bytes` is the size of the whole cloud in canonical records;
    /// the level is chosen from it.
    pub fn new(
        options: BuildOptions,
        stride: usize,
        dir: &Path,
        expected_bytes: u64,
        memory_budget: usize,
    ) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let target = (memory_budget as f64 * CELL_FRACTION).max(1.0);
        let mut level = 0u32;
        while level < 4 && (expected_bytes as f64) / 8f64.powi(level as i32) > target {
            level += 1;
        }
        while 8u64.pow(level) > MAX_CELLS {
            level -= 1;
        }
        Ok(Self {
            options,
            stride,
            level,
            dir: dir.to_path_buf(),
            pending: BTreeMap::new(),
            pending_bytes: 0,
            counts: BTreeMap::new(),
        })
    }

    pub fn level(&self) -> u32 {
        self.level
    }

    /// Send a batch of canonical records to their cells.
    pub fn distribute(&mut self, records: &[u8]) -> Result<()> {
        for record in records.chunks_exact(self.stride) {
            let key = self.cell_of(record);
            let bucket = self.pending.entry(key).or_default();
            bucket.extend_from_slice(record);
            self.pending_bytes += self.stride;
            *self.counts.entry(key).or_default() += 1;
            if bucket.len() >= SPILL_BYTES {
                let bytes = std::mem::take(bucket);
                self.pending_bytes -= bytes.len();
                append(&self.path(key), &bytes)?;
            }
        }
        Ok(())
    }

    /// Which level-`K` cell a record belongs to.
    ///
    /// By walking the octree rather than by scaling into a grid: the two agree
    /// mathematically and disagree on the boundary, and the walk is the one the
    /// build itself uses, so a point can never be distributed to one cell and
    /// then partitioned into another.
    fn cell_of(&self, record: &[u8]) -> OctreeKey {
        let raw = position(record);
        let p = [
            dequantize(raw[0], self.options.scale[0], self.options.offset[0]),
            dequantize(raw[1], self.options.scale[1], self.options.offset[1]),
            dequantize(raw[2], self.options.scale[2], self.options.offset[2]),
        ];
        let mut key = OctreeKey::ROOT;
        for _ in 0..self.level {
            let center = key.bounds(&self.options.cube).center();
            key = key.child(child_index(center, p));
        }
        key
    }

    fn path(&self, key: OctreeKey) -> PathBuf {
        self.dir.join(format!("{}.bin", key.ept_name()))
    }

    /// Build every cell, then the levels above them.
    pub fn finish(mut self, sink: &mut dyn NodeSink) -> Result<()> {
        for (key, bytes) in std::mem::take(&mut self.pending) {
            if !bytes.is_empty() {
                append(&self.path(key), &bytes)?;
            }
        }
        self.pending_bytes = 0;

        // Survivors of every cell, which are the input to the top levels.
        let mut top: Vec<u8> = Vec::new();
        let cells: Vec<OctreeKey> = self.counts.keys().copied().collect();

        for key in cells {
            let path = self.path(key);
            let records = read_file(&path)?;
            std::fs::remove_file(&path).ok();
            if records.is_empty() {
                continue;
            }

            let (kept, children) = partition(&records, key, self.stride, &self.options);
            drop(records);
            // The cell's own node is NOT emitted here: the top pass will take
            // some of these points for the levels above, and what is left over
            // is what the node actually holds.
            top.extend_from_slice(&kept);
            for (index, bucket) in children.into_iter().enumerate() {
                if bucket.is_empty() {
                    continue;
                }
                build_subtree(
                    bucket,
                    key.child(index as u8),
                    self.stride,
                    &self.options,
                    sink,
                )?;
            }
        }

        // Levels 0..K, from the survivors. At K the recursion stops and hands
        // the bucket over whole — its descendants are already written.
        build_top(top, OctreeKey::ROOT, self.stride, &self.options, self.level, sink)?;

        std::fs::remove_dir_all(&self.dir).ok();
        Ok(())
    }
}

/// Like [`build_subtree`], but a node at `stop_at` takes its bucket whole and
/// does not recurse.
fn build_top(
    records: Vec<u8>,
    key: OctreeKey,
    stride: usize,
    options: &BuildOptions,
    stop_at: u32,
    sink: &mut dyn NodeSink,
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    if key.level >= stop_at {
        return sink.node(BuiltNode { key, records });
    }
    let (kept, children) = partition(&records, key, stride, options);
    sink.node(BuiltNode { key, records: kept })?;
    drop(records);
    for (index, bucket) in children.into_iter().enumerate() {
        if bucket.is_empty() {
            continue;
        }
        build_top(
            bucket,
            key.child(index as u8),
            stride,
            options,
            stop_at,
            sink,
        )?;
    }
    Ok(())
}

fn append(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    match File::open(path) {
        Ok(file) => {
            let mut out = Vec::new();
            BufReader::with_capacity(1 << 20, file).read_to_end(&mut out)?;
            Ok(out)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(Error::Io(err)),
    }
}
