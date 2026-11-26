//! Email: Mailgun when configured, a local outbox otherwise.
//!
//! * Mailgun's HTTP API (`POST {base}/v3/{domain}/messages`, basic auth `api:<key>`, form body:
//!   `from`, `to`, `subject`, `text`, `html`, `o:tag`). The US endpoint is tried first and the EU
//!   one second (a domain in the other region answers 401/403/404), and the one that worked is
//!   remembered and tried first from then on — the same approach as the other apps in this
//!   workspace's parent folder. One quick retry for 429/502/503/504 and refused connections.
//! * Without `MAILGUN_API_KEY` (or domain, or sender), every email is written to
//!   `<data>/outbox/` as an RFC 5322 `.eml` (multipart/alternative, quoted-printable UTF-8) plus
//!   an `.html` preview, named so they sort by time, so development and tests can read exactly
//!   what would have been sent.
//!
//! Templates (pure functions building an [`Email`]): [`welcome`], [`digest`], [`password_reset`].
//! Owner: the imagery & mail builder.

mod mime;
mod templates;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;

pub use templates::{
    DEFAULT_ACCENT, DIGEST_MAX_ITEMS, accent_on_dark, accent_on_light, digest, password_reset,
    welcome,
};

/// Mailgun's two regions, tried in this order unless `MAILGUN_BASE_URL` says otherwise.
pub const MAILGUN_US: &str = "https://api.mailgun.net";
pub const MAILGUN_EU: &str = "https://api.eu.mailgun.net";

#[derive(Clone, Debug)]
pub struct Config {
    /// `MAILGUN_API_KEY`; None means "write to the outbox".
    pub api_key: Option<String>,
    /// `MAILGUN_DOMAIN`.
    pub domain: String,
    /// `MAILGUN_SENDER_EMAIL`: an address or `Name <address>`.
    pub sender: String,
    /// `MAILGUN_BASE_URL` overrides the endpoints (tests); default US then EU.
    pub base_urls: Vec<String>,
    /// Where outbox emails are written.
    pub outbox_dir: PathBuf,
    /// `PUBLIC_URL`, for links in emails (no trailing slash).
    pub public_url: String,
    /// The product name in emails.
    pub app_name: String,
}

impl Config {
    /// From the environment, with the outbox under `data_dir`.
    pub fn from_env(data_dir: &Path) -> Config {
        let get = |k: &str| {
            std::env::var(k)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Config {
            api_key: get("MAILGUN_API_KEY"),
            domain: get("MAILGUN_DOMAIN").unwrap_or_default(),
            sender: get("MAILGUN_SENDER_EMAIL").unwrap_or_default(),
            base_urls: get("MAILGUN_BASE_URL")
                .map(|u| vec![u.trim_end_matches('/').to_string()])
                .unwrap_or_else(|| vec![MAILGUN_US.into(), MAILGUN_EU.into()]),
            outbox_dir: data_dir.join("outbox"),
            public_url: get("PUBLIC_URL")
                .unwrap_or_else(|| "http://localhost:8080".into())
                .trim_end_matches('/')
                .to_string(),
            app_name: "Prospect".into(),
        }
    }

    /// Where a person changes how often they hear from us (linked from every digest).
    pub fn settings_url(&self) -> String {
        format!("{}/settings", self.public_url.trim_end_matches('/'))
    }
}

/// One email, with both a plain-text and an HTML body.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Email {
    pub to: String,
    pub subject: String,
    pub text: String,
    pub html: String,
    /// What kind of email this is ("welcome", "digest", "password-reset"): Mailgun's `o:tag`,
    /// the outbox file name. Set by the templates; may be empty.
    pub tag: String,
}

/// Where an email went.
#[derive(Clone, Debug, PartialEq)]
pub enum Delivery {
    Mailgun { id: String },
    Outbox { path: PathBuf },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// A recipient or message that can't be sent (bad address, empty subject…).
    Invalid(String),
    /// Mailgun refused it or couldn't be reached.
    Mailgun(String),
    /// The outbox couldn't be written.
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Invalid(m) => write!(f, "Invalid email: {m}"),
            Error::Mailgun(m) => write!(f, "Mailgun: {m}"),
            Error::Io(m) => write!(f, "Outbox: {m}"),
        }
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------------------------

/// A practical check for a deliverable address: one `@`, a sensible local part (letters, digits
/// and ``!#$%&'*+/=?^_`{|}~.-``, no leading, trailing or double dots, at most 64 characters),
/// and a domain of dot-separated labels (letters, digits, hyphens; internationalised letters
/// allowed) ending in a top-level domain of two or more letters. At most 254 characters; no
/// spaces, quotes, angle brackets or control characters.
pub fn is_valid_email(addr: &str) -> bool {
    if addr.len() > 254 || addr.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some((local, domain)) = addr.rsplit_once('@') else {
        return false;
    };
    if local.is_empty()
        || local.len() > 64
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
    {
        return false;
    }
    if !local.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || "!#$%&'*+/=?^_`{|}~.-".contains(c)
            || (!c.is_ascii() && c.is_alphanumeric())
    }) {
        return false;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 || domain.len() > 253 {
        return false;
    }
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_alphanumeric() || c == '-')
    };
    let tld = labels[labels.len() - 1];
    labels.iter().all(|l| label_ok(l))
        && tld.chars().count() >= 2
        && tld
            .chars()
            .all(|c| c.is_alphabetic() || c == '-' || c.is_ascii_digit())
        && !tld.chars().all(|c| c.is_ascii_digit())
}

/// The bare address of a recipient written as `addr` or `Name <addr>`.
fn recipient(to: &str) -> String {
    mime::parse_mailbox(to).1
}

// ---------------------------------------------------------------------------------------------
// Mailer
// ---------------------------------------------------------------------------------------------

/// Outbox file sequence, so names are unique and keep their order within one millisecond.
static OUTBOX_SEQ: AtomicU64 = AtomicU64::new(0);

/// Pause before the one retry of a transient Mailgun failure.
const RETRY_DELAY: Duration = Duration::from_millis(600);

pub struct Mailer {
    cfg: Config,
    agent: ureq::Agent,
    /// The Mailgun endpoint that last worked, tried first next time.
    endpoint: Mutex<Option<String>>,
}

impl std::fmt::Debug for Mailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mailer")
            .field("live", &self.is_live())
            .field("domain", &self.cfg.domain)
            .field("endpoint", &self.endpoint())
            .finish()
    }
}

impl Mailer {
    pub fn new(cfg: Config) -> Mailer {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(concat!("opportunity/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        Mailer {
            cfg,
            agent,
            endpoint: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// True when emails really go out through Mailgun (key, domain and sender all set).
    pub fn is_live(&self) -> bool {
        self.cfg.api_key.is_some() && !self.cfg.domain.is_empty() && !self.cfg.sender.is_empty()
    }

    /// The Mailgun endpoint that last accepted a message, if any.
    pub fn endpoint(&self) -> Option<String> {
        self.endpoint
            .lock()
            .map(|e| e.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }

    /// Send through Mailgun, or write to the outbox when Mailgun isn't configured.
    pub fn send(&self, email: &Email) -> Result<Delivery, Error> {
        let to = recipient(&email.to);
        if !is_valid_email(&to) {
            return Err(Error::Invalid(format!(
                "\"{}\" is not a valid email address",
                model::text::truncate(&email.to, 80)
            )));
        }
        let subject = model::text::squash(&email.subject);
        if subject.is_empty() {
            return Err(Error::Invalid("the subject is empty".into()));
        }
        if email.text.trim().is_empty() && email.html.trim().is_empty() {
            return Err(Error::Invalid("the message has no body".into()));
        }
        let tag = sanitize_tag(&email.tag);
        if self.is_live() {
            self.send_mailgun(email, &to, &subject, &tag)
        } else {
            self.write_outbox(email, &to, &subject, &tag)
        }
    }

    /// The From header: the configured sender, named after the app when it is a bare address.
    fn from(&self) -> (String, String) {
        let (name, addr) = mime::parse_mailbox(&self.cfg.sender);
        let addr = if is_valid_email(&addr) {
            addr
        } else {
            let host = model::links::host(&self.cfg.public_url)
                .filter(|h| h.contains('.'))
                .unwrap_or_else(|| "localhost".into());
            format!("no-reply@{host}")
        };
        let name = if name.is_empty() {
            model::text::squash(&self.cfg.app_name)
        } else {
            name
        };
        (name, addr)
    }

    fn list_unsubscribe(&self, tag: &str) -> Option<String> {
        let url = self.cfg.settings_url();
        (tag == "digest" && model::links::is_http_url(&url)).then(|| format!("<{url}>"))
    }

    /// The endpoints to try: the one that worked last time first, then the rest in order.
    fn endpoints(&self) -> Vec<String> {
        let mut list: Vec<String> = self
            .cfg
            .base_urls
            .iter()
            .map(|b| b.trim().trim_end_matches('/').to_string())
            .filter(|b| !b.is_empty())
            .collect();
        if list.is_empty() {
            list = vec![MAILGUN_US.into(), MAILGUN_EU.into()];
        }
        if let Some(known) = self.endpoint() {
            list.retain(|b| *b != known);
            list.insert(0, known);
        }
        list
    }

    fn send_mailgun(
        &self,
        email: &Email,
        to: &str,
        subject: &str,
        tag: &str,
    ) -> Result<Delivery, Error> {
        let key = self.cfg.api_key.as_deref().unwrap_or("").trim();
        let (name, addr) = self.from();
        let mut form: Vec<(&str, String)> = vec![
            ("from", mime::mailbox(&name, &addr)),
            ("to", to.to_string()),
            ("subject", subject.to_string()),
        ];
        if !email.text.trim().is_empty() {
            form.push(("text", email.text.clone()));
        }
        if !email.html.trim().is_empty() {
            form.push(("html", email.html.clone()));
        }
        if !tag.is_empty() {
            form.push(("o:tag", tag.to_string()));
        }
        // Links go straight to the pages (reset tokens never pass through a tracking redirect).
        form.push(("o:tracking-clicks", "no".into()));
        if let Some(lu) = self.list_unsubscribe(tag) {
            form.push(("h:List-Unsubscribe", lu));
        }
        let mut tried: Vec<String> = Vec::new();
        for base in self.endpoints() {
            match self.post(&base, key, &form) {
                Ok(id) => {
                    if let Ok(mut e) = self.endpoint.lock() {
                        *e = Some(base);
                    }
                    return Ok(Delivery::Mailgun { id });
                }
                Err((status, msg)) => {
                    tried.push(if status == 0 {
                        format!("{base}: {msg}")
                    } else {
                        format!("{base} answered {status}: {msg}")
                    });
                    // A domain in the other region answers 401/403/404: try that region.
                    if !matches!(status, 401 | 403 | 404) {
                        break;
                    }
                }
            }
        }
        Err(Error::Mailgun(tried.join("; ")))
    }

    /// One POST to one endpoint (plus one retry when the failure is clearly transient and the
    /// message can't have been accepted). Ok(message id) or Err((status, message)), status 0 for
    /// a network error.
    fn post(
        &self,
        base: &str,
        key: &str,
        form: &[(&str, String)],
    ) -> Result<String, (u16, String)> {
        let url = format!("{base}/v3/{}/messages", path_escape(self.cfg.domain.trim()));
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("api:{key}"))
        );
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = self
                .agent
                .post(&url)
                .header("Authorization", &auth)
                .header("Accept", "application/json")
                .send_form(form.iter().map(|(k, v)| (*k, v.as_str())));
            match result {
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    let body = resp
                        .body_mut()
                        .with_config()
                        .limit(64 * 1024)
                        .lossy_utf8(true)
                        .read_to_string()
                        .unwrap_or_default();
                    if (200..300).contains(&status) {
                        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                        let id = v["id"]
                            .as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("(no id)");
                        return Ok(id.to_string());
                    }
                    if attempt == 1 && matches!(status, 429 | 502 | 503 | 504) {
                        std::thread::sleep(RETRY_DELAY);
                        continue;
                    }
                    return Err((status, mailgun_message(&body)));
                }
                Err(e) => {
                    let never_sent = matches!(
                        e,
                        ureq::Error::ConnectionFailed
                            | ureq::Error::Timeout(ureq::Timeout::Connect)
                            | ureq::Error::Timeout(ureq::Timeout::Resolve)
                    ) || matches!(&e, ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::ConnectionRefused);
                    if attempt == 1 && never_sent {
                        std::thread::sleep(RETRY_DELAY);
                        continue;
                    }
                    return Err((0, format!("couldn't reach Mailgun: {e}")));
                }
            }
        }
    }

    fn write_outbox(
        &self,
        email: &Email,
        to: &str,
        subject: &str,
        tag: &str,
    ) -> Result<Delivery, Error> {
        let dir = &self.cfg.outbox_dir;
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Io(format!("couldn't create {}: {e}", dir.display())))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let unix = now.as_secs() as i64;
        let (name, addr) = self.from();
        let domain = addr
            .rsplit_once('@')
            .map(|(_, d)| d.to_string())
            .unwrap_or_else(|| "localhost".into());
        let mut extra: Vec<(String, String)> = Vec::new();
        if !tag.is_empty() {
            extra.push(("X-Mailgun-Tag".into(), tag.to_string()));
        }
        if let Some(lu) = self.list_unsubscribe(tag) {
            extra.push(("List-Unsubscribe".into(), lu));
        }
        let kind = if tag.is_empty() { "email" } else { tag };
        for _ in 0..1000 {
            let seq = OUTBOX_SEQ.fetch_add(1, Ordering::Relaxed);
            let stem = format!("{}-{:06}-{kind}", compact_timestamp(now), seq % 1_000_000);
            let eml_path = dir.join(format!("{stem}.eml"));
            let message = mime::Message {
                from: mime::mailbox(&name, &addr),
                to: to.to_string(),
                subject,
                text: &email.text,
                html: &email.html,
                date_unix: unix,
                message_id: format!("<{}.{seq}.{}@{domain}>", now.as_millis(), mime::token()),
                extra: extra.clone(),
            };
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&eml_path);
            let mut file = match file {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(Error::Io(format!(
                        "couldn't write {}: {e}",
                        eml_path.display()
                    )));
                }
            };
            file.write_all(mime::build(&message).as_bytes())
                .map_err(|e| Error::Io(format!("couldn't write {}: {e}", eml_path.display())))?;
            let preview = if email.html.trim().is_empty() {
                format!(
                    "<!DOCTYPE html><meta charset=\"utf-8\"><pre>{}</pre>",
                    model::text::escape_html(&email.text)
                )
            } else {
                email.html.clone()
            };
            let html_path = dir.join(format!("{stem}.html"));
            std::fs::write(&html_path, preview)
                .map_err(|e| Error::Io(format!("couldn't write {}: {e}", html_path.display())))?;
            return Ok(Delivery::Outbox { path: eml_path });
        }
        Err(Error::Io(format!(
            "couldn't find a free file name in {}",
            dir.display()
        )))
    }
}

/// "20261126T091530.123Z": sortable, safe in file names on every OS.
fn compact_timestamp(now: Duration) -> String {
    let unix = now.as_secs() as i64;
    let secs = unix.rem_euclid(model::date::DAY_SECS);
    format!(
        "{}T{:02}{:02}{:02}.{:03}Z",
        model::date::ymd(unix).replace('-', ""),
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        now.subsec_millis()
    )
}

/// Tags are short ASCII words (Mailgun: at most 128 ASCII characters).
fn sanitize_tag(tag: &str) -> String {
    tag.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Percent-encode a path segment (domains are plain, but never trust configuration blindly).
fn path_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Mailgun's `{"message": "..."}`, or a snippet of whatever came back.
fn mailgun_message(body: &str) -> String {
    let parsed: Option<String> = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_string));
    let msg = parsed.unwrap_or_else(|| body.to_string());
    let msg = model::text::truncate(&model::text::squash(&msg), 200);
    if msg.is_empty() {
        "no details".into()
    } else {
        msg
    }
}

#[cfg(test)]
mod tests;
