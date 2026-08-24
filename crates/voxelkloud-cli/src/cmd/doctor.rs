//! `voxelkloud doctor` — why does this deployment feel broken?
//!
//! Aimed at a cloud somebody else deployed, in whatever tool they used. Every
//! streaming format here rests on a small set of HTTP behaviours, and when one
//! is missing the failure surfaces as something else entirely: no `206` looks
//! like a corrupt file, no `Access-Control-Expose-Headers` looks like a
//! truncated read, a transport `Content-Encoding` on an already-compressed
//! payload looks like a decoder bug. Each of those costs somebody an afternoon.
//!
//! So this asks the questions directly and says what to change. It grades a
//! Potree deployment as readily as one of ours — the checks are properties of
//! the server, and the format only decides which files get asked for.

use std::time::Duration;

use clap::Args as ClapArgs;
use serde_json::{json, Value};

use voxelkloud_io::cloud::{CloudInfo, FormatId, HierarchyStats};
use voxelkloud_io::error::Result;
use voxelkloud_io::format::Cloud;

use crate::http::{self, HttpStore};
use crate::out::{bytes, count, millis, Output};

#[derive(ClapArgs)]
pub struct Args {
    /// The deployed cloud: a URL to a directory, a manifest, or a COPC file.
    pub target: String,

    /// Also walk the hierarchy. More requests, and the only way to see the
    /// shape of the index.
    #[arg(long)]
    pub deep: bool,

    /// Seconds to wait on any one request.
    #[arg(long, default_value_t = 30)]
    pub timeout: u64,

    /// The browser origin to test CORS against.
    #[arg(long, default_value = "https://example.com")]
    pub origin: String,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    /// Nothing to do.
    Pass,
    /// Works, and will cost something: latency, bandwidth, a stall.
    Warn,
    /// Streaming is broken, or will be for some clients.
    Fail,
}

struct Finding {
    level: Level,
    /// Stable identifier, for a CI job that wants to allow one.
    code: &'static str,
    title: String,
    detail: String,
    /// What to change. Empty when there is nothing to do.
    fix: String,
}

impl Finding {
    fn pass(code: &'static str, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            level: Level::Pass,
            code,
            title: title.into(),
            detail: detail.into(),
            fix: String::new(),
        }
    }
    fn warn(
        code: &'static str,
        title: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            level: Level::Warn,
            code,
            title: title.into(),
            detail: detail.into(),
            fix: fix.into(),
        }
    }
    fn fail(
        code: &'static str,
        title: impl Into<String>,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            level: Level::Fail,
            code,
            title: title.into(),
            detail: detail.into(),
            fix: fix.into(),
        }
    }
}

pub fn run(args: &Args, out: &Output) -> Result<bool> {
    let mut findings: Vec<Finding> = Vec::new();

    let remote = http::is_url(&args.target);
    if !remote {
        out.note(
            "This is a local path, so the transport checks are skipped — they are \
             questions about a server. Only the shape of the cloud is graded.",
        );
    }

    // Opening the cloud comes first: the transport checks need to know which
    // file to ask for, and asking for the manifest of a format the target is
    // not would grade the wrong thing.
    let cloud = crate::cmd::inspect::open(&args.target, args.timeout)?;
    let info = cloud.info().clone();
    let probe_path = probe_target(&cloud);

    if remote {
        let (base, _) = http::split_target(&args.target);
        let store = HttpStore::new(base.clone(), Duration::from_secs(args.timeout));
        let url = http::join(&base, &probe_path);
        transport_checks(&store, &url, &args.origin, &mut findings);
    }

    let hierarchy = if args.deep {
        let stats = cloud.hierarchy()?;
        shape_checks(&info, &stats, &mut findings);
        Some(stats)
    } else {
        None
    };
    manifest_checks(&info, &mut findings);

    let worst = findings.iter().map(|f| f.level).max().unwrap_or(Level::Pass);

    if out.json {
        out.json(&document(&args.target, &info, hierarchy.as_ref(), &findings, worst));
    } else {
        print(out, &args.target, &info, hierarchy.as_ref(), &findings, worst);
    }
    Ok(worst < Level::Fail)
}

/// The file whose delivery actually matters, per format.
///
/// Not the manifest: a manifest is one small JSON read once, and every property
/// worth checking is about the *payload* — the file the viewer will ask for
/// hundreds of times, ranged, from a browser.
fn probe_target(cloud: &Cloud) -> String {
    match cloud {
        Cloud::Potree(_) => "octree.bin".to_string(),
        Cloud::Ept(c) => c.node_path(voxelkloud_io::octree::OctreeKey::ROOT),
        // A tileset's payload is one file per tile, so the first one stands in
        // for all of them: same host, same headers, same story.
        Cloud::Tileset(c) => c.probe_path(),
        // A COPC, LAS or E57 file is one object, and the target already names
        // it.
        Cloud::Copc(_) | Cloud::Las(_) | Cloud::E57(_) => String::new(),
    }
}

fn transport_checks(store: &HttpStore, url: &str, origin: &str, findings: &mut Vec<Finding>) {
    let agent = store.agent();

    // 1. Does a range come back as a range?
    let ranged = http::probe(agent, url, Some("bytes=0-15"), 64);
    if let Some(error) = &ranged.error {
        findings.push(Finding::fail(
            "unreachable",
            "the payload could not be fetched",
            format!("{}: {error}", ranged.url),
            "Check the URL, DNS and TLS before anything else here means much.",
        ));
        return;
    }

    match ranged.status {
        206 => {
            let range = ranged.header("content-range").unwrap_or("");
            findings.push(Finding::pass(
                "range",
                "byte ranges",
                format!("206 Partial Content, {} bytes, Content-Range: {range}", ranged.body_len()),
            ));
            if ranged.body_len() != 16 {
                findings.push(Finding::warn(
                    "range-length",
                    "the range came back the wrong size",
                    format!(
                        "Asked for 16 bytes and got {}. Something between here and the \
                         file is rewriting the body.",
                        ranged.body_len()
                    ),
                    "Look for a proxy or CDN transforming responses.",
                ));
            }
        }
        200 => findings.push(Finding::fail(
            "no-range",
            "the server ignores Range",
            format!(
                "Asked for bytes 0-15 and got 200 with {} bytes. Every format here \
                 streams by asking for pieces of a file; without 206 the viewer must \
                 download whole files.",
                ranged.body_len()
            ),
            "Enable byte ranges. nginx: on by default unless `gzip` rewrites the body. \
             `python -m http.server`: no; use `voxelkloud serve`. S3/GCS/R2: supported, \
             so a 200 means a proxy in front is stripping the header.",
        )),
        403 | 404 => findings.push(Finding::fail(
            "payload-missing",
            format!("HTTP {} for the payload", ranged.status),
            format!(
                "{url} answered {}. The manifest resolved, so the cloud is deployed \
                 with its data files missing or unreadable.",
                ranged.status
            ),
            "Check that the whole directory was uploaded, and that its objects are as \
             public as the manifest is.",
        )),
        status => findings.push(Finding::fail(
            "payload-status",
            format!("HTTP {status} for the payload"),
            format!("{url} answered {status}."),
            "",
        )),
    }

    // 2. Is the range being served over an encoded body?
    //
    // The worst failure in this space, because everything still looks fine: the
    // server compresses the payload, the browser transparently decodes it, and
    // the byte offsets the viewer computed no longer address what it gets.
    match ranged.header("content-encoding") {
        Some(encoding) if !encoding.eq_ignore_ascii_case("identity") => {
            findings.push(Finding::fail(
                "range-encoded",
                "the payload is transport-compressed",
                format!(
                    "The ranged response carries Content-Encoding: {encoding}. Byte \
                     offsets then address the compressed stream, which is not what any \
                     reader computed — and the payload is already compressed by its own \
                     format, so this buys nothing."
                ),
                "Exclude point cloud payloads from compression. nginx: drop \
                 `application/octet-stream` from `gzip_types` and set `gzip off` for \
                 `.bin`/`.laz`. Cloudflare and friends: turn off auto-compression for \
                 these paths.",
            ));
        }
        _ => findings.push(Finding::pass(
            "range-encoded",
            "no transport compression on the payload",
            "The bytes on the wire are the bytes in the file.",
        )),
    }

    // 3. Does the browser see the range headers?
    let cors = http::probe_with_origin(agent, url, origin, Some("bytes=0-15"));
    match cors.header("access-control-allow-origin") {
        Some(value) if value == "*" || value == origin => {
            findings.push(Finding::pass(
                "cors",
                "CORS",
                format!("Access-Control-Allow-Origin: {value}"),
            ));

            // Allowing the request is not enough. A browser hides every header
            // not named here, and a reader that cannot see `Content-Range`
            // cannot learn the file's size — which is how a COPC open fails
            // with a message about the file rather than about the policy.
            let exposed = cors
                .header("access-control-expose-headers")
                .unwrap_or("")
                .to_ascii_lowercase();
            let missing: Vec<&str> = ["content-range", "content-length", "accept-ranges"]
                .into_iter()
                .filter(|h| !(exposed.contains(h) || exposed.contains('*')))
                .collect();
            if missing.is_empty() {
                findings.push(Finding::pass(
                    "cors-expose",
                    "range headers are readable from a browser",
                    "Access-Control-Expose-Headers covers Content-Range.",
                ));
            } else {
                findings.push(Finding::warn(
                    "cors-expose",
                    "the browser cannot read the range headers",
                    format!(
                        "Access-Control-Expose-Headers does not name {}. The bytes \
                         arrive; the metadata about them does not.",
                        missing.join(", ")
                    ),
                    "Add `Access-Control-Expose-Headers: Content-Range, Content-Length, \
                     Accept-Ranges`.",
                ));
            }
        }
        Some(value) => findings.push(Finding::fail(
            "cors",
            "CORS names a different origin",
            format!(
                "Access-Control-Allow-Origin: {value}, against an Origin of {origin}. \
                 A page anywhere else cannot read this."
            ),
            "Widen the policy, or serve the viewer from the same origin.",
        )),
        None => findings.push(Finding::fail(
            "cors",
            "no CORS headers",
            format!(
                "The response to an Origin of {origin} carries no \
                 Access-Control-Allow-Origin, so a browser will refuse it. `curl` \
                 succeeding here proves nothing — it does not enforce this."
            ),
            "S3: put a CORS policy on the bucket. nginx: `add_header \
             Access-Control-Allow-Origin *`. Then re-run, because a cached preflight \
             can outlive the fix.",
        )),
    }

    // 4. Does a preflight succeed? A ranged fetch triggers one, and a server
    //    that answers the GET correctly may still refuse the OPTIONS.
    let preflight = http::preflight(agent, url, origin);
    if preflight.error.is_none() && !(200..300).contains(&preflight.status) {
        findings.push(Finding::warn(
            "preflight",
            "the CORS preflight is refused",
            format!(
                "OPTIONS with Access-Control-Request-Headers: range answered {}. A \
                 browser sends this before a ranged fetch to another origin.",
                preflight.status
            ),
            "Allow OPTIONS, and answer it with `Access-Control-Allow-Headers: Range`.",
        ));
    }

    // 5. Cache policy. Not correctness, but it decides whether a second visit
    //    to the same cloud costs the same as the first.
    match ranged.header("cache-control") {
        Some(value) if value.contains("no-store") || value.contains("no-cache") => {
            findings.push(Finding::warn(
                "cache",
                "the payload is marked uncacheable",
                format!("Cache-Control: {value}. Every pan re-fetches nodes already seen."),
                "Point cloud payloads never change. `Cache-Control: public, max-age=31536000, immutable`.",
            ))
        }
        Some(value) => findings.push(Finding::pass("cache", "caching", format!("Cache-Control: {value}"))),
        None => findings.push(Finding::warn(
            "cache",
            "no Cache-Control on the payload",
            "The browser will guess, and its guess is usually a revalidation per node.",
            "`Cache-Control: public, max-age=31536000, immutable` — these files are \
             content, not state.",
        )),
    }

    // 6. Latency, stated rather than judged. It is the term nothing in the
    //    format can improve, and it sets the floor on time to first points.
    let rtt = ranged.elapsed.as_secs_f64() * 1000.0;
    findings.push(Finding::pass(
        "latency",
        "round trip",
        format!(
            "{} for a 16-byte range. Opening a cloud is at least three of these before \
             the first point is drawn.",
            millis(rtt)
        ),
    ));
}

fn manifest_checks(info: &CloudInfo, findings: &mut Vec<Finding>) {
    if info.format == FormatId::Las {
        findings.push(Finding::fail(
            "not-indexed",
            "this file has no index",
            format!(
                "A bare {} carries no hierarchy, so there is nothing to stream: a viewer \
                 must download all {} of it before the first point appears.",
                if info.encoding.as_deref() == Some("laszip") { "LAZ" } else { "LAS" },
                info.data_bytes.map(bytes).unwrap_or_else(|| "of".into())
            ),
            "Convert it: `voxelkloud convert <file> -o out.copc.laz`.",
        ));
    }

    for warning in &info.warnings {
        findings.push(Finding::warn(
            "manifest",
            format!("manifest: {}", warning.code),
            format!("{} — {}", warning.path, warning.message),
            "",
        ));
    }

    if info.crs.is_none() {
        findings.push(Finding::warn(
            "no-crs",
            "the cloud declares no projection",
            "It renders fine on its own and cannot be placed next to anything else. \
             PotreeConverter drops the projection of everything it converts, so this is \
             expected on a Potree cloud and recoverable only from the source file."
                .to_string(),
            "Convert from the original with `voxelkloud convert`, which carries the CRS \
             through.",
        ));
    }
}

/// Checks about the shape of the index, which decide how it feels to open.
fn shape_checks(info: &CloudInfo, stats: &HierarchyStats, findings: &mut Vec<Finding>) {
    if stats.nodes == 0 {
        findings.push(Finding::fail(
            "empty-hierarchy",
            "the hierarchy holds no nodes",
            "Nothing can be drawn.",
            "Re-run the conversion; the index is missing or unreadable.",
        ));
        return;
    }

    let counted = stats.total_points();
    if info.point_count > 0 && counted != info.point_count {
        findings.push(Finding::warn(
            "point-count",
            "the index and the manifest disagree",
            format!(
                "The nodes hold {} points; the manifest claims {}.",
                count(counted),
                count(info.point_count)
            ),
            "Usually a conversion that was interrupted. Re-convert and compare again.",
        ));
    }

    // Average payload per node is the number that decides whether opening the
    // cloud is bound by bandwidth or by round trips. Below ~50 KB the request
    // overhead dominates; above a few MB the first frame waits on one file.
    if stats.data_bytes > 0 {
        let average = stats.data_bytes / stats.nodes;
        if average < 32 * 1024 {
            findings.push(Finding::warn(
                "small-nodes",
                "the nodes are small",
                format!(
                    "{} across {} nodes, {} each on average. At a typical 40 ms round \
                     trip the viewer spends more time waiting than reading.",
                    bytes(stats.data_bytes),
                    count(stats.nodes),
                    bytes(average)
                ),
                "Convert with a larger node size, or serve over HTTP/2 so the requests \
                 pipeline.",
            ));
        } else if average > 8 * 1024 * 1024 {
            findings.push(Finding::warn(
                "large-nodes",
                "the nodes are large",
                format!("{} each on average. The first frame waits on a whole one.", bytes(average)),
                "Convert with a smaller node size.",
            ));
        } else {
            findings.push(Finding::pass(
                "node-size",
                "node size",
                format!(
                    "{} across {} nodes, {} each on average.",
                    bytes(stats.data_bytes),
                    count(stats.nodes),
                    bytes(average)
                ),
            ));
        }
    }

    if stats.hierarchy_requests > 8 {
        findings.push(Finding::warn(
            "hierarchy-requests",
            "the index takes many requests to read",
            format!(
                "{} reads for {} of hierarchy. Each is a round trip before any point is \
                 drawn.",
                stats.hierarchy_requests,
                bytes(stats.hierarchy_bytes)
            ),
            "Convert with a larger hierarchy step, which trades a bigger first read for \
             fewer of them.",
        ));
    } else {
        findings.push(Finding::pass(
            "hierarchy",
            "index",
            format!(
                "depth {}, {} nodes, {} in {} read{}.",
                stats.depth,
                count(stats.nodes),
                bytes(stats.hierarchy_bytes),
                stats.hierarchy_requests,
                if stats.hierarchy_requests == 1 { "" } else { "s" }
            ),
        ));
    }
}

fn print(
    out: &Output,
    target: &str,
    info: &CloudInfo,
    hierarchy: Option<&HierarchyStats>,
    findings: &[Finding],
    worst: Level,
) {
    out.line("");
    out.heading(&format!("doctor  {target}"));
    out.field("format", info.format.title());
    out.field("points", count(info.point_count));
    if let Some(stats) = hierarchy {
        out.field("nodes", format!("{}, depth {}", count(stats.nodes), stats.depth));
    }
    out.line("");

    for finding in findings {
        let mark = match finding.level {
            Level::Pass => out.ok_mark(),
            Level::Warn => out.warn_mark(),
            Level::Fail => out.fail_mark(),
        };
        out.line(format!("  {mark:<4}  {}", out.bold(&finding.title)));
        if !finding.detail.is_empty() {
            out.line(format!("        {}", finding.detail));
        }
        if !finding.fix.is_empty() {
            out.line(format!("        {}", out.dim(&format!("fix: {}", finding.fix))));
        }
        out.line("");
    }

    let failures = findings.iter().filter(|f| f.level == Level::Fail).count();
    let warnings = findings.iter().filter(|f| f.level == Level::Warn).count();
    out.line(match worst {
        Level::Pass => "Nothing to fix.".to_string(),
        Level::Warn => format!("{warnings} thing{} to look at.", plural(warnings)),
        Level::Fail => format!(
            "{failures} thing{} broken, {warnings} to look at.",
            plural(failures)
        ),
    });
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn document(
    target: &str,
    info: &CloudInfo,
    hierarchy: Option<&HierarchyStats>,
    findings: &[Finding],
    worst: Level,
) -> Value {
    json!({
        "target": target,
        "format": info.format.name(),
        "points": info.point_count,
        "status": match worst {
            Level::Pass => "pass",
            Level::Warn => "warn",
            Level::Fail => "fail",
        },
        "hierarchy": hierarchy.map(|stats| json!({
            "nodes": stats.nodes,
            "depth": stats.depth,
            "dataBytes": stats.data_bytes,
            "indexBytes": stats.hierarchy_bytes,
            "indexReads": stats.hierarchy_requests,
        })),
        "findings": findings.iter().map(|f| json!({
            "level": match f.level {
                Level::Pass => "pass",
                Level::Warn => "warn",
                Level::Fail => "fail",
            },
            "code": f.code,
            "title": f.title,
            "detail": f.detail,
            "fix": f.fix,
        })).collect::<Vec<_>>(),
    })
}
