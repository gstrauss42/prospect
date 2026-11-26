//! Microsoft 365, read-only, through Microsoft Graph with delegated permissions: recent Outlook
//! Inbox mail and upcoming calendar events that look like opportunities.
//!
//! # Signing in
//!
//! OAuth 2.0 authorization code flow with PKCE (S256) on the Microsoft identity platform v2.0, as a
//! confidential client:
//!
//! * [`authorize_url`] sends the person to `{ms_login}/{tenant}/oauth2/v2.0/authorize?client_id=…&
//!   response_type=code&redirect_uri=…&response_mode=query&scope=…&state=…&code_challenge=…&
//!   code_challenge_method=S256&prompt=select_account`. The tenant is the administrator's
//!   `MS_TENANT` ("organizations" by default; "common" or a tenant id/domain are also valid).
//! * [`exchange_code`] posts to `{ms_login}/{tenant}/oauth2/v2.0/token` (form: client_id,
//!   client_secret, grant_type=authorization_code, code, redirect_uri, code_verifier, scope).
//! * [`refresh`] uses the same endpoint with grant_type=refresh_token. Microsoft returns a new
//!   refresh token each time; it replaces the old one.
//!
//! Every token reply's `scope` is what the access token can actually do. It can be more than we
//! asked for: an administrator may have consented other permissions for the app, and Microsoft
//! adds them to every token. So the granted `scope` (percent-decoded when Microsoft encodes it)
//! becomes [`Credential::scopes`] verbatim, and the facade refuses the token if anything in it is
//! not on the read-only allow-list. A reply without a granted `scope` is refused here: what can't
//! be verified isn't used. Nothing in this module reads data with a token before the facade has
//! checked its scopes.
//!
//! The account shown to the person comes from the ID token's `tid`, `oid`, `name` and
//! `preferred_username` claims, decoded for display only: never for authorization, and the
//! signature isn't checked (the token came straight from Microsoft's token endpoint over TLS).
//! [`verify`] refreshes it from `GET /v1.0/me`. (The code exchange can't call Graph itself: it gets
//! no [`Gateway`], so the read couldn't be audited.)
//!
//! No pasted tokens: OAuth only.
//!
//! # Reading (GET only, through the [`Gateway`])
//!
//! | Endpoint | Used for |
//! |---|---|
//! | `/v1.0/me` | [`verify`]: the connection works, and whose it is |
//! | `/v1.0/me/mailFolders/inbox/messages` | [`fetch`]: recent Inbox mail |
//! | `/v1.0/me/calendarView` | [`fetch`]: events from today to 60 days ahead |
//! | `/v1.0/me/messages` | not read by this module; allow-listed for the core gateway tests |
//!
//! Only list endpoints with `$select`: item ids can contain `/`, which the gateway refuses in paths.
//!
//! * **Mail** comes from the Inbox only (not Sent, Deleted, Junk or other folders): subject,
//!   sender, received time, the 255-character `bodyPreview` and the Outlook `webLink`. Graph can't
//!   combine `$search` with `$filter` or `$orderby` on messages, so the query is a KQL `$search`
//!   of the single-word keywords joined with `OR` (no quotes, colons or other KQL syntax inside;
//!   phrases are matched locally). Graph sorts search results newest first, so the date window is
//!   applied here and paging stops once a page reaches past it. If the mailbox rejects `$search`
//!   (HTTP 400/501), the fallback is `$filter=receivedDateTime ge …` with
//!   `$orderby=receivedDateTime desc` (the ordered property leads the filter, as Graph requires)
//!   and local ranking.
//! * **Calendar**: `calendarView` from today to 60 days ahead, in UTC, ordered by start: subject,
//!   organizer, start, location, `bodyPreview`, `webLink`, whether it was cancelled and the
//!   person's response. Cancelled and routine events (stand-ups, 1:1s, focus time) are dropped.
//! * Items are ranked by keyword hits (subject over preview, each word counted once), with a
//!   nudge for mail that greets the person by first name and for invitations still awaiting a
//!   reply; newsletters and notifications are pushed far down; near duplicates (a thread's
//!   replies, a newsletter's issues, a series' occurrences) are collapsed to the best one; the
//!   list is capped at `max_items`.
//! * Pages are bounded (4 × 50 messages, 3 × 50 events). Next-page links go back through the
//!   gateway, which re-checks them; one it refuses ends paging with what was read so far.
//! * Throttling: a 429 with `Retry-After` of at most 5 s is waited out and retried once, as is a
//!   503/504 or a 429 without `Retry-After` (after 1–2 s); at most 10 s of waiting per fetch.
//!   Otherwise the source stops, what was read is returned, and when nothing could be read the
//!   error is too ([`Error::RateLimited`] for throttling). 401 always fails the fetch
//!   ([`Error::Unauthorized`]: refresh or reconnect); a 403 on one source still returns the other.
//!
//! Owner: the Microsoft connector builder.

use std::time::Duration;

use base64::Engine as _;
use serde_json::Value;

use crate::gateway::{Reply, with_query};
use crate::{
    Account, Auth, Credential, Endpoints, Error, FetchQuery, Gateway, OAuthApp, OAuthHttp, Pkce,
    Policy, TokenInput,
};
use model::ContextItem;
use model::text::{squash, truncate};

pub const POLICY: Policy = Policy {
    read_paths: &[
        "/v1.0/me",
        "/v1.0/me/mailFolders/inbox/messages",
        "/v1.0/me/calendarView",
        "/v1.0/me/messages",
    ],
    token_paths: &["/*/oauth2/v2.0/token"],
    requested_scopes: &[
        "openid",
        "profile",
        "email",
        "offline_access",
        "User.Read",
        "Mail.Read",
        "Calendars.Read",
    ],
    allowed_scopes: &[
        "User.Read",
        "Mail.Read",
        "Mail.ReadBasic",
        "Calendars.Read",
        "Calendars.ReadBasic",
    ],
    token_mode: false,
    reads: "Recent emails in your Outlook Inbox and upcoming calendar events that look like opportunities (subject, sender, date and a short preview).",
};

/// Microsoft's tenant id for personal (consumer) Microsoft accounts.
const CONSUMER_TENANT: &str = "9188040d-6c67-4c5b-b112-36a304b66dad";
/// Fields read from each message and each event: nothing else is asked for.
const MAIL_FIELDS: &str = "subject,from,receivedDateTime,bodyPreview,webLink";
const EVENT_FIELDS: &str =
    "subject,organizer,start,location,bodyPreview,webLink,isCancelled,responseStatus";
const PAGE_SIZE: &str = "50";
const MAIL_PAGES: usize = 4;
const EVENT_PAGES: usize = 3;
const CALENDAR_DAYS: i64 = 60;
/// Most single-word keywords sent to Graph's search (any further ones only rank). If a mailbox
/// ever refuses a query this long, the `$filter` fallback still runs.
const MAX_SEARCH_TERMS: usize = 40;
/// Longest `Retry-After` waited out before retrying once.
const MAX_RETRY_AFTER: u64 = 5;
/// Most seconds one fetch spends waiting for retries.
const WAIT_BUDGET: u64 = 10;
/// Assumed lifetime when a token reply has no `expires_in` (Microsoft's are about an hour).
const DEFAULT_TOKEN_SECS: i64 = 3600;
/// Longest lifetime believed, so a token is refreshed, and its granted scopes checked again, at
/// least daily (Microsoft's longest-lived access tokens last up to 28 hours).
const MAX_TOKEN_SECS: i64 = 86_400;

// ---------------------------------------------------------------------------------------------
// OAuth
// ---------------------------------------------------------------------------------------------

pub fn authorize_url(eps: &Endpoints, app: &OAuthApp, state: &str, pkce: &Pkce) -> String {
    with_query(
        &format!("{}/{}/oauth2/v2.0/authorize", eps.ms_login, tenant(app)),
        &[
            ("client_id", app.client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", &app.redirect_uri),
            ("response_mode", "query"),
            ("scope", &POLICY.requested_scopes.join(" ")),
            ("state", state),
            ("code_challenge", &pkce.challenge),
            ("code_challenge_method", "S256"),
            ("prompt", "select_account"),
        ],
    )
}

/// The tenant path segment: "organizations" when unset. Anything that isn't a plain name stays
/// percent-encoded in one segment, so a malformed `MS_TENANT` fails at Microsoft (and at the
/// token-endpoint check) instead of silently widening who can sign in.
fn tenant(app: &OAuthApp) -> String {
    match app.tenant.trim() {
        "" => "organizations".to_string(),
        t => crate::gateway::encode(t),
    }
}

fn token_url(eps: &Endpoints, app: &OAuthApp) -> String {
    format!("{}/{}/oauth2/v2.0/token", eps.ms_login, tenant(app))
}

pub fn exchange_code(
    http: &OAuthHttp,
    eps: &Endpoints,
    app: &OAuthApp,
    code: &str,
    pkce: &Pkce,
) -> Result<Credential, Error> {
    let scope = POLICY.requested_scopes.join(" ");
    let (status, json) = http.post_form(
        &token_url(eps, app),
        &[
            ("client_id", app.client_id.as_str()),
            ("client_secret", &app.client_secret),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &app.redirect_uri),
            ("code_verifier", &pkce.verifier),
            ("scope", &scope),
        ],
        None,
    )?;
    let grant = Grant::from_reply(status, &json)?;
    let account = account_from_id_token(grant.id_token.as_deref());
    Ok(Credential {
        access_token: grant.access_token,
        refresh_token: grant.refresh_token,
        expires_at: Some(grant.expires_at),
        scopes: grant.scopes,
        account,
        ..Default::default()
    })
}

pub fn refresh(
    http: &OAuthHttp,
    eps: &Endpoints,
    app: &OAuthApp,
    cred: &Credential,
) -> Result<Credential, Error> {
    let Some(old) = cred.refresh_token.as_deref().filter(|t| !t.is_empty()) else {
        return Err(Error::Unauthorized(
            "there is no refresh token; sign in to Microsoft 365 again".into(),
        ));
    };
    let scope = POLICY.requested_scopes.join(" ");
    let (status, json) = http.post_form(
        &token_url(eps, app),
        &[
            ("client_id", app.client_id.as_str()),
            ("client_secret", &app.client_secret),
            ("grant_type", "refresh_token"),
            ("refresh_token", old),
            ("scope", &scope),
        ],
        None,
    )?;
    let grant = Grant::from_reply(status, &json)?;
    Ok(Credential {
        provider: cred.provider.clone(),
        mode: cred.mode.clone(),
        access_token: grant.access_token,
        // Microsoft rotates refresh tokens: keep the new one, the old only if none came back.
        refresh_token: grant.refresh_token.or_else(|| cred.refresh_token.clone()),
        expires_at: Some(grant.expires_at),
        // What this token can do, as granted now: re-checked by the facade.
        scopes: grant.scopes,
        basic_user: None,
        account: cred.account.clone(),
    })
}

/// A successful token reply.
#[derive(Debug)]
struct Grant {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: i64,
    scopes: Vec<String>,
    id_token: Option<String>,
}

impl Grant {
    fn from_reply(status: u16, json: &Value) -> Result<Grant, Error> {
        if !(200..300).contains(&status) || json.get("error").is_some() {
            return Err(token_error(status, json));
        }
        let text = |k: &str| {
            json[k]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let access_token = text("access_token")
            .ok_or_else(|| Error::Invalid("Microsoft's token reply had no access token".into()))?;
        if let Some(t) = text("token_type")
            && !t.eq_ignore_ascii_case("bearer")
        {
            return Err(Error::Invalid(format!(
                "Microsoft issued a {t:?} token; only bearer tokens are used"
            )));
        }
        let scopes = granted_scopes(json).ok_or_else(|| {
            Error::Invalid(
                "Microsoft didn't say which permissions it granted, so the connection wasn't used"
                    .into(),
            )
        })?;
        let lifetime = json["expires_in"]
            .as_i64()
            .or_else(|| text("expires_in").and_then(|s| s.parse().ok()))
            .unwrap_or(DEFAULT_TOKEN_SECS)
            .clamp(0, MAX_TOKEN_SECS);
        Ok(Grant {
            access_token,
            refresh_token: text("refresh_token"),
            expires_at: model::date::now_unix() + lifetime,
            scopes,
            id_token: text("id_token"),
        })
    }
}

/// The granted scopes of a token reply, exactly as granted (split, and percent-decoded when the
/// value is URL-encoded). None when Microsoft didn't say.
fn granted_scopes(json: &Value) -> Option<Vec<String>> {
    let raw = json["scope"].as_str()?;
    let raw = if raw.contains('%') {
        percent_decode(raw)
    } else {
        raw.to_string()
    };
    let scopes = crate::parse_scopes(&raw);
    (!scopes.is_empty()).then_some(scopes)
}

/// `application/x-www-form-urlencoded` decoding: `+` is a space, `%XX` a byte; anything else,
/// including a stray `%`, is kept as it is.
fn percent_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        (c as char).to_digit(16).map(|d| d as u8)
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(hi * 16 + lo);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// An OAuth error reply as an [`Error`]: the error code and the first line of Microsoft's
/// description (never the request, so never a code, secret or token).
fn token_error(status: u16, json: &Value) -> Error {
    let code = json["error"].as_str().unwrap_or("").trim();
    let description = json["error_description"]
        .as_str()
        .and_then(|d| d.lines().next())
        .map(|d| truncate(d.trim(), 200))
        .unwrap_or_default();
    let detail = match (code.is_empty(), description.is_empty()) {
        (false, false) => format!("{code}: {description}"),
        (false, true) => code.to_string(),
        (true, false) => description,
        (true, true) => format!("HTTP {status} without a token"),
    };
    match code {
        // The code, the PKCE verifier or the refresh token is no good (expired, used, revoked,
        // password changed…), or Microsoft wants the person back: only signing in again helps.
        "invalid_grant" | "interaction_required" | "consent_required" | "login_required" => {
            Error::Unauthorized(detail)
        }
        "invalid_client" | "unauthorized_client" => Error::NotConfigured(format!(
            "Microsoft refused this server's app registration ({detail}). The administrator needs to check MS_CLIENT_ID, MS_CLIENT_SECRET and MS_TENANT."
        )),
        _ => Error::Provider(status, detail),
    }
}

pub fn connect_with_token(gw: &Gateway, input: &TokenInput) -> Result<Credential, Error> {
    let _ = (gw, input);
    Err(Error::Unsupported(
        "Microsoft 365 can only be connected by signing in with Microsoft.".into(),
    ))
}

// ---------------------------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------------------------

pub fn verify(gw: &Gateway, cred: &Credential) -> Result<Account, Error> {
    let me = gw.get_json(
        &format!("{}/v1.0/me", gw.endpoints().ms_graph),
        &[("$select", "id,displayName,mail,userPrincipalName")],
        &bearer(cred),
    )?;
    if !me.is_object() {
        return Err(Error::Invalid(
            "Microsoft Graph's /me wasn't an object".into(),
        ));
    }
    Ok(account_from_me(&me, &cred.account))
}

fn bearer(cred: &Credential) -> Auth {
    Auth::Bearer(cred.access_token.clone())
}

/// The claims of a JWT (here: the ID token), decoded without verification. Display only.
fn jwt_claims(token: &str) -> Option<Value> {
    if token.len() > 16 * 1024 {
        return None;
    }
    let payload = token.split('.').nth(1)?.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(Value::is_object)
}

fn claim(v: &Value, key: &str) -> Option<String> {
    v[key]
        .as_str()
        .map(squash)
        .filter(|s| !s.is_empty())
        .map(|s| truncate(&s, 200))
}

fn account_from_id_token(id_token: Option<&str>) -> Account {
    let claims = id_token.and_then(jwt_claims).unwrap_or(Value::Null);
    let tid = claim(&claims, "tid");
    let oid = claim(&claims, "oid");
    let username = claim(&claims, "preferred_username").or_else(|| claim(&claims, "email"));
    Account {
        id: match (&tid, &oid) {
            (Some(t), Some(o)) => format!("{t}:{o}"),
            (None, Some(o)) => o.clone(),
            _ => String::new(),
        },
        name: organisation(tid.as_deref(), username.as_deref()),
        user: username
            .or_else(|| claim(&claims, "name"))
            .unwrap_or_default(),
        url: outlook_url(tid.as_deref()),
    }
}

/// `base` brought up to date with Graph's `/me`.
fn account_from_me(me: &Value, base: &Account) -> Account {
    let mut a = base.clone();
    let email = claim(me, "mail").or_else(|| claim(me, "userPrincipalName"));
    match (&email, claim(me, "displayName")) {
        (Some(e), _) => a.user = e.clone(),
        (None, Some(name)) if a.user.is_empty() => a.user = name,
        _ => {}
    }
    if a.id.is_empty()
        && let Some(oid) = claim(me, "id")
    {
        a.id = oid;
    }
    if a.name.is_empty() {
        a.name = organisation(None, email.as_deref());
    }
    if a.url.is_empty() {
        a.url = outlook_url(None);
    }
    a
}

/// What to call the organisation: the person's email domain (a tenant's name would need another
/// permission), or that it's a personal account.
fn organisation(tid: Option<&str>, username: Option<&str>) -> String {
    if tid == Some(CONSUMER_TENANT) {
        return "Personal Microsoft account".into();
    }
    username
        .and_then(|u| u.rsplit_once('@'))
        .map(|(_, domain)| domain.trim().to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "Microsoft 365".into())
}

fn outlook_url(tid: Option<&str>) -> String {
    if tid == Some(CONSUMER_TENANT) {
        "https://outlook.live.com/".into()
    } else {
        "https://outlook.office.com/".into()
    }
}

// ---------------------------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------------------------

pub fn fetch(gw: &Gateway, cred: &Credential, q: &FetchQuery) -> Result<Vec<ContextItem>, Error> {
    if q.max_items == 0 {
        return Ok(Vec::new());
    }
    let today = model::date::parse_ymd(&q.today)
        .map(|_| q.today.clone())
        .unwrap_or_else(model::date::today);
    let today_day = model::date::parse_ymd(&today).unwrap_or(0);
    let since_day = today_day - i64::from(q.since_days.clamp(1, 365));
    let terms = Terms::new(q.keywords.iter());
    let mut reader = Reader {
        gw,
        auth: bearer(cred),
        wait_budget: WAIT_BUDGET,
    };

    let mail = read_mail(
        &mut reader,
        &MailContext {
            terms: &terms,
            since_day,
            first_name: first_name(&cred.account),
        },
    );
    if let Some(e @ (Error::Unauthorized(_) | Error::Blocked(_))) = &mail.error {
        return Err(e.clone());
    }
    // Throttled on mail: Graph limits per app and mailbox, so the calendar would be too.
    let events = match &mail.error {
        Some(e @ Error::RateLimited(_)) => Source {
            items: Vec::new(),
            error: Some(e.clone()),
        },
        _ => read_events(&mut reader, &terms, &today, today_day),
    };
    if let Some(e @ (Error::Unauthorized(_) | Error::Blocked(_))) = &events.error {
        return Err(e.clone());
    }
    // Nothing read and something went wrong: say what, rather than return an empty list.
    if mail.items.is_empty()
        && events.items.is_empty()
        && let Some(e) = mail.error.or(events.error)
    {
        return Err(match e {
            Error::Provider(503 | 504, _) => Error::RateLimited(None),
            e => e,
        });
    }
    let mut all = mail.items;
    all.extend(events.items);
    Ok(rank(all, today_day, q.max_items))
}

/// Reads through the gateway with one bearer token, waiting out short throttling.
struct Reader<'a, 'g> {
    gw: &'a Gateway<'g>,
    auth: Auth,
    /// Seconds still allowed for waiting before retries.
    wait_budget: u64,
}

/// What one paged listing gave: the items read, and the error that stopped it, if any.
struct Pages {
    values: Vec<Value>,
    error: Option<Error>,
}

impl Reader<'_, '_> {
    /// One GET, retried once after a short wait when Graph asks for one (429 with a small
    /// `Retry-After`) or is briefly unavailable (503/504, or 429 without `Retry-After`).
    fn get(
        &mut self,
        url: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Result<Reply, Error> {
        let first = self.gw.get(url, query, &self.auth, headers);
        let wait = match &first {
            Err(Error::RateLimited(Some(s))) if *s <= MAX_RETRY_AFTER => (*s).max(1),
            Err(Error::RateLimited(None)) => 2,
            Err(Error::Provider(503 | 504, _)) => 1,
            _ => return first,
        };
        if wait > self.wait_budget {
            return first;
        }
        self.wait_budget -= wait;
        std::thread::sleep(Duration::from_secs(wait));
        self.gw.get(url, query, &self.auth, headers)
    }

    /// Up to `max_pages` pages of a Graph collection, following `@odata.nextLink` (through the
    /// gateway, which re-checks every link). `stop` sees each page and can end paging early.
    fn pages(
        &mut self,
        url: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        max_pages: usize,
        stop: &dyn Fn(&[Value]) -> bool,
    ) -> Pages {
        let mut values = Vec::new();
        let mut next: Option<String> = None;
        for page in 0..max_pages {
            let reply = match &next {
                None => self.get(url, query, headers),
                Some(link) => {
                    let link = self.next_link(url, link);
                    self.get(&link, &[], headers)
                }
            };
            let json = match reply {
                Ok(r) => r.json,
                // A next-page link the gateway won't follow (it audited why): keep what we have.
                Err(Error::Blocked(_)) if page > 0 => break,
                Err(e) => {
                    return Pages {
                        values,
                        error: Some(e),
                    };
                }
            };
            let Some(batch) = json["value"].as_array() else {
                return Pages {
                    values,
                    error: Some(Error::Invalid(
                        "a Microsoft Graph list without \"value\"".into(),
                    )),
                };
            };
            let done = stop(batch);
            values.extend(batch.iter().cloned());
            next = json["@odata.nextLink"].as_str().map(str::to_string);
            if done || next.is_none() {
                break;
            }
        }
        Pages {
            values,
            error: None,
        }
    }

    /// Graph's next-page link, as given; or, when the gateway wouldn't accept it only because its
    /// path differs from ours in letter case (Graph paths are case-insensitive, the allow-list
    /// isn't), the same query on our own spelling of the path. Anything else is left for the
    /// gateway to refuse.
    fn next_link(&self, base_url: &str, link: &str) -> String {
        if self.gw.refusal(link).is_none() {
            return link.to_string();
        }
        match link.split_once('?') {
            Some((head, query)) if head.eq_ignore_ascii_case(base_url) => {
                format!("{base_url}?{query}")
            }
            _ => link.to_string(),
        }
    }
}

/// One source's ranked candidates, and the error that cut it short, if any.
struct Source {
    items: Vec<Scored>,
    error: Option<Error>,
}

struct Scored {
    item: ContextItem,
    score: i32,
    /// Days since the epoch of the item's date (received, or the event's start).
    day: i64,
    /// Items with the same group are near duplicates: one conversation, one recurring event, one
    /// newsletter. Only the best of each is kept.
    group: String,
}

fn read_mail(reader: &mut Reader, cx: &MailContext) -> Source {
    let url = format!(
        "{}/v1.0/me/mailFolders/inbox/messages",
        reader.gw.endpoints().ms_graph
    );
    let since_day = cx.since_day;
    if let Some(search) = cx.terms.search() {
        // Newest first: stop at the first page that reaches past the window.
        let past_window = |page: &[Value]| {
            page.iter().any(|m| {
                day_of(m["receivedDateTime"].as_str().unwrap_or("")).is_some_and(|d| d < since_day)
            })
        };
        let pages = reader.pages(
            &url,
            &[
                ("$search", search.as_str()),
                ("$select", MAIL_FIELDS),
                ("$top", PAGE_SIZE),
            ],
            &[],
            MAIL_PAGES,
            &past_window,
        );
        let unsupported = matches!(pages.error, Some(Error::Provider(400 | 501, _)));
        if !(unsupported && pages.values.is_empty()) {
            return scored(pages, |m| email_item(m, cx, true));
        }
    }
    let (y, m, d) = model::date::civil_from_days(since_day);
    let filter = format!("receivedDateTime ge {y:04}-{m:02}-{d:02}T00:00:00Z");
    let pages = reader.pages(
        &url,
        &[
            ("$filter", filter.as_str()),
            ("$orderby", "receivedDateTime desc"),
            ("$select", MAIL_FIELDS),
            ("$top", PAGE_SIZE),
        ],
        &[],
        MAIL_PAGES,
        &|_| false,
    );
    scored(pages, |m| email_item(m, cx, false))
}

fn read_events(reader: &mut Reader, terms: &Terms, today: &str, today_day: i64) -> Source {
    let url = format!("{}/v1.0/me/calendarView", reader.gw.endpoints().ms_graph);
    let end = model::date::add_days(today, CALENDAR_DAYS).unwrap_or_else(|| today.to_string());
    let start = format!("{today}T00:00:00Z");
    let end = format!("{end}T00:00:00Z");
    let event_terms = Terms::new(
        terms
            .words
            .iter()
            .chain(terms.phrases.iter())
            .map(String::as_str)
            .chain(EVENT_WORDS.iter().copied()),
    );
    let pages = reader.pages(
        &url,
        &[
            ("startDateTime", start.as_str()),
            ("endDateTime", end.as_str()),
            ("$select", EVENT_FIELDS),
            ("$orderby", "start/dateTime"),
            ("$top", PAGE_SIZE),
        ],
        &[("Prefer", "outlook.timezone=\"UTC\"")],
        EVENT_PAGES,
        &|_| false,
    );
    scored(pages, |e| event_item(e, &event_terms, today_day))
}

fn scored(pages: Pages, map: impl Fn(&Value) -> Option<Scored>) -> Source {
    Source {
        items: pages.values.iter().filter_map(map).collect(),
        error: pages.error,
    }
}

/// Days since the epoch of an ISO 8601 date-time's date part.
fn day_of(timestamp: &str) -> Option<i64> {
    model::date::ymd_prefix(timestamp).and_then(|d| model::date::parse_ymd(&d))
}

/// The person's first name, lowercase, from the account: the first part of the address
/// ("dana.okafor@…") or the first word of the name. None when it doesn't look like a name.
fn first_name(account: &Account) -> Option<String> {
    let user = account.user.trim();
    let head = match user.split_once('@') {
        Some((local, _)) => local.split(['.', '_', '-', '+']).next().unwrap_or(""),
        None => user.split_whitespace().next().unwrap_or(""),
    };
    let name = head.to_lowercase();
    (name.chars().count() >= 2 && name.chars().all(char::is_alphabetic)).then_some(name)
}

/// (display name, address) of a Graph recipient.
fn person(v: &Value) -> (String, String) {
    let e = &v["emailAddress"];
    (
        squash(e["name"].as_str().unwrap_or("")),
        squash(e["address"].as_str().unwrap_or("")).to_ascii_lowercase(),
    )
}

/// How mail is judged: the keywords, the window, and who it's for.
struct MailContext<'a> {
    terms: &'a Terms,
    since_day: i64,
    /// The person's first name, lowercase, when it can be told from the account.
    first_name: Option<String>,
}

fn email_item(m: &Value, cx: &MailContext, found_by_search: bool) -> Option<Scored> {
    let received = m["receivedDateTime"].as_str().unwrap_or("");
    let date = model::date::ymd_prefix(received)?;
    let day = model::date::parse_ymd(&date)?;
    if day < cx.since_day {
        return None;
    }
    let url = m["webLink"].as_str().unwrap_or("").trim().to_string();
    if url.is_empty() {
        return None;
    }
    let subject = squash(m["subject"].as_str().unwrap_or(""));
    let preview = squash(m["bodyPreview"].as_str().unwrap_or(""));
    let (name, address) = person(&m["from"]);
    let mut score = cx.terms.score(&subject, &preview);
    if found_by_search {
        // Graph matched it, perhaps further into the body than the preview shows.
        score += 1;
    }
    if let Some(first) = &cx.first_name
        && Text::new(&preview)
            .tokens
            .iter()
            .take(8)
            .any(|t| t == first)
    {
        // "Hi Dana, …": written to the person, not to a list.
        score += 2;
    }
    let bulk = looks_bulk(&address, &preview);
    if bulk {
        // Newsletters are dense with keywords; only the most pertinent survive, low down.
        score = score / 3 - 1;
    }
    if score <= 0 {
        return None;
    }
    let group = if bulk {
        format!("sender {address}")
    } else {
        thread_key(&subject)
    };
    Some(Scored {
        item: ContextItem {
            kind: "email".into(),
            title: if subject.is_empty() {
                "(no subject)".into()
            } else {
                subject
            },
            text: preview,
            author: if name.is_empty() { address } else { name },
            container: "Inbox".into(),
            url,
            date: Some(date),
            ..Default::default()
        },
        score,
        day,
        group,
    })
}

fn event_item(e: &Value, terms: &Terms, today_day: i64) -> Option<Scored> {
    if e["isCancelled"].as_bool() == Some(true) {
        return None;
    }
    let date = model::date::ymd_prefix(e["start"]["dateTime"].as_str().unwrap_or(""))?;
    let day = model::date::parse_ymd(&date)?;
    let url = e["webLink"].as_str().unwrap_or("").trim().to_string();
    if url.is_empty() || day < today_day {
        return None;
    }
    let subject = squash(e["subject"].as_str().unwrap_or(""));
    if is_routine(&subject) {
        return None;
    }
    let preview = squash(e["bodyPreview"].as_str().unwrap_or(""));
    let mut score = terms.score(&subject, &preview);
    let response = e["responseStatus"]["response"].as_str().unwrap_or("");
    if score > 0 && matches!(response, "notResponded" | "none" | "tentativelyAccepted") {
        // An invitation still open: a decision the person can still make.
        score += 1;
    }
    if score <= 0 {
        return None;
    }
    let location = squash(e["location"]["displayName"].as_str().unwrap_or(""));
    let (name, address) = person(&e["organizer"]);
    let group = thread_key(&subject);
    Some(Scored {
        item: ContextItem {
            kind: "event".into(),
            title: if subject.is_empty() {
                "(no subject)".into()
            } else {
                subject
            },
            text: match (location.is_empty(), preview.is_empty()) {
                (true, _) => preview,
                (false, true) => format!("Location: {location}."),
                (false, false) => format!("Location: {location}. {preview}"),
            },
            author: if name.is_empty() { address } else { name },
            container: "Calendar".into(),
            url,
            date: Some(date),
            ..Default::default()
        },
        score,
        day,
        group,
    })
}

/// Most relevant first (ties: closest to today), the best of each group of near duplicates, at
/// most `max`.
fn rank(mut all: Vec<Scored>, today_day: i64, max: usize) -> Vec<ContextItem> {
    all.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then((a.day - today_day).abs().cmp(&(b.day - today_day).abs()))
    });
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut out = Vec::new();
    for s in all {
        let key = (s.item.kind.clone(), s.group);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(s.item);
        if out.len() >= max {
            break;
        }
    }
    out
}

/// A subject's words without reply/forward prefixes and bare numbers, for spotting the same
/// conversation or series twice ("RE: Invitation…", "Weekly digest #119" and "#120").
fn thread_key(subject: &str) -> String {
    let mut words = words_of(subject);
    while words
        .first()
        .is_some_and(|w| matches!(w.as_str(), "re" | "fw" | "fwd" | "aw" | "wg" | "sv"))
    {
        words.remove(0);
    }
    words.retain(|w| !w.chars().all(|c| c.is_ascii_digit()));
    words.join(" ")
}

/// Words that mark an event worth attention, on top of the keywords.
const EVENT_WORDS: &[&str] = &[
    "conference",
    "summit",
    "keynote",
    "panel",
    "panelist",
    "webinar",
    "workshop",
    "interview",
    "meetup",
    "hackathon",
    "expo",
    "forum",
    "award",
    "ceremony",
    "gala",
    "pitch",
    "roundtable",
    "fireside",
    "speaker",
    "invitation",
];

/// Everyday meetings, by subject.
fn is_routine(subject: &str) -> bool {
    const WORDS: &[&str] = &[
        "standup", "sync", "lunch", "ooo", "weekly", "daily", "retro", "sprint", "dentist",
        "doctor",
    ];
    const PHRASES: &[&str] = &[
        "stand up",
        "1 1",
        "one on one",
        "catch up",
        "check in",
        "focus time",
        "out of office",
    ];
    let t = Text::new(subject);
    WORDS.iter().any(|w| t.tokens.iter().any(|tok| tok == w))
        || PHRASES.iter().any(|p| t.has_phrase(p))
}

/// Newsletters, marketing and automated notifications.
fn looks_bulk(address: &str, preview: &str) -> bool {
    const SENDERS: &[&str] = &[
        "noreply",
        "no-reply",
        "donotreply",
        "do-not-reply",
        "newsletter",
        "notification",
        "mailer",
        "digest",
        "marketing",
        "promo",
        "deals",
        "offers",
    ];
    const FOOTERS: &[&str] = &[
        "unsubscribe",
        "view in browser",
        "view this email in your browser",
        "manage your preferences",
        "email preferences",
    ];
    let preview = preview.to_lowercase();
    SENDERS.iter().any(|s| address.contains(s)) || FOOTERS.iter().any(|f| preview.contains(f))
}

/// Opportunity keywords: single words (searchable on Graph and matched as words) and phrases
/// (matched locally), all lowercase.
struct Terms {
    words: Vec<String>,
    phrases: Vec<String>,
}

impl Terms {
    fn new<S: AsRef<str>>(keywords: impl Iterator<Item = S>) -> Terms {
        let mut t = Terms {
            words: Vec::new(),
            phrases: Vec::new(),
        };
        for k in keywords {
            let k = words_of(k.as_ref()).join(" ");
            if k.is_empty() {
                continue;
            }
            let list = if k.contains(' ') {
                &mut t.phrases
            } else {
                &mut t.words
            };
            if !list.contains(&k) {
                list.push(k);
            }
        }
        t
    }

    /// A KQL `$search` value: the single-word keywords joined with `OR`, quoted as Graph wants.
    /// Only letters and digits reach it, so nothing in it is KQL syntax; None without any word.
    fn search(&self) -> Option<String> {
        let words: Vec<&str> = self
            .words
            .iter()
            .map(String::as_str)
            .filter(|w| !matches!(*w, "and" | "or" | "not" | "near"))
            .take(MAX_SEARCH_TERMS)
            .collect();
        (!words.is_empty()).then(|| format!("\"{}\"", words.join(" OR ")))
    }

    /// 3 per distinct word or phrase of the subject that is a keyword, plus 1 per one in the
    /// preview (at most 4 from the preview). A word counts once however many keywords it
    /// matches ("programme" is both "program" and "programme").
    fn score(&self, subject: &str, preview: &str) -> i32 {
        let (s, p) = (Text::new(subject), Text::new(preview));
        let in_subject = self.hits(&s);
        let in_preview = self.hits(&p).min(4);
        3 * in_subject + in_preview
    }

    fn hits(&self, text: &Text) -> i32 {
        let mut seen: Vec<&str> = Vec::new();
        for t in &text.tokens {
            if !seen.contains(&t.as_str()) && self.words.iter().any(|w| Text::inflects(t, w)) {
                seen.push(t);
            }
        }
        let phrases = self.phrases.iter().filter(|p| text.has_phrase(p)).count();
        (seen.len() + phrases) as i32
    }
}

/// Lowercase words: runs of letters and digits.
fn words_of(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Text as words, for matching keywords.
struct Text {
    tokens: Vec<String>,
    /// The words joined by single spaces, with a space at each end.
    joined: String,
}

impl Text {
    fn new(s: &str) -> Text {
        let tokens = words_of(s);
        let joined = format!(" {} ", tokens.join(" "));
        Text { tokens, joined }
    }

    #[cfg(test)]
    fn has_word(&self, w: &str) -> bool {
        self.tokens.iter().any(|t| Text::inflects(t, w))
    }

    /// Is `token` the word `w` or a close inflection of it? "invite" matches "invited" and
    /// "invites", "program" matches "programme", "panel" matches "panelist".
    fn inflects(token: &str, w: &str) -> bool {
        token == w
            || token.strip_prefix(w).is_some_and(|rest| {
                matches!(rest, "s" | "es" | "d" | "ed")
                    || (w.chars().count() >= 5 && rest.chars().count() <= 3)
            })
    }

    fn has_phrase(&self, phrase: &str) -> bool {
        self.joined.contains(&format!(" {phrase} "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn app(tenant: &str) -> OAuthApp {
        OAuthApp {
            client_id: "client-1".into(),
            client_secret: "s3cret".into(),
            redirect_uri: "https://opp.example/api/connectors/microsoft/callback".into(),
            tenant: tenant.into(),
        }
    }

    #[test]
    fn the_authorize_url_asks_for_read_scopes_with_pkce() {
        let eps = Endpoints::default();
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        let url = authorize_url(&eps, &app(""), "st8", &pkce);
        assert!(
            url.starts_with(
                "https://login.microsoftonline.com/organizations/oauth2/v2.0/authorize?"
            )
        );
        for part in [
            "client_id=client-1",
            "response_type=code",
            "redirect_uri=https%3A%2F%2Fopp.example%2Fapi%2Fconnectors%2Fmicrosoft%2Fcallback",
            "response_mode=query",
            "scope=openid%20profile%20email%20offline_access%20User.Read%20Mail.Read%20Calendars.Read",
            "state=st8",
            "code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            "code_challenge_method=S256",
        ] {
            assert!(url.contains(part), "{part} missing from {url}");
        }
        assert!(!url.contains("Send") && !url.contains("Write"));
    }

    #[test]
    fn the_tenant_is_one_path_segment() {
        let eps = Endpoints::default();
        let pkce = Pkce::new();
        let single = authorize_url(&eps, &app("contoso.onmicrosoft.com"), "s", &pkce);
        assert!(single.contains("/contoso.onmicrosoft.com/oauth2/v2.0/authorize?"));
        let evil = authorize_url(&eps, &app("x/../common"), "s", &pkce);
        assert!(
            evil.contains("/x%2F..%2Fcommon/oauth2/"),
            "a malformed tenant must not change the path: {evil}"
        );
    }

    #[test]
    fn granted_scopes_are_kept_as_granted() {
        let plain =
            json!({"scope": "openid profile https://graph.microsoft.com/Mail.Read User.Read"});
        assert_eq!(
            granted_scopes(&plain).unwrap(),
            vec![
                "openid",
                "profile",
                "https://graph.microsoft.com/Mail.Read",
                "User.Read"
            ]
        );
        let encoded = json!({"scope": "https%3A%2F%2Fgraph.microsoft.com%2Fmail.read+https%3A%2F%2Fgraph.microsoft.com%2FMail.Send"});
        assert_eq!(
            granted_scopes(&encoded).unwrap(),
            vec![
                "https://graph.microsoft.com/mail.read",
                "https://graph.microsoft.com/Mail.Send"
            ]
        );
        assert_eq!(granted_scopes(&json!({"scope": "  "})), None);
        assert_eq!(granted_scopes(&json!({})), None);
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%41"), "%zzA");
    }

    #[test]
    fn a_token_reply_without_granted_scopes_is_refused() {
        let reply = json!({"access_token": "at", "token_type": "Bearer", "expires_in": 3599});
        assert!(matches!(
            Grant::from_reply(200, &reply),
            Err(Error::Invalid(_))
        ));
        let ok = json!({"access_token": "at", "token_type": "Bearer", "expires_in": "3599",
            "scope": "Mail.Read", "refresh_token": "rt"});
        let g = Grant::from_reply(200, &ok).unwrap();
        assert_eq!(g.scopes, vec!["Mail.Read"]);
        assert_eq!(g.refresh_token.as_deref(), Some("rt"));
        assert!(g.expires_at > model::date::now_unix() + 3500);
        let forever = json!({"access_token": "at", "expires_in": i64::MAX, "scope": "Mail.Read"});
        let g = Grant::from_reply(200, &forever).unwrap();
        assert!(g.expires_at <= model::date::now_unix() + MAX_TOKEN_SECS);
        let mac = json!({"access_token": "at", "token_type": "MAC", "scope": "Mail.Read"});
        assert!(matches!(
            Grant::from_reply(200, &mac),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn token_errors_map_to_what_the_person_or_admin_can_do() {
        let e = |code: &str| {
            token_error(
                400,
                &json!({"error": code, "error_description": "AADSTS50173: The provided grant has expired due to it being revoked.\r\nTrace ID: abc\r\nCorrelation ID: def"}),
            )
        };
        match e("invalid_grant") {
            Error::Unauthorized(m) => {
                assert!(m.starts_with("invalid_grant: AADSTS50173"));
                assert!(!m.contains("Trace ID"), "only the first line: {m}");
            }
            other => panic!("{other:?}"),
        }
        assert!(e("invalid_grant").needs_reconnect());
        assert!(matches!(e("interaction_required"), Error::Unauthorized(_)));
        assert!(matches!(e("invalid_client"), Error::NotConfigured(_)));
        assert!(matches!(e("invalid_scope"), Error::Provider(400, _)));
        assert!(matches!(
            token_error(500, &json!({})),
            Error::Provider(500, _)
        ));
    }

    fn jwt(claims: Value) -> String {
        let b = |v: &Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string().as_bytes())
        };
        format!(
            "{}.{}.c2ln",
            b(&json!({"alg": "RS256", "typ": "JWT"})),
            b(&claims)
        )
    }

    #[test]
    fn the_account_comes_from_id_token_claims() {
        let token = jwt(json!({"tid": "t-1", "oid": "o-1", "name": "Dana Okafor",
            "preferred_username": "dana@brightwater.example"}));
        let a = account_from_id_token(Some(&token));
        assert_eq!(a.id, "t-1:o-1");
        assert_eq!(a.user, "dana@brightwater.example");
        assert_eq!(a.name, "brightwater.example");
        assert_eq!(a.url, "https://outlook.office.com/");
        let personal = account_from_id_token(Some(&jwt(
            json!({"tid": CONSUMER_TENANT, "oid": "o", "name": "Sam"}),
        )));
        assert_eq!(personal.name, "Personal Microsoft account");
        assert_eq!(personal.user, "Sam");
        assert_eq!(account_from_id_token(Some("not.a-jwt")).id, "");
        assert_eq!(account_from_id_token(None).id, "");
        let me = json!({"id": "o-1", "displayName": "Dana Okafor", "mail": "dana.okafor@brightwater.example"});
        let b = account_from_me(&me, &a);
        assert_eq!(
            (b.id.as_str(), b.user.as_str()),
            ("t-1:o-1", "dana.okafor@brightwater.example")
        );
        let fresh = account_from_me(&me, &Account::default());
        assert_eq!(fresh.id, "o-1");
        assert_eq!(fresh.name, "brightwater.example");
    }

    fn terms() -> Terms {
        Terms::new(crate::DEFAULT_KEYWORDS.iter())
    }

    #[test]
    fn the_search_holds_only_plain_words() {
        let t = Terms::new(
            [
                "speaker",
                "call for",
                "co-founder",
                "rfp",
                "OR",
                "x:y",
                "\"quoted\"",
                "speaker",
            ]
            .iter(),
        );
        assert_eq!(t.search().unwrap(), "\"speaker OR rfp OR quoted\"");
        assert_eq!(
            t.phrases,
            vec!["call for", "co founder", "x y"],
            "phrases stay local"
        );
        assert_eq!(Terms::new(["call for"].iter()).search(), None);
        let all = terms().search().unwrap();
        let words = crate::DEFAULT_KEYWORDS
            .iter()
            .filter(|k| !k.contains(' '))
            .count();
        assert_eq!(all.matches(" OR ").count(), words.min(MAX_SEARCH_TERMS) - 1);
        let many: Vec<String> = (0..60).map(|i| format!("w{i}")).collect();
        assert_eq!(
            Terms::new(many.iter())
                .search()
                .unwrap()
                .matches(" OR ")
                .count(),
            MAX_SEARCH_TERMS - 1
        );
        assert!(
            all.chars()
                .all(|c| c.is_alphanumeric() || c == ' ' || c == '"')
        );
    }

    #[test]
    fn words_match_with_inflections_only() {
        let t = Text::new("Invited to the Panelist's programme, roles & RFPs!");
        assert!(t.has_word("invite") && t.has_word("panel") && t.has_word("program"));
        assert!(t.has_word("role") && t.has_word("rfp"));
        assert!(!Text::new("Control plane").has_word("role"));
        assert!(!Text::new("Programming notes").has_word("program"));
        assert!(Text::new("A call for papers").has_phrase("call for"));
        assert!(!Text::new("A callfor papers").has_phrase("call for"));
    }

    fn message(subject: &str, preview: &str, from: &str, received: &str) -> Value {
        json!({"subject": subject, "bodyPreview": preview, "receivedDateTime": received,
            "from": {"emailAddress": {"name": "", "address": from}},
            "webLink": "https://outlook.office365.com/owa/?ItemID=AAMk1&exvsurl=1&viewmodel=ReadMessageItem"})
    }

    #[test]
    fn emails_are_scored_and_newsletters_pushed_down() {
        let t = terms();
        let cx = MailContext {
            terms: &t,
            since_day: model::date::parse_ymd("2026-10-27").unwrap(),
            first_name: Some("dana".into()),
        };
        let invite = message(
            "Invitation to speak at Data Summit Europe",
            "We'd love you as a speaker on our panel.",
            "priya@summit.example",
            "2026-11-20T09:00:00Z",
        );
        let s = email_item(&invite, &cx, false).unwrap();
        assert_eq!(s.item.kind, "email");
        assert_eq!(s.item.container, "Inbox");
        assert_eq!(s.item.author, "priya@summit.example");
        assert_eq!(s.item.date.as_deref(), Some("2026-11-20"));
        assert_eq!(
            s.score,
            3 + 2,
            "invitation in the subject; speaker, panel below"
        );
        let personal = message(
            "Invitation to speak at Data Summit Europe",
            "Hi Dana, we'd love you as a speaker on our panel.",
            "priya@summit.example",
            "2026-11-20T09:00:00Z",
        );
        assert_eq!(
            email_item(&personal, &cx, false).unwrap().score,
            s.score + 2
        );
        let digest = message(
            "Funding rounds, new roles and hiring news",
            "Funding, roles, hiring and partnerships this week. Unsubscribe",
            "newsletter@weekly.example",
            "2026-11-21T09:00:00Z",
        );
        let d = email_item(&digest, &cx, true).unwrap();
        assert!(
            d.score < s.score,
            "a keyword-dense newsletter ranks below a personal invitation"
        );
        let newsletter = message(
            "Don't miss this opportunity",
            "Save 40% today. Unsubscribe | View in browser",
            "deals@offers.example",
            "2026-11-21T09:00:00Z",
        );
        assert!(email_item(&newsletter, &cx, true).is_none());
        let old = message(
            "Invitation to speak",
            "",
            "a@b.example",
            "2026-10-01T09:00:00Z",
        );
        assert!(email_item(&old, &cx, true).is_none(), "outside the window");
        let plain = message(
            "Lunch?",
            "Are you free?",
            "lucy@b.example",
            "2026-11-21T09:00:00Z",
        );
        assert!(email_item(&plain, &cx, false).is_none());
        assert!(
            email_item(&plain, &cx, true).is_some(),
            "a search match counts even when the preview doesn't show why"
        );
    }

    #[test]
    fn first_names_come_from_the_account() {
        let a = |user: &str| Account {
            user: user.into(),
            ..Default::default()
        };
        assert_eq!(
            first_name(&a("dana.okafor@brightwater.example")).as_deref(),
            Some("dana")
        );
        assert_eq!(first_name(&a("Dana Okafor")).as_deref(), Some("dana"));
        assert_eq!(first_name(&a("d@x.example")), None);
        assert_eq!(first_name(&a("u12345@x.example")), None);
        assert_eq!(first_name(&a("")), None);
    }

    fn event(subject: &str, start: &str, cancelled: bool, response: &str) -> Value {
        json!({"subject": subject, "bodyPreview": "", "isCancelled": cancelled,
            "start": {"dateTime": start, "timeZone": "UTC"},
            "location": {"displayName": "Oslo Science Park"},
            "organizer": {"emailAddress": {"name": "Henrik Aas", "address": "henrik@odi.example"}},
            "responseStatus": {"response": response},
            "webLink": "https://outlook.office365.com/owa/?itemid=AAMk2&exvsurl=1&path=/calendar/item"})
    }

    #[test]
    fn events_skip_cancelled_and_routine_meetings() {
        let today = model::date::parse_ymd("2026-11-26").unwrap();
        let t = Terms::new(
            crate::DEFAULT_KEYWORDS
                .iter()
                .copied()
                .chain(EVENT_WORDS.iter().copied()),
        );
        let panel = event(
            "Panel: the future of open data",
            "2026-12-08T16:00:00.0000000",
            false,
            "notResponded",
        );
        let s = event_item(&panel, &t, today).unwrap();
        assert_eq!(s.item.kind, "event");
        assert_eq!(s.item.container, "Calendar");
        assert_eq!(s.item.author, "Henrik Aas");
        assert_eq!(s.item.date.as_deref(), Some("2026-12-08"));
        assert_eq!(s.item.text, "Location: Oslo Science Park.");
        let accepted = event(
            "Panel: the future of open data",
            "2026-12-08T16:00:00.0000000",
            false,
            "accepted",
        );
        assert_eq!(event_item(&accepted, &t, today).unwrap().score, s.score - 1);
        let cancelled = event("Panel: x", "2026-12-08T16:00:00.0000000", true, "none");
        assert!(event_item(&cancelled, &t, today).is_none());
        let standup = event(
            "Team standup",
            "2026-12-08T09:30:00.0000000",
            false,
            "accepted",
        );
        assert!(event_item(&standup, &t, today).is_none());
        let one_on_one = event(
            "1:1 Dana / Sam (programme updates)",
            "2026-12-08T09:30:00.0000000",
            false,
            "accepted",
        );
        assert!(event_item(&one_on_one, &t, today).is_none());
    }

    #[test]
    fn ranking_collapses_near_duplicates_and_caps() {
        let scored = |title: &str, kind: &str, score: i32, day: i64| Scored {
            item: ContextItem {
                kind: kind.into(),
                title: title.into(),
                ..Default::default()
            },
            score,
            day,
            group: thread_key(title),
        };
        let mut newsletter = scored("The Data Weekly #120", "email", 2, 11);
        newsletter.group = "sender newsletter@weekly.example".into();
        let mut other_issue = scored("The Data Weekly #121: grants", "email", 3, 11);
        other_issue.group = newsletter.group.clone();
        let all = vec![
            scored("Invitation to speak", "email", 6, 10),
            scored("RE: Invitation to speak", "email", 7, 11),
            scored("Panel", "event", 7, 14),
            scored("Panel", "email", 4, 9),
            scored("RFP", "email", 3, 12),
            newsletter,
            other_issue,
        ];
        let ranked = rank(all, 12, 10);
        let titles: Vec<&str> = ranked.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "RE: Invitation to speak",
                "Panel",
                "Panel",
                "RFP",
                "The Data Weekly #121: grants"
            ]
        );
        assert_eq!(
            ranked[1].kind, "event",
            "an event and an email with the same name are different things"
        );
        let few: Vec<Scored> = ["a", "b", "c"]
            .iter()
            .map(|t| scored(t, "email", 1, 0))
            .collect();
        assert_eq!(rank(few, 0, 2).len(), 2);
        assert_eq!(
            thread_key("FW: RE: Candidate 165 feedback"),
            "candidate feedback"
        );
    }
}
