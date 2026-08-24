//! `voxelkloud optimize` — the same cloud, better bytes.
//!
//! The second thing you can run against a deployment somebody else built, and
//! the pair to `doctor`: one says the payloads are bigger than they need to be,
//! the other fixes it without rebuilding anything.
//!
//! What it does *not* do is the point. Converting a survey is minutes of work
//! and produces a different tree, with different nodes holding different
//! points; a viewer's cache, a CDN's cache and any URL anybody wrote down are
//! all invalidated by it. This reads the tree that exists and rewrites only the
//! bytes inside each node, so every node keeps its key, its count and its
//! place.

use std::path::PathBuf;

use clap::Args as ClapArgs;
use serde_json::json;

use voxelkloud_io::error::{Error, Result};
use voxelkloud_io::optimize::{optimize, OptimizeOptions};
use voxelkloud_io::write::potree::PotreeEncoding;

use crate::out::{bytes, count, millis, Output};

#[derive(ClapArgs)]
pub struct Args {
    /// A Potree v2 directory.
    pub input: PathBuf,

    /// Where to write the re-encoded cloud.
    #[arg(long, short)]
    pub output: PathBuf,

    /// `brotli` or `default`. Omit to keep the encoding the cloud already uses.
    #[arg(long, short)]
    pub encoding: Option<String>,

    /// Attribute names to leave out, comma separated, spelled as the manifest
    /// spells them: `--drop gps-time,"point source id"`.
    #[arg(long, value_delimiter = ',')]
    pub drop: Vec<String>,

    /// Brotli quality, 0 to 11. Higher is smaller and slower.
    #[arg(long, default_value_t = 6)]
    pub quality: u32,

    /// Overwrite the output if it is already there.
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: &Args, out: &Output) -> Result<bool> {
    let encoding = match args.encoding.as_deref() {
        None => None,
        Some("brotli" | "BROTLI") => Some(PotreeEncoding::Brotli),
        Some("default" | "DEFAULT") => Some(PotreeEncoding::Default),
        Some(other) => {
            return Err(Error::Unsupported(format!(
                "{other:?} is not an encoding. Potree v2 has two: default and brotli."
            )))
        }
    };

    if args.output.exists() {
        if !args.force {
            return Err(Error::Source(format!(
                "{} already exists. Pass --force to overwrite it.",
                args.output.display()
            )));
        }
        std::fs::remove_dir_all(&args.output).map_err(Error::Io)?;
    }

    let mut options = OptimizeOptions::new(args.input.clone(), args.output.clone());
    options.encoding = encoding;
    options.drop = args.drop.clone();
    options.quality = args.quality;

    let interactive =
        !out.quiet && !out.json && std::io::IsTerminal::is_terminal(&std::io::stderr());
    let mut last = std::time::Instant::now();
    let started = std::time::Instant::now();
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
        eprint!("\r  re-encoding {:>5.1}%  {} points", percent, count(done));
    };

    let report = optimize(&options, &mut progress)?;
    let seconds = started.elapsed().as_secs_f64();
    if interactive {
        eprint!("\r{:60}\r", "");
    }

    if out.json {
        out.json(&json!({
            "output": args.output.display().to_string(),
            "nodes": report.nodes,
            "points": report.points,
            "bytesBefore": report.bytes_before,
            "bytesAfter": report.bytes_after,
            "recordBefore": report.record_before,
            "recordAfter": report.record_after,
            "encodingBefore": report.encoding_before,
            "encodingAfter": report.encoding_after,
            "dropped": report.dropped,
            "seconds": seconds,
            "warnings": report.warnings.iter().map(|w| json!({
                "code": w.code, "path": w.path, "message": w.message
            })).collect::<Vec<_>>(),
        }));
    } else {
        out.line("");
        out.heading(&format!("wrote {}", args.output.display()));
        out.field(
            "nodes",
            format!("{} kept, {} points", count(report.nodes), count(report.points)),
        );
        if report.encoding_before != report.encoding_after {
            out.field(
                "encoding",
                format!("{} → {}", report.encoding_before, report.encoding_after),
            );
        } else {
            out.field("encoding", &report.encoding_before);
        }
        if !report.dropped.is_empty() {
            out.field(
                "dropped",
                format!(
                    "{} — {} bytes per point saved",
                    report.dropped.join(", "),
                    report.record_before - report.record_after
                ),
            );
        }
        out.field(
            "payload",
            match report.ratio() {
                Some(ratio) => format!(
                    "{} → {} ({:.0}% of what it was)",
                    bytes(report.bytes_before),
                    bytes(report.bytes_after),
                    ratio * 100.0
                ),
                None => bytes(report.bytes_after),
            },
        );
        out.field("took", millis(seconds * 1000.0));
    }

    for warning in &report.warnings {
        out.warn(format!("{} — {}", warning.path, warning.message));
    }
    Ok(true)
}
