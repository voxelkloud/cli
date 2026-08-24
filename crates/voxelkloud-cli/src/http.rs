//! HTTP, blocking, with the exchange left visible.
//!
//! The one transport the library does not own. `inspect` needs bytes and would
//! be happy with anything; `doctor` needs the *response* — the status, the
//! headers, how long it took — because that is its entire output. So this
//! exposes both: a [`Store`] for readers, and [`probe`] for diagnostics.
//!
//! Point clouds are served as static files, and every read below is a `GET`
//! with a `Range`. That is deliberate and not an optimisation: the whole design
//! of COPC, Potree v2 and EPT rests on a server that answers 206, and a tool
//! that quietly fell back to whole-file reads would hide the one failure people
//! most need told about.

use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ureq::Agent;
use voxelkloud_io::error::{Error, Result};
use voxelkloud_io::source::{ByteSource, Store};

/// Sent on every request. A server operator reading their logs should be able
/// to tell what hit them, and the version is the first thing a bug report wants.
pub fn user_agent() -> String {
    format!("voxelkloud/{}", env!("CARGO_PKG_VERSION"))
}

pub fn agent(timeout: Duration) -> Agent {
    Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent(user_agent())
        // `doctor` has to see a 404 and a 416 as answers, not as failures. The
        // commands that want a status to be fatal check it themselves.
        .http_status_as_error(false)
        .build()
        .into()
}

/// Is this string something we would fetch?
pub fn is_url(target: &str) -> bool {
    target.starts_with("http://") || target.starts_with("https://")
}

/// Join a relative path onto a base that names a directory.
///
/// Deliberately not a URL parser. The bases this takes are the ones a person
/// pastes — a directory, or a manifest inside one — and the relative paths are
/// the fixed names the formats define. A query string is carried on the base
/// and dropped from the join, because a signature that authorises one object
/// does not authorise its siblings anyway.
pub fn join(base: &str, relative: &str) -> String {
    let base = base.split(['?', '#']).next().unwrap_or(base);
    if relative.is_empty() {
        return base.to_string();
    }
    if base.ends_with('/') {
        format!("{base}{relative}")
    } else {
        format!("{base}/{relative}")
    }
}

/// The directory the target lives in, and the name inside it.
///
/// A trailing slash means the whole thing is a directory. Otherwise the last
/// segment is a name — which may be a manifest (`metadata.json`) or a file
/// (`autzen.copc.laz`), and only reading it can say which.
pub fn split_target(target: &str) -> (String, String) {
    let clean = target.split(['?', '#']).next().unwrap_or(target);
    if clean.ends_with('/') {
        return (clean.to_string(), String::new());
    }
    match clean.rfind('/') {
        // Keep the slash: everything here joins onto a directory.
        Some(at) if at > "https://".len() => {
            (clean[..=at].to_string(), clean[at + 1..].to_string())
        }
        _ => (format!("{clean}/"), String::new()),
    }
}

/// One HTTP exchange, as `doctor` needs to see it.
pub struct Probe {
    pub url: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// What came back, capped by the caller's `max_body`.
    pub body: Vec<u8>,
    /// Wall clock for the whole exchange, body included.
    pub elapsed: Duration,
    /// A transport failure — DNS, TLS, refused, timed out. A 404 is not one.
    pub error: Option<String>,
}

impl Probe {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn ok(&self) -> bool {
        self.error.is_none() && (200..300).contains(&self.status)
    }

    pub fn body_len(&self) -> usize {
        self.body.len()
    }
}

/// Fetch a URL, optionally with a `Range`, and report everything about it.
///
/// `max_body` caps what is read: `doctor` asks for a megabyte of a file that
/// might be gigabytes, and the point is the headers.
pub fn probe(agent: &Agent, url: &str, range: Option<&str>, max_body: usize) -> Probe {
    let start = Instant::now();
    let mut request = agent.get(url);
    if let Some(range) = range {
        request = request.header("Range", range);
    }

    finish(request.call(), url, start, max_body)
}

/// Turn a ureq outcome into a [`Probe`], reading at most `max_body` bytes.
fn finish(
    outcome: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    url: &str,
    start: Instant,
    max_body: usize,
) -> Probe {
    match outcome {
        Ok(mut response) => {
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_string(),
                        v.to_str().unwrap_or("<not text>").to_string(),
                    )
                })
                .collect();
            let mut body = Vec::new();
            let read = response
                .body_mut()
                .as_reader()
                .take(max_body as u64)
                .read_to_end(&mut body);
            Probe {
                url: url.to_string(),
                status,
                headers,
                body,
                elapsed: start.elapsed(),
                error: read.err().map(|e| e.to_string()),
            }
        }
        Err(err) => Probe {
            url: url.to_string(),
            status: 0,
            headers: Vec::new(),
            body: Vec::new(),
            elapsed: start.elapsed(),
            error: Some(err.to_string()),
        },
    }
}

/// Fetch with an `Origin`, which is what makes a server answer the way it would
/// for a browser.
///
/// The distinction matters more than it sounds: a `curl` that succeeds proves
/// nothing about CORS, because CORS is enforced by the client. The only way to
/// see the policy is to ask as a page would.
pub fn probe_with_origin(agent: &Agent, url: &str, origin: &str, range: Option<&str>) -> Probe {
    let start = Instant::now();
    let mut request = agent.get(url).header("Origin", origin);
    if let Some(range) = range {
        request = request.header("Range", range);
    }
    finish(request.call(), url, start, 64)
}

/// The `OPTIONS` a browser sends before a ranged cross-origin fetch.
pub fn preflight(agent: &Agent, url: &str, origin: &str) -> Probe {
    let start = Instant::now();
    let request = agent
        .options(url)
        .header("Origin", origin)
        .header("Access-Control-Request-Method", "GET")
        .header("Access-Control-Request-Headers", "range");
    finish(request.call(), url, start, 0)
}

/// A URL prefix, as a store.
pub struct HttpStore {
    base: String,
    agent: Agent,
}

impl HttpStore {
    pub fn new(base: impl Into<String>, timeout: Duration) -> Self {
        Self {
            base: base.into(),
            agent: agent(timeout),
        }
    }

    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    pub fn url(&self, relative: &str) -> String {
        join(&self.base, relative)
    }
}

impl Store for HttpStore {
    fn open(&self, relative: &str) -> Result<Arc<dyn ByteSource>> {
        Ok(Arc::new(HttpSource::new(
            self.url(relative),
            self.agent.clone(),
        )))
    }

    fn exists(&self, relative: &str) -> bool {
        // One byte, not a HEAD: some static hosts answer HEAD with a 403 while
        // serving the object happily, and a one-byte range costs the same
        // round trip while also proving the server does ranges at all.
        let probe = probe(&self.agent, &self.url(relative), Some("bytes=0-0"), 1);
        probe.ok()
    }

    fn label(&self) -> String {
        self.base.clone()
    }
}

/// One remote object.
pub struct HttpSource {
    url: String,
    agent: Agent,
    /// Learned once, from whichever response first stated it.
    size: Mutex<Option<u64>>,
}

impl HttpSource {
    pub fn new(url: String, agent: Agent) -> Self {
        Self {
            url,
            agent,
            size: Mutex::new(None),
        }
    }

    fn remember(&self, size: u64) {
        if let Ok(mut slot) = self.size.lock() {
            *slot = Some(size);
        }
    }
}

impl ByteSource for HttpSource {
    fn size(&self) -> Result<u64> {
        if let Ok(slot) = self.size.lock() {
            if let Some(size) = *slot {
                return Ok(size);
            }
        }
        // A one-byte range answers both questions at once: whether the server
        // does ranges, and how big the object is — `Content-Range` ends with
        // the total, which `Content-Length` on a ranged response does not give.
        let probe = probe(&self.agent, &self.url, Some("bytes=0-0"), 1);
        if let Some(error) = probe.error {
            return Err(Error::Source(format!("{}: {error}", self.url)));
        }
        if let Some(total) = probe
            .header("content-range")
            .and_then(|value| value.rsplit('/').next())
            .and_then(|total| total.trim().parse::<u64>().ok())
        {
            self.remember(total);
            return Ok(total);
        }
        if probe.status == 200 {
            // The server ignored the range and sent the whole object. Its
            // `Content-Length` is then the size, and the read path below will
            // have to slice locally.
            if let Some(length) = probe
                .header("content-length")
                .and_then(|value| value.parse::<u64>().ok())
            {
                self.remember(length);
                return Ok(length);
            }
        }
        Err(Error::Source(format!(
            "{}: HTTP {} and no Content-Range or Content-Length, so the size is unknown",
            self.url, probe.status
        )))
    }

    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let end = offset + len as u64 - 1;
        let mut probe = probe(
            &self.agent,
            &self.url,
            Some(&format!("bytes={offset}-{end}")),
            len,
        );
        if let Some(error) = probe.error {
            return Err(Error::Source(format!("{}: {error}", self.url)));
        }
        match probe.status {
            206 => {}
            200 => {
                return Err(Error::Source(format!(
                    "{}: the server answered 200 to a Range request, so it does not \
                     support byte ranges. Run `voxelkloud doctor` against it.",
                    self.url
                )))
            }
            416 => {
                return Err(Error::Source(format!(
                    "{}: range {offset}-{end} is past the end of the object",
                    self.url
                )))
            }
            status => {
                return Err(Error::Source(format!(
                    "{}: HTTP {status} for range {offset}-{end}",
                    self.url
                )))
            }
        }
        if probe.body.len() != len {
            return Err(Error::Truncated {
                need: len as u64,
                got: probe.body.len() as u64,
                what: format!("{} range {offset}-{end}", self.url),
            });
        }
        Ok(std::mem::take(&mut probe.body))
    }

    fn read_all(&self) -> Result<Vec<u8>> {
        let mut probe = probe(&self.agent, &self.url, None, usize::MAX);
        if let Some(error) = probe.error {
            return Err(Error::Source(format!("{}: {error}", self.url)));
        }
        if probe.status != 200 {
            return Err(Error::Source(format!(
                "{}: HTTP {}",
                self.url, probe.status
            )));
        }
        self.remember(probe.body.len() as u64);
        Ok(std::mem::take(&mut probe.body))
    }

    fn label(&self) -> String {
        self.url.clone()
    }
}
