//! Hard read-only connectors to a person's work tools: Slack, Microsoft 365 (Outlook mail and
//! calendar) and Confluence. They let the engine find opportunities in the person's own work
//! context — an internal role, an invitation to speak, an RFP in the inbox, a hackathon announced
//! in a channel — and understand what they work on.
//!
//! # The read-only guarantee
//!
//! Nothing in this crate can change anything in a connected tool. That is enforced in four
//! independent layers, so no single bug or compromised component can turn a read into a write:
//!
//! 1. **The credential can't write.** We request only read scopes from the provider
//!    ([`Policy::requested_scopes`]). Where the provider lets the person or an admin grant more,
//!    the token still only carries what was requested.
//! 2. **We check what was granted.** Every token — on connect, on every refresh, and for Slack on
//!    every single call (its `x-oauth-scopes` header) — is checked against the provider's
//!    read-only allow-list ([`Policy::allowed_scopes`]) *and* a hard-coded deny-list of write
//!    patterns ([`verify_scopes`]). A token that can do more is refused with
//!    [`Error::WriteScopes`]: the connection is not saved, or is suspended.
//! 3. **There is no code path that writes.** Connector code reaches data only through
//!    [`Gateway`], whose one network method is an HTTP `GET`, to an allow-listed origin, on an
//!    allow-listed read endpoint ([`Policy::read_paths`] — exact Slack methods such as
//!    `search.messages`, because Slack accepts GET even for write methods), with no redirects,
//!    no method-override headers and no path tricks. The only other request the crate can make is
//!    [`OAuthHttp`]'s form POST to the identity provider's *token* endpoint, which can't reach a
//!    data API. Everything is audited ([`Audit`]), including refusals.
//! 4. **The AI never gets tools.** Connectors are called by deterministic server code; the model
//!    only ever receives text (as [`model::ContextItem`]s) and can't cause any request at all.
//!
//! Plus data minimisation: content is fetched at run time, trimmed, passed to the engine and not
//! stored; tokens are stored encrypted (the `vault` crate) and never reach the browser.
//!
//! # Shape
//!
//! [`Connectors`] is the facade the server uses: OAuth begin/finish, connecting with a pasted
//! token where the provider allows it, refresh, verify and fetch. Each provider lives in its own
//! module ([`slack`], [`microsoft`], [`confluence`]) with its [`Policy`] and these functions:
//! `authorize_url`, `exchange_code`, `refresh`, `connect_with_token`, `verify`, `fetch`.
//!
//! The core (this file, `gateway.rs`, `pkce.rs`) is owned by the lead; each provider module by
//! its connector builder.

pub mod confluence;
pub mod gateway;
pub mod microsoft;
pub mod pkce;
pub mod slack;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use gateway::{Auth, Gateway, OAuthHttp, Reply};
pub use model::ContextItem;
pub use pkce::Pkce;

// ---------------------------------------------------------------------------------------------
// Providers and their read-only policies
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Slack,
    Microsoft,
    Confluence,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Slack, Provider::Microsoft, Provider::Confluence];

    pub fn id(self) -> &'static str {
        match self {
            Provider::Slack => "slack",
            Provider::Microsoft => "microsoft",
            Provider::Confluence => "confluence",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::Slack => "Slack",
            Provider::Microsoft => "Microsoft 365",
            Provider::Confluence => "Confluence",
        }
    }

    pub fn parse(s: &str) -> Option<Provider> {
        Provider::ALL
            .into_iter()
            .find(|p| p.id() == s.trim().to_ascii_lowercase())
    }

    pub fn policy(self) -> &'static Policy {
        match self {
            Provider::Slack => &slack::POLICY,
            Provider::Microsoft => &microsoft::POLICY,
            Provider::Confluence => &confluence::POLICY,
        }
    }
}

/// What a provider may be asked for: the read-only allow-lists.
#[derive(Debug)]
pub struct Policy {
    /// Read endpoints the [`Gateway`] may GET, as paths relative to the provider's API base.
    /// A pattern matches the whole path; `*` matches exactly one segment, and a final `**`
    /// matches any number of further segments. Keep them exact: on Slack even GET can write.
    pub read_paths: &'static [&'static str],
    /// Token endpoints [`OAuthHttp`] may POST to, relative to the provider's auth base.
    pub token_paths: &'static [&'static str],
    /// The scopes requested at authorization.
    pub requested_scopes: &'static [&'static str],
    /// Every scope a token may carry. Anything else on a token → [`Error::WriteScopes`].
    pub allowed_scopes: &'static [&'static str],
    /// Whether a person can connect by pasting a token (instead of OAuth).
    pub token_mode: bool,
    /// What the person is told the connection can read, in one plain line.
    pub reads: &'static str,
}

/// Substrings that mark a scope as write-capable, whatever any allow-list says. Matched
/// case-insensitively. This is the backstop if someone ever adds a write scope by mistake.
const WRITE_MARKERS: &[&str] = &[
    "write",
    "readwrite",
    "send",
    "admin",
    "manage",
    "delete",
    "create",
    "update",
    "edit",
    "post",
    "fullcontrol",
    "full_access",
    "impersonat",
    "chat:write",
    "files:write",
    "remote_files",
    "commands",
    "incoming-webhook",
    "dnd:write",
    "pins:write",
    "reactions:write",
    "reminders:write",
    "stars:write",
    "usergroups:write",
    "users.profile:write",
    "bookmarks:write",
];

/// Scopes that only identify the user or keep the session alive. Always acceptable.
const IDENTITY_SCOPES: &[&str] = &["openid", "profile", "email", "offline_access"];

/// Normalise one scope: trim, lowercase, and drop a Microsoft Graph resource prefix.
fn normalize_scope(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    let s = s.strip_prefix("https://graph.microsoft.com/").unwrap_or(&s);
    s.to_string()
}

/// Split a scope string as providers send it (space- or comma-separated).
pub fn parse_scopes(s: &str) -> Vec<String> {
    s.split([' ', ','])
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect()
}

/// Ok when every granted scope is a known read-only scope of the provider. Otherwise
/// [`Error::WriteScopes`] with the offending scopes, as granted.
pub fn verify_scopes(p: Provider, granted: &[String]) -> Result<(), Error> {
    let allowed: Vec<String> = p
        .policy()
        .allowed_scopes
        .iter()
        .map(|s| normalize_scope(s))
        .collect();
    let mut bad = Vec::new();
    for g in granted {
        let n = normalize_scope(g);
        if n.is_empty() {
            continue;
        }
        let identity = IDENTITY_SCOPES.contains(&n.as_str());
        let marked = WRITE_MARKERS.iter().any(|m| n.contains(m));
        if marked || !(identity || allowed.contains(&n)) {
            bad.push(g.trim().to_string());
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(Error::WriteScopes(bad))
    }
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// The gateway refused a request that isn't an allow-listed read: a bug or an attack.
    /// Never retried; always audited.
    Blocked(String),
    /// The token can do more than read. The connection is refused (or suspended) until the
    /// person reconnects with read-only access.
    WriteScopes(Vec<String>),
    /// The provider's OAuth app isn't configured on this server.
    NotConfigured(String),
    /// Not available for this provider (e.g. pasting a Microsoft token).
    Unsupported(String),
    /// The token was rejected (expired or revoked): refresh, or reconnect.
    Unauthorized(String),
    /// The account isn't allowed to read this.
    Forbidden(String),
    /// Too many requests; seconds to wait when the provider said.
    RateLimited(Option<u64>),
    /// The provider answered with an error.
    Provider(u16, String),
    Network(String),
    /// The provider's reply wasn't what was expected.
    Invalid(String),
}

impl Error {
    /// True when the person has to reconnect (nothing will work until they do).
    pub fn needs_reconnect(&self) -> bool {
        matches!(self, Error::Unauthorized(_) | Error::WriteScopes(_))
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Blocked(m) => write!(f, "Refused a request that isn't a permitted read: {m}"),
            Error::WriteScopes(s) => write!(
                f,
                "This connection could do more than read ({}), so it wasn't used. Reconnect with read-only access.",
                s.join(", ")
            ),
            Error::NotConfigured(m) => write!(f, "{m}"),
            Error::Unsupported(m) => write!(f, "{m}"),
            Error::Unauthorized(m) => {
                write!(f, "The connection was rejected; please reconnect ({m})")
            }
            Error::Forbidden(m) => write!(f, "Not allowed to read this: {m}"),
            Error::RateLimited(Some(s)) => write!(f, "Rate limited; try again in {s} s"),
            Error::RateLimited(None) => write!(f, "Rate limited; try again shortly"),
            Error::Provider(s, m) => write!(f, "The service answered {s}: {m}"),
            Error::Network(m) => write!(f, "Couldn't reach the service: {m}"),
            Error::Invalid(m) => write!(f, "Unexpected reply: {m}"),
        }
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------------------------------------
// Credentials, accounts, inputs
// ---------------------------------------------------------------------------------------------

/// Who and what a connection is. Shown to the person; holds no secrets.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    /// Stable id: Slack team id, Microsoft tenant + user object id, Atlassian cloud id.
    pub id: String,
    /// Workspace, organisation or site name.
    pub name: String,
    /// The person as the provider knows them (name or email).
    pub user: String,
    /// Workspace or site URL, if any.
    pub url: String,
}

/// A connection's credentials. The server stores it sealed in the vault, never in the clear.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Credential {
    /// [`Provider::id`].
    pub provider: String,
    /// "oauth" or "token" (pasted).
    pub mode: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: Option<i64>,
    /// Granted scopes, as verified read-only. Empty when the provider can't tell us (Confluence
    /// API tokens) — then only layers 3 and 4 apply, which the UI says.
    pub scopes: Vec<String>,
    /// Confluence API-token mode: the account email the token is used with.
    pub basic_user: Option<String>,
    pub account: Account,
}

impl Credential {
    /// True when the access token expires within two minutes.
    pub fn needs_refresh(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|t| t - 120 <= now)
    }

    /// How read-only is enforced for this connection: "provider" when the token's scopes are
    /// known and verified read-only (all four layers), "app" when only Opportunity's own
    /// enforcement applies (a pasted token whose scopes the provider doesn't reveal).
    pub fn enforcement(&self) -> &'static str {
        if self.scopes.is_empty() {
            "app"
        } else {
            "provider"
        }
    }
}

/// What a person pastes to connect without OAuth (where the provider allows it).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenInput {
    /// Slack user token (xoxp-…) or Atlassian API token.
    pub token: String,
    /// Confluence: the Atlassian account email.
    pub email: String,
    /// Confluence: the site, e.g. `https://acme.atlassian.net`.
    pub site: String,
}

/// What to fetch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FetchQuery {
    /// Look back this many days.
    pub since_days: u32,
    /// Return at most this many items (the most relevant first).
    pub max_items: usize,
    /// Words that tend to mark an opportunity; providers use them to search or to rank.
    pub keywords: Vec<String>,
    /// Today (UTC) as YYYY-MM-DD.
    pub today: String,
}

impl Default for FetchQuery {
    fn default() -> Self {
        FetchQuery {
            since_days: 30,
            max_items: 60,
            keywords: DEFAULT_KEYWORDS.iter().map(|s| s.to_string()).collect(),
            today: model::date::today(),
        }
    }
}

/// Words that often mark an opportunity in messages, mail and pages.
pub const DEFAULT_KEYWORDS: &[&str] = &[
    "opportunity",
    "opening",
    "hiring",
    "role",
    "position",
    "apply",
    "applications",
    "deadline",
    "call for",
    "proposal",
    "rfp",
    "tender",
    "grant",
    "funding",
    "hackathon",
    "competition",
    "award",
    "nomination",
    "conference",
    "speaker",
    "speaking",
    "panel",
    "invitation",
    "invite",
    "partnership",
    "partner",
    "collaborate",
    "mentor",
    "mentoring",
    "programme",
    "program",
    "fellowship",
    "scholarship",
    "volunteer",
    "promotion",
    "secondment",
    "looking for",
    "intro",
];

/// A provider's OAuth app, as registered by whoever runs this server.
#[derive(Clone, Debug, PartialEq)]
pub struct OAuthApp {
    pub client_id: String,
    pub client_secret: String,
    /// `{PUBLIC_URL}/api/connectors/{provider}/callback`.
    pub redirect_uri: String,
    /// Microsoft only: "organizations" (work accounts, the default), "common", or a tenant id.
    pub tenant: String,
}

// ---------------------------------------------------------------------------------------------
// Endpoints (overridable for tests)
// ---------------------------------------------------------------------------------------------

/// Base URLs for every provider. Production defaults are fixed https origins; tests replace them
/// with the mock server (only loopback http is ever accepted as a replacement).
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoints {
    /// Slack Web API and token endpoint (`SLACK_API_BASE`, default `https://slack.com`).
    pub slack_api: String,
    /// Slack authorize page (`SLACK_AUTH_BASE`, default `https://slack.com`).
    pub slack_auth: String,
    /// Microsoft identity platform (`MS_LOGIN_BASE`, default `https://login.microsoftonline.com`).
    pub ms_login: String,
    /// Microsoft Graph (`MS_GRAPH_BASE`, default `https://graph.microsoft.com`).
    pub ms_graph: String,
    /// Atlassian authorize and token (`ATLASSIAN_AUTH_BASE`, default `https://auth.atlassian.com`).
    pub atlassian_auth: String,
    /// Atlassian API gateway (`ATLASSIAN_API_BASE`, default `https://api.atlassian.com`).
    pub atlassian_api: String,
    /// Confluence sites a pasted token may be used with. Production: `https://*.atlassian.net`
    /// (exactly one subdomain label). Tests add the mock site via `CONFLUENCE_SITE_BASES`.
    pub confluence_sites: Vec<String>,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            slack_api: "https://slack.com".into(),
            slack_auth: "https://slack.com".into(),
            ms_login: "https://login.microsoftonline.com".into(),
            ms_graph: "https://graph.microsoft.com".into(),
            atlassian_auth: "https://auth.atlassian.com".into(),
            atlassian_api: "https://api.atlassian.com".into(),
            confluence_sites: vec!["https://*.atlassian.net".into()],
        }
    }
}

impl Endpoints {
    /// Defaults, with test overrides from the environment. An override that isn't https or
    /// loopback http is ignored (with a warning on stderr).
    pub fn from_env() -> Endpoints {
        let mut e = Endpoints::default();
        let pick = |var: &str, slot: &mut String| {
            if let Ok(v) = std::env::var(var) {
                let v = v.trim().trim_end_matches('/').to_string();
                if v.is_empty() {
                    return;
                }
                if gateway::acceptable_base(&v) {
                    *slot = v;
                } else {
                    eprintln!(
                        "connectors: ignoring {var}={v}: only https, or http on loopback, is allowed"
                    );
                }
            }
        };
        pick("SLACK_API_BASE", &mut e.slack_api);
        pick("SLACK_AUTH_BASE", &mut e.slack_auth);
        pick("MS_LOGIN_BASE", &mut e.ms_login);
        pick("MS_GRAPH_BASE", &mut e.ms_graph);
        pick("ATLASSIAN_AUTH_BASE", &mut e.atlassian_auth);
        pick("ATLASSIAN_API_BASE", &mut e.atlassian_api);
        if let Ok(v) = std::env::var("CONFLUENCE_SITE_BASES") {
            for b in v
                .split(',')
                .map(|s| s.trim().trim_end_matches('/'))
                .filter(|s| !s.is_empty())
            {
                if gateway::acceptable_base(b) {
                    e.confluence_sites.push(b.to_string());
                }
            }
        }
        e
    }

    /// Bases the gateway may read data from for a provider.
    pub fn data_bases(&self, p: Provider) -> Vec<String> {
        match p {
            Provider::Slack => vec![self.slack_api.clone()],
            Provider::Microsoft => vec![self.ms_graph.clone()],
            Provider::Confluence => {
                let mut v = vec![self.atlassian_api.clone()];
                v.extend(self.confluence_sites.iter().cloned());
                v
            }
        }
    }

    /// Bases whose token endpoints [`OAuthHttp`] may POST to.
    pub fn token_bases(&self, p: Provider) -> Vec<String> {
        match p {
            Provider::Slack => vec![self.slack_api.clone()],
            Provider::Microsoft => vec![self.ms_login.clone()],
            Provider::Confluence => vec![self.atlassian_auth.clone()],
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------------------------

/// One request (or refusal) made on a person's behalf. Metadata only: never content, never
/// secrets, never query values.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditEvent {
    pub provider: String,
    /// "read", "token", "blocked" or "scope_rejected".
    pub action: String,
    /// "GET" or "POST".
    pub method: String,
    /// Origin and path, without the query string.
    pub target: String,
    /// HTTP status, 0 when nothing was sent.
    pub status: u16,
    pub bytes: u64,
    pub ms: u64,
    /// Unix seconds.
    pub at: i64,
    pub note: String,
}

/// Where audit events go (the server writes them to the database, per person).
pub trait Audit: Send + Sync {
    fn record(&self, event: AuditEvent);
}

/// Discards events.
pub struct NoAudit;

impl Audit for NoAudit {
    fn record(&self, _: AuditEvent) {}
}

/// Keeps events in memory (tests, and collecting a run's events before writing them).
#[derive(Default)]
pub struct MemoryAudit(pub Mutex<Vec<AuditEvent>>);

impl Audit for MemoryAudit {
    fn record(&self, event: AuditEvent) {
        if let Ok(mut v) = self.0.lock() {
            v.push(event);
        }
    }
}

impl MemoryAudit {
    pub fn events(&self) -> Vec<AuditEvent> {
        self.0.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------
// The facade
// ---------------------------------------------------------------------------------------------

/// What the server uses. Cheap to share between threads.
pub struct Connectors {
    eps: Endpoints,
    apps: HashMap<Provider, OAuthApp>,
    agent: ureq::Agent,
}

/// Whether a provider can be connected on this server, for the settings screen.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Availability {
    pub provider: &'static str,
    pub label: &'static str,
    /// The OAuth app is configured (the recommended way to connect).
    pub oauth: bool,
    /// A token can be pasted instead.
    pub token: bool,
    /// What a connection can read, in one plain line.
    pub reads: &'static str,
    /// The scopes requested, to show the person exactly what they're granting.
    pub scopes: Vec<&'static str>,
}

impl Connectors {
    /// OAuth apps from `SLACK_CLIENT_ID`/`SLACK_CLIENT_SECRET`, `MS_CLIENT_ID`/`MS_CLIENT_SECRET`
    /// (+ `MS_TENANT`, default "organizations"), `ATLASSIAN_CLIENT_ID`/`ATLASSIAN_CLIENT_SECRET`;
    /// redirect URIs under `public_url`; endpoints from [`Endpoints::from_env`].
    pub fn from_env(public_url: &str) -> Connectors {
        let get = |k: &str| {
            std::env::var(k)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let base = public_url.trim().trim_end_matches('/');
        let mut apps = Vec::new();
        for (p, id, secret) in [
            (Provider::Slack, "SLACK_CLIENT_ID", "SLACK_CLIENT_SECRET"),
            (Provider::Microsoft, "MS_CLIENT_ID", "MS_CLIENT_SECRET"),
            (
                Provider::Confluence,
                "ATLASSIAN_CLIENT_ID",
                "ATLASSIAN_CLIENT_SECRET",
            ),
        ] {
            if let (Some(client_id), Some(client_secret)) = (get(id), get(secret)) {
                apps.push((
                    p,
                    OAuthApp {
                        client_id,
                        client_secret,
                        redirect_uri: format!("{base}/api/connectors/{}/callback", p.id()),
                        tenant: if p == Provider::Microsoft {
                            get("MS_TENANT").unwrap_or_else(|| "organizations".into())
                        } else {
                            String::new()
                        },
                    },
                ));
            }
        }
        Connectors::new(Endpoints::from_env(), apps)
    }

    pub fn new(eps: Endpoints, apps: Vec<(Provider, OAuthApp)>) -> Connectors {
        Connectors {
            eps,
            apps: apps.into_iter().collect(),
            agent: gateway::agent(Duration::from_secs(30)),
        }
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.eps
    }

    pub fn availability(&self) -> Vec<Availability> {
        Provider::ALL
            .iter()
            .map(|&p| Availability {
                provider: p.id(),
                label: p.label(),
                oauth: self.apps.contains_key(&p),
                token: p.policy().token_mode,
                reads: p.policy().reads,
                scopes: p.policy().requested_scopes.to_vec(),
            })
            .collect()
    }

    fn app(&self, p: Provider) -> Result<&OAuthApp, Error> {
        self.apps.get(&p).ok_or_else(|| {
            Error::NotConfigured(format!("{} isn't set up on this server yet: its OAuth app needs to be configured by the administrator.", p.label()))
        })
    }

    fn gateway<'a>(&'a self, p: Provider, audit: &'a dyn Audit) -> Gateway<'a> {
        Gateway::new(p, &self.eps, self.agent.clone(), audit)
    }

    fn oauth_http<'a>(&'a self, p: Provider, audit: &'a dyn Audit) -> OAuthHttp<'a> {
        OAuthHttp::new(p, &self.eps, self.agent.clone(), audit)
    }

    /// Start OAuth: the URL to send the person to, and the PKCE verifier to keep (server-side,
    /// with `state`) until the callback.
    pub fn begin(&self, p: Provider, state: &str) -> Result<(String, String), Error> {
        let app = self.app(p)?;
        let pkce = Pkce::new();
        let url = match p {
            Provider::Slack => slack::authorize_url(&self.eps, app, state, &pkce),
            Provider::Microsoft => microsoft::authorize_url(&self.eps, app, state, &pkce),
            Provider::Confluence => confluence::authorize_url(&self.eps, app, state, &pkce),
        };
        Ok((url, pkce.verifier))
    }

    /// Finish OAuth with the callback's `code` and the stored PKCE verifier. The credential is
    /// only returned once its scopes are verified read-only.
    pub fn finish(
        &self,
        p: Provider,
        code: &str,
        verifier: &str,
        audit: &dyn Audit,
    ) -> Result<Credential, Error> {
        let app = self.app(p)?;
        let http = self.oauth_http(p, audit);
        let pkce = Pkce::from_verifier(verifier);
        let mut cred = match p {
            Provider::Slack => slack::exchange_code(&http, &self.eps, app, code, &pkce)?,
            Provider::Microsoft => microsoft::exchange_code(&http, &self.eps, app, code, &pkce)?,
            Provider::Confluence => confluence::exchange_code(&http, &self.eps, app, code, &pkce)?,
        };
        cred.provider = p.id().into();
        cred.mode = "oauth".into();
        self.check_granted(p, &cred, audit)?;
        // The code exchange can't make audited reads. When it couldn't fill in the account
        // (Confluence), verify through the gateway: that also proves the token can read, and
        // re-checks per-site scopes. Security failures stop the sign-in; a transient failure
        // doesn't (the account fills in on the next verify).
        if cred.account.id.is_empty() || cred.account.name.is_empty() {
            let gw = self.gateway(p, audit);
            let verified = audited(
                p,
                audit,
                match p {
                    Provider::Slack => slack::verify(&gw, &cred),
                    Provider::Microsoft => microsoft::verify(&gw, &cred),
                    Provider::Confluence => confluence::verify(&gw, &cred),
                },
            );
            match verified {
                Ok(account) if account != Account::default() => cred.account = account,
                Ok(_) => {}
                Err(e)
                    if e.needs_reconnect()
                        || matches!(
                            e,
                            Error::Forbidden(_) | Error::Blocked(_) | Error::Invalid(_)
                        ) =>
                {
                    return Err(e);
                }
                Err(_) => {}
            }
        }
        Ok(cred)
    }

    /// Connect with a pasted token (Slack user token, Atlassian API token).
    pub fn connect_token(
        &self,
        p: Provider,
        input: &TokenInput,
        audit: &dyn Audit,
    ) -> Result<Credential, Error> {
        if !p.policy().token_mode {
            return Err(Error::Unsupported(format!(
                "{} can only be connected by signing in (OAuth).",
                p.label()
            )));
        }
        let gw = self.gateway(p, audit);
        let mut cred = match p {
            Provider::Slack => slack::connect_with_token(&gw, input)?,
            Provider::Microsoft => microsoft::connect_with_token(&gw, input)?,
            Provider::Confluence => confluence::connect_with_token(&gw, input)?,
        };
        cred.provider = p.id().into();
        cred.mode = "token".into();
        self.check_granted(p, &cred, audit)?;
        Ok(cred)
    }

    /// A refreshed credential when the access token is about to expire, None when it's fine.
    pub fn refresh_if_needed(
        &self,
        cred: &Credential,
        now: i64,
        audit: &dyn Audit,
    ) -> Result<Option<Credential>, Error> {
        let p = provider_of(cred)?;
        if cred.mode != "oauth" || !cred.needs_refresh(now) || cred.refresh_token.is_none() {
            return Ok(None);
        }
        let app = self.app(p)?;
        let http = self.oauth_http(p, audit);
        let mut fresh = match p {
            Provider::Slack => slack::refresh(&http, &self.eps, app, cred)?,
            Provider::Microsoft => microsoft::refresh(&http, &self.eps, app, cred)?,
            Provider::Confluence => confluence::refresh(&http, &self.eps, app, cred)?,
        };
        fresh.provider = cred.provider.clone();
        fresh.mode = cred.mode.clone();
        if fresh.account == Account::default() {
            fresh.account = cred.account.clone();
        }
        self.check_granted(p, &fresh, audit)?;
        Ok(Some(fresh))
    }

    /// Check the connection still works (and, where the provider tells us, is still read-only).
    pub fn verify(&self, cred: &Credential, audit: &dyn Audit) -> Result<Account, Error> {
        let p = provider_of(cred)?;
        self.check_granted(p, cred, audit)?;
        let gw = self.gateway(p, audit);
        audited(
            p,
            audit,
            match p {
                Provider::Slack => slack::verify(&gw, cred),
                Provider::Microsoft => microsoft::verify(&gw, cred),
                Provider::Confluence => confluence::verify(&gw, cred),
            },
        )
    }

    /// Recent items worth the engine's attention, most relevant first, sanitized and capped.
    pub fn fetch(
        &self,
        cred: &Credential,
        q: &FetchQuery,
        audit: &dyn Audit,
    ) -> Result<Vec<ContextItem>, Error> {
        let p = provider_of(cred)?;
        self.check_granted(p, cred, audit)?;
        let gw = self.gateway(p, audit);
        let items = audited(
            p,
            audit,
            match p {
                Provider::Slack => slack::fetch(&gw, cred, q),
                Provider::Microsoft => microsoft::fetch(&gw, cred, q),
                Provider::Confluence => confluence::fetch(&gw, cred, q),
            },
        )?;
        let mut out: Vec<ContextItem> = items
            .into_iter()
            .filter_map(|mut i| {
                i.source = p.id().into();
                i.sanitized()
            })
            .collect();
        out.truncate(q.max_items);
        Ok(out)
    }

    fn check_granted(
        &self,
        p: Provider,
        cred: &Credential,
        audit: &dyn Audit,
    ) -> Result<(), Error> {
        // An OAuth token always reports what was granted; one that doesn't is refused (fail closed).
        if cred.mode == "oauth" && cred.scopes.is_empty() {
            let e = Error::Invalid(format!(
                "{} didn't say what access it granted, so the connection wasn't used",
                p.label()
            ));
            audit.record(AuditEvent {
                provider: p.id().into(),
                action: "scope_rejected".into(),
                note: e.to_string(),
                at: model::date::now_unix(),
                ..Default::default()
            });
            return Err(e);
        }
        if let Err(e) = verify_scopes(p, &cred.scopes) {
            audit.record(AuditEvent {
                provider: p.id().into(),
                action: "scope_rejected".into(),
                note: e.to_string(),
                at: model::date::now_unix(),
                ..Default::default()
            });
            return Err(e);
        }
        Ok(())
    }
}

/// Records a `scope_rejected` audit event when a provider itself refused a token for its live
/// scopes (e.g. Slack's per-call `x-oauth-scopes` check), then passes the result through.
fn audited<T>(p: Provider, audit: &dyn Audit, r: Result<T, Error>) -> Result<T, Error> {
    if let Err(e @ Error::WriteScopes(_)) = &r {
        audit.record(AuditEvent {
            provider: p.id().into(),
            action: "scope_rejected".into(),
            note: e.to_string(),
            at: model::date::now_unix(),
            ..Default::default()
        });
    }
    r
}

fn provider_of(cred: &Credential) -> Result<Provider, Error> {
    Provider::parse(&cred.provider)
        .ok_or_else(|| Error::Invalid(format!("unknown provider {:?}", cred.provider)))
}

#[cfg(test)]
mod tests;
