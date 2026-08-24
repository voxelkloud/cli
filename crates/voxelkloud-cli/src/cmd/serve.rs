//! `voxelkloud serve` — a static server that gets the three things right.
//!
//! Byte ranges, CORS, and not re-compressing what is already compressed. Every
//! streaming format in this project rests on those, and the usual answers —
//! `python -m http.server`, a default nginx, an S3 bucket behind a CDN — get at
//! least one of them wrong. `python -m http.server` answered no ranges at all
//! until 3.7 and still sends none for HEAD; a stock nginx gzips `.bin` on the
//! fly and then the browser sees a `Content-Encoding` on a payload that is
//! already Brotli inside; a bucket without a CORS policy fails in a way whose
//! error message names the wrong thing.
//!
//! So this is deliberately small and deliberately opinionated, and it is the
//! same server `voxelkloud doctor` grades other deployments against.
//!
//! Not a production server. It has no TLS, no access log worth the name, no
//! rate limiting and no write path.

use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use clap::Args as ClapArgs;

use voxelkloud_io::error::{Error, Result};

use crate::out::{bytes as human_bytes, Output};

#[derive(ClapArgs)]
pub struct Args {
    /// Directory to serve. Defaults to the working directory.
    #[arg(default_value = ".")]
    pub root: PathBuf,

    #[arg(long, short, default_value_t = 8080)]
    pub port: u16,

    /// Address to bind. `127.0.0.1` by default — `0.0.0.0` to reach it from
    /// another machine, which also exposes the directory to your network.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Value for `Access-Control-Allow-Origin`.
    #[arg(long, default_value = "*")]
    pub cors: String,

    /// How many connections to handle at once.
    #[arg(long, default_value_t = 16)]
    pub threads: usize,

    /// Seconds to put in `Cache-Control`. Zero sends no header at all.
    ///
    /// Off by default because this is a development server and a converted
    /// cloud gets rewritten into the same directory; a cached node from the
    /// previous run is a debugging session nobody enjoys. A real deployment
    /// wants a year, which is what `voxelkloud doctor` says.
    #[arg(long, default_value_t = 0)]
    pub cache: u32,
}

pub fn run(args: &Args, out: &Output) -> Result<bool> {
    let root = args
        .root
        .canonicalize()
        .map_err(|e| Error::Source(format!("{}: {e}", args.root.display())))?;
    if !root.is_dir() {
        return Err(Error::Source(format!(
            "{}: not a directory",
            root.display()
        )));
    }

    let listener = TcpListener::bind((args.host.as_str(), args.port))
        .map_err(|e| Error::Source(format!("cannot bind {}:{}: {e}", args.host, args.port)))?;
    let bound = listener.local_addr().map_err(Error::Io)?;

    out.line("");
    out.heading(&format!("serving {}", root.display()));
    out.field("url", format!("http://{bound}/"));
    out.field("cors", &args.cors);
    out.field("ranges", "yes, single range, suffix ranges included");
    if args.cache > 0 {
        out.field("cache", format!("public, max-age={}, immutable", args.cache));
    }
    out.line("");
    out.note("Ctrl-C to stop.");

    let config = Arc::new(Config {
        root,
        cors: args.cors.clone(),
        cache: args.cache,
    });

    // A fixed pool rather than a thread per connection: a browser opening a
    // large cloud makes hundreds of requests in a burst, and one thread each
    // would spend more time being created than serving.
    let (sender, receiver) = mpsc::channel::<TcpStream>();
    let receiver = Arc::new(Mutex::new(receiver));
    for _ in 0..args.threads.max(1) {
        let receiver = Arc::clone(&receiver);
        let config = Arc::clone(&config);
        std::thread::spawn(move || loop {
            let next = {
                let Ok(guard) = receiver.lock() else { return };
                guard.recv()
            };
            match next {
                Ok(stream) => serve_connection(stream, &config),
                Err(_) => return,
            }
        });
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if sender.send(stream).is_err() {
                    break;
                }
            }
            Err(err) => out.warn(format!("accept failed: {err}")),
        }
    }
    Ok(true)
}

struct Config {
    root: PathBuf,
    cors: String,
    cache: u32,
}

/// Content types by extension.
///
/// `.laz` and `.copc.laz` are `application/octet-stream` on purpose. There is a
/// registered type for LAS, nothing reads it, and an unexpected one is one more
/// thing for a proxy to decide to transform.
fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "json" => "application/json",
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "txt" | "md" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        // Enough of the web to serve a built site next to the clouds it shows,
        // which is what this ends up doing. A `sitemap.xml` handed over as
        // octet-stream is the same class of mistake `doctor` complains about
        // on other people's hosts.
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// Handle one connection, keeping it open for further requests.
///
/// Keep-alive is not a nicety here. Opening a Potree cloud is hundreds of small
/// ranged reads, and a fresh TCP handshake for each turns a local server into a
/// misleading benchmark.
fn serve_connection(stream: TcpStream, config: &Config) {
    let _ = stream.set_nodelay(true);
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut writer = BufWriter::new(write_half);

    loop {
        match handle_request(&mut reader, &mut writer, config) {
            Ok(true) => {
                if writer.flush().is_err() {
                    return;
                }
            }
            _ => {
                let _ = writer.flush();
                return;
            }
        }
    }
}

/// Read one request and answer it. `Ok(true)` to keep the connection.
fn handle_request(
    reader: &mut BufReader<TcpStream>,
    writer: &mut BufWriter<TcpStream>,
    config: &Config,
) -> std::io::Result<bool> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(false);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let mut range: Option<String> = None;
    let mut keep_alive = true;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            return Ok(false);
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("range") {
            range = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("connection") && value.eq_ignore_ascii_case("close") {
            keep_alive = false;
        }
    }

    if method == "OPTIONS" {
        respond(writer, 204, "No Content", config, &[], None, keep_alive)?;
        return Ok(keep_alive);
    }
    if method != "GET" && method != "HEAD" {
        respond_text(writer, 405, "Method Not Allowed", config, keep_alive)?;
        return Ok(keep_alive);
    }

    let Some(path) = resolve(&config.root, &target) else {
        respond_text(writer, 403, "Forbidden", config, keep_alive)?;
        return Ok(keep_alive);
    };

    // A directory listing is what makes this usable for poking at a converted
    // cloud without knowing what the converter named things.
    if path.is_dir() {
        return listing(writer, config, &path, &target, method == "HEAD", keep_alive);
    }

    let Ok(mut file) = std::fs::File::open(&path) else {
        respond_text(writer, 404, "Not Found", config, keep_alive)?;
        return Ok(keep_alive);
    };
    let size = file.metadata()?.len();
    let mime = content_type(&path);

    let Some(range) = range else {
        let headers = [
            ("Content-Type".to_string(), mime.to_string()),
            ("Content-Length".to_string(), size.to_string()),
            ("Accept-Ranges".to_string(), "bytes".to_string()),
        ];
        respond(writer, 200, "OK", config, &headers, None, keep_alive)?;
        if method == "GET" {
            std::io::copy(&mut file, writer)?;
        }
        return Ok(keep_alive);
    };

    // Only single ranges. It is the only form a browser `fetch` issues and the
    // only one any format here needs; multipart would be code nothing runs.
    let Some((start, end)) = parse_range(&range, size) else {
        let headers = [(
            "Content-Range".to_string(),
            format!("bytes */{size}"),
        )];
        respond(writer, 416, "Range Not Satisfiable", config, &headers, None, keep_alive)?;
        return Ok(keep_alive);
    };

    let length = end - start + 1;
    let headers = [
        ("Content-Type".to_string(), mime.to_string()),
        ("Content-Length".to_string(), length.to_string()),
        ("Accept-Ranges".to_string(), "bytes".to_string()),
        (
            "Content-Range".to_string(),
            format!("bytes {start}-{end}/{size}"),
        ),
    ];
    respond(writer, 206, "Partial Content", config, &headers, None, keep_alive)?;
    if method == "GET" {
        file.seek(SeekFrom::Start(start))?;
        std::io::copy(&mut file.take(length), writer)?;
    }
    Ok(keep_alive)
}

/// `bytes=start-end`, `bytes=start-`, `bytes=-suffix`.
fn parse_range(header: &str, size: u64) -> Option<(u64, u64)> {
    let spec = header.strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let (start, end) = if first.is_empty() {
        let suffix: u64 = last.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        (size.saturating_sub(suffix), size.checked_sub(1)?)
    } else {
        let start: u64 = first.parse().ok()?;
        let end = if last.is_empty() {
            size.checked_sub(1)?
        } else {
            last.parse::<u64>().ok()?.min(size.checked_sub(1)?)
        };
        (start, end)
    };
    if start > end || start >= size {
        return None;
    }
    Some((start, end))
}

/// Resolve a request target under the root, refusing anything that escapes.
fn resolve(root: &Path, target: &str) -> Option<PathBuf> {
    let path = target.split(['?', '#']).next().unwrap_or("/");
    let decoded = percent_decode(path);
    let mut out = root.to_path_buf();
    for part in decoded.split('/') {
        match part {
            "" | "." => continue,
            ".." => return None,
            _ => {
                // A component that is not plain text — an absolute path, a
                // drive prefix — is refused rather than normalised.
                let candidate = Path::new(part);
                if candidate.components().count() != 1
                    || !matches!(candidate.components().next(), Some(Component::Normal(_)))
                {
                    return None;
                }
                out.push(part);
            }
        }
    }
    Some(out)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(value) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn listing(
    writer: &mut BufWriter<TcpStream>,
    config: &Config,
    path: &Path,
    target: &str,
    head_only: bool,
    keep_alive: bool,
) -> std::io::Result<bool> {
    let index = path.join("index.html");
    if index.is_file() {
        let body = std::fs::read(&index)?;
        let headers = [
            ("Content-Type".to_string(), "text/html; charset=utf-8".to_string()),
            ("Content-Length".to_string(), body.len().to_string()),
        ];
        respond(writer, 200, "OK", config, &headers, None, keep_alive)?;
        if !head_only {
            writer.write_all(&body)?;
        }
        return Ok(keep_alive);
    }

    let mut entries: Vec<(String, Option<u64>)> = Vec::new();
    if let Ok(read) = std::fs::read_dir(path) {
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let size = entry.metadata().ok().map(|m| m.len()).filter(|_| !is_dir);
            entries.push((if is_dir { format!("{name}/") } else { name }, size));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let base = target.split(['?', '#']).next().unwrap_or("/");
    let mut body = format!(
        "<!doctype html><meta charset=utf-8><title>{base}</title>\
         <style>body{{font:14px ui-monospace,monospace;padding:2rem;max-width:60rem}}\
         a{{text-decoration:none}}td{{padding:.15rem 1.5rem .15rem 0}}</style>\
         <h1>{base}</h1><table>"
    );
    for (name, size) in entries {
        body.push_str(&format!(
            "<tr><td><a href=\"{name}\">{name}</a></td><td>{}</td></tr>",
            size.map(human_bytes).unwrap_or_default()
        ));
    }
    body.push_str("</table>");

    let headers = [
        ("Content-Type".to_string(), "text/html; charset=utf-8".to_string()),
        ("Content-Length".to_string(), body.len().to_string()),
    ];
    respond(writer, 200, "OK", config, &headers, None, keep_alive)?;
    if !head_only {
        writer.write_all(body.as_bytes())?;
    }
    Ok(keep_alive)
}

fn respond_text(
    writer: &mut BufWriter<TcpStream>,
    status: u16,
    reason: &str,
    config: &Config,
    keep_alive: bool,
) -> std::io::Result<()> {
    let body = format!("{status} {reason}\n");
    let headers = [
        ("Content-Type".to_string(), "text/plain; charset=utf-8".to_string()),
        ("Content-Length".to_string(), body.len().to_string()),
    ];
    respond(writer, status, reason, config, &headers, Some(&body), keep_alive)
}

fn respond(
    writer: &mut BufWriter<TcpStream>,
    status: u16,
    reason: &str,
    config: &Config,
    headers: &[(String, String)],
    body: Option<&str>,
    keep_alive: bool,
) -> std::io::Result<()> {
    write!(writer, "HTTP/1.1 {status} {reason}\r\n")?;
    write!(
        writer,
        "Access-Control-Allow-Origin: {}\r\n\
         Access-Control-Allow-Methods: GET, HEAD, OPTIONS\r\n\
         Access-Control-Allow-Headers: Range, Content-Type\r\n\
         Access-Control-Expose-Headers: Content-Range, Content-Length, Accept-Ranges\r\n",
        config.cors
    )?;
    if config.cache > 0 {
        write!(
            writer,
            "Cache-Control: public, max-age={}, immutable\r\n",
            config.cache
        )?;
    }
    // Never a `Content-Encoding`. Every payload this serves is either already
    // compressed by its own format or is a small manifest, and a transport
    // encoding on top is the failure `doctor` exists to find.
    write!(
        writer,
        "Connection: {}\r\n",
        if keep_alive { "keep-alive" } else { "close" }
    )?;
    let mut has_length = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-length") {
            has_length = true;
        }
        write!(writer, "{name}: {value}\r\n")?;
    }
    if !has_length && body.is_none() && status == 204 {
        write!(writer, "Content-Length: 0\r\n")?;
    }
    write!(writer, "\r\n")?;
    if let Some(body) = body {
        writer.write_all(body.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_parse_the_three_forms() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        // Past the end clamps rather than failing: the spec says a range whose
        // end exceeds the length is satisfied by the rest of the file.
        assert_eq!(parse_range("bytes=900-2000", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=0-10,20-30", 1000), None);
        assert_eq!(parse_range("chunks=0-10", 1000), None);
    }

    #[test]
    fn resolve_refuses_anything_that_escapes() {
        let root = Path::new("/srv/clouds");
        assert_eq!(
            resolve(root, "/autzen/metadata.json"),
            Some(PathBuf::from("/srv/clouds/autzen/metadata.json"))
        );
        assert_eq!(resolve(root, "/../etc/passwd"), None);
        assert_eq!(resolve(root, "/autzen/../../etc/passwd"), None);
        // Percent-encoded traversal is the same attack spelled differently, and
        // decoding before the check is what makes it fail too.
        assert_eq!(resolve(root, "/%2e%2e/etc/passwd"), None);
    }
}
