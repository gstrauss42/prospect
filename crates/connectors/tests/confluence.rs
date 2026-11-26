//! The Confluence connector end to end, through the public facade, against the mock upstream
//! (`tests/mock/server.py`, which routes Atlassian's paths to `confluence_mock.py`).
//!
//! Each test starts its own mock on a free port in 9990–9999, one test at a time, and stops it
//! when it ends (even on failure). Every test finishes by checking the read-only guarantee from
//! both sides: the mock saw no write attempt (`GET /_violations` is `[]`, every data request was a
//! GET), and the audit holds only GET reads and token POSTs.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use connectors::{
    Account, Connectors, ContextItem, Credential, Endpoints, Error, FetchQuery, MemoryAudit,
    OAuthApp, Provider, TokenInput,
};
use serde_json::Value;

const REDIRECT_URI: &str = "http://localhost:8080/api/connectors/confluence/callback";
const CLIENT_ID: &str = "mock-atlassian-client-id";
const CLIENT_SECRET: &str = "mock-atlassian-client-secret";
const CLOUD_ID: &str = "4f8b2c1a-7d3e-4b6a-9c0f-1e2d3c4b5a69";
const SANDBOX_CLOUD_ID: &str = "9a0b1c2d-3e4f-4a5b-8c6d-7e8f9a0b1c2d";
const EMAIL: &str = "sam.okafor@lumenfield.example";
const CLASSIC_TOKEN: &str = "mock-classic-api-token";
const SCOPED_TOKEN: &str = "mock-scoped-api-token";
const WRITE: &str = "write:confluence-content";
const READ_SCOPES: [&str; 5] = [
    "search:confluence",
    "read:confluence-content.summary",
    "read:confluence-space.summary",
    "read:confluence-user",
    "offline_access",
];
/// The pages in the mock that are real opportunities (or say where the company is heading).
const OPPORTUNITIES: [&str; 9] = [
    "Innovation Week 2026: call for project ideas",
    "Mentoring programme: applications open for the spring cohort",
    "Call for proposals: represent Lumenfield at DevConf Europe 2027",
    "Open roles: internal applications welcome",
    "2027 strategy: where Lumenfield is heading",
    "We're sponsoring the City Robotics Challenge: volunteers wanted",
    "R&D grants: internal funding round for Q1",
    "Secondment opportunity: six months with the Singapore office",
    "Café Lumen partnership: baristas wanted for the opening week",
];
const SPACES: [&str; 7] = [
    "Innovation Lab",
    "People & Culture",
    "Engineering",
    "Strategy & Planning",
    "IT Help Desk",
    "Marketing",
    "Workplace",
];

// ---------------------------------------------------------------------------------------------
// The mock server
// ---------------------------------------------------------------------------------------------

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn one_at_a_time() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

/// The mock upstream server, stopped (by PID) when dropped.
struct Mock {
    child: Child,
    _stdout: BufReader<ChildStdout>,
    base: String,
}

impl Mock {
    fn start() -> Mock {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/mock/server.py");
        for port in 9990..=9999u16 {
            if TcpListener::bind(("127.0.0.1", port)).is_err() {
                continue;
            }
            let mut child = Command::new("python3")
                .arg(script)
                .args(["--port", &port.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("python3 runs the mock server");
            let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
            let mut line = String::new();
            let _ = stdout.read_line(&mut line);
            // It prints this only once it has the port; another process may have won the race.
            if line.contains("listening on") {
                return Mock {
                    child,
                    _stdout: stdout,
                    base: format!("http://127.0.0.1:{port}"),
                };
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("no free port in 9990-9999 for the mock server");
    }

    fn get(&self, path: &str) -> Value {
        let mut resp = http()
            .get(&format!("{}{path}", self.base))
            .call()
            .expect("the mock answers");
        let text = resp.body_mut().read_to_string().expect("a body");
        serde_json::from_str(&text).expect("JSON from the mock")
    }

    fn violations(&self) -> Vec<Value> {
        self.get("/_violations")
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn log(&self) -> Vec<Value> {
        self.get("/_log").as_array().cloned().unwrap_or_default()
    }

    fn site(&self) -> String {
        format!("{}/confluence-site", self.base)
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A plain client for the test's own requests: it plays the browser, and reads the mock's
/// control endpoints. Redirects are not followed.
fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into()
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

fn connectors(mock: &Mock) -> Connectors {
    let base = &mock.base;
    let eps = Endpoints {
        slack_api: format!("{base}/slack"),
        slack_auth: format!("{base}/slack"),
        ms_login: format!("{base}/mslogin"),
        ms_graph: format!("{base}/msgraph"),
        atlassian_auth: format!("{base}/atlassian-auth"),
        atlassian_api: format!("{base}/atlassian-api"),
        confluence_sites: vec!["https://*.atlassian.net".into(), mock.site()],
    };
    let app = OAuthApp {
        client_id: CLIENT_ID.into(),
        client_secret: CLIENT_SECRET.into(),
        redirect_uri: REDIRECT_URI.into(),
        tenant: String::new(),
    };
    Connectors::new(eps, vec![(Provider::Confluence, app)])
}

/// A query value from a URL, decoded (`+` as a space, as the mock's redirect writes it).
fn param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| decode(v))
    })
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).expect("ascii");
                out.push(u8::from_str_radix(hex, 16).expect("hex"));
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).expect("utf-8")
}

/// The browser's part of OAuth: open the authorize URL (with a mock behaviour), don't follow the
/// redirect, and read the code and state from its `Location`.
fn authorize(c: &Connectors, behaviour: &str) -> (String, String) {
    let state = format!("state-{behaviour}");
    let (url, verifier) = c.begin(Provider::Confluence, &state).expect("begin");
    let resp = http()
        .get(&format!("{url}&mock_behavior={behaviour}"))
        .call()
        .expect("the authorize page answers");
    assert_eq!(resp.status().as_u16(), 302, "the mock's instant Allow");
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("a Location header")
        .to_string();
    assert!(
        location.starts_with(&format!("{REDIRECT_URI}?")),
        "{location}"
    );
    assert_eq!(param(&location, "state"), Some(state));
    (param(&location, "code").expect("a code"), verifier)
}

fn connect(c: &Connectors, behaviour: &str, audit: &MemoryAudit) -> Credential {
    let (code, verifier) = authorize(c, behaviour);
    c.finish(Provider::Confluence, &code, &verifier, audit)
        .expect("finish")
}

fn token_input(token: &str, email: &str, site: &str) -> TokenInput {
    TokenInput {
        token: token.into(),
        email: email.into(),
        site: site.into(),
    }
}

fn searches(audit: &MemoryAudit) -> Vec<(String, u16)> {
    audit
        .events()
        .into_iter()
        .filter(|e| e.target.ends_with("/wiki/rest/api/search"))
        .map(|e| (e.target, e.status))
        .collect()
}

/// What a good fetch looks like against the mock's data.
fn check_items(mock: &Mock, items: &[ContextItem]) {
    let links = format!("{}/wiki/spaces/", mock.site());
    let today = model::date::parse_ymd(&model::date::today()).expect("today");
    assert!(items.len() >= OPPORTUNITIES.len(), "{} items", items.len());
    for i in items {
        assert_eq!((i.source.as_str(), i.kind.as_str()), ("confluence", "page"));
        assert!(
            i.url.starts_with(&links),
            "links open the page on the site: {}",
            i.url
        );
        assert!(!i.title.is_empty() && !i.text.is_empty(), "{i:?}");
        assert!(
            !i.title.contains("@@@") && !i.text.contains("@@@"),
            "highlight markup removed: {i:?}"
        );
        assert!(
            !i.text.contains("&amp;") && !i.text.contains("&quot;"),
            "{i:?}"
        );
        assert!(
            SPACES.contains(&i.container.as_str()),
            "space: {}",
            i.container
        );
        let day = i
            .date
            .as_deref()
            .and_then(model::date::parse_ymd)
            .expect("a date");
        assert!((0..=30).contains(&(today - day)), "within 30 days: {i:?}");
    }
    let top: Vec<&str> = items
        .iter()
        .take(OPPORTUNITIES.len())
        .map(|i| i.title.as_str())
        .collect();
    for title in OPPORTUNITIES {
        assert!(
            top.contains(&title),
            "{title} ranks above routine pages: {top:?}"
        );
    }
    for routine in [
        "VPN setup guide",
        "Q4 platform team retro",
        "Office move: parking and access cards",
        "Summer hackathon 2026: results",
        "Autumn conference recap",
    ] {
        assert!(
            items.iter().all(|i| i.title != routine),
            "{routine} isn't relevant or recent"
        );
    }
    let grants = items
        .iter()
        .find(|i| i.title.starts_with("R&D grants"))
        .expect("the grants page");
    assert!(
        grants
            .text
            .contains("\"small, measurable\" beats \"big & vague\" <every time>"),
        "excerpts are plain text: {}",
        grants.text
    );
    let cafe = items
        .iter()
        .find(|i| i.title.starts_with("Café Lumen"))
        .expect("the café page");
    assert!(
        cafe.url.ends_with(
            "/wiki/spaces/OPS/pages/393225/Caf%C3%A9+Lumen+partnership+baristas+wanted+for+the+opening+week"
        ),
        "{}",
        cafe.url
    );
    let blog = items
        .iter()
        .find(|i| i.title.contains("City Robotics"))
        .expect("the blog post");
    assert!(blog.url.contains("/wiki/spaces/MKT/blog/"), "{}", blog.url);
}

/// The read-only guarantee, seen from both sides.
fn assert_only_reads(mock: &Mock, audit: &MemoryAudit, scope_rejections_expected: bool) {
    assert_eq!(
        mock.violations(),
        Vec::<Value>::new(),
        "write attempts reached Atlassian"
    );
    let api = format!("{}/atlassian-api/", mock.base);
    let site = format!("{}/confluence-site/", mock.base);
    let token = format!("{}/atlassian-auth/oauth/token", mock.base);
    for e in audit.events() {
        match e.action.as_str() {
            "read" => {
                assert_eq!(e.method, "GET", "{e:?}");
                assert!(
                    e.target.starts_with(&api) || e.target.starts_with(&site),
                    "{e:?}"
                );
            }
            "token" => {
                assert_eq!(e.method, "POST", "{e:?}");
                assert_eq!(e.target, token, "{e:?}");
            }
            "scope_rejected" if scope_rejections_expected => {
                assert_eq!(
                    (e.method.as_str(), e.status),
                    ("", 0),
                    "nothing sent: {e:?}"
                );
            }
            _ => panic!("unexpected audit event {e:?}"),
        }
        assert!(
            !e.target.contains('?'),
            "no query values in the audit: {e:?}"
        );
        for secret in [
            "mock-access-",
            "mock-refresh-",
            "mock-code-",
            "api-token",
            CLIENT_SECRET,
        ] {
            assert!(
                !e.note.contains(secret) && !e.target.contains(secret),
                "no secrets in the audit: {e:?}"
            );
        }
    }
    for r in mock.log() {
        let (method, path) = (
            r["method"].as_str().unwrap_or(""),
            r["path"].as_str().unwrap_or(""),
        );
        if path.starts_with("/atlassian-api/")
            || path.starts_with("/confluence-site/")
            || path == "/atlassian-auth/authorize"
        {
            assert_eq!(method, "GET", "{method} {path}");
        } else if path == "/atlassian-auth/oauth/token" {
            assert_eq!(method, "POST", "{method} {path}");
        } else {
            panic!("unexpected request {method} {path}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// OAuth 2.0 (3LO)
// ---------------------------------------------------------------------------------------------

#[test]
fn oauth_begin_finish_verify_fetch() {
    let _serial = one_at_a_time();
    let mock = Mock::start();
    let c = connectors(&mock);
    let audit = MemoryAudit::default();

    let (url, verifier) = c.begin(Provider::Confluence, "st8").expect("begin");
    assert!(
        url.starts_with(&format!("{}/atlassian-auth/authorize?", mock.base)),
        "{url}"
    );
    assert_eq!(
        param(&url, "scope"),
        Some(READ_SCOPES.join(" ")),
        "read scopes only"
    );
    for (k, v) in [
        ("audience", "api.atlassian.com"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("state", "st8"),
        ("response_type", "code"),
        ("prompt", "consent"),
    ] {
        assert_eq!(param(&url, k).as_deref(), Some(v), "{k}");
    }
    assert_eq!(verifier.len(), 43);

    let (code, verifier) = authorize(&c, "normal");
    let cred = c
        .finish(Provider::Confluence, &code, &verifier, &audit)
        .expect("finish");
    assert_eq!(
        (cred.provider.as_str(), cred.mode.as_str()),
        ("confluence", "oauth")
    );
    assert_eq!(cred.scopes, READ_SCOPES, "exactly what Atlassian granted");
    assert_eq!(cred.enforcement(), "provider");
    assert!(cred.access_token.starts_with("mock-access-"));
    assert!(
        cred.refresh_token
            .as_deref()
            .is_some_and(|t| t.starts_with("mock-refresh-"))
    );
    let now = model::date::now_unix();
    assert!(cred.expires_at.is_some_and(|t| t > now + 3000) && !cred.needs_refresh(now));
    assert!(cred.basic_user.is_none());

    // The code exchange went out as Atlassian documents it: JSON, with the client secret and
    // the redirect URI, and no PKCE verifier (Atlassian's 3LO has none).
    let exchange = mock
        .log()
        .into_iter()
        .find(|r| r["path"] == "/atlassian-auth/oauth/token")
        .expect("a token request");
    let body: Value = serde_json::from_str(exchange["body"].as_str().unwrap_or("")).expect("JSON");
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["redirect_uri"], REDIRECT_URI);
    assert_eq!(body["client_id"], CLIENT_ID);
    assert!(body.get("code_verifier").is_none());

    // A code works once.
    let again = c.finish(Provider::Confluence, &code, &verifier, &audit);
    assert!(matches!(again, Err(Error::Unauthorized(_))), "{again:?}");

    let account = c.verify(&cred, &audit).expect("verify");
    assert_eq!(
        account,
        Account {
            id: CLOUD_ID.into(),
            name: "lumenfield".into(),
            user: "Dana Whitfield".into(),
            url: mock.site(),
        }
    );
    let cred = Credential { account, ..cred };

    let items = c
        .fetch(&cred, &FetchQuery::default(), &audit)
        .expect("fetch");
    check_items(&mock, &items);
    let pages = searches(&audit);
    assert_eq!(pages.len(), 2, "two pages of results: {pages:?}");
    let gateway = format!(
        "{}/atlassian-api/ex/confluence/{CLOUD_ID}/wiki/rest/api/search",
        mock.base
    );
    assert!(
        pages.iter().all(|(t, s)| *t == gateway && *s == 200),
        "{pages:?}"
    );

    // Fewer items: one request, the best ones.
    let few = c
        .fetch(
            &cred,
            &FetchQuery {
                max_items: 5,
                ..Default::default()
            },
            &audit,
        )
        .expect("fetch five");
    assert_eq!(few.len(), 5);
    assert!(
        few.iter()
            .all(|i| OPPORTUNITIES.contains(&i.title.as_str())),
        "{few:?}"
    );
    assert_eq!(searches(&audit).len(), 3);

    // Straight after finish (no account stored yet), fetch finds the site itself.
    let bare = Credential {
        account: Account::default(),
        ..cred.clone()
    };
    assert_eq!(
        c.fetch(&bare, &FetchQuery::default(), &audit)
            .expect("fetch"),
        items
    );

    assert_only_reads(&mock, &audit, false);
}

#[test]
fn the_stored_site_is_the_one_read() {
    let _serial = one_at_a_time();
    let mock = Mock::start();
    let c = connectors(&mock);
    let audit = MemoryAudit::default();
    let cred = connect(&c, "two_sites", &audit);
    // Two sites granted, none chosen yet: the first Atlassian lists.
    assert_eq!(
        c.verify(&cred, &audit).expect("verify").id,
        SANDBOX_CLOUD_ID
    );
    let main = Credential {
        account: Account {
            id: CLOUD_ID.into(),
            ..Default::default()
        },
        ..cred.clone()
    };
    let account = c.verify(&main, &audit).expect("verify");
    assert_eq!(
        (account.id.as_str(), account.url.clone()),
        (CLOUD_ID, mock.site())
    );
    let items = c
        .fetch(
            &Credential {
                account,
                ..cred.clone()
            },
            &FetchQuery::default(),
            &audit,
        )
        .expect("fetch");
    check_items(&mock, &items);
    let gone = Credential {
        account: Account {
            id: "11111111-2222-3333-4444-555555555555".into(),
            ..Default::default()
        },
        ..cred
    };
    let e = c.fetch(&gone, &FetchQuery::default(), &audit).unwrap_err();
    assert!(
        matches!(e, Error::Unauthorized(_)) && e.needs_reconnect(),
        "{e:?}"
    );
    assert_only_reads(&mock, &audit, false);
}

#[test]
fn write_capable_grants_are_refused_at_finish_and_at_refresh() {
    let _serial = one_at_a_time();
    let mock = Mock::start();
    let c = connectors(&mock);
    let audit = MemoryAudit::default();
    let write = Error::WriteScopes(vec![WRITE.into()]);

    // At finish: the token reply grants a write scope.
    let (code, verifier) = authorize(&c, "write_scope");
    let e = c
        .finish(Provider::Confluence, &code, &verifier, &audit)
        .unwrap_err();
    assert_eq!(e, write);
    assert!(e.needs_reconnect());

    // At refresh: read-only at first, then the rotated token can write.
    let cred = connect(&c, "write_on_refresh", &audit);
    assert_eq!(cred.scopes, READ_SCOPES);
    let now = model::date::now_unix();
    assert!(cred.needs_refresh(now), "a 60-second token");
    assert_eq!(c.refresh_if_needed(&cred, now, &audit).unwrap_err(), write);

    // A site that lists a write scope for the token is refused at sign-in: the facade verifies
    // every new connection before returning it, so it is never saved and nothing is searched.
    let (code, verifier) = authorize(&c, "resource_write");
    assert_eq!(
        c.finish(Provider::Confluence, &code, &verifier, &audit)
            .unwrap_err(),
        write
    );

    let rejected = audit
        .events()
        .iter()
        .filter(|e| e.action == "scope_rejected")
        .count();
    assert_eq!(
        rejected, 3,
        "the facade audits the finish, refresh and site-scope refusals"
    );
    assert!(
        searches(&audit).is_empty(),
        "nothing was searched with those tokens"
    );
    assert_only_reads(&mock, &audit, true);
}

#[test]
fn refresh_tokens_rotate_and_a_reused_one_ends_the_connection() {
    let _serial = one_at_a_time();
    let mock = Mock::start();
    let c = connectors(&mock);
    let audit = MemoryAudit::default();
    let first = connect(&c, "expiring", &audit);
    let now = model::date::now_unix();
    assert!(first.needs_refresh(now));

    let second = c
        .refresh_if_needed(&first, now, &audit)
        .expect("refresh")
        .expect("refreshed");
    assert_ne!(second.access_token, first.access_token);
    assert_ne!(
        second.refresh_token, first.refresh_token,
        "the NEW refresh token is kept"
    );
    assert!(second.expires_at > first.expires_at && !second.needs_refresh(now));
    assert_eq!(second.scopes, READ_SCOPES, "the grant was read again");
    assert_eq!(
        (second.provider.as_str(), second.mode.as_str()),
        ("confluence", "oauth")
    );
    assert!(
        c.refresh_if_needed(&second, now, &audit)
            .expect("no-op")
            .is_none()
    );
    assert!(
        !c.fetch(&second, &FetchQuery::default(), &audit)
            .expect("fetch")
            .is_empty()
    );

    // The kept token refreshes again...
    let due = |cred: &Credential| Credential {
        expires_at: Some(now),
        ..cred.clone()
    };
    let third = c
        .refresh_if_needed(&due(&second), now, &audit)
        .expect("refresh again")
        .expect("refreshed");
    // ...but the rotated-out one is refused, and Atlassian's breach detection then revokes the
    // whole family, so the person has to reconnect.
    let e = c.refresh_if_needed(&due(&first), now, &audit).unwrap_err();
    assert!(
        matches!(e, Error::Unauthorized(_)) && e.needs_reconnect(),
        "{e:?}"
    );
    let e = c.refresh_if_needed(&due(&third), now, &audit).unwrap_err();
    assert!(e.needs_reconnect(), "{e:?}");
    for cred in [&first, &second, &third] {
        let rt = cred.refresh_token.as_deref().unwrap_or("-");
        assert!(!e.to_string().contains(rt), "no token in the message");
    }
    assert_only_reads(&mock, &audit, false);
}

#[test]
fn revoked_and_rejected_tokens_need_a_reconnect() {
    let _serial = one_at_a_time();
    let mock = Mock::start();
    let c = connectors(&mock);
    let audit = MemoryAudit::default();

    let cred = connect(&c, "revoked", &audit);
    let now = model::date::now_unix();
    let e = c.refresh_if_needed(&cred, now, &audit).unwrap_err();
    assert!(
        matches!(e, Error::Unauthorized(_)) && e.needs_reconnect(),
        "{e:?}"
    );

    let rejected = Credential {
        access_token: "mock-access-not-a-real-token".into(),
        ..cred
    };
    for e in [
        c.fetch(&rejected, &FetchQuery::default(), &audit)
            .unwrap_err(),
        c.verify(&rejected, &audit).unwrap_err(),
    ] {
        assert!(
            matches!(e, Error::Unauthorized(_)) && e.needs_reconnect(),
            "{e:?}"
        );
    }

    // A reply that doesn't say what was granted can't be checked, so it isn't used.
    let (code, verifier) = authorize(&c, "no_scope");
    let e = c
        .finish(Provider::Confluence, &code, &verifier, &audit)
        .unwrap_err();
    assert!(matches!(e, Error::Invalid(_)), "{e:?}");
    assert_only_reads(&mock, &audit, false);
}

#[test]
fn rate_limits_give_retry_after_or_partial_results() {
    let _serial = one_at_a_time();
    let mock = Mock::start();
    let c = connectors(&mock);
    let audit = MemoryAudit::default();

    let limited = connect(&c, "rate_limited", &audit);
    assert_eq!(
        c.fetch(&limited, &FetchQuery::default(), &audit)
            .unwrap_err(),
        Error::RateLimited(Some(30))
    );

    let later = connect(&c, "rate_limited_page2", &audit);
    let items = c
        .fetch(&later, &FetchQuery::default(), &audit)
        .expect("what the first page found");
    assert!(
        !items.is_empty() && items.len() <= 25,
        "{} items",
        items.len()
    );
    let statuses: Vec<u16> = searches(&audit).into_iter().map(|(_, s)| s).collect();
    assert_eq!(statuses, [429, 200, 429]);
    assert_only_reads(&mock, &audit, false);
}

// ---------------------------------------------------------------------------------------------
// API tokens
// ---------------------------------------------------------------------------------------------

#[test]
fn api_tokens_are_used_with_basic_auth_where_atlassian_accepts_them() {
    let _serial = one_at_a_time();
    let mock = Mock::start();
    let c = connectors(&mock);
    let site = mock.site();

    // A classic token works on the site's own API (the gateway refuses it first).
    let audit = MemoryAudit::default();
    let classic = c
        .connect_token(
            Provider::Confluence,
            &token_input(
                &format!(" {CLASSIC_TOKEN} "),
                EMAIL,
                &format!("{site}/wiki/"),
            ),
            &audit,
        )
        .expect("connect with a classic token");
    assert_eq!(
        (classic.provider.as_str(), classic.mode.as_str()),
        ("confluence", "token")
    );
    assert!(
        classic.scopes.is_empty(),
        "Atlassian doesn't reveal a token's scopes"
    );
    assert_eq!(classic.enforcement(), "app");
    assert_eq!(classic.access_token, CLASSIC_TOKEN);
    assert_eq!(classic.basic_user.as_deref(), Some(EMAIL));
    assert!(classic.refresh_token.is_none() && classic.expires_at.is_none());
    assert_eq!(
        (
            classic.account.id.as_str(),
            classic.account.user.as_str(),
            classic.account.url.as_str()
        ),
        (CLOUD_ID, "Sam Okafor", site.as_str())
    );
    assert_eq!(c.verify(&classic, &audit).expect("verify"), classic.account);
    let items = c
        .fetch(&classic, &FetchQuery::default(), &audit)
        .expect("fetch");
    check_items(&mock, &items);
    let own_api = format!("{site}/wiki/rest/api/search");
    assert!(
        searches(&audit)
            .iter()
            .all(|(t, s)| *t == own_api && *s == 200)
    );
    assert!(
        c.refresh_if_needed(&classic, model::date::now_unix(), &audit)
            .expect("nothing to refresh")
            .is_none()
    );
    assert_only_reads(&mock, &audit, false);

    // A scoped token works only through the gateway; links still open the site.
    let audit = MemoryAudit::default();
    let scoped = c
        .connect_token(
            Provider::Confluence,
            &token_input(SCOPED_TOKEN, EMAIL, &site),
            &audit,
        )
        .expect("connect with a scoped token");
    assert_eq!(scoped.enforcement(), "app");
    assert_eq!(
        (scoped.account.id.as_str(), scoped.account.url.as_str()),
        (CLOUD_ID, site.as_str())
    );
    let items = c
        .fetch(&scoped, &FetchQuery::default(), &audit)
        .expect("fetch");
    check_items(&mock, &items);
    let gateway = format!(
        "{}/atlassian-api/ex/confluence/{CLOUD_ID}/wiki/rest/api/search",
        mock.base
    );
    assert!(
        searches(&audit)
            .iter()
            .all(|(t, s)| *t == gateway && *s == 200)
    );
    assert_only_reads(&mock, &audit, false);
}

#[test]
fn api_token_sites_and_inputs_are_checked_before_anything_is_sent() {
    let _serial = one_at_a_time();
    let mock = Mock::start();
    let c = connectors(&mock);
    let audit = MemoryAudit::default();
    let site = mock.site();
    let bad_sites = [
        "http://lumenfield.atlassian.net".to_string(),
        "https://lumenfield.atlassian.net.evil.example".into(),
        "https://evil.example".into(),
        "https://evilatlassian.net".into(),
        "https://a.b.atlassian.net".into(),
        "https://lumenfield.atlassian.net/wiki/spaces/ENG".into(),
        "https://lumenfield.atlassian.net/rest/api".into(),
        "https://lumenfield.atlassian.net:8443".into(),
        "https://sam@lumenfield.atlassian.net".into(),
        "https://lumenfield.atlassian.net?next=/x".into(),
        "https://lumenfield.atlassian.net#top".into(),
        "javascript:alert(1)".into(),
        String::new(),
        format!("{}/elsewhere", mock.base),
        format!("{site}/wiki/rest/api"),
        site.replace("127.0.0.1", "localhost"),
    ];
    for bad in &bad_sites {
        let r = c.connect_token(
            Provider::Confluence,
            &token_input(CLASSIC_TOKEN, EMAIL, bad),
            &audit,
        );
        assert!(
            matches!(r, Err(Error::Invalid(_))),
            "{bad:?} must be refused, got {r:?}"
        );
    }
    for (token, email) in [
        ("", EMAIL),
        ("two words", EMAIL),
        (CLASSIC_TOKEN, ""),
        (CLASSIC_TOKEN, "not-an-email"),
        (CLASSIC_TOKEN, "sam:x@lumenfield.example"),
    ] {
        let r = c.connect_token(
            Provider::Confluence,
            &token_input(token, email, &site),
            &audit,
        );
        assert!(
            matches!(r, Err(Error::Invalid(_))),
            "{token:?} {email:?}: {r:?}"
        );
    }
    assert!(
        audit.events().is_empty(),
        "nothing was sent: {:?}",
        audit.events()
    );
    assert!(mock.log().is_empty(), "the mock saw nothing");
    assert_only_reads(&mock, &audit, false);
}

#[test]
fn api_tokens_atlassian_refuses_are_not_connected() {
    let _serial = one_at_a_time();
    let mock = Mock::start();
    let c = connectors(&mock);
    let audit = MemoryAudit::default();
    let site = mock.site();

    let wrong = "mock-wrong-api-token";
    let e = c
        .connect_token(
            Provider::Confluence,
            &token_input(wrong, EMAIL, &site),
            &audit,
        )
        .unwrap_err();
    assert!(matches!(e, Error::Unauthorized(_)), "{e:?}");
    assert!(
        !e.to_string().contains(wrong),
        "the token never appears in a message"
    );

    // The site answers a failed sign-in as a signed-out visitor: still not connected.
    let e = c
        .connect_token(
            Provider::Confluence,
            &token_input(
                CLASSIC_TOKEN,
                "anonymous.fallback@lumenfield.example",
                &site,
            ),
            &audit,
        )
        .unwrap_err();
    assert!(matches!(e, Error::Unauthorized(_)), "{e:?}");

    // A stored token-mode credential whose site was tampered with is refused before any read.
    let classic = c
        .connect_token(
            Provider::Confluence,
            &token_input(CLASSIC_TOKEN, EMAIL, &site),
            &audit,
        )
        .expect("connect");
    let before = audit.events().len();
    let tampered = Credential {
        account: Account {
            url: "https://evil.example".into(),
            ..classic.account.clone()
        },
        ..classic
    };
    assert!(matches!(
        c.fetch(&tampered, &FetchQuery::default(), &audit),
        Err(Error::Invalid(_))
    ));
    assert_eq!(audit.events().len(), before, "nothing was sent");
    assert_only_reads(&mock, &audit, false);
}
