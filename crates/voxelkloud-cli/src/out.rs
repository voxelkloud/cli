//! Output: one human stream and one machine stream, decided once.
//!
//! Two rules, both learned from tools that break in pipelines. The data goes to
//! stdout and everything else to stderr, so `voxelkloud inspect --json | jq`
//! works while the warnings still reach the terminal. And colour is decided by
//! the environment first — `NO_COLOR`, then whether stdout is a terminal —
//! because a tool that writes escape codes into a log file is a tool people
//! wrap in `sed`.

use std::io::{IsTerminal, Write};

use serde_json::Value;

pub struct Output {
    pub json: bool,
    pub quiet: bool,
    color: bool,
}

impl Output {
    pub fn new(json: bool, no_color: bool, quiet: bool) -> Self {
        // `--json` turns colour off too: the escape codes would land inside the
        // document, and no consumer of it is a terminal.
        let color = !no_color
            && !json
            && std::env::var_os("NO_COLOR").is_none()
            && std::io::stdout().is_terminal();
        Self { json, quiet, color }
    }

    /// A line of data. Suppressed in `--json` mode, where the document is the
    /// only thing on stdout.
    pub fn line(&self, text: impl AsRef<str>) {
        if self.json || self.quiet {
            return;
        }
        println!("{}", text.as_ref());
    }

    /// A section heading.
    pub fn heading(&self, text: &str) {
        self.line(self.bold(text));
    }

    /// A `label  value` row, aligned to `width`.
    pub fn field(&self, label: &str, value: impl AsRef<str>) {
        self.line(format!("  {:<22} {}", self.dim(label), value.as_ref()));
    }

    /// The one JSON document, printed at the end of a command.
    pub fn json(&self, value: &Value) {
        if !self.json {
            return;
        }
        let mut stdout = std::io::stdout().lock();
        let _ = serde_json::to_writer_pretty(&mut stdout, value);
        let _ = stdout.write_all(b"\n");
    }

    /// A tolerated anomaly. Always stderr: it is not the answer, and a `--json`
    /// consumer gets it inside the document instead.
    pub fn warn(&self, text: impl AsRef<str>) {
        if self.quiet {
            return;
        }
        eprintln!("{} {}", self.paint("warning:", "33"), text.as_ref());
    }

    pub fn note(&self, text: impl AsRef<str>) {
        if self.quiet || self.json {
            return;
        }
        eprintln!("{}", self.dim(text.as_ref()));
    }

    pub fn error(&self, text: &str) {
        eprintln!("{} {text}", self.paint("error:", "31"));
    }

    pub fn ok_mark(&self) -> String {
        self.paint("ok", "32")
    }

    pub fn fail_mark(&self) -> String {
        self.paint("fail", "31")
    }

    pub fn warn_mark(&self) -> String {
        self.paint("warn", "33")
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint(text, "1")
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint(text, "2")
    }

    fn paint(&self, text: &str, code: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

/// Bytes as a human reads them. Binary units, because every number here is a
/// file size or a byte range and the tools people compare against use them.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A count with thousands separators, which is how a point count is read.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A duration, at the precision the number deserves.
pub fn millis(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else if ms >= 10.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{ms:.1} ms")
    }
}
