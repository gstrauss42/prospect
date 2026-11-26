//! HTTP for the image sources: JSON searches and size-capped, sniffed image downloads.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::Config;
use crate::formats::{is_complete, sniff};

/// Largest JSON reply accepted from a search API.
const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;

/// A downloaded, identified image.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub url: String,
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

impl Image {
    pub fn aspect(&self) -> f64 {
        self.width as f64 / self.height.max(1) as f64
    }
}

/// Why a download didn't produce a usable image.
#[derive(Clone, Debug, PartialEq)]
pub enum FetchError {
    /// Couldn't connect, timed out, or the connection dropped.
    Network(String),
    /// The server answered with a non-success status.
    Status(u16),
    /// Bigger than the cap.
    TooBig,
    /// Not a JPEG, PNG or WebP (an HTML error page, SVG…), or cut short.
    NotImage,
    /// Not a URL we fetch.
    BadUrl,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Network(m) => write!(f, "network: {m}"),
            FetchError::Status(s) => write!(f, "HTTP {s}"),
            FetchError::TooBig => write!(f, "too big"),
            FetchError::NotImage => write!(f, "not a complete JPEG, PNG or WebP"),
            FetchError::BadUrl => write!(f, "not an allowed URL"),
        }
    }
}

/// At most `max` requests at a time through it. Wikimedia answers 429 to bursts of thumbnail
/// downloads, so its hosts get a narrow gate while other hosts are fetched fully in parallel.
struct Gate {
    max: usize,
    busy: Mutex<usize>,
    freed: Condvar,
}

struct GateGuard<'a>(&'a Gate);

impl Gate {
    fn new(max: usize) -> Gate {
        Gate {
            max,
            busy: Mutex::new(0),
            freed: Condvar::new(),
        }
    }

    fn enter(&self) -> GateGuard<'_> {
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        while *busy >= self.max {
            busy = self.freed.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        *busy += 1;
        GateGuard(self)
    }
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        let mut busy = self.0.busy.lock().unwrap_or_else(|e| e.into_inner());
        *busy = busy.saturating_sub(1);
        self.0.freed.notify_one();
    }
}

fn is_wikimedia(url: &str) -> bool {
    model::links::host(url).is_some_and(|h| h == "wikimedia.org" || h.ends_with(".wikimedia.org"))
}

pub struct Net {
    agent: ureq::Agent,
    /// Plain http is only allowed when the sources themselves are configured over http (the
    /// mock in tests); against the real APIs every fetch is https.
    allow_http: bool,
    wikimedia: Gate,
}

impl Net {
    pub fn new(cfg: &Config) -> Net {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(cfg.user_agent.as_str())
            .timeout_connect(Some(cfg.timeout.min(Duration::from_secs(10))))
            .timeout_global(Some(cfg.timeout))
            .max_redirects(4)
            .build()
            .into();
        let allow_http =
            cfg.openverse_url.starts_with("http://") || cfg.commons_api_url.starts_with("http://");
        Net {
            agent,
            allow_http,
            wikimedia: Gate::new(2),
        }
    }

    fn allowed(&self, url: &str) -> bool {
        model::links::is_http_url(url)
            && (self.allow_http || url.trim().to_ascii_lowercase().starts_with("https://"))
    }

    /// GET a JSON API with query parameters.
    pub fn get_json(&self, url: &str, query: &[(&str, &str)]) -> Result<Value, String> {
        if !self.allowed(url) {
            return Err(format!("not an allowed URL: {url}"));
        }
        let resp = self
            .agent
            .get(url)
            .query_pairs(query.iter().copied())
            .header("Accept", "application/json")
            .call();
        let mut resp = resp.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_JSON_BYTES)
            .read_to_vec()
            .map_err(|e| e.to_string())?;
        if !(200..300).contains(&status) {
            let text = String::from_utf8_lossy(&body);
            return Err(format!(
                "HTTP {status}: {}",
                model::text::truncate(&model::text::squash(&text), 160)
            ));
        }
        serde_json::from_slice(&body).map_err(|e| format!("reply is not JSON: {e}"))
    }

    /// Download an image of at most `cap` bytes and identify it from its bytes.
    pub fn get_image(&self, url: &str, cap: usize) -> Result<Image, FetchError> {
        if !self.allowed(url) {
            return Err(FetchError::BadUrl);
        }
        let _turn = is_wikimedia(url).then(|| self.wikimedia.enter());
        let mut attempt = 0;
        let mut resp = loop {
            attempt += 1;
            // A narrow Accept makes some servers (Openverse's thumbnails) answer 406; anything
            // that isn't JPEG, PNG or WebP is rejected by sniffing anyway.
            let resp = self
                .agent
                .get(url)
                .header(
                    "Accept",
                    "image/jpeg,image/png,image/webp,image/*;q=0.8,*/*;q=0.5",
                )
                .call();
            let resp = resp.map_err(|e| FetchError::Network(e.to_string()))?;
            let status = resp.status().as_u16();
            if status == 429 && attempt == 1 {
                // One polite retry after the server's own delay (capped).
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .map(|s| Duration::from_secs(s.min(3)))
                    .unwrap_or(Duration::from_millis(1500));
                std::thread::sleep(wait);
                continue;
            }
            break resp;
        };
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(FetchError::Status(status));
        }
        if resp
            .body()
            .content_length()
            .is_some_and(|len| len > cap as u64)
        {
            return Err(FetchError::TooBig);
        }
        let bytes = match resp
            .body_mut()
            .with_config()
            .limit(cap as u64)
            .read_to_vec()
        {
            Ok(b) => b,
            Err(ureq::Error::BodyExceedsLimit(_)) => return Err(FetchError::TooBig),
            Err(e) => return Err(FetchError::Network(e.to_string())),
        };
        let (mime, width, height) = sniff(&bytes).ok_or(FetchError::NotImage)?;
        if !is_complete(&bytes) {
            return Err(FetchError::NotImage);
        }
        Ok(Image {
            url: url.to_string(),
            bytes,
            mime,
            width,
            height,
        })
    }
}

/// Run `f` on every item on its own thread and collect the results in order. A panicking job
/// yields None instead of taking the caller down.
pub fn parallel<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync) -> Vec<Option<R>> {
    let f = &f;
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .into_iter()
            .map(|item| s.spawn(move || f(item)))
            .collect();
        handles.into_iter().map(|h| h.join().ok()).collect()
    })
}
