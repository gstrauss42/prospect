//! HTTP plumbing on tiny_http: worker threads, request reading with a body cap, the CSRF rule,
//! security headers, panic isolation and one access-log line per request.
//!
//! Three tiny_http behaviours shape this file:
//!
//! * An unread body is drained when the request is dropped, into a buffer as large as the bytes
//!   still announced by `Content-Length` — one request announcing 100 TB aborts a plain tiny_http
//!   process. So bodies are always read first, an oversized one is drained only when that is
//!   cheap, and a request announcing an absurd body is abandoned (leaked), never dropped.
//! * Reads have no timeout (a read timeout on the listener would also time out `accept`). Bodies
//!   tiny_http hasn't buffered itself (over 1 KiB, chunked) are therefore read on a short-lived
//!   helper thread, so a client that stalls mid-body never holds a request worker. Writes do get
//!   a timeout (`sys::set_send_timeout`), so a client that stops reading can't either.
//! * The accept thread stops for good after any accept error (out of file descriptors, say);
//!   the workers notice and bind a fresh listener.
//!
//! For exposure on the open internet, a reverse proxy in front (body size cap, header and idle
//! timeouts) remains the recommended setup.

use std::io::Read;
use std::net::{IpAddr, TcpListener};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::util::lock;
use crate::{api, app, assets, db, sys};

/// Largest JSON body accepted.
pub const MAX_BODY: usize = 64 * 1024;
/// Largest announced body that is drained (read and discarded) after answering 413.
const MAX_DRAIN: usize = 16 * 1024 * 1024;
/// Bodies up to this size (without `Expect: 100-continue`) are read by tiny_http before the
/// request reaches us.
const TINY_HTTP_BUFFERED: usize = 1024;
/// Longest a single write to a client may block.
const SEND_TIMEOUT_SECS: i64 = 30;

pub const CSP: &str = "default-src 'self'; img-src 'self' data: https:; style-src 'self'; script-src 'self'; connect-src 'self'; font-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

// ---------------------------------------------------------------------------------------------
// Requests, responses, errors
// ---------------------------------------------------------------------------------------------

/// A request with its body already read.
pub struct Req {
    /// Upper-case method; HEAD is served as GET (tiny_http leaves the body out).
    pub method: String,
    pub path: String,
    pub query: String,
    /// Lower-case names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The client address (from the proxy headers when `TRUST_PROXY` is on).
    pub ip: Option<IpAddr>,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// A query parameter (first occurrence), percent-decoding not needed for our parameters.
    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (k == name).then_some(v)
        })
    }

    /// The client IP as a string for rate-limit keys.
    pub fn ip_key(&self) -> String {
        self.ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "unknown".into())
    }
}

pub struct Resp {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json(status: u16, value: &Value) -> Resp {
        Resp {
            status,
            content_type: "application/json",
            headers: Vec::new(),
            body: serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    pub fn empty(status: u16) -> Resp {
        Resp {
            status,
            content_type: "",
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn bytes(status: u16, content_type: &'static str, body: Vec<u8>) -> Resp {
        Resp {
            status,
            content_type,
            headers: Vec::new(),
            body,
        }
    }

    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Resp {
        self.headers.push((name, value.into()));
        self
    }

    pub fn has_header(&self, name: &str) -> bool {
        self.headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    pub fn error(e: &ApiError) -> Resp {
        let mut err = json!({ "code": e.code, "message": e.message });
        if let Some(f) = e.field {
            err["field"] = json!(f);
        }
        let mut r = Resp::json(e.status, &json!({ "error": err }));
        if let Some(secs) = e.retry_after {
            r.headers.push(("Retry-After", secs.to_string()));
        }
        r
    }
}

/// An API error: becomes `{ "error": { "code", "message", "field"? } }` with its status.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub field: Option<&'static str>,
    pub retry_after: Option<i64>,
}

impl ApiError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            code,
            message: message.into(),
            field: None,
            retry_after: None,
        }
    }

    pub fn field(mut self, field: &'static str) -> ApiError {
        self.field = Some(field);
        self
    }

    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError::new(400, "bad_request", message)
    }

    pub fn unauthorized() -> ApiError {
        ApiError::new(401, "unauthorized", "Please sign in to continue.")
    }

    pub fn forbidden() -> ApiError {
        ApiError::new(
            403,
            "forbidden",
            "This request was blocked for your security. Reload the page and try again.",
        )
    }

    pub fn not_found() -> ApiError {
        ApiError::new(404, "not_found", "We couldn't find that.")
    }

    pub fn method_not_allowed() -> ApiError {
        ApiError::new(
            405,
            "method_not_allowed",
            "That action isn't supported here.",
        )
    }

    pub fn conflict(message: impl Into<String>) -> ApiError {
        ApiError::new(409, "conflict", message)
    }

    pub fn too_large() -> ApiError {
        ApiError::new(413, "payload_too_large", "That request is too large.")
    }

    pub fn validation(field: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(422, "validation", message).field(field)
    }

    pub fn rate_limited(retry_after: i64, message: impl Into<String>) -> ApiError {
        ApiError {
            retry_after: Some(retry_after.max(1)),
            ..ApiError::new(429, "rate_limited", message)
        }
    }

    pub fn internal() -> ApiError {
        ApiError::new(
            500,
            "server_error",
            "Something went wrong on our side. Please try again.",
        )
    }

    pub fn ai_unavailable(message: impl Into<String>) -> ApiError {
        ApiError::new(503, "ai_unavailable", message)
    }
}

impl From<db::Error> for ApiError {
    fn from(e: db::Error) -> ApiError {
        error!("database: {e}");
        ApiError::internal()
    }
}

// ---------------------------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------------------------

/// Bind the listening socket (with a send timeout for every client) before the threads start.
pub fn bind(addr: &str) -> Result<TcpListener, String> {
    let listener = TcpListener::bind(addr).map_err(|e| format!("can't listen on {addr}: {e}"))?;
    if !sys::set_send_timeout(&listener, SEND_TIMEOUT_SECS) {
        debug!("no socket send timeout on this platform");
    }
    Ok(listener)
}

/// The live tiny_http server; replaced when its accept thread dies.
static CURRENT: Mutex<Option<Arc<tiny_http::Server>>> = Mutex::new(None);

fn current() -> Option<Arc<tiny_http::Server>> {
    lock(&CURRENT).clone()
}

pub struct Server {
    threads: Vec<JoinHandle<()>>,
}

pub fn start(listener: TcpListener, threads: usize) -> Result<Server, String> {
    let server = tiny_http::Server::from_listener(listener, None)
        .map_err(|e| format!("can't start the HTTP server: {e}"))?;
    *lock(&CURRENT) = Some(Arc::new(server));
    let threads = (0..threads.max(1))
        .map(|i| {
            std::thread::Builder::new()
                .name(format!("http-{i}"))
                .spawn(serve)
                .map_err(|e| format!("can't start an HTTP thread: {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Server { threads })
}

impl Server {
    /// Stop taking requests (the workers see `stopping`) and give in-flight ones up to `grace`.
    pub fn stop(self, grace: Duration) {
        let deadline = Instant::now() + grace;
        for t in &self.threads {
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        lock(&CURRENT).take();
    }
}

fn serve() {
    while !app().stopping() {
        let Some(server) = current() else {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        match server.recv_timeout(Duration::from_millis(500)) {
            Ok(Some(rq)) => handle(rq),
            Ok(None) => {}
            // tiny_http reports the error that ended its accept thread exactly once.
            Err(e) => relisten(&e),
        }
    }
}

/// Bind a fresh listener after tiny_http's accept thread stopped (it does on any accept error,
/// e.g. when the process briefly runs out of file descriptors).
fn relisten(e: &std::io::Error) {
    static BUSY: AtomicBool = AtomicBool::new(false);
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    error!("the HTTP listener stopped ({e}); starting a new one");
    while !app().stopping() {
        let fresh = bind(&app().cfg.addr)
            .and_then(|l| tiny_http::Server::from_listener(l, None).map_err(|e| e.to_string()));
        match fresh {
            Ok(server) => {
                *lock(&CURRENT) = Some(Arc::new(server));
                info!("listening again on {}", app().cfg.addr);
                break;
            }
            Err(e) => {
                warn!("couldn't listen again yet: {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
    BUSY.store(false, Ordering::SeqCst);
}

enum Body {
    Read(Vec<u8>),
    TooLarge,
    Abandon(usize),
    Broken,
}

fn read_body(rq: &mut tiny_http::Request) -> Body {
    if let Some(n) = rq.body_length() {
        if n > MAX_DRAIN {
            return Body::Abandon(n);
        }
        if n > MAX_BODY {
            return Body::TooLarge;
        }
        if n == 0 {
            return Body::Read(Vec::new());
        }
    }
    let mut buf = Vec::new();
    match rq
        .as_reader()
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut buf)
    {
        Ok(_) if buf.len() > MAX_BODY => Body::TooLarge,
        Ok(_) => Body::Read(buf),
        Err(_) => Body::Broken,
    }
}

/// True when reading the body can't block: there is none, or tiny_http already buffered it.
fn body_buffered(rq: &tiny_http::Request, req: &Req) -> bool {
    if req.header("transfer-encoding").is_some() {
        return false;
    }
    match rq.body_length() {
        None | Some(0) => true,
        Some(n) => n <= TINY_HTTP_BUFFERED && req.header("expect").is_none(),
    }
}

/// Answer a request whose body couldn't be taken, and drop it. Dropping may block while tiny_http
/// drains what the client still sends, so for bodies that weren't buffered this runs on the
/// helper thread, never on a worker.
fn reject(rq: tiny_http::Request, req: &Req, body: Body, started: Instant) {
    let resp = match body {
        Body::Read(_) | Body::TooLarge => {
            finish(Resp::error(&ApiError::too_large()), false).with_header("Connection", "close")
        }
        Body::Broken => finish(
            Resp::error(&ApiError::bad_request("The request body couldn't be read.")),
            false,
        ),
        Body::Abandon(n) => {
            // Dropping this request would make tiny_http allocate `n` bytes to drain it, which
            // can abort the process; leaking one connection is the lesser evil.
            warn!(
                "abandoned a request from {} announcing a {n}-byte body",
                req.ip_key()
            );
            std::mem::forget(rq);
            return;
        }
    };
    access_log(req, &resp, started);
    respond(rq, resp);
}

fn handle(rq: tiny_http::Request) {
    let started = Instant::now();
    let method = match rq.method().as_str() {
        "HEAD" => "GET".to_string(),
        m => m.to_ascii_uppercase(),
    };
    let url = rq.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };
    let headers: Vec<(String, String)> = rq
        .headers()
        .iter()
        .map(|h| {
            (
                h.field.as_str().as_str().to_ascii_lowercase(),
                h.value.as_str().to_string(),
            )
        })
        .collect();
    let ip = client_ip(rq.remote_addr().map(|a| a.ip()), &headers);
    let req = Req {
        method,
        path,
        query,
        headers,
        body: Vec::new(),
        ip,
    };

    // tiny_http hands upgrade requests the raw socket as their "body"; we never upgrade.
    if req
        .header("connection")
        .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"))
    {
        let resp = finish(
            Resp::error(&ApiError::bad_request(
                "Connection upgrades aren't supported.",
            )),
            false,
        );
        access_log(&req, &resp, started);
        respond(rq, resp);
        return;
    }

    if body_buffered(&rq, &req) {
        process(rq, req, started);
    } else {
        // The body may still be on its way: take it, and answer, on a helper thread, so a client
        // that stalls mid-body never holds a worker.
        let spawned = std::thread::Builder::new()
            .name("http-body".into())
            .spawn(move || process(rq, req, started));
        if let Err(e) = spawned {
            error!("couldn't start a thread for a request body: {e}");
        }
    }
}

/// Read the body, route the request and answer it.
fn process(mut rq: tiny_http::Request, mut req: Req, started: Instant) {
    match read_body(&mut rq) {
        Body::Read(body) => req.body = body,
        other => return reject(rq, &req, other, started),
    }
    let resp = match catch_unwind(AssertUnwindSafe(|| dispatch(&req))) {
        Ok(r) => r,
        Err(_) => {
            error!("panic while handling {} {}", req.method, req.path);
            finish(Resp::error(&ApiError::internal()), false)
        }
    };
    access_log(&req, &resp, started);
    respond(rq, resp);
}

fn access_log(req: &Req, resp: &Resp, started: Instant) {
    info!(
        "{} \"{} {}\" {} {}B {}ms",
        req.ip_key(),
        req.method,
        truncate_path(&req.path),
        resp.status,
        resp.body.len(),
        started.elapsed().as_millis()
    );
}

fn truncate_path(p: &str) -> &str {
    match p.char_indices().nth(200) {
        Some((i, _)) => &p[..i],
        None => p,
    }
}

fn respond(rq: tiny_http::Request, resp: Resp) {
    let mut out = tiny_http::Response::from_data(resp.body).with_status_code(resp.status);
    if !resp.content_type.is_empty()
        && let Ok(h) =
            tiny_http::Header::from_bytes(&b"Content-Type"[..], resp.content_type.as_bytes())
    {
        out.add_header(h);
    }
    for (name, value) in &resp.headers {
        if let Ok(h) = tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            out.add_header(h);
        }
    }
    if let Err(e) = rq.respond(out) {
        debug!("http: couldn't send the response: {e}");
    }
}

/// Route a request: health check, API, or the web client.
fn dispatch(req: &Req) -> Resp {
    if req.path == "/healthz" {
        return finish(health(), false);
    }
    if req.path == "/__admin" {
        if req.method != "GET" {
            return finish(Resp::error(&ApiError::method_not_allowed()), false);
        }
        return finish(crate::gate::admin_page(), true);
    }
    if req.path == "/__admin/flags" {
        if req.method != "POST" {
            return finish(Resp::error(&ApiError::method_not_allowed()), false);
        }
        if !csrf_ok(req) {
            return finish(Resp::error(&ApiError::forbidden()), false);
        }
        return finish(crate::gate::admin_flags(req), false);
    }
    if req.path == "/api/gate" {
        if req.method != "POST" {
            return finish(Resp::error(&ApiError::method_not_allowed()), false);
        }
        if !csrf_ok(req) {
            return finish(Resp::error(&ApiError::forbidden()), false);
        }
        return finish(crate::gate::unlock(req), false);
    }
    if !crate::gate::open(&req.path) && !crate::gate::unlocked(req) {
        let resp = crate::gate::locked(req);
        let html = resp.content_type.starts_with("text/html");
        return finish(resp, html);
    }
    if req.path == "/api" || req.path.starts_with("/api/") {
        if matches!(req.method.as_str(), "POST" | "PUT" | "DELETE" | "PATCH") && !csrf_ok(req) {
            return finish(Resp::error(&ApiError::forbidden()), false);
        }
        return finish(api::handle(req), false);
    }
    let resp = assets::serve(req);
    let html = resp.content_type.starts_with("text/html");
    finish(resp, html)
}

fn health() -> Resp {
    let version = env!("CARGO_PKG_VERSION");
    if db::healthy() {
        Resp::json(200, &json!({ "ok": true, "version": version }))
    } else {
        Resp::json(503, &json!({ "ok": false, "version": version }))
    }
}

/// Headers on every response, plus the CSP on HTML.
fn finish(mut resp: Resp, html: bool) -> Resp {
    resp.headers
        .push(("X-Content-Type-Options", "nosniff".into()));
    resp.headers
        .push(("Referrer-Policy", "strict-origin-when-cross-origin".into()));
    resp.headers.push(("X-Frame-Options", "DENY".into()));
    if html {
        resp.headers.push(("Content-Security-Policy", CSP.into()));
    }
    if app().cfg.secure() {
        resp.headers
            .push(("Strict-Transport-Security", "max-age=15552000".into()));
    }
    resp
}

/// The CSRF rule: JSON content type, and an `Origin` (when sent) matching the host or PUBLIC_URL.
fn csrf_ok(req: &Req) -> bool {
    let json = req.header("content-type").is_some_and(|ct| {
        ct.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/json")
    });
    if !json {
        return false;
    }
    let Some(origin) = req.header("origin") else {
        return true;
    };
    let origin = origin.trim().to_ascii_lowercase();
    if origin == app().cfg.public_origin() {
        return true;
    }
    let Some((_, authority)) = origin.split_once("://") else {
        return false; // "null" and other opaque origins
    };
    let mut hosts: Vec<&str> = req.header("host").into_iter().collect();
    if app().cfg.trust_proxy {
        hosts.extend(
            req.header("x-forwarded-host")
                .map(|h| h.split(',').next().unwrap_or("").trim()),
        );
    }
    hosts.iter().any(|h| h.eq_ignore_ascii_case(authority))
}

/// The client address (see [`forwarded_client`]).
fn client_ip(peer: Option<IpAddr>, headers: &[(String, String)]) -> Option<IpAddr> {
    let cfg = &app().cfg;
    forwarded_client(
        peer,
        headers,
        cfg.trust_proxy,
        cfg.trust_proxy_hops,
        cfg.client_ip_header.as_deref(),
    )
}

/// The client address. With `trust` (`TRUST_PROXY`) on: the edge's own client header when it's
/// configured and present (`CLIENT_IP_HEADER`, such as Cloudflare's `CF-Connecting-IP`), else the
/// `X-Forwarded-For` entry `hops` from the right (the one the outermost trusted proxy added;
/// entries further left can be forged by the visitor). Otherwise the socket peer.
fn forwarded_client(
    peer: Option<IpAddr>,
    headers: &[(String, String)],
    trust: bool,
    hops: usize,
    client_header: Option<&str>,
) -> Option<IpAddr> {
    if !trust {
        return peer;
    }
    if let Some(name) = client_header
        && let Some(ip) = headers
            .iter()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| crate::util::parse_forwarded_ip(v))
    {
        return Some(ip);
    }
    let chain: Vec<IpAddr> = headers
        .iter()
        .filter(|(k, _)| k == "x-forwarded-for")
        .flat_map(|(_, v)| v.split(','))
        .filter_map(crate::util::parse_forwarded_ip)
        .collect();
    if chain.is_empty() {
        return peer;
    }
    Some(chain[chain.len().saturating_sub(hops.max(1))])
}

#[cfg(test)]
mod client_ip_tests {
    use super::forwarded_client;
    use std::net::IpAddr;

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }
    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn without_trust_the_socket_peer_is_the_client() {
        let headers = h(&[
            ("x-forwarded-for", "1.2.3.4"),
            ("cf-connecting-ip", "5.6.7.8"),
        ]);
        assert_eq!(
            forwarded_client(ip("10.0.0.1"), &headers, false, 2, Some("cf-connecting-ip")),
            ip("10.0.0.1")
        );
    }

    #[test]
    fn one_proxy_means_the_last_entry() {
        let headers = h(&[("x-forwarded-for", "9.9.9.9, 81.2.70.4")]);
        assert_eq!(
            forwarded_client(ip("10.0.0.1"), &headers, true, 1, None),
            ip("81.2.70.4")
        );
    }

    #[test]
    fn behind_cloudflare_and_render_the_visitor_is_second_from_the_right_and_forgery_doesnt_help() {
        // A visitor's forged entry, then the visitor (added by Cloudflare), then the Cloudflare
        // server (added by Render's proxy).
        let headers = h(&[("x-forwarded-for", "6.6.6.6, 81.2.70.4, 172.68.193.161")]);
        assert_eq!(
            forwarded_client(ip("10.0.0.1"), &headers, true, 2, None),
            ip("81.2.70.4")
        );
        // Fewer entries than hops: the leftmost one is the best there is.
        let short = h(&[("x-forwarded-for", "81.2.70.4")]);
        assert_eq!(
            forwarded_client(ip("10.0.0.1"), &short, true, 2, None),
            ip("81.2.70.4")
        );
    }

    #[test]
    fn the_edge_client_header_wins_when_configured_and_valid() {
        let headers = h(&[
            ("x-forwarded-for", "81.2.70.4, 172.68.193.161"),
            ("cf-connecting-ip", "2001:db8::7"),
        ]);
        assert_eq!(
            forwarded_client(ip("10.0.0.1"), &headers, true, 2, Some("cf-connecting-ip")),
            ip("2001:db8::7")
        );
        // Not configured: ignored, however it's set.
        assert_eq!(
            forwarded_client(ip("10.0.0.1"), &headers, true, 2, None),
            ip("81.2.70.4")
        );
        // Configured but garbage or missing: X-Forwarded-For, then the peer.
        let junk = h(&[
            ("x-forwarded-for", "81.2.70.4, 172.68.193.161"),
            ("cf-connecting-ip", "not an ip"),
        ]);
        assert_eq!(
            forwarded_client(ip("10.0.0.1"), &junk, true, 2, Some("cf-connecting-ip")),
            ip("81.2.70.4")
        );
        assert_eq!(
            forwarded_client(ip("10.0.0.1"), &[], true, 2, Some("cf-connecting-ip")),
            ip("10.0.0.1")
        );
    }
}
