//! Slack, read-only, with a **user token** (`xoxp-…`), so the connector sees exactly what the
//! person can already see in Slack and nothing more.
//!
//! # Connecting
//!
//! * **OAuth v2, user scopes only.** `{slack_auth}/oauth/v2/authorize?client_id=…&user_scope=
//!   search:read,users:read&redirect_uri=…&state=…`. There is no `scope` parameter, so no bot
//!   user and no bot token. Then `POST {slack_api}/api/oauth.v2.access` with the code and the
//!   redirect URI, the client id and secret sent with HTTP Basic auth as Slack recommends. The user
//!   token is in `authed_user`. Its granted `scope` is checked here and again by the facade. If a
//!   bot token came back too, which only happens when someone asked for bot scopes, its scopes are
//!   checked as well and the bot token is dropped: it is never stored or used.
//! * **No PKCE.** Slack accepts PKCE only from apps that opt in. Opting in marks the app as a
//!   *public* client, limits its refresh tokens to 30 days, and can't be undone without Slack
//!   support. Apps that haven't opted in may not send PKCE arguments (`pkce_not_allowed`).
//!   Opportunity is a confidential, server-side client. The code is useless without the client
//!   secret, which never leaves the server, and the server binds each sign-in to the person's
//!   session with `state`. So the verifier the facade generates is deliberately not sent.
//! * **Token rotation** is optional and recommended. When it is on, the token is `xoxe.xoxp-…`. It
//!   expires after 12 hours and comes with a single-use refresh token. [`refresh`] trades the
//!   refresh token at the same endpoint (`grant_type=refresh_token`) and keeps the new one.
//! * **A pasted token** must be a plain user token. Bot (`xoxb-`), app-level (`xapp-`), rotating
//!   (`xoxe.`), refresh, session and legacy tokens are refused before anything is sent. A legacy
//!   all-access token that does start with `xoxp-` is refused because of its scopes.
//!
//! # Reading
//!
//! Reads are GET only, to three exact methods: `auth.test` (who the token is), `search.messages`
//! (the messages) and `users.info` (the names of authors and of people mentioned). Every Web API
//! reply carries `x-oauth-scopes`, the scopes the token *really* has. That header goes through
//! [`verify_scopes`] on every single call, before the reply is used, so a token that has gained a
//! write scope fails with [`Error::WriteScopes`] on its very next call. A successful reply without
//! the header is treated as suspicious, and nothing in it is used.
//!
//! # Searching
//!
//! `search.messages` takes Slack's query syntax. Each search is limited to the window with
//! `after:` (YYYY-MM-DD, exclusive):
//!
//! * the strongest opportunity markers get a search of their own, one word or `"quoted phrase"`
//!   each, which means the same whether Slack joins several words with AND or with OR;
//! * the next ones go into a few `a OR b OR c` searches. Slack documents the `OR` operator for its
//!   search; if it isn't honoured, these searches simply find nothing;
//! * one search looks for messages that mention the person (`<@U…>`).
//!
//! Results are deduplicated by permalink and scored by keyword strength, mentions, direct
//! messages and recency. Messages the person wrote themselves count for less. The list is then
//! capped. Slack signals rate limits with HTTP 429 and `Retry-After` (or `ratelimited`). When that
//! happens, what was gathered so far is returned, or [`Error::RateLimited`] when there is nothing.
//!
//! Errors never contain a token or message content. From a reply, only Slack's error code reaches
//! a message, and only when it looks like one (a lowercase identifier).

use std::collections::HashMap;

use model::ContextItem;
use model::date::{DAY_SECS, now_unix, parse_ymd, ymd};
use model::text::{squash, truncate};
use serde_json::Value;

use crate::gateway::{Reply, with_query};
use crate::{
    Account, Auth, Credential, DEFAULT_KEYWORDS, Endpoints, Error, FetchQuery, Gateway, OAuthApp,
    OAuthHttp, Pkce, Policy, Provider, TokenInput, parse_scopes, verify_scopes,
};

pub const POLICY: Policy = Policy {
    read_paths: &["/api/auth.test", "/api/search.messages", "/api/users.info"],
    token_paths: &["/api/oauth.v2.access"],
    requested_scopes: &["search:read", "users:read"],
    // Read-only scopes a token may carry. Only the two above are requested; the rest are accepted
    // so that a token that happens to have other read scopes isn't refused as if it could write.
    allowed_scopes: &[
        "search:read",
        "users:read",
        "users:read.email",
        "team:read",
        "channels:read",
        "groups:read",
        "im:read",
        "mpim:read",
        "channels:history",
        "groups:history",
        "im:history",
        "mpim:history",
        "identity.basic",
    ],
    token_mode: true,
    reads: "Recent messages that look like opportunities, found with Slack search in the channels and direct messages you can already see, and the names of the people involved.",
};

/// Keywords that get a search of their own.
const SINGLE_SEARCHES: usize = 6;
/// `a OR b OR …` searches for the keywords after those, and how many keywords each one takes.
const OR_SEARCHES: usize = 2;
const OR_TERMS: usize = 8;
/// Matches asked for per search (Slack allows up to 100).
const PER_SEARCH: &str = "25";
/// Names looked up with `users.info` per fetch: authors first, then people mentioned.
const MAX_NAME_LOOKUPS: usize = 20;
/// Characters of message text kept per item.
const TEXT_CHARS: usize = 1200;
/// Words of the message used in an item's title.
const TITLE_WORDS: usize = 8;
/// Rotating access tokens always last this long ("will always expire in 43,200 seconds").
const ROTATING_TOKEN_SECS: i64 = 43_200;

/// The order in which keywords earn a search of their own: the most distinctive markers of an
/// opportunity in workplace chat first. A keyword not listed here (one a caller chose) comes
/// right after "speaker".
const SEARCH_ORDER: &[&str] = &[
    "hackathon",
    "call for",
    "hiring",
    "speaker",
    "intro",
    "partnership",
    "opportunity",
    "secondment",
    "applications",
    "mentoring",
    "nomination",
    "fellowship",
    "grant",
    "rfp",
    "tender",
    "scholarship",
    "volunteer",
    "conference",
    "deadline",
    "apply",
    "funding",
    "award",
    "competition",
    "promotion",
    "invitation",
    "looking for",
    "mentor",
    "programme",
    "proposal",
    "speaking",
    "panel",
    "opening",
    "position",
    "partner",
    "collaborate",
    "invite",
    "program",
    "role",
];

/// Scopes of legacy all-access tokens (custom-integration "API tokens", legacy bots).
const LEGACY_SCOPES: &[&str] = &["identify", "read", "post", "client", "bot", "admin"];

const BOT_TOKEN: &str = "That's a Slack bot token (xoxb-…). Opportunity reads Slack as you, with a user token (xoxp-…), so it only ever sees what you can see. Connect with Slack, or paste a user token.";
const APP_TOKEN: &str = "That's a Slack app-level token (xapp-…), which can't read messages. Connect with Slack, or paste a user token (xoxp-…).";
const ROTATING_TOKEN: &str = "That's a rotating Slack token (xoxe.…). It expires within 12 hours and can only be renewed by signing in. Connect with Slack instead, or paste a user token that doesn't rotate (xoxp-…).";
const REFRESH_TOKEN: &str = "That's a Slack refresh token (xoxe-…), not an access token. Connect with Slack, or paste a user token (xoxp-…).";
const LEGACY_TOKEN: &str = "That looks like a legacy or browser-session Slack token. Those can do everything you can, including posting, so they aren't accepted. Connect with Slack, or paste a user token (xoxp-…) from an app that has only read scopes.";
const NOT_A_USER_TOKEN: &str = "That doesn't look like a Slack user token (they start with xoxp-). Connect with Slack, or paste a user token.";
const NO_SEARCH: &str = "this Slack token can't search messages, because it doesn't have the search:read permission. Connect with Slack, or paste a token from an app with the search:read user scope.";
const NO_SCOPES_HEADER: &str = "Slack didn't report what this token is allowed to do (no x-oauth-scopes header), so nothing in its reply was used. A proxy that strips headers can cause this.";
const NOT_USER_GRANT: &str = "Slack issued a token that isn't a user token, so it wasn't used. The Slack app should have user scopes only.";
const OTHER_WORKSPACE: &str =
    "this Slack token belongs to a different workspace than the one that was connected";

// ---------------------------------------------------------------------------------------------
// The functions the facade calls
// ---------------------------------------------------------------------------------------------

/// Where to send the person to approve the connection. User scopes only; no PKCE (see the
/// module docs).
pub fn authorize_url(eps: &Endpoints, app: &OAuthApp, state: &str, pkce: &Pkce) -> String {
    let _ = pkce; // Deliberately not sent: see "No PKCE" in the module docs.
    with_query(
        &format!("{}/oauth/v2/authorize", eps.slack_auth),
        &[
            ("client_id", &app.client_id),
            ("user_scope", &POLICY.requested_scopes.join(",")),
            ("redirect_uri", &app.redirect_uri),
            ("state", state),
        ],
    )
}

/// Trade the callback's code for a user token.
pub fn exchange_code(
    http: &OAuthHttp,
    eps: &Endpoints,
    app: &OAuthApp,
    code: &str,
    pkce: &Pkce,
) -> Result<Credential, Error> {
    let _ = pkce; // Deliberately not sent: see "No PKCE" in the module docs.
    let code = code.trim();
    if code.is_empty() {
        return Err(Error::Invalid("Slack's sign-in reply had no code".into()));
    }
    let json = token_call(
        http,
        eps,
        app,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &app.redirect_uri),
        ],
    )?;
    grant(&json, None)
}

/// A fresh access token when token rotation is on. A credential without a refresh token (no
/// rotation, or pasted) is returned unchanged.
pub fn refresh(
    http: &OAuthHttp,
    eps: &Endpoints,
    app: &OAuthApp,
    cred: &Credential,
) -> Result<Credential, Error> {
    let Some(refresh_token) = cred
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Ok(cred.clone());
    };
    let json = token_call(
        http,
        eps,
        app,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
    )?;
    let mut fresh = grant(&json, Some(cred))?;
    fresh.provider = cred.provider.clone();
    fresh.mode = cred.mode.clone();
    Ok(fresh)
}

/// Connect with a pasted user token: refuse anything else before sending it anywhere, then ask
/// Slack who it is and what it can do.
pub fn connect_with_token(gw: &Gateway, input: &TokenInput) -> Result<Credential, Error> {
    let token = input.token.trim();
    if let Some(why) = pasted_token_problem(token) {
        return Err(Error::Unsupported(why.into()));
    }
    let auth = Auth::Bearer(token.to_string());
    let reply = gw.get(&api_url(gw, "auth.test"), &[], &auth, &[])?;
    if let Some(why) = legacy_problem(reply.oauth_scopes.as_deref()) {
        return Err(Error::Unsupported(why));
    }
    let me = check(reply)?;
    let account = identity(&me.json)?;
    require_search(&me.scopes)?;
    Ok(Credential {
        access_token: token.to_string(),
        scopes: me.scopes,
        account,
        ..Default::default()
    })
}

/// Check the token still works and is still read-only; who and where it is.
pub fn verify(gw: &Gateway, cred: &Credential) -> Result<Account, Error> {
    let auth = stored_auth(cred)?;
    let me = call(gw, &auth, "auth.test", &[])?;
    let account = identity(&me.json)?;
    if !cred.account.id.is_empty() && account.id != cred.account.id {
        return Err(Error::Unauthorized(OTHER_WORKSPACE.into()));
    }
    Ok(account)
}

/// Recent messages that look like opportunities, most relevant first, at most `q.max_items`.
pub fn fetch(gw: &Gateway, cred: &Credential, q: &FetchQuery) -> Result<Vec<ContextItem>, Error> {
    let auth = stored_auth(cred)?;
    if q.max_items == 0 {
        return Ok(Vec::new());
    }
    // Who the token is, and a scope check, before any message is read.
    let me = call(gw, &auth, "auth.test", &[])?;
    let who = identity(&me.json)?;
    let my_id = text_of(&me.json["user_id"]);
    let window = Window::new(&q.today, q.since_days);
    let keywords = keywords(&q.keywords);
    let mut found = Found::default();
    let mut halted = None;
    for search in plan(&keywords, &my_id, &window.after) {
        let query = [
            ("query", search.query.as_str()),
            ("count", PER_SEARCH),
            ("sort", "score"),
        ];
        match call(gw, &auth, "search.messages", &query) {
            Ok(reply) => found.add(&reply.json, &search, &window, &who.url),
            // Keep what was gathered; a security or authorisation problem still stops everything.
            Err(e @ (Error::RateLimited(_) | Error::Network(_) | Error::Provider(..))) => {
                halted = Some(e);
                break;
            }
            Err(e) => return Err(e),
        }
    }
    if found.list.is_empty() {
        return match halted {
            Some(e) => Err(e),
            None => Ok(Vec::new()),
        };
    }
    let ranked = found.ranked(&keywords, &my_id, &window, q.max_items);
    let names = names(gw, &auth, &ranked)?;
    Ok(ranked.iter().map(|m| m.item(&names)).collect())
}

// ---------------------------------------------------------------------------------------------
// Tokens and the token endpoint
// ---------------------------------------------------------------------------------------------

/// Plain identifier characters only, and a sane length: refuses anything that could be more than
/// a token (spaces, line breaks, "Bearer …").
fn well_formed(token: &str) -> bool {
    (12..=512).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// A user access token, plain or rotating.
fn is_user_token(token: &str) -> bool {
    (token.starts_with("xoxp-") || token.starts_with("xoxe.xoxp-")) && well_formed(token)
}

/// Why a pasted token can't be used, or None when it's a plain user token.
fn pasted_token_problem(token: &str) -> Option<&'static str> {
    if token.starts_with("xoxp-") && well_formed(token) {
        return None;
    }
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| token.starts_with(p));
    Some(if starts(&["xoxb-", "xoxe.xoxb-"]) {
        BOT_TOKEN
    } else if starts(&["xapp-", "xoxe.xapp-"]) {
        APP_TOKEN
    } else if starts(&["xoxe."]) {
        ROTATING_TOKEN
    } else if starts(&["xoxe-"]) {
        REFRESH_TOKEN
    } else if starts(&["xoxa-", "xoxr-", "xoxs-", "xoxc-", "xoxd-", "xoxo-"]) {
        LEGACY_TOKEN
    } else {
        NOT_A_USER_TOKEN
    })
}

/// A legacy all-access token, told apart by its umbrella scopes.
fn legacy_problem(scopes_header: Option<&str>) -> Option<String> {
    let legacy: Vec<String> = parse_scopes(scopes_header?)
        .into_iter()
        .filter(|s| LEGACY_SCOPES.contains(&s.to_ascii_lowercase().as_str()))
        .collect();
    (!legacy.is_empty()).then(|| {
        format!(
            "This is a legacy Slack token with all-access permissions ({}), which include posting, so it isn't accepted. Connect with Slack, or paste a user token from an app that has only read scopes.",
            legacy.join(", ")
        )
    })
}

/// The stored token as request auth, if it is a user token.
fn stored_auth(cred: &Credential) -> Result<Auth, Error> {
    let token = cred.access_token.trim();
    if is_user_token(token) {
        Ok(Auth::Bearer(token.to_string()))
    } else {
        Err(Error::Unauthorized(
            "this Slack connection doesn't hold a user token".into(),
        ))
    }
}

fn require_search(scopes: &[String]) -> Result<(), Error> {
    if scopes
        .iter()
        .any(|s| s.trim().eq_ignore_ascii_case("search:read"))
    {
        Ok(())
    } else {
        Err(Error::Forbidden(NO_SEARCH.into()))
    }
}

/// POST to `oauth.v2.access` (client id and secret in HTTP Basic auth) and insist on `ok`.
fn token_call(
    http: &OAuthHttp,
    eps: &Endpoints,
    app: &OAuthApp,
    form: &[(&str, &str)],
) -> Result<Value, Error> {
    let url = format!("{}/api/oauth.v2.access", eps.slack_api);
    let (status, json) = http.post_form(&url, form, Some((&app.client_id, &app.client_secret)))?;
    if json.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(json)
    } else {
        Err(slack_error(status, &json))
    }
}

/// The credential in an `oauth.v2.access` reply. The user token is in `authed_user` after a code
/// exchange and at the top level after refreshing a user token. `previous` is the credential being
/// refreshed: its scopes, refresh token and account carry over when the reply doesn't repeat them.
fn grant(json: &Value, previous: Option<&Credential>) -> Result<Credential, Error> {
    let authed = &json["authed_user"];
    let nested = authed.get("access_token").and_then(Value::as_str).is_some();
    let holder = if nested { authed } else { json };
    let token = text_of(&holder["access_token"]);
    if token.is_empty() {
        return Err(Error::Invalid("Slack's reply had no user token".into()));
    }
    if text_of(&holder["token_type"]) != "user" || !is_user_token(&token) {
        return Err(Error::Unsupported(NOT_USER_GRANT.into()));
    }
    // A bot token alongside the user token means bot scopes were granted. Check them; the bot
    // token itself goes no further than this function.
    if nested && json.get("access_token").is_some_and(|t| !t.is_null()) {
        verify_scopes(
            Provider::Slack,
            &parse_scopes(json["scope"].as_str().unwrap_or("")),
        )?;
    }
    let scopes = match holder.get("scope").and_then(Value::as_str) {
        Some(s) => parse_scopes(s),
        None => previous.map(|p| p.scopes.clone()).unwrap_or_default(),
    };
    if scopes.is_empty() {
        return Err(Error::Invalid(
            "Slack didn't say which permissions were granted, so the connection wasn't used".into(),
        ));
    }
    verify_scopes(Provider::Slack, &scopes)?;
    if previous.is_none() {
        require_search(&scopes)?;
    }
    let expires_in = holder["expires_in"]
        .as_i64()
        .filter(|s| *s > 0)
        .or_else(|| token.starts_with("xoxe.").then_some(ROTATING_TOKEN_SECS));
    let refresh_token = holder["refresh_token"]
        .as_str()
        .map(str::trim)
        .filter(|t| well_formed(t))
        .map(str::to_string)
        .or_else(|| previous.and_then(|p| p.refresh_token.clone()));
    let account = match previous {
        Some(p) => p.account.clone(),
        None => Account {
            id: text_of(&json["team"]["id"]),
            name: text_of(&json["team"]["name"]),
            // Only the member id is known until `verify` asks `auth.test` for the name.
            user: text_of(&authed["id"]),
            url: String::new(),
        },
    };
    Ok(Credential {
        access_token: token,
        refresh_token,
        expires_at: expires_in.map(|s| now_unix() + s),
        scopes,
        account,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------------------------
// Web API calls: every reply's real scopes are checked before it is used
// ---------------------------------------------------------------------------------------------

/// A successful Web API reply whose token scopes were verified read-only.
struct Checked {
    json: Value,
    scopes: Vec<String>,
}

fn api_url(gw: &Gateway, method: &str) -> String {
    format!("{}/api/{method}", gw.endpoints().slack_api)
}

fn call(gw: &Gateway, auth: &Auth, method: &str, query: &[(&str, &str)]) -> Result<Checked, Error> {
    check(gw.get(&api_url(gw, method), query, auth, &[])?)
}

/// Layer 2 on every call. When Slack reports the token's scopes they must be read-only, whatever
/// else the reply says. A successful reply that doesn't report them isn't trusted.
fn check(reply: Reply) -> Result<Checked, Error> {
    let ok = reply.json.get("ok").and_then(Value::as_bool) == Some(true);
    let scopes = reply
        .oauth_scopes
        .as_deref()
        .map(parse_scopes)
        .unwrap_or_default();
    if !scopes.is_empty() {
        verify_scopes(Provider::Slack, &scopes)?;
    } else if ok {
        return Err(Error::Invalid(NO_SCOPES_HEADER.into()));
    }
    if !ok {
        return Err(slack_error(reply.status, &reply.json));
    }
    Ok(Checked {
        json: reply.json,
        scopes,
    })
}

/// Slack's `{"ok": false, "error": "…"}` as an [`Error`]. Only the error code (and, for a
/// missing scope, the scope names) ever reaches the message.
fn slack_error(status: u16, json: &Value) -> Error {
    // On `missing_scope` Slack lists the token's scopes in `provided`: they must be read-only too.
    if let Some(provided) = json.get("provided").and_then(Value::as_str)
        && let Err(e) = verify_scopes(Provider::Slack, &parse_scopes(provided))
    {
        return e;
    }
    let code = error_code(json.get("error").and_then(Value::as_str).unwrap_or(""));
    let said = |what: &str| format!("{what} ({code})");
    // Unauthorized messages are shown inside parentheses already.
    let rejected = |what: &str| Error::Unauthorized(format!("{what}: {code}"));
    match code.as_str() {
        "invalid_auth" => rejected("Slack no longer accepts this token"),
        "not_authed" => rejected("Slack didn't receive a token"),
        "token_revoked" => rejected("the token was revoked in Slack"),
        "token_expired" => rejected("the token has expired"),
        "account_inactive" => rejected("the Slack account is deactivated"),
        "invalid_refresh_token" => rejected("Slack refused to renew the connection"),
        "invalid_code" => rejected("Slack's sign-in code was invalid or had expired"),
        "missing_scope" => {
            let needed = json
                .get("needed")
                .and_then(Value::as_str)
                .and_then(scope_names)
                .unwrap_or_else(|| code.clone());
            Error::Forbidden(format!(
                "the token lacks a permission this needs ({needed})"
            ))
        }
        "access_denied"
        | "no_permission"
        | "team_access_not_granted"
        | "ekm_access_denied"
        | "enterprise_is_restricted"
        | "accesslimited"
        | "two_factor_setup_required"
        | "not_allowed_token_type"
        | "workspace_denied_app" => Error::Forbidden(said("Slack refused")),
        "invalid_client_id"
        | "bad_client_secret"
        | "bad_redirect_uri"
        | "oauth_authorization_url_mismatch"
        | "invalid_grant_type"
        | "pkce_not_allowed"
        | "invalid_code_verifier" => Error::NotConfigured(format!(
            "Slack refused this server's app settings ({code}). The administrator should check SLACK_CLIENT_ID, SLACK_CLIENT_SECRET and the Slack app's redirect URL."
        )),
        "ratelimited" => Error::RateLimited(None),
        "" => Error::Provider(status, "Slack answered with an unnamed error".into()),
        _ => Error::Provider(status, said("Slack answered with an error")),
    }
}

/// Slack's error codes are lowercase identifiers (`token_revoked`). Anything else is replaced
/// whole, so no other text from a reply can reach a message or a log.
fn error_code(s: &str) -> String {
    let s = s.trim();
    let plain = (1..=60).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.');
    if plain {
        s.to_string()
    } else {
        "unrecognised_error".into()
    }
}

/// Scope names as Slack lists them (`search:read,users:read`), or None for anything else.
fn scope_names(s: &str) -> Option<String> {
    let s = s.trim();
    let plain = (1..=200).contains(&s.len())
        && s.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(b, b':' | b'.' | b'_' | b',' | b' ' | b'-')
        });
    plain.then(|| s.to_string())
}

/// The account `auth.test` describes. Refuses a bot identity whatever the token looked like.
fn identity(json: &Value) -> Result<Account, Error> {
    if json
        .get("bot_id")
        .and_then(Value::as_str)
        .is_some_and(|b| !b.is_empty())
    {
        return Err(Error::Unsupported(BOT_TOKEN.into()));
    }
    let account = Account {
        id: text_of(&json["team_id"]),
        name: text_of(&json["team"]),
        user: text_of(&json["user"]),
        url: Some(text_of(&json["url"]))
            .filter(|u| model::links::is_http_url(u))
            .unwrap_or_default(),
    };
    if account.id.is_empty() {
        return Err(Error::Invalid(
            "Slack didn't say which workspace this token belongs to".into(),
        ));
    }
    Ok(account)
}

fn text_of(v: &Value) -> String {
    v.as_str().map(str::trim).unwrap_or("").to_string()
}

// ---------------------------------------------------------------------------------------------
// Planning the searches
// ---------------------------------------------------------------------------------------------

/// The days to look at. `after` is the day before the first one, because `after:` is exclusive.
struct Window {
    after: String,
    /// First second of the window and the end of today (Unix seconds, UTC).
    start: i64,
    end: i64,
}

impl Window {
    fn new(today: &str, since_days: u32) -> Window {
        let today = parse_ymd(today.trim()).unwrap_or_else(|| now_unix().div_euclid(DAY_SECS));
        let since = i64::from(since_days.clamp(1, 365));
        let after = today - since;
        Window {
            after: ymd(after * DAY_SECS),
            start: (after + 1) * DAY_SECS,
            end: (today + 1) * DAY_SECS,
        }
    }

    /// 1.0 for a message from today, falling to 0.5 at the start of the window.
    fn recency(&self, ts: i64) -> f32 {
        let span = (self.end - self.start).max(1) as f32;
        let age = ((self.end - ts) as f32 / span).clamp(0.0, 1.0);
        1.0 - 0.5 * age
    }
}

/// The caller's keywords made safe to put in a Slack query: lowercase letters, digits, spaces,
/// `'`, `&` and inner hyphens only, so a keyword can never become a search modifier (`in:`,
/// `from:`, `-word`). Falls back to the defaults.
fn keywords(given: &[String]) -> Vec<String> {
    let mut out = clean_keywords(given.iter().map(String::as_str));
    if out.is_empty() {
        out = clean_keywords(DEFAULT_KEYWORDS.iter().copied());
    }
    out.truncate(60);
    out
}

fn clean_keywords<'a>(list: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for k in list {
        if let Some(k) = clean_keyword(k)
            && !out.contains(&k)
        {
            out.push(k);
        }
    }
    out
}

fn clean_keyword(k: &str) -> Option<String> {
    let mapped: String = k
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '\'' | '&' | '-') {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .to_lowercase();
    let k = squash(&mapped)
        .trim_start_matches('-')
        .trim_end_matches('-')
        .trim()
        .to_string();
    let n = k.chars().count();
    if !(2..=40).contains(&n) || matches!(k.as_str(), "or" | "and" | "not") {
        return None;
    }
    Some(k)
}

/// How much a keyword says "opportunity" in workplace chat.
fn weight(keyword: &str) -> f32 {
    match keyword {
        "hackathon" | "call for" | "rfp" | "tender" | "grant" | "fellowship" | "scholarship"
        | "secondment" | "nomination" | "hiring" | "speaker" | "applications" => 3.0,
        "role" | "program" | "partner" | "collaborate" | "invite" | "panel" | "position"
        | "opening" => 1.0,
        _ => 2.0,
    }
}

fn priority(keyword: &str) -> usize {
    SEARCH_ORDER
        .iter()
        .position(|k| *k == keyword)
        .map_or(7, |i| i * 2)
}

/// A keyword as a search term: a phrase is quoted.
fn term(keyword: &str) -> String {
    if keyword.contains(' ') {
        format!("\"{keyword}\"")
    } else {
        keyword.to_string()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// One keyword: every match contains it.
    Single,
    /// `a OR b OR …`: every match contains one of them.
    Any,
    /// Mentions of the person.
    Mention,
}

#[derive(Debug)]
struct Search {
    query: String,
    terms: Vec<String>,
    kind: Kind,
}

fn plan(keywords: &[String], me: &str, after: &str) -> Vec<Search> {
    let mut ordered: Vec<&String> = keywords.iter().collect();
    ordered.sort_by_key(|k| priority(k));
    let (single, rest) = ordered.split_at(ordered.len().min(SINGLE_SEARCHES));
    let mut out: Vec<Search> = single
        .iter()
        .map(|k| Search {
            query: format!("{} after:{after}", term(k)),
            terms: vec![(*k).clone()],
            kind: Kind::Single,
        })
        .collect();
    for group in rest.chunks(OR_TERMS).take(OR_SEARCHES) {
        let any: Vec<String> = group.iter().map(|k| term(k)).collect();
        out.push(Search {
            query: format!("{} after:{after}", any.join(" OR ")),
            terms: group.iter().map(|k| (*k).clone()).collect(),
            kind: Kind::Any,
        });
    }
    if is_member_id(me) {
        out.push(Search {
            query: format!("<@{me}> after:{after}"),
            terms: Vec::new(),
            kind: Kind::Mention,
        });
    }
    out
}

/// A Slack member id: `U…`, or `W…` on Enterprise Grid.
fn is_member_id(id: &str) -> bool {
    (2..=32).contains(&id.len())
        && (id.starts_with('U') || id.starts_with('W'))
        && id
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

// ---------------------------------------------------------------------------------------------
// Collecting, ranking, naming
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Place {
    Channel(String),
    Direct,
    Group,
}

impl Place {
    fn label(&self) -> String {
        match self {
            Place::Channel(name) => format!("#{name}"),
            Place::Direct => "Direct message".into(),
            Place::Group => "Group DM".into(),
        }
    }

    fn is_direct(&self) -> bool {
        !matches!(self, Place::Channel(_))
    }
}

/// One message found by one or more searches. `raw` is Slack's text: kept in memory for this
/// fetch only, never logged.
#[derive(Debug)]
struct Message {
    url: String,
    ts: i64,
    raw: String,
    place: Place,
    author_id: String,
    username: String,
    /// Keywords whose own search found it (Slack says it contains them).
    terms: Vec<String>,
    /// Found by an `a OR b` search.
    any_hit: bool,
    score: f32,
}

#[derive(Default)]
struct Found {
    list: Vec<Message>,
    by_url: HashMap<String, usize>,
}

impl Found {
    fn add(&mut self, json: &Value, search: &Search, window: &Window, team_url: &str) {
        let Some(matches) = json["messages"]["matches"].as_array() else {
            return;
        };
        for m in matches {
            let Some(ts) = m["ts"].as_str().and_then(parse_ts) else {
                continue;
            };
            if ts < window.start {
                continue;
            }
            let channel = &m["channel"];
            let url = match m["permalink"].as_str().map(str::trim) {
                Some(u) if model::links::is_http_url(u) => u.to_string(),
                _ => match permalink(team_url, &text_of(&channel["id"]), m["ts"].as_str()) {
                    Some(u) => u,
                    None => continue,
                },
            };
            let index = match self.by_url.get(&url) {
                Some(&i) => i,
                None => {
                    let raw = message_text(m);
                    if raw.trim().is_empty() {
                        continue;
                    }
                    self.list.push(Message {
                        url: url.clone(),
                        ts,
                        raw,
                        place: place(m),
                        author_id: text_of(&m["user"]),
                        username: text_of(&m["username"]),
                        terms: Vec::new(),
                        any_hit: false,
                        score: 0.0,
                    });
                    self.by_url.insert(url, self.list.len() - 1);
                    self.list.len() - 1
                }
            };
            let msg = &mut self.list[index];
            match search.kind {
                Kind::Single => {
                    for t in &search.terms {
                        if !msg.terms.contains(t) {
                            msg.terms.push(t.clone());
                        }
                    }
                }
                Kind::Any => msg.any_hit = true,
                Kind::Mention => {}
            }
        }
    }

    /// Scored, filtered, best first, capped.
    fn ranked(
        mut self,
        keywords: &[String],
        me: &str,
        window: &Window,
        cap: usize,
    ) -> Vec<Message> {
        for m in &mut self.list {
            m.score = score(m, keywords, me, window);
        }
        self.list.retain(|m| m.score > 0.0);
        // Stable: equal scores keep the order the searches found them in.
        self.list
            .sort_by(|a, b| b.score.total_cmp(&a.score).then(b.ts.cmp(&a.ts)));
        self.list.truncate(cap);
        self.list
    }
}

/// Keyword strength (the three strongest keywords, with diminishing weight), plus a mention of
/// the person and a direct message, halved for the person's own messages, times recency. Zero
/// for what isn't worth passing on.
fn score(m: &Message, keywords: &[String], me: &str, window: &Window) -> f32 {
    let plain = squash(&readable(&m.raw, &HashMap::new()).to_lowercase());
    let mut weights: Vec<f32> = keywords
        .iter()
        .filter(|k| m.terms.contains(k) || has_word(&plain, k))
        .map(|k| weight(k))
        .collect();
    if weights.is_empty() && m.any_hit {
        // Slack matched one of the OR'd keywords in a form this check doesn't recognise.
        weights.push(1.0);
    }
    weights.sort_by(|a, b| b.total_cmp(a));
    let mut base: f32 = weights
        .iter()
        .zip([1.0, 0.5, 0.25])
        .map(|(w, f)| w * f)
        .sum();
    if mentions(&m.raw, me) {
        base += 2.0;
    }
    if m.place.is_direct() {
        base += 1.0;
    }
    if !me.is_empty() && m.author_id == me {
        base *= 0.5;
    }
    if base < 1.0 {
        return 0.0;
    }
    base * window.recency(m.ts)
}

/// `word` appears in `text` at the start of a word (so "partner" also finds "partners").
fn has_word(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(i) = text[from..].find(word) {
        let at = from + i;
        if text[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric())
        {
            return true;
        }
        from = at + word.len();
    }
    false
}

fn mentions(raw: &str, me: &str) -> bool {
    is_member_id(me) && (raw.contains(&format!("<@{me}>")) || raw.contains(&format!("<@{me}|")))
}

/// Member ids mentioned in a message, in order of appearance.
fn mentioned_ids(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split("<@").skip(1) {
        let id: String = part
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            .collect();
        if is_member_id(&id) && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Seconds of a Slack timestamp ("1508284197.000015").
fn parse_ts(ts: &str) -> Option<i64> {
    let secs = ts.trim().split('.').next()?;
    if secs.is_empty() || !secs.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    secs.parse::<i64>().ok().filter(|s| *s > 0)
}

/// A message link built the way Slack builds them, for a match that came without one.
fn permalink(team_url: &str, channel: &str, ts: Option<&str>) -> Option<String> {
    let ts = ts?.trim();
    let ok_ts = !ts.is_empty() && ts.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    let ok_channel = !channel.is_empty() && channel.bytes().all(|b| b.is_ascii_alphanumeric());
    if !ok_ts || !ok_channel || !model::links::is_http_url(team_url) {
        return None;
    }
    Some(format!(
        "{}/archives/{channel}/p{}",
        team_url.trim_end_matches('/'),
        ts.replace('.', "")
    ))
}

fn place(m: &Value) -> Place {
    let channel = &m["channel"];
    let kind = text_of(&m["type"]);
    let name = text_of(&channel["name"]);
    let flag = |f: &str| channel[f].as_bool() == Some(true);
    if kind == "im" || flag("is_im") {
        Place::Direct
    } else if kind == "mpim" || flag("is_mpim") || name.starts_with("mpdm-") {
        Place::Group
    } else if name.is_empty() {
        Place::Channel("channel".into())
    } else {
        Place::Channel(name)
    }
}

/// The message's text, or what its attachments say when it has none (posts by apps and
/// workflows often put everything there).
fn message_text(m: &Value) -> String {
    let text = m["text"].as_str().unwrap_or("").trim();
    if !text.is_empty() {
        return text.to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    for a in m["attachments"].as_array().into_iter().flatten() {
        let own: Vec<String> = ["pretext", "title", "text"]
            .iter()
            .map(|f| text_of(&a[*f]))
            .filter(|s| !s.is_empty())
            .collect();
        if own.is_empty() {
            // The plain-text summary, for an attachment that has nothing else.
            parts.push(text_of(&a["fallback"]));
        } else {
            parts.extend(own);
        }
    }
    parts.retain(|p| !p.is_empty());
    parts.join("\n")
}

/// Look up the names of the authors, then of the people mentioned, of the messages kept. Every
/// reply's scopes are checked like any other; a name that can't be had is simply left out.
fn names(gw: &Gateway, auth: &Auth, msgs: &[Message]) -> Result<HashMap<String, String>, Error> {
    let mut wanted: Vec<String> = Vec::new();
    for m in msgs {
        if is_member_id(&m.author_id) && !wanted.contains(&m.author_id) {
            wanted.push(m.author_id.clone());
        }
    }
    for m in msgs {
        for id in mentioned_ids(&m.raw) {
            if !wanted.contains(&id) {
                wanted.push(id);
            }
        }
    }
    let mut names = HashMap::new();
    for id in wanted.into_iter().take(MAX_NAME_LOOKUPS) {
        match call(gw, auth, "users.info", &[("user", &id)]) {
            Ok(reply) => {
                if let Some(name) = person_name(&reply.json["user"]) {
                    names.insert(id, name);
                }
            }
            Err(
                e @ (Error::WriteScopes(_)
                | Error::Unauthorized(_)
                | Error::Blocked(_)
                | Error::Invalid(_)),
            ) => return Err(e),
            // No users:read, or slow down: the handles Slack gave will do.
            Err(Error::Forbidden(_) | Error::RateLimited(_) | Error::Network(_)) => break,
            Err(_) => {}
        }
    }
    Ok(names)
}

fn person_name(user: &Value) -> Option<String> {
    [
        &user["real_name"],
        &user["profile"]["real_name"],
        &user["profile"]["display_name"],
        &user["name"],
    ]
    .into_iter()
    .map(|v| squash(v.as_str().unwrap_or("")))
    .find(|s| !s.is_empty())
    .map(|s| truncate(&s, 80))
}

impl Message {
    fn item(&self, names: &HashMap<String, String>) -> ContextItem {
        let text = readable(&self.raw, names);
        let place = self.place.label();
        let author = names
            .get(&self.author_id)
            .cloned()
            .unwrap_or_else(|| self.username.clone());
        ContextItem {
            source: Provider::Slack.id().into(),
            kind: "message".into(),
            title: format!("{place} · {}", headline(&text)),
            text: truncate(text.trim(), TEXT_CHARS),
            author,
            container: place,
            url: self.url.clone(),
            date: Some(ymd(self.ts)),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Slack's markup as plain text
// ---------------------------------------------------------------------------------------------

/// Slack message markup as readable text: `<@U1>` becomes `@Name` (from `names`, else the label,
/// else "@someone"), `<#C1|general>` becomes `#general`, `<https://x|label>` becomes
/// `label (https://x)`, `<!here>` becomes `@here`; `&lt;`, `&gt;` and `&amp;` are unescaped.
fn readable(raw: &str, names: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('>') {
            Some(close) if !after[..close].contains('<') => {
                out.push_str(&control(&after[..close], names));
                rest = &after[close + 1..];
            }
            _ => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    let out = out
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace(['\u{e000}', '\u{e001}'], "");
    tidy(&out)
}

/// One `<…>` control sequence as text.
fn control(inner: &str, names: &HashMap<String, String>) -> String {
    let (body, label) = match inner.split_once('|') {
        Some((b, l)) => (b, Some(l.trim()).filter(|l| !l.is_empty())),
        None => (inner, None),
    };
    if let Some(id) = body.strip_prefix('@') {
        let name = names
            .get(id)
            .map(String::as_str)
            .or(label.map(|l| l.trim_start_matches('@')))
            .unwrap_or("someone");
        return format!("@{name}");
    }
    if body.starts_with('#') {
        return format!("#{}", label.unwrap_or("channel"));
    }
    if let Some(cmd) = body.strip_prefix('!') {
        return match cmd {
            "here" | "channel" | "everyone" => format!("@{cmd}"),
            _ if cmd.starts_with("subteam^") => {
                format!("@{}", label.unwrap_or("team").trim_start_matches('@'))
            }
            _ => label.unwrap_or("").to_string(),
        };
    }
    if let Some(address) = body.strip_prefix("mailto:") {
        return label.unwrap_or(address).to_string();
    }
    if body.starts_with("http://") || body.starts_with("https://") {
        return match label {
            Some(l) if l != body => format!("{l} ({body})"),
            _ => body.to_string(),
        };
    }
    label.unwrap_or(body).to_string()
}

/// Trailing spaces off each line, at most one blank line in a row, no control characters.
fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let line: String = line
            .chars()
            .map(|c| if c == '\t' { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect();
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 || out.is_empty() {
                continue;
            }
        } else {
            blank = 0;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    out.trim_end().to_string()
}

/// The first words of a message for its title, without emoji codes or bold/italic markers.
fn headline(text: &str) -> String {
    let words: Vec<&str> = text
        .split_whitespace()
        .filter(|w| !(w.len() > 2 && w.starts_with(':') && w.ends_with(':')))
        .map(|w| w.trim_matches(|c| matches!(c, '*' | '_' | '~' | '`')))
        .filter(|w| !w.is_empty())
        .collect();
    let mut h = words
        .iter()
        .take(TITLE_WORDS)
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    if words.len() > TITLE_WORDS {
        h.push('…');
    }
    truncate(&h, 90)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryAudit, NoAudit};
    use std::time::Duration;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn reply(scopes: Option<&str>, json: Value) -> Reply {
        Reply {
            status: 200,
            json,
            oauth_scopes: scopes.map(str::to_string),
        }
    }

    #[test]
    fn the_policy_reads_three_methods_and_asks_for_two_read_scopes() {
        assert_eq!(
            POLICY.read_paths,
            &["/api/auth.test", "/api/search.messages", "/api/users.info"]
        );
        assert_eq!(POLICY.requested_scopes, &["search:read", "users:read"]);
        let eps = Endpoints::default();
        let gw = Gateway::new(
            Provider::Slack,
            &eps,
            crate::gateway::agent(Duration::from_secs(1)),
            &NoAudit,
        );
        for method in [
            "team.info",
            "conversations.history",
            "conversations.replies",
            "chat.postMessage",
            "chat.scheduleMessage",
            "reactions.add",
            "auth.revoke",
            "search.all",
            "assistant.search.context",
        ] {
            assert!(
                gw.refusal(&format!("https://slack.com/api/{method}"))
                    .is_some(),
                "{method} must not be readable"
            );
        }
        assert!(
            gw.refusal("https://slack.com/api/search.messages?query=x")
                .is_none()
        );
    }

    #[test]
    fn the_authorize_url_asks_for_user_scopes_only_and_no_pkce() {
        let app = OAuthApp {
            client_id: "123.456".into(),
            client_secret: "secret".into(),
            redirect_uri: "https://opp.example/api/connectors/slack/callback".into(),
            tenant: String::new(),
        };
        let url = authorize_url(&Endpoints::default(), &app, "st&te", &Pkce::new());
        assert!(url.starts_with("https://slack.com/oauth/v2/authorize?"));
        assert!(url.contains("client_id=123.456"));
        assert!(url.contains("user_scope=search%3Aread%2Cusers%3Aread"));
        assert!(url.contains(
            "redirect_uri=https%3A%2F%2Fopp.example%2Fapi%2Fconnectors%2Fslack%2Fcallback"
        ));
        assert!(url.contains("state=st%26te"));
        assert!(
            !url.contains("&scope=") && !url.contains("?scope="),
            "no bot scopes"
        );
        assert!(!url.contains("code_challenge"));
        assert!(!url.contains("secret"));
    }

    #[test]
    fn pasted_tokens_must_be_plain_user_tokens() {
        assert_eq!(pasted_token_problem("xoxp-1234-5678-9012-abcdef"), None);
        let cases = [
            ("xoxb-1234-5678-abcdef", BOT_TOKEN),
            ("xoxe.xoxb-1-abcdefghij", BOT_TOKEN),
            ("xapp-1-A123-456-abcdef", APP_TOKEN),
            ("xoxe.xoxp-1-1234-abcdef", ROTATING_TOKEN),
            ("xoxe-1-abcdefghijklmnop", REFRESH_TOKEN),
            ("xoxs-1234-5678-abcdef", LEGACY_TOKEN),
            ("xoxc-1234-5678-abcdef", LEGACY_TOKEN),
            ("xoxa-2-1234-5678-abcd", LEGACY_TOKEN),
            ("hunter2-not-a-token", NOT_A_USER_TOKEN),
            ("xoxp-1234 5678", NOT_A_USER_TOKEN),
            ("xoxp-1234\r\nX-Evil: 1", NOT_A_USER_TOKEN),
            ("xoxp-", NOT_A_USER_TOKEN),
            ("", NOT_A_USER_TOKEN),
        ];
        for (token, why) in cases {
            assert_eq!(pasted_token_problem(token), Some(why), "{token:?}");
        }
        assert!(is_user_token("xoxe.xoxp-1-1234-abcdef"), "rotating, stored");
        assert!(!is_user_token("xoxb-1234-5678-abcdef"));
    }

    #[test]
    fn legacy_all_access_tokens_are_recognised_by_their_scopes() {
        assert!(legacy_problem(Some("identify,read,post,client")).is_some());
        assert!(legacy_problem(Some("identify, bot")).is_some());
        assert!(legacy_problem(Some("search:read,users:read")).is_none());
        assert!(legacy_problem(None).is_none());
    }

    #[test]
    fn every_reply_is_checked_for_write_scopes_and_must_report_them() {
        let ok = json_ok();
        assert!(check(reply(Some("search:read, users:read"), ok.clone())).is_ok());
        assert_eq!(
            check(reply(Some("search:read,chat:write"), ok.clone())).err(),
            Some(Error::WriteScopes(s(&["chat:write"])))
        );
        assert!(
            matches!(check(reply(None, ok.clone())), Err(Error::Invalid(m)) if m.contains("x-oauth-scopes")),
            "a successful reply without the header isn't trusted"
        );
        assert!(matches!(check(reply(Some(""), ok)), Err(Error::Invalid(_))));
        // Write scopes win over whatever error the reply carries.
        assert_eq!(
            check(reply(
                Some("search:read,chat:write"),
                serde_json::json!({"ok": false, "error": "ratelimited"})
            ))
            .err(),
            Some(Error::WriteScopes(s(&["chat:write"])))
        );
        // Errors from an invalid token come without scopes; that's fine.
        assert!(matches!(
            check(reply(
                None,
                serde_json::json!({"ok": false, "error": "token_revoked"})
            )),
            Err(Error::Unauthorized(_))
        ));
    }

    fn json_ok() -> Value {
        serde_json::json!({"ok": true})
    }

    #[test]
    fn slack_errors_map_to_errors_without_leaking_anything() {
        let e = |j: Value| slack_error(200, &j);
        for code in [
            "invalid_auth",
            "not_authed",
            "token_revoked",
            "token_expired",
            "account_inactive",
            "invalid_refresh_token",
            "invalid_code",
        ] {
            assert!(
                matches!(e(serde_json::json!({"ok": false, "error": code})), Error::Unauthorized(m) if m.contains(code)),
                "{code}"
            );
        }
        assert_eq!(
            e(
                serde_json::json!({"ok": false, "error": "missing_scope", "needed": "search:read", "provided": "users:read"})
            ),
            Error::Forbidden("the token lacks a permission this needs (search:read)".into())
        );
        assert_eq!(
            e(
                serde_json::json!({"ok": false, "error": "missing_scope", "needed": "search:read", "provided": "users:read,chat:write"})
            ),
            Error::WriteScopes(s(&["chat:write"])),
            "the provided scopes are checked too"
        );
        assert_eq!(
            e(serde_json::json!({"ok": false, "error": "ratelimited"})),
            Error::RateLimited(None)
        );
        assert!(matches!(
            e(serde_json::json!({"ok": false, "error": "bad_client_secret"})),
            Error::NotConfigured(_)
        ));
        assert!(matches!(
            e(serde_json::json!({"ok": false, "error": "internal_error"})),
            Error::Provider(200, _)
        ));
        for junk in [
            "xoxp-123-secret",
            "Your message: lunch at noon",
            "<script>",
            "No query passed",
        ] {
            let text = e(serde_json::json!({"ok": false, "error": junk})).to_string();
            assert!(text.contains("unrecognised_error"), "{text}");
            assert!(
                !text.contains("xoxp") && !text.contains("lunch") && !text.contains('<'),
                "{text}"
            );
        }
    }

    #[test]
    fn a_code_exchange_keeps_only_a_read_only_user_token() {
        let reply = serde_json::json!({
            "ok": true, "app_id": "A1",
            "authed_user": {"id": "U1", "scope": "search:read,users:read", "access_token": "xoxp-1-2-3-abcdef", "token_type": "user"},
            "team": {"id": "T1", "name": "Acme"}, "enterprise": null, "is_enterprise_install": false
        });
        let c = grant(&reply, None).unwrap();
        assert_eq!(c.access_token, "xoxp-1-2-3-abcdef");
        assert_eq!(c.scopes, s(&["search:read", "users:read"]));
        assert_eq!((c.refresh_token, c.expires_at), (None, None));
        assert_eq!(
            c.account,
            Account {
                id: "T1".into(),
                name: "Acme".into(),
                user: "U1".into(),
                url: String::new()
            }
        );

        let mut with_bot = reply.clone();
        with_bot["access_token"] = "xoxb-17-19-pdFZ".into();
        with_bot["token_type"] = "bot".into();
        with_bot["scope"] = "commands,incoming-webhook".into();
        assert_eq!(
            grant(&with_bot, None).err(),
            Some(Error::WriteScopes(s(&["commands", "incoming-webhook"])))
        );
        with_bot["scope"] = "users:read".into();
        let c = grant(&with_bot, None).unwrap();
        assert!(
            c.access_token.starts_with("xoxp-"),
            "the bot token is dropped"
        );

        let mut writer = reply.clone();
        writer["authed_user"]["scope"] = "search:read,chat:write".into();
        assert_eq!(
            grant(&writer, None).err(),
            Some(Error::WriteScopes(s(&["chat:write"])))
        );

        let mut no_search = reply.clone();
        no_search["authed_user"]["scope"] = "users:read".into();
        assert!(matches!(grant(&no_search, None), Err(Error::Forbidden(_))));

        let mut bot_only = reply.clone();
        bot_only["authed_user"]["token_type"] = "bot".into();
        assert!(matches!(grant(&bot_only, None), Err(Error::Unsupported(_))));

        let mut no_scope = reply;
        no_scope["authed_user"]
            .as_object_mut()
            .unwrap()
            .remove("scope");
        assert!(matches!(grant(&no_scope, None), Err(Error::Invalid(_))));
    }

    #[test]
    fn a_refresh_reply_rotates_both_tokens() {
        let before = Credential {
            access_token: "xoxe.xoxp-1-old".into(),
            refresh_token: Some("xoxe-1-old".into()),
            expires_at: Some(1),
            scopes: s(&["search:read", "users:read"]),
            account: Account {
                id: "T1".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let reply = serde_json::json!({
            "ok": true, "access_token": "xoxe.xoxp-1-new", "token_type": "user",
            "expires_in": 43200, "refresh_token": "xoxe-1-new-refresh", "team": {"id": "T1"}
        });
        let now = now_unix();
        let fresh = grant(&reply, Some(&before)).unwrap();
        assert_eq!(fresh.access_token, "xoxe.xoxp-1-new");
        assert_eq!(fresh.refresh_token.as_deref(), Some("xoxe-1-new-refresh"));
        assert!(fresh.expires_at.unwrap() >= now + 43_200);
        assert_eq!(fresh.scopes, before.scopes, "kept when not repeated");
        assert_eq!(fresh.account, before.account);

        let mut bot = reply;
        bot["token_type"] = "bot".into();
        assert!(matches!(
            grant(&bot, Some(&before)),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn refresh_without_a_refresh_token_changes_nothing_and_sends_nothing() {
        let eps = Endpoints::default();
        let audit = MemoryAudit::default();
        let http = OAuthHttp::new(
            Provider::Slack,
            &eps,
            crate::gateway::agent(Duration::from_secs(1)),
            &audit,
        );
        let app = OAuthApp {
            client_id: "id".into(),
            client_secret: "secret".into(),
            redirect_uri: "https://x.example/cb".into(),
            tenant: String::new(),
        };
        let cred = Credential {
            access_token: "xoxp-1-2-3-abcdef".into(),
            ..Default::default()
        };
        assert_eq!(refresh(&http, &eps, &app, &cred).unwrap(), cred);
        assert!(audit.events().is_empty());
    }

    #[test]
    fn auth_test_identities() {
        let user = serde_json::json!({"ok": true, "url": "https://acme.slack.com/", "team": "Acme", "user": "grace", "team_id": "T1", "user_id": "W1"});
        assert_eq!(
            identity(&user).unwrap(),
            Account {
                id: "T1".into(),
                name: "Acme".into(),
                user: "grace".into(),
                url: "https://acme.slack.com/".into()
            }
        );
        let mut bot = user.clone();
        bot["bot_id"] = "B1".into();
        assert!(matches!(identity(&bot), Err(Error::Unsupported(_))));
        let mut nowhere = user;
        nowhere["team_id"] = "".into();
        assert!(matches!(identity(&nowhere), Err(Error::Invalid(_))));
    }

    #[test]
    fn keywords_cannot_become_search_modifiers() {
        let k = keywords(&s(&[
            "Hackathon",
            "hackathon",
            "  call   for ",
            "in:#secret",
            "-from:<@U1>",
            "\"quoted\"",
            "or",
            "x",
            "R&D",
        ]));
        assert_eq!(
            k,
            s(&[
                "hackathon",
                "call for",
                "in secret",
                "from u1",
                "quoted",
                "r&d"
            ])
        );
        assert!(
            k.iter()
                .all(|k| !k.contains(':') && !k.contains('"') && !k.starts_with('-'))
        );
        assert_eq!(keywords(&[]).len(), DEFAULT_KEYWORDS.len());
        assert_eq!(keywords(&s(&["", "  "])).len(), DEFAULT_KEYWORDS.len());
    }

    #[test]
    fn searches_are_planned_from_the_strongest_keywords() {
        let k = keywords(&[]);
        let p = plan(&k, "U0ADA", "2026-10-27");
        let queries: Vec<&str> = p.iter().map(|s| s.query.as_str()).collect();
        assert_eq!(
            &queries[..6],
            &[
                "hackathon after:2026-10-27",
                "\"call for\" after:2026-10-27",
                "hiring after:2026-10-27",
                "speaker after:2026-10-27",
                "intro after:2026-10-27",
                "partnership after:2026-10-27",
            ]
        );
        assert_eq!(
            queries[6],
            "opportunity OR secondment OR applications OR mentoring OR nomination OR fellowship OR grant OR rfp after:2026-10-27"
        );
        assert!(queries[7].starts_with("tender OR scholarship OR volunteer"));
        assert_eq!(queries[8], "<@U0ADA> after:2026-10-27");
        assert_eq!(p.len(), SINGLE_SEARCHES + OR_SEARCHES + 1);
        // A caller's own keyword comes right after the first four.
        let mine = plan(&s(&["role", "kubernetes", "hiring"]), "", "2026-01-01");
        let q: Vec<&str> = mine.iter().map(|s| s.query.as_str()).collect();
        assert_eq!(
            q,
            &[
                "hiring after:2026-01-01",
                "kubernetes after:2026-01-01",
                "role after:2026-01-01"
            ]
        );
    }

    #[test]
    fn the_window_ends_today_and_after_is_exclusive() {
        let w = Window::new("2026-11-26", 30);
        assert_eq!(w.after, "2026-10-27");
        assert_eq!(ymd(w.start), "2026-10-28");
        assert_eq!(ymd(w.end), "2026-11-27");
        assert!((w.recency(w.end) - 1.0).abs() < 1e-6);
        assert!((w.recency(w.start) - 0.5).abs() < 1e-6);
        assert_eq!(Window::new("2026-11-26", 0).after, "2026-11-25");
        assert_eq!(Window::new("not a date", 7).after.len(), 10);
    }

    #[test]
    fn slack_markup_becomes_plain_text() {
        let mut names = HashMap::new();
        names.insert("U1".to_string(), "Ada Okafor".to_string());
        let raw = "Hi <@U1>, ping <@U2> or <@U3|sam> in <#C1|devrel> &amp; <#C2>. <!here> \
                   <!subteam^S1|@eng> <!date^1392734382^{date}|Feb 18> \
                   <https://x.example/a?b=1&amp;c=2|the form>, <https://x.example> \
                   <mailto:a@b.example|a@b.example> 1 &lt; 2 &gt; 0 \u{e000}hit\u{e001} <not closed";
        assert_eq!(
            readable(raw, &names),
            "Hi @Ada Okafor, ping @someone or @sam in #devrel & #channel. @here @eng Feb 18 \
             the form (https://x.example/a?b=1&c=2), https://x.example a@b.example 1 < 2 > 0 hit <not closed"
        );
        assert_eq!(readable("a\n\n\n\nb  \n", &names), "a\n\nb");
        assert_eq!(
            mentioned_ids("<@U1> <@W2|x> <@U1> <@bad> <#C1>"),
            s(&["U1", "W2"])
        );
    }

    #[test]
    fn headlines_are_short_and_clean() {
        assert_eq!(
            headline(":rocket: *Build Week internal hackathon* is on 4–5 December! Form a team"),
            "Build Week internal hackathon is on 4–5 December!…"
        );
        assert_eq!(headline("Short one"), "Short one");
    }

    #[test]
    fn words_match_at_word_starts() {
        assert!(has_word("we have partners", "partner"));
        assert!(has_word("call for speakers", "call for"));
        assert!(!has_word("counterpartner", "partner"));
        assert!(has_word("x counterpartner partner", "partner"));
        assert!(!has_word("anything", ""));
        assert!(has_word("r&d proposals", "r&d"));
    }

    fn msg(raw: &str, place: Place, author: &str, ts: i64) -> Message {
        Message {
            url: format!("https://acme.slack.com/archives/C1/p{ts}"),
            ts,
            raw: raw.into(),
            place,
            author_id: author.into(),
            username: String::new(),
            terms: Vec::new(),
            any_hit: false,
            score: 0.0,
        }
    }

    #[test]
    fn ranking_prefers_strong_signals_mentions_and_recency() {
        let w = Window::new("2026-11-26", 30);
        let k = keywords(&[]);
        let day = |d: i64| w.end - d * DAY_SECS - 3600;
        let chan = || Place::Channel("general".into());
        let strong = msg("Call for speakers: submit by Friday", chan(), "U2", day(5));
        let weak = msg("Deploying role-based access", chan(), "U2", day(1));
        let mention = msg("<@U1> fancy a panel slot?", chan(), "U2", day(5));
        let mine = msg("I'm looking for a reviewer", chan(), "U1", day(1));
        let old = msg("Call for speakers: submit by Friday", chan(), "U2", day(25));
        let dm = msg("Could you give me an intro?", Place::Direct, "U2", day(5));
        let sc = |m: &Message| score(m, &k, "U1", &w);
        assert!(sc(&strong) > sc(&weak));
        assert!(sc(&strong) > sc(&old), "recency counts");
        assert!(sc(&mention) > sc(&weak));
        assert!(sc(&dm) > sc(&msg("Could you give me an intro?", chan(), "U2", day(5))));
        assert_eq!(
            sc(&mine),
            1.0 * w.recency(day(1)),
            "own messages count for half"
        );
        assert_eq!(
            sc(&msg("Lunch?", chan(), "U2", day(1))),
            0.0,
            "nothing to say for it"
        );
        let mut found_by_or = msg("Opportunities abound", chan(), "U2", day(1));
        found_by_or.any_hit = true;
        assert!(sc(&found_by_or) > 0.0);
    }

    #[test]
    fn matches_are_collected_deduplicated_and_kept_inside_the_window() {
        let w = Window::new("2026-11-26", 30);
        let inside = w.end - 2 * DAY_SECS;
        let reply = serde_json::json!({"ok": true, "messages": {"matches": [
            {"type": "message", "channel": {"id": "C1", "name": "devrel"}, "user": "U2", "username": "sam",
             "ts": format!("{inside}.000100"), "text": "Call for speakers", "permalink": "https://acme.slack.com/archives/C1/p1"},
            {"type": "im", "channel": {"id": "D1", "name": "U2"}, "user": "U2", "username": "sam",
             "ts": format!("{inside}.000200"), "text": "intro please"},
            {"type": "message", "channel": {"id": "G1", "name": "mpdm-a--b-1", "is_mpim": true}, "user": "U2",
             "ts": format!("{inside}.000300"), "text": "", "attachments": [{"title": "New opening", "text": "Apply now", "fallback": "New opening: Apply now"}],
             "permalink": "https://acme.slack.com/archives/G1/p3"},
            {"type": "message", "channel": {"id": "C1", "name": "devrel"}, "user": "U2",
             "ts": format!("{}.000100", w.start - 10), "text": "too old", "permalink": "https://acme.slack.com/archives/C1/p0"},
            {"type": "message", "channel": {"id": "C1", "name": "devrel"}, "user": "U2",
             "ts": format!("{inside}.000400"), "text": "   ", "permalink": "https://acme.slack.com/archives/C1/p4"},
        ]}});
        let single = Search {
            query: String::new(),
            terms: s(&["speaker"]),
            kind: Kind::Single,
        };
        let mut f = Found::default();
        f.add(&reply, &single, &w, "https://acme.slack.com/");
        f.add(&reply, &single, &w, "https://acme.slack.com/");
        assert_eq!(f.list.len(), 3, "deduplicated, empty and old ones dropped");
        assert_eq!(f.list[0].terms, s(&["speaker"]));
        assert_eq!(f.list[1].place, Place::Direct);
        assert_eq!(
            f.list[1].url,
            format!("https://acme.slack.com/archives/D1/p{inside}000200"),
            "a missing permalink is built"
        );
        assert_eq!(f.list[2].place, Place::Group);
        assert_eq!(f.list[2].raw, "New opening\nApply now");
    }

    #[test]
    fn items_carry_names_places_and_dates() {
        let mut names = HashMap::new();
        names.insert("U2".to_string(), "Sam Whitfield".to_string());
        names.insert("U1".to_string(), "Ada Okafor".to_string());
        let m = msg(
            ":microphone: Call for speakers <@U1>",
            Place::Channel("devrel".into()),
            "U2",
            1_795_682_592,
        );
        let i = m.item(&names);
        assert_eq!(i.title, "#devrel · Call for speakers @Ada Okafor");
        assert_eq!(i.text, ":microphone: Call for speakers @Ada Okafor");
        assert_eq!(
            (i.author.as_str(), i.container.as_str(), i.kind.as_str()),
            ("Sam Whitfield", "#devrel", "message")
        );
        assert_eq!(i.date.as_deref(), Some("2026-11-26"));
        let mut anon = msg("hello", Place::Direct, "", 1_795_682_592);
        anon.username = "careers-bot".into();
        let i = anon.item(&HashMap::new());
        assert_eq!(
            (i.author.as_str(), i.title.as_str()),
            ("careers-bot", "Direct message · hello")
        );
        assert!(i.clone().sanitized().is_some());
    }

    #[test]
    fn timestamps_and_ids() {
        assert_eq!(parse_ts("1508284197.000015"), Some(1_508_284_197));
        assert_eq!(parse_ts("abc"), None);
        assert_eq!(parse_ts("-5.1"), None);
        assert!(is_member_id("U0ADA0001") && is_member_id("W12345678"));
        assert!(!is_member_id("C123") && !is_member_id("U1>") && !is_member_id("u123"));
    }
}
