//! The only two ways this crate touches the network.
//!
//! * [`Gateway`]: HTTP **GET** to an allow-listed read endpoint of a provider's data API. It has
//!   no way to send any other method — not as a parameter, not as a header
//!   (`X-HTTP-Method-Override` and friends are refused), not via redirects (never followed).
//! * [`OAuthHttp`]: a form **POST** to the provider's token endpoint (code exchange, refresh) —
//!   an identity endpoint that can't change anything in the person's tools.
//!
//! Both check every URL before sending, refuse anything else with [`Error::Blocked`], and audit
//! every request and refusal.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::{Audit, AuditEvent, Endpoints, Error, Provider};

/// Largest response body accepted, in bytes.
pub const MAX_BODY: u64 = 4 * 1024 * 1024;

/// Extra request headers connector code may set (lowercase). Authorization comes only from
/// [`Auth`]; nothing that could change the method is ever allowed.
const ALLOWED_HEADERS: &[&str] = &["accept", "accept-language", "consistencylevel", "prefer"];

/// How a request authenticates.
#[derive(Clone, PartialEq)]
pub enum Auth {
    Bearer(String),
    Basic { user: String, secret: String },
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Bearer(_) => write!(f, "Bearer(…)"),
            Auth::Basic { user, .. } => write!(f, "Basic({user}, …)"),
        }
    }
}

impl Auth {
    fn header(&self) -> String {
        match self {
            Auth::Bearer(t) => format!("Bearer {t}"),
            Auth::Basic { user, secret } => {
                use base64::Engine as _;
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{secret}"))
                )
            }
        }
    }
}

/// A successful (2xx) JSON reply.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub status: u16,
    pub json: Value,
    /// Slack's `x-oauth-scopes` header: the scopes the token actually has.
    pub oauth_scopes: Option<String>,
}

/// The HTTP agent connectors use: no redirects (a redirect could leave the allow-list), short
/// timeouts, and non-2xx answers returned rather than raised.
pub(crate) fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(timeout))
        .user_agent(concat!(
            "Opportunity-Connector/",
            env!("CARGO_PKG_VERSION"),
            " (read-only)"
        ))
        .build()
        .into()
}

/// True for a base URL connectors may be pointed at: https anywhere, or http on loopback only.
pub(crate) fn acceptable_base(base: &str) -> bool {
    match split_url(base) {
        Some(u) => u.scheme == "https" || (u.scheme == "http" && is_loopback(&u.host)),
        None => false,
    }
}

fn is_loopback(host: &str) -> bool {
    host == "127.0.0.1" || host == "localhost" || host == "[::1]"
}

/// A URL split into the parts the checks need. Scheme and host are lowercased, default ports
/// dropped; the path is kept exactly as sent.
#[derive(Debug, PartialEq)]
pub(crate) struct Parts {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub query: String,
}

impl Parts {
    fn origin(&self) -> String {
        match self.port {
            Some(p) => format!("{}://{}:{p}", self.scheme, self.host),
            None => format!("{}://{}", self.scheme, self.host),
        }
    }
}

pub(crate) fn split_url(url: &str) -> Option<Parts> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return None;
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control())
        || url.contains('#')
        || url.contains('\\')
    {
        return None;
    }
    let (authority, path_query) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => match rest.find('?') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        },
    };
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = if authority.starts_with('[') {
        // An IPv6 literal: [::1] or [::1]:8080.
        let end = authority.find(']')?;
        let rest = &authority[end + 1..];
        let port = match rest.strip_prefix(':') {
            Some(p) => Some(p.parse::<u16>().ok()?),
            None if rest.is_empty() => None,
            None => return None,
        };
        (&authority[..=end], port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p.parse::<u16>().ok()?)),
            None => (authority, None),
        }
    };
    let host = host.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let port = match (scheme.as_str(), port) {
        ("https", Some(443)) | ("http", Some(80)) => None,
        (_, p) => p,
    };
    let (path, query) = match path_query.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path_query, ""),
    };
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    };
    Some(Parts {
        scheme,
        host,
        port,
        path,
        query: query.to_string(),
    })
}

/// The path of `url` relative to `base`, when `url` is under `base`. `base` may be a wildcard
/// origin `https://*.example.net` (exactly one extra label).
fn relative_path(url: &Parts, base: &str) -> Option<String> {
    let b = split_url(base)?;
    if b.scheme != url.scheme || b.port != url.port {
        return None;
    }
    if let Some(suffix) = b.host.strip_prefix("*.") {
        let label = url.host.strip_suffix(suffix)?.strip_suffix('.')?;
        if label.is_empty() || label.contains('.') || b.scheme != "https" {
            return None;
        }
    } else if b.host != url.host {
        return None;
    }
    let prefix = b.path.trim_end_matches('/');
    if prefix.is_empty() {
        return Some(url.path.clone());
    }
    if url.path == prefix {
        return Some("/".into());
    }
    url.path
        .strip_prefix(prefix)
        .filter(|rest| rest.starts_with('/'))
        .map(str::to_string)
}

/// True when a path is free of tricks: no empty, `.` or `..` segments, no encoded slashes, dots or
/// backslashes, no `;` parameters.
fn clean_path(path: &str) -> bool {
    if !path.starts_with('/') {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    if ["%2f", "%2e", "%5c", "%00", ";"]
        .iter()
        .any(|bad| lower.contains(bad))
    {
        return false;
    }
    let segments: Vec<&str> = path[1..].split('/').collect();
    let last = segments.len() - 1;
    segments.iter().enumerate().all(|(i, s)| {
        if *s == "." || *s == ".." {
            return false;
        }
        // An empty segment is only fine as the very end of "/" itself.
        !s.is_empty() || (i == last && path == "/")
    })
}

/// Does `path` match an allow-list pattern? `*` = one segment, final `**` = any remaining.
pub(crate) fn path_matches(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.trim_start_matches('/').split('/').collect();
    let got: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    for (i, p) in pat.iter().enumerate() {
        if *p == "**" && i == pat.len() - 1 {
            return got.len() >= i;
        }
        match got.get(i) {
            Some(g) if *p == "*" && !g.is_empty() => {}
            Some(g) if g == p => {}
            _ => return false,
        }
    }
    got.len() == pat.len()
}

/// Percent-encode for a URL query component (RFC 3986 unreserved characters stay).
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `url` with `query` pairs appended (encoded).
pub fn with_query(url: &str, query: &[(&str, &str)]) -> String {
    if query.is_empty() {
        return url.to_string();
    }
    let q: Vec<String> = query
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect();
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}{}", q.join("&"))
}

fn now() -> i64 {
    model::date::now_unix()
}

fn target_of(url: &str) -> String {
    match split_url(url) {
        Some(p) => format!("{}{}", p.origin(), p.path),
        None => model::text::truncate(url.split('?').next().unwrap_or(""), 200),
    }
}

/// Read data from a provider. The only data-API access in this crate, and GET only.
pub struct Gateway<'a> {
    provider: Provider,
    eps: &'a Endpoints,
    agent: ureq::Agent,
    audit: &'a dyn Audit,
}

impl<'a> Gateway<'a> {
    pub fn new(
        provider: Provider,
        eps: &'a Endpoints,
        agent: ureq::Agent,
        audit: &'a dyn Audit,
    ) -> Gateway<'a> {
        Gateway {
            provider,
            eps,
            agent,
            audit,
        }
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    pub fn endpoints(&self) -> &Endpoints {
        self.eps
    }

    /// Why `url` may not be read, or None when it may: it must be under one of the provider's
    /// data bases, with a clean path that matches a read pattern exactly.
    pub fn refusal(&self, url: &str) -> Option<String> {
        let Some(parts) = split_url(url) else {
            return Some("not a valid http(s) URL".into());
        };
        let bases = self.eps.data_bases(self.provider);
        let Some(rel) = bases.iter().find_map(|b| relative_path(&parts, b)) else {
            return Some(format!(
                "{} is not a {} data origin",
                parts.origin(),
                self.provider.label()
            ));
        };
        if !clean_path(&parts.path) || !clean_path(&rel) {
            return Some("the path is not allowed".into());
        }
        if !self
            .provider
            .policy()
            .read_paths
            .iter()
            .any(|p| path_matches(p, &rel))
        {
            return Some(format!("{rel} is not an allow-listed read endpoint"));
        }
        None
    }

    fn blocked(&self, url: &str, why: String) -> Error {
        self.audit.record(AuditEvent {
            provider: self.provider.id().into(),
            action: "blocked".into(),
            method: "GET".into(),
            target: target_of(url),
            at: now(),
            note: why.clone(),
            ..Default::default()
        });
        Error::Blocked(why)
    }

    /// GET a JSON document. `url` is absolute (build it from [`Gateway::endpoints`], or take a
    /// provider's next-page link); `query` pairs are appended encoded; `headers` may only be
    /// `Accept`, `Accept-Language`, `ConsistencyLevel` or `Prefer`.
    pub fn get(
        &self,
        url: &str,
        query: &[(&str, &str)],
        auth: &Auth,
        headers: &[(&str, &str)],
    ) -> Result<Reply, Error> {
        let full = with_query(url, query);
        if let Some(why) = self.refusal(&full) {
            return Err(self.blocked(&full, why));
        }
        for (name, value) in headers {
            let n = name.trim().to_ascii_lowercase();
            if !ALLOWED_HEADERS.contains(&n.as_str()) || value.contains(['\r', '\n']) {
                return Err(self.blocked(&full, format!("header {name:?} is not allowed")));
            }
        }
        let started = Instant::now();
        let mut req = self
            .agent
            .get(&full)
            .header("Authorization", &auth.header())
            .header("Accept", "application/json");
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let result = req.call();
        let ms = started.elapsed().as_millis() as u64;
        let mut resp = match result {
            Ok(r) => r,
            Err(e) => {
                self.audit_read(&full, 0, 0, ms, &e.to_string());
                return Err(Error::Network(e.to_string()));
            }
        };
        let status = resp.status().as_u16();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let oauth_scopes = header("x-oauth-scopes");
        let retry_after = header("retry-after").and_then(|s| s.trim().parse::<u64>().ok());
        if (300..400).contains(&status) {
            self.audit_read(&full, status, 0, ms, "redirect refused");
            return Err(Error::Blocked(format!(
                "the service redirected (HTTP {status}); redirects are never followed"
            )));
        }
        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string();
        let body = match body {
            Ok(b) => b,
            Err(e) => {
                self.audit_read(&full, status, 0, ms, "unreadable or too large");
                return Err(Error::Invalid(format!("reading the reply: {e}")));
            }
        };
        self.audit_read(&full, status, body.len() as u64, ms, "");
        match status {
            200..=299 => {
                let json = serde_json::from_str(&body)
                    .map_err(|e| Error::Invalid(format!("not JSON ({e})")))?;
                Ok(Reply {
                    status,
                    json,
                    oauth_scopes,
                })
            }
            401 => Err(Error::Unauthorized(error_message(&body))),
            403 => Err(Error::Forbidden(error_message(&body))),
            429 => Err(Error::RateLimited(retry_after)),
            _ => Err(Error::Provider(status, error_message(&body))),
        }
    }

    /// [`Gateway::get`] and return just the JSON.
    pub fn get_json(&self, url: &str, query: &[(&str, &str)], auth: &Auth) -> Result<Value, Error> {
        self.get(url, query, auth, &[]).map(|r| r.json)
    }

    fn audit_read(&self, url: &str, status: u16, bytes: u64, ms: u64, note: &str) {
        self.audit.record(AuditEvent {
            provider: self.provider.id().into(),
            action: "read".into(),
            method: "GET".into(),
            target: target_of(url),
            status,
            bytes,
            ms,
            at: now(),
            note: note.into(),
        });
    }
}

/// The token endpoint of a provider's identity platform: code exchange and refresh. The only
/// non-GET request in this crate, and it can't reach a data API.
pub struct OAuthHttp<'a> {
    provider: Provider,
    eps: &'a Endpoints,
    agent: ureq::Agent,
    audit: &'a dyn Audit,
}

impl<'a> OAuthHttp<'a> {
    pub fn new(
        provider: Provider,
        eps: &'a Endpoints,
        agent: ureq::Agent,
        audit: &'a dyn Audit,
    ) -> OAuthHttp<'a> {
        OAuthHttp {
            provider,
            eps,
            agent,
            audit,
        }
    }

    pub fn endpoints(&self) -> &Endpoints {
        self.eps
    }

    /// Why `url` isn't a token endpoint of this provider, or None when it is.
    pub fn refusal(&self, url: &str) -> Option<String> {
        let Some(parts) = split_url(url) else {
            return Some("not a valid http(s) URL".into());
        };
        if !parts.query.is_empty() {
            return Some("token endpoints take no query string".into());
        }
        let bases = self.eps.token_bases(self.provider);
        let Some(rel) = bases.iter().find_map(|b| relative_path(&parts, b)) else {
            return Some(format!(
                "{} is not a {} identity origin",
                parts.origin(),
                self.provider.label()
            ));
        };
        if !clean_path(&rel)
            || !self
                .provider
                .policy()
                .token_paths
                .iter()
                .any(|p| path_matches(p, &rel))
        {
            return Some(format!("{rel} is not a token endpoint"));
        }
        None
    }

    /// POST `form` (urlencoded) to a token endpoint, optionally with HTTP basic client
    /// authentication. Returns the JSON reply whatever its status, for the provider module to
    /// interpret (OAuth errors come back as JSON with 400).
    pub fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
        basic: Option<(&str, &str)>,
    ) -> Result<(u16, Value), Error> {
        let body: Vec<String> = form
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
            .collect();
        self.post(
            url,
            "application/x-www-form-urlencoded",
            body.join("&"),
            basic,
        )
    }

    /// POST a JSON body to a token endpoint (Atlassian documents its token endpoint this way).
    pub fn post_json(&self, url: &str, body: &Value) -> Result<(u16, Value), Error> {
        self.post(url, "application/json", body.to_string(), None)
    }

    fn post(
        &self,
        url: &str,
        content_type: &str,
        body: String,
        basic: Option<(&str, &str)>,
    ) -> Result<(u16, Value), Error> {
        if let Some(why) = self.refusal(url) {
            self.audit.record(AuditEvent {
                provider: self.provider.id().into(),
                action: "blocked".into(),
                method: "POST".into(),
                target: target_of(url),
                at: now(),
                note: why.clone(),
                ..Default::default()
            });
            return Err(Error::Blocked(why));
        }
        let started = Instant::now();
        let mut req = self
            .agent
            .post(url)
            .header("Content-Type", content_type)
            .header("Accept", "application/json");
        if let Some((user, secret)) = basic {
            req = req.header(
                "Authorization",
                &Auth::Basic {
                    user: user.into(),
                    secret: secret.into(),
                }
                .header(),
            );
        }
        let result = req.send(body.as_bytes());
        let ms = started.elapsed().as_millis() as u64;
        let record = |status: u16, bytes: u64, note: &str| {
            self.audit.record(AuditEvent {
                provider: self.provider.id().into(),
                action: "token".into(),
                method: "POST".into(),
                target: target_of(url),
                status,
                bytes,
                ms,
                at: now(),
                note: note.into(),
            })
        };
        let mut resp = match result {
            Ok(r) => r,
            Err(e) => {
                record(0, 0, "network error");
                return Err(Error::Network(e.to_string()));
            }
        };
        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            record(status, 0, "redirect refused");
            return Err(Error::Blocked(format!(
                "the identity service redirected (HTTP {status})"
            )));
        }
        let text = resp
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        record(status, text.len() as u64, "");
        if status == 429 {
            return Err(Error::RateLimited(None));
        }
        let json = serde_json::from_str(&text)
            .map_err(|e| Error::Invalid(format!("token reply is not JSON ({e}), HTTP {status}")))?;
        Ok((status, json))
    }
}

/// A human error message from a provider's error body (Slack, Graph and Atlassian shapes).
pub fn error_message(body: &str) -> String {
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return model::text::truncate(&model::text::squash(body), 200),
    };
    let pick = |v: &Value| v.as_str().map(str::to_string);
    pick(&v["error"]["message"])
        .or_else(|| pick(&v["error_description"]))
        .or_else(|| pick(&v["message"]))
        .or_else(|| pick(&v["error"]))
        .or_else(|| v["errorMessages"][0].as_str().map(str::to_string))
        .unwrap_or_else(|| model::text::truncate(&model::text::squash(body), 200))
}
