//! Confluence Cloud, read-only.
//!
//! Two ways to connect:
//!
//! * **OAuth 2.0 (3LO)**, the recommended way. [`authorize_url`] sends the person to
//!   `{atlassian_auth}/authorize` with `audience=api.atlassian.com`, the read scopes of [`POLICY`],
//!   `response_type=code` and `prompt=consent`. [`exchange_code`] posts the code to
//!   `{atlassian_auth}/oauth/token` (JSON, as Atlassian documents it) and keeps the access token,
//!   the rotating refresh token, the expiry and the scopes Atlassian says it granted. [`refresh`]
//!   reads the grant again on every rotation, and the facade refuses the credential, at connect or
//!   at any refresh, if one of those scopes can write. Data then goes through the API gateway,
//!   `{atlassian_api}/ex/confluence/{cloudId}/wiki/rest/api/…`, with the bearer token.
//!   Atlassian doesn't support PKCE for apps created in its developer console (ECO-283 is still
//!   open), so no challenge is sent: the code is protected by `state`, the exact redirect URI and
//!   the client secret, which never leaves the server.
//! * **A pasted API token**: the site (`https://<name>.atlassian.net`, checked against
//!   [`Endpoints::confluence_sites`]), the account email and an API token, sent with HTTP basic
//!   auth. Atlassian accepts a *scoped* token (the recommended kind, created with read scopes only)
//!   only through the API gateway, so the site's cloud id is read from `{site}/_edge/tenant_info`
//!   and the gateway is tried first; a classic token works on `{site}/wiki/rest/api/…`, which is
//!   tried next. Atlassian doesn't reveal a token's scopes, so [`Credential::scopes`] stays empty
//!   and the connection is "app"-enforced: only Opportunity's own layers (the gateway's GET-only
//!   allow-list, no AI tools) stand between it and a write.
//!
//! Reads (GET only, the exact paths in [`POLICY`]): `oauth/token/accessible-resources` (which
//! sites an OAuth token reaches and with which scopes, checked again on every verify and fetch),
//! `wiki/rest/api/user/current` (who is connected), `wiki/rest/api/search` (CQL) and, for API
//! tokens, `_edge/tenant_info` (the site's public cloud id).
//!
//! [`fetch`] runs one CQL search (pages and blog posts modified within `since_days` that mention
//! any keyword, newest first) over at most four pages of 25 results. It keeps titles, excerpts
//! with the highlight markup removed, spaces, dates and links built from the site's own URL, then
//! ranks them and returns the best `max_items`. Page bodies are never requested.
//!
//! An OAuth connection's account (cloud id, site, user) comes from [`verify`], and [`fetch`] looks
//! the site up again on every run. [`exchange_code`] can't do it: it is handed no audited
//! [`Gateway`], and every read is audited.
//!
//! Owner: the Confluence connector builder.

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::gateway::{acceptable_base, split_url, with_query};
use crate::{
    Account, Auth, Credential, DEFAULT_KEYWORDS, Endpoints, Error, FetchQuery, Gateway, OAuthApp,
    OAuthHttp, Pkce, Policy, Provider, TokenInput, parse_scopes, verify_scopes,
};
use model::ContextItem;
use model::date;
use model::text::{squash, truncate};

pub const POLICY: Policy = Policy {
    read_paths: &[
        // OAuth: the sites a token reaches, and the scopes it has on each.
        "/oauth/token/accessible-resources",
        // Through the Atlassian API gateway (OAuth tokens and scoped API tokens).
        "/ex/confluence/*/wiki/rest/api/search",
        "/ex/confluence/*/wiki/rest/api/user/current",
        // A site's own API (classic API tokens).
        "/wiki/rest/api/search",
        "/wiki/rest/api/user/current",
        // A site's public cloud id, so a scoped API token can use the gateway.
        "/_edge/tenant_info",
    ],
    token_paths: &["/oauth/token"],
    requested_scopes: &[
        "search:confluence",
        "read:confluence-content.summary",
        "read:confluence-space.summary",
        "read:confluence-user",
        "offline_access",
    ],
    allowed_scopes: &[
        "read:confluence-content.summary",
        "read:confluence-content.all",
        "search:confluence",
        "read:confluence-space.summary",
        "read:confluence-user",
        "read:confluence-props",
        "read:confluence-groups",
        "read:page:confluence",
        "read:space:confluence",
        "read:content-details:confluence",
        "read:user:confluence",
        "read:me",
        "read:account",
    ],
    token_mode: true,
    reads: "Titles, short excerpts, spaces and links of Confluence pages and blog posts you can already see that mention opportunities, found with Confluence search.",
};

/// Results per search request.
const PAGE_SIZE: usize = 25;
/// Search requests per fetch, at most.
const MAX_PAGES: usize = 4;
/// Keywords put into one CQL query, at most.
const MAX_KEYWORDS: usize = 40;
/// Atlassian's documented access-token lifetime, assumed when a reply leaves it out.
const DEFAULT_LIFETIME_SECS: i64 = 3600;

// ---------------------------------------------------------------------------------------------
// OAuth 2.0 (3LO)
// ---------------------------------------------------------------------------------------------

pub fn authorize_url(eps: &Endpoints, app: &OAuthApp, state: &str, pkce: &Pkce) -> String {
    // Not sent: Atlassian's 3LO doesn't support PKCE for developer-console apps (module docs).
    let _ = pkce;
    with_query(
        &format!("{}/authorize", eps.atlassian_auth),
        &[
            ("audience", "api.atlassian.com"),
            ("client_id", &app.client_id),
            ("scope", &POLICY.requested_scopes.join(" ")),
            ("redirect_uri", &app.redirect_uri),
            ("state", state),
            ("response_type", "code"),
            ("prompt", "consent"),
        ],
    )
}

pub fn exchange_code(
    http: &OAuthHttp,
    eps: &Endpoints,
    app: &OAuthApp,
    code: &str,
    pkce: &Pkce,
) -> Result<Credential, Error> {
    let _ = pkce;
    let code = code.trim();
    if code.is_empty() || code.len() > 4096 || code.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "the sign-in code from Atlassian is missing or malformed".into(),
        ));
    }
    let body = json!({
        "grant_type": "authorization_code",
        "client_id": app.client_id,
        "client_secret": app.client_secret,
        "code": code,
        "redirect_uri": app.redirect_uri,
    });
    let (status, reply) = http.post_json(&token_url(eps), &body)?;
    let t = read_tokens(status, &reply, Step::Exchange)?;
    let scopes = match t.scopes {
        Some(s) if !s.is_empty() => s,
        _ => return Err(no_scope_error()),
    };
    Ok(Credential {
        access_token: t.access,
        refresh_token: t.refresh,
        expires_at: Some(t.expires_at),
        scopes,
        ..Default::default()
    })
}

pub fn refresh(
    http: &OAuthHttp,
    eps: &Endpoints,
    app: &OAuthApp,
    cred: &Credential,
) -> Result<Credential, Error> {
    let Some(old) = cred
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Err(Error::Unauthorized(
            "this connection has no refresh token".into(),
        ));
    };
    let body = json!({
        "grant_type": "refresh_token",
        "client_id": app.client_id,
        "client_secret": app.client_secret,
        "refresh_token": old,
    });
    let (status, reply) = http.post_json(&token_url(eps), &body)?;
    let t = read_tokens(status, &reply, Step::Refresh)?;
    // The grant as Atlassian states it now. Left out, it is unchanged (RFC 6749 §5.1 and §6), so
    // the scopes verified before still hold; listed but empty is not a reply we can trust.
    let scopes = match t.scopes {
        Some(s) if !s.is_empty() => s,
        Some(_) => return Err(no_scope_error()),
        None if !cred.scopes.is_empty() => cred.scopes.clone(),
        None => return Err(no_scope_error()),
    };
    Ok(Credential {
        provider: cred.provider.clone(),
        mode: cred.mode.clone(),
        access_token: t.access,
        // Rotating refresh tokens: the one just used is now disabled, so keep the new one.
        refresh_token: t.refresh.or_else(|| Some(old.to_string())),
        expires_at: Some(t.expires_at),
        scopes,
        basic_user: None,
        account: cred.account.clone(),
    })
}

fn token_url(eps: &Endpoints) -> String {
    format!("{}/oauth/token", eps.atlassian_auth)
}

fn no_scope_error() -> Error {
    Error::Invalid(
        "Atlassian didn't say which access it granted, so the connection can't be checked as read-only and wasn't used".into(),
    )
}

/// A token endpoint reply, read.
#[derive(Debug, PartialEq)]
struct Tokens {
    access: String,
    refresh: Option<String>,
    expires_at: i64,
    /// What Atlassian says it granted, as granted; None when the reply leaves `scope` out.
    scopes: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    Exchange,
    Refresh,
}

fn read_tokens(status: u16, reply: &Value, step: Step) -> Result<Tokens, Error> {
    if !(200..300).contains(&status) {
        return Err(token_error(status, reply, step));
    }
    let access = reply["access_token"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Invalid("Atlassian's token reply had no access token".into()))?
        .to_string();
    let refresh = reply["refresh_token"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let lifetime = match &reply["expires_in"] {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|n| *n > 0)
    .unwrap_or(DEFAULT_LIFETIME_SECS);
    let scopes = match reply.get("scope") {
        Some(Value::String(s)) => Some(parse_scopes(s)),
        Some(Value::Array(a)) => Some(
            a.iter()
                .filter_map(Value::as_str)
                .flat_map(parse_scopes)
                .collect(),
        ),
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(Error::Invalid(
                "Atlassian's token reply had an odd scope".into(),
            ));
        }
    };
    Ok(Tokens {
        access,
        refresh,
        expires_at: date::now_unix().saturating_add(lifetime),
        scopes,
    })
}

/// An error from the token endpoint, in our words: the provider's own description is never
/// echoed, so nothing sent (code, refresh token, client secret) can end up in a message.
fn token_error(status: u16, reply: &Value, step: Step) -> Error {
    let code: String = reply["error"]
        .as_str()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(40)
        .collect();
    let client_refused = status == 401
        || matches!(
            code.as_str(),
            "invalid_client" | "unauthorized_client" | "access_denied"
        );
    match (code.as_str(), step) {
        ("invalid_grant", Step::Exchange) => Error::Unauthorized(
            "Atlassian didn't accept the sign-in code; it may have expired or been used already"
                .into(),
        ),
        ("invalid_grant", Step::Refresh) => Error::Unauthorized(
            "Atlassian no longer accepts this connection's refresh token: it was revoked, went unused for 90 days, or the password changed".into(),
        ),
        _ if client_refused => Error::Provider(
            status,
            "Atlassian refused this server's OAuth app; the administrator should check ATLASSIAN_CLIENT_ID and ATLASSIAN_CLIENT_SECRET".into(),
        ),
        ("", _) => Error::Provider(status, "the token request failed".into()),
        (c, _) => Error::Provider(status, format!("the token request failed ({c})")),
    }
}

// ---------------------------------------------------------------------------------------------
// API tokens
// ---------------------------------------------------------------------------------------------

pub fn connect_with_token(gw: &Gateway, input: &TokenInput) -> Result<Credential, Error> {
    let site = normalize_site(gw.endpoints(), &input.site)?;
    let email = clean_email(&input.email)?;
    let token = clean_token(&input.token)?;
    let auth = Auth::Basic {
        user: email.clone(),
        secret: token.clone(),
    };
    let cloud_id = tenant_cloud_id(gw, &site, &auth)?;
    let (target, user) = probe(gw, &site, cloud_id.as_deref(), auth)?;
    Ok(Credential {
        access_token: token,
        basic_user: Some(email),
        // Atlassian doesn't reveal an API token's scopes: empty, so "app" enforcement.
        scopes: Vec::new(),
        account: Account {
            id: cloud_id.unwrap_or_else(|| host_of(&target.site)),
            name: site_label(&target.site),
            user,
            url: target.site,
        },
        ..Default::default()
    })
}

/// The site a person typed, as its canonical base URL: `https://<name>.atlassian.net` (or one of
/// the exact bases tests add to [`Endpoints::confluence_sites`]). A trailing `/` or `/wiki` is
/// dropped; plain http, other hosts, more labels, ports, other paths, queries, fragments and
/// credentials are refused.
pub(crate) fn normalize_site(eps: &Endpoints, raw: &str) -> Result<String, Error> {
    let refuse = || {
        Error::Invalid(
            "The site must be your Confluence Cloud address, such as https://your-company.atlassian.net".into(),
        )
    };
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 255 || raw.contains(['?', '#', '@', '\\']) {
        return Err(refuse());
    }
    let url = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let p = split_url(&url).ok_or_else(refuse)?;
    let mut path = p.path.trim_end_matches('/').to_string();
    if path.to_ascii_lowercase().ends_with("/wiki") {
        path.truncate(path.len() - "/wiki".len());
    }
    for base in &eps.confluence_sites {
        let Some(b) = split_url(base) else { continue };
        if b.scheme != p.scheme || b.port != p.port {
            continue;
        }
        let base_path = b.path.trim_end_matches('/');
        if let Some(suffix) = b.host.strip_prefix("*.") {
            if p.scheme != "https" || !base_path.is_empty() || !path.is_empty() {
                continue;
            }
            let label = p
                .host
                .strip_suffix(suffix)
                .and_then(|h| h.strip_suffix('.'));
            if label.is_some_and(dns_label) {
                return Ok(format!("https://{}", p.host));
            }
        } else if b.host == p.host && base_path == path {
            return Ok(base.trim().trim_end_matches('/').to_string());
        }
    }
    Err(refuse())
}

/// One DNS label: 1–63 lowercase letters, digits or hyphens, not starting or ending in a hyphen.
fn dns_label(l: &str) -> bool {
    (1..=63).contains(&l.len())
        && l.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !l.starts_with('-')
        && !l.ends_with('-')
}

fn clean_email(raw: &str) -> Result<String, Error> {
    let e = raw.trim();
    let shaped = match e.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.contains('@')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        }
        None => false,
    };
    // No ':' either: basic auth splits user and secret at the first one.
    if shaped
        && e.len() <= 254
        && !e
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == ':')
    {
        Ok(e.to_string())
    } else {
        Err(Error::Invalid(
            "Enter the email address of your Atlassian account".into(),
        ))
    }
}

fn clean_token(raw: &str) -> Result<String, Error> {
    let t = raw.trim();
    if t.is_empty() || t.len() > 2048 || t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Err(Error::Invalid(
            "Paste the API token exactly as Atlassian showed it".into(),
        ))
    } else {
        Ok(t.to_string())
    }
}

/// The site's cloud id from its public `/_edge/tenant_info`, or None when it can't be had (then
/// only the site's own API is tried, which classic tokens use).
fn tenant_cloud_id(gw: &Gateway, site: &str, auth: &Auth) -> Result<Option<String>, Error> {
    match gw.get_json(&format!("{site}/_edge/tenant_info"), &[], auth) {
        Ok(v) => Ok(v["cloudId"]
            .as_str()
            .map(str::trim)
            .filter(|id| is_cloud_id(id))
            .map(str::to_string)),
        Err(e @ Error::Blocked(_)) => Err(e),
        Err(_) => Ok(None),
    }
}

// ---------------------------------------------------------------------------------------------
// Verify and fetch
// ---------------------------------------------------------------------------------------------

pub fn verify(gw: &Gateway, cred: &Credential) -> Result<Account, Error> {
    if token_mode(cred) {
        let (target, user) = token_target(gw, cred)?;
        return Ok(Account {
            id: if cred.account.id.trim().is_empty() {
                host_of(&target.site)
            } else {
                cred.account.id.clone()
            },
            name: site_label(&target.site),
            user,
            url: target.site,
        });
    }
    let auth = bearer(cred)?;
    let want = Some(cred.account.id.trim()).filter(|id| !id.is_empty());
    let site = accessible_site(gw, &auth, want)?;
    let me = gw.get_json(
        &format!(
            "{}/wiki/rest/api/user/current",
            gateway_api(gw.endpoints(), &site.cloud_id)
        ),
        &[],
        &auth,
    )?;
    let user = known_user(&me).ok_or_else(|| {
        Error::Unauthorized("Atlassian didn't recognise the connected account".into())
    })?;
    Ok(Account {
        id: site.cloud_id,
        name: site.name,
        user,
        url: site.url,
    })
}

pub fn fetch(gw: &Gateway, cred: &Credential, q: &FetchQuery) -> Result<Vec<ContextItem>, Error> {
    if q.max_items == 0 {
        return Ok(Vec::new());
    }
    let target = if token_mode(cred) {
        token_target(gw, cred)?.0
    } else {
        oauth_target(gw, cred)?
    };
    let url = format!("{}/wiki/rest/api/search", target.api);
    let cql = build_cql(q);
    let limit = PAGE_SIZE.to_string();
    let mut found: Vec<Found> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut next: Option<NextPage> = None;
    for page in 0..pages_for(q.max_items) {
        let mut query: Vec<(&str, &str)> = vec![
            ("cql", cql.as_str()),
            ("limit", limit.as_str()),
            ("excerpt", "highlight"),
        ];
        if let Some(n) = &next {
            query.push(("next", "true"));
            query.push(("cursor", n.cursor.as_str()));
            if let Some(start) = &n.start {
                query.push(("start", start.as_str()));
            }
        }
        let reply = match gw.get_json(&url, &query, &target.auth) {
            Ok(v) => v,
            // A refused request is a bug or an attack: never papered over with partial results.
            Err(e @ Error::Blocked(_)) => return Err(e),
            Err(e) if page == 0 => return Err(e),
            // Rejected, forbidden or rate limited part-way: keep what earlier pages found.
            Err(_) => break,
        };
        let Some(results) = reply["results"].as_array() else {
            if page == 0 {
                return Err(Error::Invalid(
                    "Confluence search replied without results".into(),
                ));
            }
            break;
        };
        for r in results {
            if let Some(f) = found_of(r, &target.site)
                && seen.insert(f.key.clone())
            {
                found.push(f);
            }
        }
        next = next_page(&reply);
        if next.is_none() {
            break;
        }
    }
    Ok(rank(found, q))
}

/// Where and how to read: the API base (without `/wiki`), the site for links, and the auth.
struct Target {
    api: String,
    site: String,
    auth: Auth,
}

fn token_mode(cred: &Credential) -> bool {
    match cred.mode.as_str() {
        "token" => true,
        "oauth" => false,
        _ => cred.basic_user.is_some(),
    }
}

fn bearer(cred: &Credential) -> Result<Auth, Error> {
    let t = cred.access_token.trim();
    if t.is_empty() {
        return Err(Error::Unauthorized(
            "this connection has no access token".into(),
        ));
    }
    Ok(Auth::Bearer(t.to_string()))
}

fn oauth_target(gw: &Gateway, cred: &Credential) -> Result<Target, Error> {
    let auth = bearer(cred)?;
    // Asked on every run: the site must still be granted, and every scope still read-only.
    let want = Some(cred.account.id.trim()).filter(|id| !id.is_empty());
    let site = accessible_site(gw, &auth, want)?;
    Ok(Target {
        api: gateway_api(gw.endpoints(), &site.cloud_id),
        site: site.url,
        auth,
    })
}

fn token_target(gw: &Gateway, cred: &Credential) -> Result<(Target, String), Error> {
    let email = cred
        .basic_user
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .ok_or_else(|| {
            Error::Invalid("this connection is missing its Atlassian account email".into())
        })?;
    let token = cred.access_token.trim();
    if token.is_empty() {
        return Err(Error::Unauthorized(
            "this connection has no API token".into(),
        ));
    }
    // The stored site is checked again, exactly as when it was typed.
    let site = normalize_site(gw.endpoints(), &cred.account.url)?;
    let cloud_id = Some(cred.account.id.trim()).filter(|id| is_cloud_id(id));
    probe(
        gw,
        &site,
        cloud_id,
        Auth::Basic {
            user: email.to_string(),
            secret: token.to_string(),
        },
    )
}

/// Finds which API accepts an API token, by asking who it signs in as: the gateway first (scoped
/// tokens work only there, and it never answers a bad token as an anonymous visitor), then the
/// site's own API (classic tokens). Returns where to read and the person's name.
fn probe(
    gw: &Gateway,
    site: &str,
    cloud_id: Option<&str>,
    auth: Auth,
) -> Result<(Target, String), Error> {
    let mut apis = Vec::new();
    if let Some(id) = cloud_id {
        apis.push(gateway_api(gw.endpoints(), id));
    }
    apis.push(site.to_string());
    let mut said: Option<String> = None;
    for api in apis {
        match gw.get_json(&format!("{api}/wiki/rest/api/user/current"), &[], &auth) {
            // A site can answer a bad token as a signed-out visitor; that's a refusal too.
            Ok(me) => {
                if let Some(user) = known_user(&me) {
                    return Ok((
                        Target {
                            api,
                            site: site.to_string(),
                            auth,
                        },
                        user,
                    ));
                }
            }
            Err(Error::Unauthorized(m) | Error::Forbidden(m)) => {
                said.get_or_insert(m);
            }
            Err(e) => return Err(e),
        }
    }
    let detail = said
        .map(|m| truncate(&squash(&m), 120))
        .filter(|m| !m.is_empty())
        .map(|m| format!("; Atlassian said: {m}"))
        .unwrap_or_default();
    Err(Error::Unauthorized(format!(
        "Atlassian didn't accept this email and API token for {site}{detail}"
    )))
}

/// The display name of a signed-in user from `user/current`, None for an anonymous visitor.
fn known_user(me: &Value) -> Option<String> {
    let account = me["accountId"].as_str().map(str::trim).unwrap_or("");
    if account.is_empty() || matches!(me["type"].as_str(), Some("anonymous" | "unknown")) {
        return None;
    }
    ["displayName", "publicName", "email"]
        .iter()
        .find_map(|k| {
            me[*k]
                .as_str()
                .map(|s| truncate(&squash(s), 80))
                .filter(|s| !s.is_empty())
        })
        .or_else(|| Some(account.to_string()))
}

/// A Confluence site an OAuth token reaches.
#[derive(Debug, PartialEq)]
struct Site {
    cloud_id: String,
    name: String,
    url: String,
}

/// The site to read, from `accessible-resources`: the one with `want` as cloud id, or else the
/// first that has Confluence search. Every resource listed must carry read-only scopes only;
/// otherwise the token can do more than read somewhere, and it isn't used at all.
fn accessible_site(gw: &Gateway, auth: &Auth, want: Option<&str>) -> Result<Site, Error> {
    let url = format!(
        "{}/oauth/token/accessible-resources",
        gw.endpoints().atlassian_api
    );
    let reply = gw.get_json(&url, &[], auth)?;
    pick_site(&reply, want)
}

fn pick_site(reply: &Value, want: Option<&str>) -> Result<Site, Error> {
    let list = reply
        .as_array()
        .ok_or_else(|| Error::Invalid("Atlassian's list of sites wasn't a list".into()))?;
    let mut sites: Vec<(bool, Site)> = Vec::new();
    for r in list {
        let scopes = string_list(&r["scopes"]);
        verify_scopes(Provider::Confluence, &scopes)?;
        if !scopes
            .iter()
            .any(|s| s.to_ascii_lowercase().contains("confluence"))
        {
            continue;
        }
        if let Some(site) = site_of(r) {
            let search = scopes
                .iter()
                .any(|s| s.trim().eq_ignore_ascii_case("search:confluence"));
            sites.push((search, site));
        }
    }
    match want {
        Some(id) => sites
            .into_iter()
            .map(|(_, s)| s)
            .find(|s| s.cloud_id == id)
            .ok_or_else(|| {
                Error::Unauthorized(
                    "this connection no longer has access to its Confluence site".into(),
                )
            }),
        None => {
            if sites.is_empty() {
                return Err(Error::Unauthorized(
                    "no Confluence site was shared with Opportunity; connect again and choose your Confluence site".into(),
                ));
            }
            let at = sites.iter().position(|(search, _)| *search).unwrap_or(0);
            Ok(sites.swap_remove(at).1)
        }
    }
}

fn site_of(r: &Value) -> Option<Site> {
    let id = r["id"].as_str()?.trim();
    if !is_cloud_id(id) {
        return None;
    }
    let url = link_base(r["url"].as_str()?)?;
    let name = r["name"]
        .as_str()
        .map(|n| truncate(&squash(n), 80))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| host_of(&url));
    Some(Site {
        cloud_id: id.to_string(),
        name,
        url,
    })
}

/// A site URL page links can start with: https (or loopback http, in tests), no query.
fn link_base(raw: &str) -> Option<String> {
    let u = raw.trim().trim_end_matches('/');
    let p = split_url(u)?;
    if !p.query.is_empty() || u.contains('?') || !acceptable_base(u) {
        return None;
    }
    Some(u.to_string())
}

fn string_list(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Value::String(s) => parse_scopes(s),
        _ => Vec::new(),
    }
}

/// A cloud id is safe as one path segment: 1–64 letters, digits and hyphens.
fn is_cloud_id(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && s.bytes().any(|b| b.is_ascii_alphanumeric())
}

fn gateway_api(eps: &Endpoints, cloud_id: &str) -> String {
    format!("{}/ex/confluence/{cloud_id}", eps.atlassian_api)
}

fn host_of(url: &str) -> String {
    split_url(url).map(|p| p.host).unwrap_or_default()
}

/// "acme" for `https://acme.atlassian.net`, the host for anything else.
fn site_label(site: &str) -> String {
    let host = host_of(site);
    match host.strip_suffix(".atlassian.net") {
        Some(label) if !label.is_empty() => label.to_string(),
        _ => host,
    }
}

// ---------------------------------------------------------------------------------------------
// The search
// ---------------------------------------------------------------------------------------------

/// Search requests needed for `max_items`: about twice as many candidates, to rank from.
fn pages_for(max_items: usize) -> usize {
    max_items
        .saturating_mul(2)
        .clamp(1, PAGE_SIZE * MAX_PAGES)
        .div_ceil(PAGE_SIZE)
}

/// `type in (page, blogpost) AND lastmodified >= "YYYY-MM-DD" AND (text ~ … OR …) ORDER BY
/// lastmodified DESC`. Every value is built from a date we computed or a keyword reduced to
/// letters, digits and spaces, then written as an escaped CQL string.
fn build_cql(q: &FetchQuery) -> String {
    let days = i64::from(q.since_days.clamp(1, 365));
    let since = match date::add_days(q.today.trim(), -days) {
        Some(d) => format!("lastmodified >= {}", cql_string(&d)),
        None => format!("lastmodified >= now({})", cql_string(&format!("-{days}d"))),
    };
    let mut cql = format!("type in (page, blogpost) AND {since}");
    let terms = text_terms(&q.keywords);
    if !terms.is_empty() {
        cql.push_str(" AND (");
        cql.push_str(&terms.join(" OR "));
        cql.push(')');
    }
    cql.push_str(" ORDER BY lastmodified DESC");
    cql
}

/// One `text ~ "…"` clause per distinct keyword: a single word as a term (so Confluence's
/// stemming applies), several words as an exact phrase. Keywords are cut down to letters, digits
/// and spaces first: Atlassian documents a two-backslash escape for its reserved characters, but
/// text searches don't honour it (CONFCLOUD-73930), and its analyzer splits on punctuation anyway.
/// Lowercase keeps Lucene's AND/OR/NOT from ever being read as operators.
fn text_terms(keywords: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for k in keyword_list(keywords) {
        let words = plain_words(&k);
        if words.is_empty() || words.chars().count() > 60 || !seen.insert(words.clone()) {
            continue;
        }
        let lucene = if words.contains(' ') {
            format!("\"{words}\"")
        } else {
            words
        };
        out.push(format!("text ~ {}", cql_string(&lucene)));
        if out.len() >= MAX_KEYWORDS {
            break;
        }
    }
    out
}

/// The keywords asked for, or the defaults when none are.
fn keyword_list(keywords: &[String]) -> Vec<String> {
    if keywords.iter().any(|k| !k.trim().is_empty()) {
        keywords.to_vec()
    } else {
        DEFAULT_KEYWORDS.iter().map(|s| s.to_string()).collect()
    }
}

/// Lowercase letters and digits, every other run of characters made one space.
fn plain_words(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            out.extend(c.to_lowercase());
        } else {
            gap = true;
        }
    }
    out
}

/// A CQL string literal: in double quotes, with `\` and `"` escaped.
fn cql_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Where the next page of results is: the cursor (and start) from `_links.next`. Only these are
/// taken from the link; the rest of the request is rebuilt by us.
#[derive(Debug, PartialEq)]
struct NextPage {
    cursor: String,
    start: Option<String>,
}

fn next_page(reply: &Value) -> Option<NextPage> {
    let link = reply["_links"]["next"].as_str()?;
    let (_, query) = link.split_once('?')?;
    let mut cursor = None;
    let mut start = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "cursor" => cursor = percent_decode(v),
            "start" if !v.is_empty() && v.len() <= 9 && v.bytes().all(|b| b.is_ascii_digit()) => {
                start = Some(v.to_string())
            }
            _ => {}
        }
    }
    let cursor =
        cursor.filter(|c| !c.is_empty() && c.len() <= 2048 && !c.chars().any(char::is_control))?;
    Some(NextPage { cursor, start })
}

/// `%XX` decoding of a query value (a `+` stays a `+`).
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = b.get(i + 1..i + 3)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// One search result, reduced.
#[derive(Debug, PartialEq)]
struct Found {
    key: String,
    title: String,
    text: String,
    author: String,
    container: String,
    url: String,
    /// YYYY-MM-DD.
    date: Option<String>,
}

fn found_of(r: &Value, site: &str) -> Option<Found> {
    let content = &r["content"];
    if !matches!(content["type"].as_str(), Some("page" | "blogpost")) {
        return None;
    }
    let webui = content["_links"]["webui"]
        .as_str()
        .or_else(|| r["url"].as_str())?;
    let url = page_url(site, webui)?;
    let title = content["title"]
        .as_str()
        .map(squash)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| clean_text(r["title"].as_str().unwrap_or("")));
    let text = truncate(&clean_text(r["excerpt"].as_str().unwrap_or("")), 1000);
    let author = [
        &content["history"]["createdBy"]["displayName"],
        &content["version"]["by"]["displayName"],
    ]
    .iter()
    .find_map(|v| v.as_str().map(squash).filter(|s| !s.is_empty()))
    .unwrap_or_default();
    let container = [
        &r["resultGlobalContainer"]["title"],
        &content["space"]["name"],
    ]
    .iter()
    .find_map(|v| v.as_str().map(clean_text).filter(|s| !s.is_empty()))
    .unwrap_or_default();
    let date = [
        &r["lastModified"],
        &content["version"]["when"],
        &content["history"]["lastUpdated"]["when"],
    ]
    .iter()
    .find_map(|v| v.as_str().and_then(date::ymd_prefix));
    let key = content["id"]
        .as_str()
        .map(|id| format!("id:{id}"))
        .unwrap_or_else(|| url.clone());
    Some(Found {
        key,
        title,
        text,
        author,
        container,
        url,
        date,
    })
}

/// The link to a page: the site + `/wiki` + its `webui` path. An absolute link is kept only when
/// it is on the same site; anything else (another origin, `//host`, odd characters) is dropped.
fn page_url(site: &str, webui: &str) -> Option<String> {
    let w = webui.trim();
    if w.is_empty()
        || w.len() > 1500
        || w.contains('\\')
        || w.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    if let Some(rest) = w.strip_prefix(site) {
        return rest.starts_with('/').then(|| w.to_string());
    }
    if !w.starts_with('/') || w.starts_with("//") {
        return None;
    }
    if w == "/wiki" || w.starts_with("/wiki/") {
        Some(format!("{site}{w}"))
    } else {
        Some(format!("{site}/wiki{w}"))
    }
}

/// Search text without its highlight markup (`@@@hl@@@…@@@endhl@@@`) or HTML escaping, squashed.
fn clean_text(s: &str) -> String {
    squash(&decode_entities(
        &s.replace("@@@hl@@@", "").replace("@@@endhl@@@", ""),
    ))
}

/// Decodes the HTML entities search excerpts use, in one pass (so `&amp;lt;` becomes `&lt;`).
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let semi = tail
            .char_indices()
            .take(12)
            .find(|(_, c)| *c == ';')
            .map(|(j, _)| j);
        if let Some(j) = semi
            && let Some(c) = entity(&tail[1..j])
        {
            out.push(c);
            rest = &tail[j + 1..];
            continue;
        }
        out.push('&');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let n = name.strip_prefix('#')?;
            let code = match n.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => n.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// Most relevant first: a keyword in the title counts 3, in the excerpt 1, and a page changed in
/// the last week gets 2 more (the last fortnight, 1). Ties go to the newer page.
fn rank(found: Vec<Found>, q: &FetchQuery) -> Vec<ContextItem> {
    let words: Vec<String> = keyword_list(&q.keywords)
        .iter()
        .map(|k| plain_words(k))
        .filter(|w| !w.is_empty())
        .collect();
    let today = date::parse_ymd(q.today.trim());
    let mut scored: Vec<(i64, Found)> = found
        .into_iter()
        .map(|f| (score(&f, &words, today), f))
        .collect();
    scored.sort_by(|(sa, a), (sb, b)| {
        sb.cmp(sa)
            .then_with(|| b.date.cmp(&a.date))
            .then_with(|| a.title.cmp(&b.title))
    });
    scored
        .into_iter()
        .take(q.max_items)
        .map(|(_, f)| ContextItem {
            source: Provider::Confluence.id().into(),
            kind: "page".into(),
            title: f.title,
            text: f.text,
            author: f.author,
            container: f.container,
            url: f.url,
            date: f.date,
        })
        .collect()
}

fn score(f: &Found, words: &[String], today: Option<i64>) -> i64 {
    let title = plain_words(&f.title);
    let text = plain_words(&f.text);
    let mut s = 0;
    for w in words {
        if has_word(&title, w) {
            s += 3;
        }
        if has_word(&text, w) {
            s += 1;
        }
    }
    if let (Some(t), Some(d)) = (today, f.date.as_deref().and_then(date::parse_ymd)) {
        match t - d {
            0..=7 => s += 2,
            8..=14 => s += 1,
            _ => {}
        }
    }
    s
}

/// True when `needle` starts a word of `hay` (both [`plain_words`]): "mentor" is in "mentoring",
/// "role" is not in "control".
fn has_word(hay: &str, needle: &str) -> bool {
    !needle.is_empty()
        && hay
            .match_indices(needle)
            .any(|(i, _)| i == 0 || hay.as_bytes()[i - 1] == b' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn query_of(url: &str) -> Vec<(String, String)> {
        url.split_once('?')
            .map(|(_, q)| q)
            .unwrap_or("")
            .split('&')
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.to_string(), percent_decode(v).unwrap()))
            .collect()
    }

    fn app() -> OAuthApp {
        OAuthApp {
            client_id: "cid".into(),
            client_secret: "csecret".into(),
            redirect_uri: "https://opp.example/api/connectors/confluence/callback".into(),
            tenant: String::new(),
        }
    }

    #[test]
    fn authorize_url_asks_for_read_scopes_and_consent_only() {
        let url = authorize_url(&Endpoints::default(), &app(), "st8", &Pkce::new());
        assert!(url.starts_with("https://auth.atlassian.com/authorize?"));
        let q = query_of(&url);
        let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("audience"), Some("api.atlassian.com"));
        assert_eq!(get("client_id"), Some("cid"));
        assert_eq!(get("response_type"), Some("code"));
        assert_eq!(get("prompt"), Some("consent"));
        assert_eq!(get("state"), Some("st8"));
        assert_eq!(
            get("redirect_uri"),
            Some("https://opp.example/api/connectors/confluence/callback")
        );
        let scopes = parse_scopes(get("scope").unwrap());
        assert_eq!(scopes, s(POLICY.requested_scopes));
        assert!(verify_scopes(Provider::Confluence, &scopes).is_ok());
        assert!(get("code_challenge").is_none(), "Atlassian 3LO has no PKCE");
    }

    #[test]
    fn sites_are_normalised_or_refused() {
        let eps = Endpoints::default();
        for (raw, want) in [
            ("https://acme.atlassian.net", "https://acme.atlassian.net"),
            ("https://ACME.Atlassian.NET/", "https://acme.atlassian.net"),
            ("acme.atlassian.net", "https://acme.atlassian.net"),
            (
                "  https://acme.atlassian.net/wiki/ ",
                "https://acme.atlassian.net",
            ),
            (
                "https://acme.atlassian.net:443/wiki",
                "https://acme.atlassian.net",
            ),
            (
                "https://my-team-2.atlassian.net",
                "https://my-team-2.atlassian.net",
            ),
        ] {
            assert_eq!(normalize_site(&eps, raw).as_deref(), Ok(want), "{raw}");
        }
        for raw in [
            "",
            "http://acme.atlassian.net",
            "https://acme.atlassian.net.evil.com",
            "https://evilatlassian.net",
            "https://atlassian.net",
            "https://a.b.atlassian.net",
            "https://-acme.atlassian.net",
            "https://acme_x.atlassian.net",
            "https://acme.atlassian.net:8443",
            "https://acme.atlassian.net/wiki/spaces/ENG",
            "https://acme.atlassian.net/other",
            "https://acme.atlassian.net//wiki",
            "https://acme.atlassian.net/wiki/wiki",
            "https://acme.atlassian.net?x=1",
            "https://acme.atlassian.net/#x",
            "https://user:pw@acme.atlassian.net",
            "https://acme.atlassian.net\\@evil.com",
            "ftp://acme.atlassian.net",
            "javascript:alert(1)",
            "https://acme.atlassian.net./",
            "http://127.0.0.1:9990/confluence-site",
        ] {
            assert!(normalize_site(&eps, raw).is_err(), "{raw} must be refused");
        }
        // Tests add exact loopback bases; only those exact bases pass.
        let mut test_eps = Endpoints::default();
        test_eps
            .confluence_sites
            .push("http://127.0.0.1:9990/confluence-site".into());
        assert_eq!(
            normalize_site(&test_eps, "http://127.0.0.1:9990/confluence-site/wiki").as_deref(),
            Ok("http://127.0.0.1:9990/confluence-site")
        );
        assert!(normalize_site(&test_eps, "http://127.0.0.1:9990/other").is_err());
        assert!(normalize_site(&test_eps, "http://127.0.0.1:9991/confluence-site").is_err());
    }

    #[test]
    fn keywords_become_safe_cql() {
        let q = FetchQuery {
            since_days: 30,
            max_items: 10,
            keywords: s(&[
                "hackathon",
                "call for",
                "Call For",
                "x\" OR type = attachment OR text ~ \"y",
                "c++ \\ * ? -draft",
                "AND",
                "!!!",
                "",
            ]),
            today: "2026-11-26".into(),
        };
        let cql = build_cql(&q);
        assert_eq!(
            cql,
            "type in (page, blogpost) AND lastmodified >= \"2026-10-27\" AND (text ~ \"hackathon\" OR text ~ \"\\\"call for\\\"\" OR text ~ \"\\\"x or type attachment or text y\\\"\" OR text ~ \"\\\"c draft\\\"\" OR text ~ \"and\") ORDER BY lastmodified DESC"
        );
        // Outside the escaped literals, nothing a keyword could inject.
        let outside: String = {
            let mut out = String::new();
            let mut in_str = false;
            let mut esc = false;
            for c in cql.chars() {
                if in_str {
                    if esc {
                        esc = false;
                    } else if c == '\\' {
                        esc = true;
                    } else if c == '"' {
                        in_str = false;
                    }
                } else if c == '"' {
                    in_str = true;
                } else {
                    out.push(c);
                }
            }
            out
        };
        assert!(!outside.contains("attachment"));
        assert_eq!(outside.matches(" OR ").count(), 4);
    }

    #[test]
    fn cql_falls_back_to_defaults_and_now() {
        let q = FetchQuery {
            since_days: 0,
            max_items: 10,
            keywords: vec![],
            today: "not a date".into(),
        };
        let cql = build_cql(&q);
        assert!(cql.contains("lastmodified >= now(\"-1d\")"), "{cql}");
        assert_eq!(
            cql.matches("text ~").count(),
            MAX_KEYWORDS.min(DEFAULT_KEYWORDS.len())
        );
        let many = FetchQuery {
            keywords: (0..100).map(|i| format!("word{i}")).collect(),
            ..q
        };
        assert_eq!(build_cql(&many).matches("text ~").count(), MAX_KEYWORDS);
    }

    #[test]
    fn excerpts_lose_highlight_markup_and_entities() {
        assert_eq!(
            clean_text(
                "Join the @@@hl@@@hackathon@@@endhl@@@ &amp; win &quot;prizes&quot; &lt;soon&gt; &#39;26 &#x2014; &amp;lt;b&amp;gt;\n now"
            ),
            "Join the hackathon & win \"prizes\" <soon> '26 \u{2014} &lt;b&gt; now"
        );
        assert_eq!(
            clean_text("Fish & chips &unknown; &"),
            "Fish & chips &unknown; &"
        );
        assert_eq!(
            clean_text("émigré &eacute; ünïcödé"),
            "émigré &eacute; ünïcödé"
        );
    }

    #[test]
    fn page_links_are_built_from_the_site() {
        let site = "https://acme.atlassian.net";
        assert_eq!(
            page_url(site, "/spaces/ENG/pages/1/Title").as_deref(),
            Some("https://acme.atlassian.net/wiki/spaces/ENG/pages/1/Title")
        );
        assert_eq!(
            page_url(site, "/wiki/spaces/ENG/pages/1").as_deref(),
            Some("https://acme.atlassian.net/wiki/spaces/ENG/pages/1")
        );
        assert_eq!(
            page_url(site, "https://acme.atlassian.net/wiki/x/AbC").as_deref(),
            Some("https://acme.atlassian.net/wiki/x/AbC")
        );
        for bad in [
            "",
            "https://evil.example/wiki/spaces/ENG",
            "https://acme.atlassian.net.evil.example/wiki",
            "//evil.example/x",
            "spaces/ENG",
            "/spaces/ENG/pages/1/A title",
            "/spaces\\ENG",
            "javascript:alert(1)",
        ] {
            assert_eq!(page_url(site, bad), None, "{bad}");
        }
    }

    #[test]
    fn next_page_takes_only_the_cursor_and_start() {
        let reply = json!({"_links": {"next": "/rest/api/search?next=true&cursor=_f_MjU%3D_sa_abc&expand=body.storage&limit=25&start=25&cql=type%3Dpage"}});
        assert_eq!(
            next_page(&reply),
            Some(NextPage {
                cursor: "_f_MjU=_sa_abc".into(),
                start: Some("25".into())
            })
        );
        assert_eq!(next_page(&json!({"_links": {}})), None);
        assert_eq!(
            next_page(&json!({"_links": {"next": "/rest/api/search?cursor=%ZZ"}})),
            None
        );
        assert_eq!(
            next_page(&json!({"_links": {"next": "/rest/api/search?cursor=a&start=-1"}})),
            Some(NextPage {
                cursor: "a".into(),
                start: None
            })
        );
    }

    #[test]
    fn token_replies_are_read_faithfully() {
        let ok = json!({
            "access_token": "at", "refresh_token": "rt", "expires_in": 3600, "token_type": "Bearer",
            "scope": "search:confluence read:confluence-user write:confluence-content offline_access"
        });
        let t = read_tokens(200, &ok, Step::Exchange).unwrap();
        assert_eq!(t.access, "at");
        assert_eq!(t.refresh.as_deref(), Some("rt"));
        assert_eq!(
            t.scopes,
            Some(s(&[
                "search:confluence",
                "read:confluence-user",
                "write:confluence-content",
                "offline_access"
            ])),
            "kept exactly as granted, so the facade sees the write scope"
        );
        assert!((t.expires_at - date::now_unix() - 3600).abs() <= 5);
        let no_scope = read_tokens(200, &json!({"access_token": "at"}), Step::Exchange).unwrap();
        assert_eq!(no_scope.scopes, None);
        assert!((no_scope.expires_at - date::now_unix() - DEFAULT_LIFETIME_SECS).abs() <= 5);
        assert!(matches!(
            read_tokens(200, &json!({"refresh_token": "rt"}), Step::Exchange),
            Err(Error::Invalid(_))
        ));
        let bad_grant = json!({"error": "invalid_grant", "error_description": "Unknown or invalid refresh token. rt-SECRET"});
        let e = read_tokens(403, &bad_grant, Step::Refresh).unwrap_err();
        assert!(matches!(e, Error::Unauthorized(_)) && e.needs_reconnect());
        assert!(
            !e.to_string().contains("rt-SECRET"),
            "never echoes the reply"
        );
        assert!(matches!(
            read_tokens(403, &bad_grant, Step::Exchange),
            Err(Error::Unauthorized(_))
        ));
        let client = json!({"error": "access_denied", "error_description": "Unauthorized"});
        assert!(matches!(
            read_tokens(401, &client, Step::Exchange),
            Err(Error::Provider(401, m)) if m.contains("ATLASSIAN_CLIENT_SECRET")
        ));
        assert!(matches!(
            read_tokens(500, &json!({}), Step::Refresh),
            Err(Error::Provider(500, _))
        ));
        assert!(matches!(
            read_tokens(400, &json!({"error": "weird<script>"}), Step::Refresh),
            Err(Error::Provider(400, m)) if m.contains("weirdscript") && !m.contains('<')
        ));
    }

    #[test]
    fn accessible_resources_must_all_be_read_only() {
        let site = |id: &str, scopes: &[&str]| json!({"id": id, "name": "Acme", "url": "https://acme.atlassian.net", "scopes": scopes, "avatarUrl": "https://x/y.png"});
        let read = ["search:confluence", "read:confluence-user"];
        let ok = json!([site("c-1", &read)]);
        assert_eq!(
            pick_site(&ok, None),
            Ok(Site {
                cloud_id: "c-1".into(),
                name: "Acme".into(),
                url: "https://acme.atlassian.net".into()
            })
        );
        let write = json!([
            site("c-1", &read),
            site("c-2", &["write:confluence-content", "search:confluence"])
        ]);
        assert_eq!(
            pick_site(&write, Some("c-1")),
            Err(Error::WriteScopes(s(&["write:confluence-content"])))
        );
        let two = json!([site("c-1", &["read:confluence-user"]), site("c-2", &read)]);
        assert_eq!(
            pick_site(&two, None).unwrap().cloud_id,
            "c-2",
            "prefers search"
        );
        assert_eq!(pick_site(&two, Some("c-1")).unwrap().cloud_id, "c-1");
        assert!(matches!(
            pick_site(&two, Some("gone")),
            Err(Error::Unauthorized(_))
        ));
        assert!(matches!(
            pick_site(&json!([]), None),
            Err(Error::Unauthorized(_))
        ));
        assert!(matches!(
            pick_site(&json!({}), None),
            Err(Error::Invalid(_))
        ));
        let path_trick = json!([site("../x", &read)]);
        assert!(pick_site(&path_trick, None).is_err());
    }

    #[test]
    fn signed_out_visitors_are_not_users() {
        assert_eq!(
            known_user(&json!({"type": "known", "accountId": "a1", "displayName": "Dana"})),
            Some("Dana".into())
        );
        assert_eq!(
            known_user(&json!({"type": "known", "accountId": "a1", "publicName": "D"})),
            Some("D".into())
        );
        assert_eq!(
            known_user(&json!({"type": "anonymous", "displayName": "Anonymous"})),
            None
        );
        assert_eq!(
            known_user(&json!({"type": "known", "accountId": "", "displayName": "X"})),
            None
        );
    }

    #[test]
    fn emails_and_tokens_are_checked() {
        assert_eq!(
            clean_email(" sam@acme.example ").as_deref(),
            Ok("sam@acme.example")
        );
        for bad in [
            "",
            "sam",
            "sam@",
            "@acme.example",
            "sam@acme",
            "s am@acme.example",
            "sam:x@acme.example",
            "a@b@c.example",
        ] {
            assert!(clean_email(bad).is_err(), "{bad}");
        }
        assert_eq!(
            clean_token(" ATATT3x-abc_123= ").as_deref(),
            Ok("ATATT3x-abc_123=")
        );
        for bad in ["", "two words", "line\nbreak"] {
            assert!(clean_token(bad).is_err());
        }
    }

    #[test]
    fn results_map_to_items_and_rank() {
        let site = "https://acme.atlassian.net";
        let result = |id: &str, kind: &str, title: &str, excerpt: &str, when: &str| {
            json!({
                "content": {"id": id, "type": kind, "status": "current", "title": title,
                    "_links": {"webui": format!("/spaces/ENG/pages/{id}/T"), "tinyui": "/x/A"}},
                "title": format!("@@@hl@@@{title}@@@endhl@@@"),
                "excerpt": excerpt,
                "url": format!("/spaces/ENG/pages/{id}/T"),
                "resultGlobalContainer": {"title": "Engineering &amp; Ops", "displayUrl": "/spaces/ENG"},
                "entityType": "content",
                "lastModified": when,
            })
        };
        let f = found_of(
            &result(
                "1",
                "page",
                "Hackathon",
                "sign up &amp; @@@hl@@@apply@@@endhl@@@",
                "2026-11-25T10:00:00.000Z",
            ),
            site,
        )
        .unwrap();
        assert_eq!(f.title, "Hackathon");
        assert_eq!(f.text, "sign up & apply");
        assert_eq!(f.container, "Engineering & Ops");
        assert_eq!(
            f.url,
            "https://acme.atlassian.net/wiki/spaces/ENG/pages/1/T"
        );
        assert_eq!(f.date.as_deref(), Some("2026-11-25"));
        assert_eq!(f.author, "");
        assert!(found_of(&result("2", "attachment", "a.pdf", "", "2026-11-25"), site).is_none());
        let found = vec![
            found_of(
                &result(
                    "3",
                    "page",
                    "Sprint notes",
                    "release deadline",
                    "2026-11-25T00:00:00Z",
                ),
                site,
            )
            .unwrap(),
            found_of(
                &result(
                    "4",
                    "page",
                    "Mentoring programme",
                    "apply now",
                    "2026-11-01T00:00:00Z",
                ),
                site,
            )
            .unwrap(),
            found_of(
                &result(
                    "5",
                    "blogpost",
                    "Control room",
                    "nothing here",
                    "2026-11-26T00:00:00Z",
                ),
                site,
            )
            .unwrap(),
            f,
        ];
        let q = FetchQuery {
            since_days: 30,
            max_items: 3,
            keywords: s(&[
                "mentor",
                "programme",
                "apply",
                "deadline",
                "hackathon",
                "role",
            ]),
            today: "2026-11-26".into(),
        };
        let items = rank(found, &q);
        let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, ["Mentoring programme", "Hackathon", "Sprint notes"]);
        assert!(
            items
                .iter()
                .all(|i| i.kind == "page" && i.source == "confluence")
        );
        assert!(!has_word("control room", "role") && has_word("open roles", "role"));
        assert!(has_word("a call for talks", "call for"));
    }

    #[test]
    fn pages_follow_max_items() {
        assert_eq!(pages_for(1), 1);
        assert_eq!(pages_for(12), 1);
        assert_eq!(pages_for(13), 2);
        assert_eq!(pages_for(60), MAX_PAGES);
        assert_eq!(pages_for(usize::MAX), MAX_PAGES);
    }
}
