//! `voxelkloud convert` — give a file an index.
//!
//! The command the whole toolchain exists for. A `.laz` on disk cannot be
//! streamed: there is no hierarchy, so a viewer downloads all of it before the
//! first point appears. Converting produces a cloud that opens in a browser at
//! any size, and the default output is COPC — a standard somebody else also
//! reads, not a format of ours.
//!
//! Several inputs, one output: the tiles a national survey ships as four
//! hundred files become one cloud, which is the shape people actually need and
//! the reason `demo/data/fetch-large.sh` exists to work around not having it.

use std::path::PathBuf;

use clap::Args as ClapArgs;
use serde_json::json;

use voxelkloud_io::convert::{convert, ConvertOptions, OutputFormat};
use voxelkloud_io::error::{Error, Result};

use crate::out::{bytes, count, millis, Output};

#[derive(ClapArgs)]
pub struct Args {
    /// LAS, LAZ, COPC or E57 files. Several are merged into one cloud.
    #[arg(required = true)]
    pub inputs: Vec<PathBuf>,

    /// Where to write. A file for `copc`, a directory for the others.
    #[arg(long, short)]
    pub output: PathBuf,

    /// `copc`, `potree`, `potree-brotli`, `ept` or `ept-binary`.
    #[arg(long, short = 'f', default_value = "copc")]
    pub format: String,

    /// Points across a node's edge. Higher is denser nodes and fewer of them.
    #[arg(long, default_value_t = voxelkloud_io::build::DEFAULT_SPAN)]
    pub span: u32,

    /// Points a node may hold without being subdivided. Larger means fewer,
    /// bigger nodes.
    #[arg(long, default_value_t = voxelkloud_io::build::DEFAULT_LEAF_POINTS)]
    pub leaf: usize,

    /// Position quantum in CRS units, e.g. `0.001`. Defaults to the finest the
    /// inputs use.
    #[arg(long)]
    pub scale: Option<f64>,

    /// Megabytes of points to hold in memory before spilling to disk.
    #[arg(long, default_value_t = 1024)]
    pub memory: u64,

    /// Where spilled points go. Defaults to a directory beside the output.
    #[arg(long)]
    pub scratch: Option<PathBuf>,

    /// Overwrite the output if it is already there.
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: &Args, out: &Output) -> Result<bool> {
    let Some(format) = OutputFormat::parse(&args.format) else {
        return Err(Error::Unsupported(format!(
            "{:?} is not an output format. Try copc, potree, potree-brotli, ept or \
             ept-binary.",
            args.format
        )));
    };

    if args.output.exists() {
        if !args.force {
            return Err(Error::Source(format!(
                "{} already exists. Pass --force to overwrite it.",
                args.output.display()
            )));
        }
        if args.output.is_dir() {
            std::fs::remove_dir_all(&args.output).map_err(Error::Io)?;
        } else {
            std::fs::remove_file(&args.output).map_err(Error::Io)?;
        }
    }

    let mut options = ConvertOptions::new(args.inputs.clone(), args.output.clone(), format);
    options.span = args.span;
    options.leaf_points = args.leaf;
    options.scale = args.scale.map(|s| [s, s, s]);
    options.memory_budget = (args.memory as usize).saturating_mul(1 << 20);
    options.scratch = args.scratch.clone();

    // Progress on stderr, and only when somebody is watching: a converter run
    // from a script should not fill a log with carriage returns.
    let interactive = !out.quiet && !out.json && std::io::IsTerminal::is_terminal(&std::io::stderr());
    let mut last = std::time::Instant::now();
    let mut progress = |done: u64, total: u64| {
        if !interactive || last.elapsed().as_millis() < 250 {
            return;
        }
        last = std::time::Instant::now();
        let percent = if total > 0 {
            done as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        eprint!("\r  reading {:>5.1}%  {} points", percent, count(done));
    };

    let report = convert(&options, &mut progress)?;
    if interactive {
        eprint!("\r{:60}\r", "");
    }

    if out.json {
        out.json(&json!({
            "output": report.write.path,
            "format": report.format.name(),
            "points": report.write.points,
            "nodes": report.write.nodes,
            "depth": report.write.depth,
            "bytes": report.write.bytes,
            "seconds": report.seconds,
            "spilled": report.spilled,
            "inputPoints": report.scan.point_count,
            "scale": report.scan.scale,
            "crs": report.scan.crs.as_ref().map(|c| c.label()),
            "warnings": report.warnings.iter().map(|w| json!({
                "code": w.code, "path": w.path, "message": w.message
            })).collect::<Vec<_>>(),
        }));
    } else {
        out.line("");
        out.heading(&format!("wrote {}", report.write.path));
        out.field("format", report.format.name());
        out.field("points", count(report.write.points));
        out.field(
            "nodes",
            format!("{}, depth {}", count(report.write.nodes), report.write.depth),
        );
        out.field("size", bytes(report.write.bytes));
        out.field(
            "took",
            format!(
                "{} ({} points/s)",
                millis(report.seconds * 1000.0),
                count((report.write.points as f64 / report.seconds.max(1e-6)) as u64)
            ),
        );
        if report.spilled {
            out.field("build", "out of core — the points went through disk");
        }
        if report.write.points != report.scan.point_count {
            // Not necessarily wrong: a truncated input has fewer points than
            // its header claims, and saying so beats a silent difference.
            out.field(
                "counted",
                format!(
                    "{} written against {} declared by the inputs",
                    count(report.write.points),
                    count(report.scan.point_count)
                ),
            );
        }
    }

    for warning in &report.warnings {
        out.warn(format!("{} — {}", warning.path, warning.message));
    }
    Ok(true)
}
