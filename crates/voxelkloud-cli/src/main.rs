//! The `voxelkloud` command.
//!
//! Four commands, one library. Everything that reads or writes a point cloud is
//! `voxelkloud-io`; everything here is about being a tool — argument parsing,
//! transport, output, exit codes.
//!
//! The split is load-bearing rather than tidy. `inspect` and `doctor` both work
//! against a deployment somebody else built, so the transport has to be
//! observable — `doctor`'s entire output is properties of the HTTP exchange —
//! and a library that hid it behind a `fetch` could not report them.

mod http;
mod out;

mod cmd {
    pub mod convert;
    pub mod doctor;
    pub mod inspect;
    pub mod optimize;
    pub mod serve;
    pub mod snapshot;
}

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use out::Output;

/// Point clouds on the web: inspect them, diagnose a deployment, serve one
/// locally, convert between formats.
#[derive(Parser)]
#[command(
    name = "voxelkloud",
    version,
    about,
    long_about = None,
    disable_help_subcommand = true,
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Machine-readable output on stdout. Diagnostics still go to stderr.
    #[arg(long, global = true)]
    json: bool,

    /// Never colour the output. `NO_COLOR` in the environment does the same.
    #[arg(long, global = true)]
    no_color: bool,

    /// Only errors.
    #[arg(long, short, global = true)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Command {
    /// What is this cloud? Format, size, attributes, projection, hierarchy.
    Inspect(cmd::inspect::Args),
    /// Diagnose a deployment: range requests, CORS, encoding, MIME, hierarchy.
    Doctor(cmd::doctor::Args),
    /// Serve a directory over HTTP with byte ranges and CORS.
    Serve(cmd::serve::Args),
    /// Convert LAS, LAZ, COPC or E57 into an indexed cloud.
    Convert(cmd::convert::Args),
    /// Re-encode a cloud for better delivery, without rebuilding its tree.
    Optimize(cmd::optimize::Args),
    /// Render a PNG thumbnail of a cloud, on the CPU, no browser involved.
    Snapshot(cmd::snapshot::Args),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let out = Output::new(cli.json, cli.no_color, cli.quiet);

    let result = match &cli.command {
        Command::Inspect(args) => cmd::inspect::run(args, &out),
        Command::Doctor(args) => cmd::doctor::run(args, &out),
        Command::Serve(args) => cmd::serve::run(args, &out),
        Command::Convert(args) => cmd::convert::run(args, &out),
        Command::Optimize(args) => cmd::optimize::run(args, &out),
        Command::Snapshot(args) => cmd::snapshot::run(args, &out),
    };

    match result {
        // A command that ran and found nothing wrong.
        Ok(true) => ExitCode::SUCCESS,
        // A command that ran and found something wrong: `doctor` on a broken
        // deployment. Distinct from a crash, and the reason a CI job can gate
        // on it.
        Ok(false) => ExitCode::from(1),
        Err(err) => {
            out.error(&err.to_string());
            ExitCode::from(2)
        }
    }
}
