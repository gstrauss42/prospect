//! The Slack connector end to end, through the public facade the way the server uses it, against
//! the mock upstream (`tests/mock/server.py`, Slack part in `tests/mock/slack_mock.py`).
//!
//! One mock server is started per run, on a free port in 9970–9979, and it exits by itself when
//! this test process ends (it watches its stdin). Tests take turns, and each one ends by checking
//! that the mock saw no write attempt and that the audit holds only GET reads of the three Slack
//! read methods and POSTs to the token endpoint.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use connectors::{
    Account, Connectors, ContextItem, Credential, Endpoints, Error, FetchQuery, MemoryAudit,
    OAuthApp, Provider, TokenInput, verify_scopes,
};
use model::date::{add_days, now_unix, today};
use serde_json::{Value, json};

const REDIRECT: &str = "http://localhost:8080/api/connectors/slack/callback";
const READS: &[&str] = &["auth.test", "search.messages", "users.info"];
const ADA: &str = "U0ADA0001";

// ---------------------------------------------------------------------------------------------
// The mock server
// ---------------------------------------------------------------------------------------------

struct Mock {
    base: String,
    // Kept for the life of the process: its stdin closing is what stops the server.
    _child: Mutex<Child>,
}

fn mock() -> &'static Mock {
    static MOCK: OnceLock<Mock> = OnceLock::new();
    MOCK.get_or_init(start_mock)
}

fn start_mock() -> Mock {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/mock");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let instance = format!("slack-tests-{}-{nanos}", std::process::id());
    // The server stops when its stdin closes, i.e. when this process ends, however it ends.
    let script = format!(
        "import os, sys, threading\n\
         sys.path.insert(0, {dir:?})\n\
         threading.Thread(target=lambda: (sys.stdin.read(), os._exit(0)), daemon=True).start()\n\
         sys.argv = ['server.py', '--port', sys.argv[1]]\n\
         import server\n\
         server.main()\n"
    );
    let verbose = std::env::var_os("MOCK_VERBOSE").is_some();
    for port in 9970..=9979u16 {
        if TcpListener::bind(("127.0.0.1", port)).is_err() {
            continue;
        }
        // -I: isolated from the environment; -B: no __pycache__ left in the source tree.
        let mut child = Command::new("python3")
            .arg("-I")
            .arg("-B")
            .arg("-c")
            .arg(&script)
            .arg(port.to_string())
            .env("SLACK_MOCK_INSTANCE", &instance)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(if verbose {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .spawn()
            .expect("python3 is needed to run the Slack tests");
        let base = format!("http://127.0.0.1:{port}");
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                break; // it exited: the port was taken after all
            }
            if let Ok(info) = get_json(&format!("{base}/slack/_mock/info")) {
                if info["instance"] == instance.as_str() {
                    return Mock {
                        base,
                        _child: Mutex::new(child),
                    };
                }
                break; // someone else's server answered
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    panic!("couldn't start the mock server on any port in 9970-9979");
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into()
}

fn get_json(url: &str) -> Result<Value, String> {
    let mut resp = agent().get(url).call().map_err(|e| e.to_string())?;
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

fn post_json(url: &str, body: &Value) -> (u16, Value) {
    let mut resp = agent()
        .post(url)
        .header("Content-Type", "application/json")
        .send(body.to_string().as_bytes())
        .expect("the mock answers");
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// Tests take turns on the one mock, each starting from a clean slate.
fn serial() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    let turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let (status, _) = post_json(&format!("{}/_reset", mock().base), &json!({}));
    assert_eq!(status, 200, "the mock resets");
    turn
}

fn control(what: &str, body: Value) {
    let (status, reply) = post_json(&format!("{}/slack/_mock/{what}", mock().base), &body);
    assert_eq!(status, 200, "{what}: {reply}");
}

/// The queries `search.messages` received since the last reset.
fn searches() -> Vec<String> {
    get_json(&format!("{}/slack/_mock/searches", mock().base))
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q.as_str().unwrap().to_string())
        .collect()
}

fn violations() -> Value {
    get_json(&format!("{}/_violations", mock().base)).unwrap()
}

// ---------------------------------------------------------------------------------------------
// The facade, pointed at the mock
// ---------------------------------------------------------------------------------------------

fn eps() -> Endpoints {
    let base = &mock().base;
    Endpoints {
        slack_api: format!("{base}/slack"),
        slack_auth: format!("{base}/slack"),
        ms_login: format!("{base}/mslogin"),
        ms_graph: format!("{base}/msgraph"),
        atlassian_auth: format!("{base}/atlassian-auth"),
        atlassian_api: format!("{base}/atlassian-api"),
        confluence_sites: Vec::new(),
    }
}

fn app(secret: &str) -> OAuthApp {
    OAuthApp {
        client_id: "mock-client-id".into(),
        client_secret: secret.into(),
        redirect_uri: REDIRECT.into(),
        tenant: String::new(),
    }
}

fn connectors() -> Connectors {
    Connectors::new(eps(), vec![(Provider::Slack, app("mock-client-secret"))])
}

fn token(t: &str) -> TokenInput {
    TokenInput {
        token: t.into(),
        ..Default::default()
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Only GET reads of the three read methods and POSTs to the token endpoint went out; nothing
/// was blocked; no secret reached the audit; the mock saw no write attempt.
fn assert_clean(audit: &MemoryAudit) {
    let base = &mock().base;
    for e in audit.events() {
        assert_eq!(e.provider, "slack", "{e:?}");
        match e.action.as_str() {
            "read" => {
                assert_eq!(e.method, "GET", "{e:?}");
                assert!(
                    READS
                        .iter()
                        .any(|m| e.target == format!("{base}/slack/api/{m}")),
                    "{e:?}"
                );
            }
            "token" => {
                assert_eq!(e.method, "POST", "{e:?}");
                assert_eq!(e.target, format!("{base}/slack/api/oauth.v2.access"));
            }
            "scope_rejected" => assert_eq!(e.status, 0, "{e:?}"),
            other => panic!("unexpected audit event {other}: {e:?}"),
        }
        assert!(
            !e.note.contains("xox") && !e.target.contains('?'),
            "no secrets or queries in the audit: {e:?}"
        );
    }
    assert_eq!(violations(), json!([]), "the mock saw a write attempt");
}

/// An error never carries a token or the client secret.
fn no_secrets(e: &Error, secrets: &[&str]) {
    let text = format!("{e} {e:?}");
    for secret in secrets.iter().chain(&["mock-client-secret"]) {
        assert!(
            secret.is_empty() || !text.contains(secret),
            "{secret} leaked: {text}"
        );
    }
}

fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn find<'a>(items: &'a [ContextItem], needle: &str) -> &'a ContextItem {
    items
        .iter()
        .find(|i| i.text.contains(needle))
        .unwrap_or_else(|| panic!("no item with {needle:?} in {:#?}", titles(items)))
}

fn position(items: &[ContextItem], needle: &str) -> Option<usize> {
    items.iter().position(|i| i.text.contains(needle))
}

fn titles(items: &[ContextItem]) -> Vec<&str> {
    items.iter().map(|i| i.title.as_str()).collect()
}

// The fictional workspace's genuine opportunities, its noise, and what must never come back.
const GENUINE: &[&str] = &[
    "Build Week internal hackathon",
    "Call for speakers",
    "panel moderator",
    "hiring internally",
    "Partnership ask",
    "intro to someone on the Orbit team",
    "mentoring programme",
    "Secondment opportunity",
    "innovation fund",
    "Fernhill Impact Award",
    "collaborate on the Helix pilot",
    "Product Analyst (Growth)",
    "Hackathon mentors wanted",
];
const NOISE: &[&str] = &["ramen", "plumber", "flaky test", "Deadline exceeded"];
const NEVER: &[&str] = &[
    "role-based access",
    "office is closed",
    "icon set",
    "recap and photos",
    "spring meetup",
];

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[test]
fn sign_in_with_slack_then_verify_and_fetch() {
    let _turn = serial();
    let c = connectors();
    let audit = MemoryAudit::default();

    let (url, verifier) = c.begin(Provider::Slack, "state-123").unwrap();
    assert!(
        url.starts_with(&format!("{}/slack/oauth/v2/authorize?", mock().base)),
        "{url}"
    );
    assert!(url.contains("client_id=mock-client-id"));
    assert!(url.contains("user_scope=search%3Aread%2Cusers%3Aread"));
    assert!(url.contains("state=state-123"));
    assert!(!url.contains("&scope=") && !url.contains("code_challenge") && !url.contains("secret"));

    // The browser's part: Slack's page sends the person back with a code. Not followed here.
    let resp = agent().get(&url).call().unwrap();
    assert_eq!(resp.status().as_u16(), 302);
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();
    assert!(location.starts_with(REDIRECT), "{location}");
    assert_eq!(
        query_param(&location, "state").as_deref(),
        Some("state-123")
    );
    let code = query_param(&location, "code").expect("a code");

    let cred = c.finish(Provider::Slack, &code, &verifier, &audit).unwrap();
    assert_eq!(
        (cred.provider.as_str(), cred.mode.as_str()),
        ("slack", "oauth")
    );
    assert!(cred.access_token.starts_with("xoxp-"));
    assert_eq!(cred.scopes, s(&["search:read", "users:read"]));
    assert_eq!(
        (cred.refresh_token.as_deref(), cred.expires_at),
        (None, None)
    );
    assert_eq!(
        (cred.account.id.as_str(), cred.account.name.as_str()),
        ("T0FERNHILL", "Fernhill Analytics")
    );
    assert_eq!(cred.enforcement(), "provider");

    let again = c
        .finish(Provider::Slack, &code, &verifier, &audit)
        .unwrap_err();
    assert!(
        matches!(again, Error::Unauthorized(_)),
        "a code works once: {again:?}"
    );

    let account = c.verify(&cred, &audit).unwrap();
    assert_eq!(
        account,
        Account {
            id: "T0FERNHILL".into(),
            name: "Fernhill Analytics".into(),
            user: "ada".into(),
            url: "https://fernhill.slack.com/".into(),
        }
    );

    let q = FetchQuery::default();
    let items = c.fetch(&cred, &q, &audit).unwrap();
    let window_start = add_days(&q.today, -30).unwrap();
    for needle in GENUINE {
        find(&items, needle);
    }
    for needle in NEVER {
        assert_eq!(position(&items, needle), None, "{needle} came back");
    }
    let last_genuine = GENUINE
        .iter()
        .filter_map(|n| position(&items, n))
        .max()
        .unwrap();
    for needle in NOISE {
        if let Some(p) = position(&items, needle) {
            assert!(
                p > last_genuine,
                "noise ({needle}) ranked above an opportunity: {:#?}",
                titles(&items)
            );
        }
    }
    assert!(items.len() <= q.max_items);
    let mut urls: Vec<&str> = items.iter().map(|i| i.url.as_str()).collect();
    urls.sort_unstable();
    urls.dedup();
    assert_eq!(urls.len(), items.len(), "deduplicated by permalink");
    for i in &items {
        assert_eq!((i.source.as_str(), i.kind.as_str()), ("slack", "message"));
        assert!(
            i.url.starts_with("https://fernhill.slack.com/archives/"),
            "{}",
            i.url
        );
        let date = i.date.as_deref().unwrap();
        assert!(
            date > window_start.as_str() && date <= q.today.as_str(),
            "{date}"
        );
        assert!(i.title.contains(" · ") && !i.text.is_empty() && !i.container.is_empty());
        assert!(
            !i.text.contains("<@") && !i.text.contains("&amp;"),
            "{}",
            i.text
        );
    }

    // The best match: a call for speakers that names the person.
    let top = &items[0];
    assert!(
        top.title
            .starts_with("#devrel · Call for speakers We have three speaking slots"),
        "{}",
        top.title
    );
    assert_eq!(
        (top.author.as_str(), top.container.as_str()),
        ("Sam Whitfield", "#devrel")
    );
    assert!(
        top.text
            .contains("@Ada Okafor your streaming-pipeline work")
    );

    let hackathon = find(&items, "Build Week internal hackathon");
    assert!(
        hackathon
            .text
            .contains("sign-up sheet (https://fernhill.example/build-week)")
    );
    assert_eq!(hackathon.container, "#announcements");
    assert!(
        find(&items, "innovation fund")
            .text
            .contains("internal R&D proposals")
    );
    let dm = find(&items, "intro to someone on the Orbit team");
    assert_eq!(
        (dm.container.as_str(), dm.author.as_str()),
        ("Direct message", "Priya Natarajan")
    );
    assert!(dm.title.starts_with("Direct message · Hi Ada!"));
    let group = find(&items, "collaborate on the Helix pilot");
    assert_eq!(group.container, "Group DM");
    assert!(
        group
            .text
            .starts_with("@Ada Okafor @Tom Reyes want to collaborate")
    );
    assert_eq!(
        find(&items, "Fernhill Impact Award").container,
        "#leadership"
    );
    let bot = find(&items, "Product Analyst (Growth)");
    assert_eq!(bot.author, "careers-bot");
    assert!(bot.text.contains("Hiring manager: @Noor Haddad."));
    assert!(find(&items, "panel moderator").url.contains("thread_ts="));
    assert!(
        find(&items, "Secondment opportunity")
            .text
            .contains("@here")
    );
    assert_eq!(
        find(&items, "Hackathon mentors wanted").author,
        "Noor Haddad"
    );

    // What was asked of Slack: a few searches, all inside the window.
    let asked = searches();
    let after = format!(" after:{window_start}");
    assert_eq!(asked.len(), 9, "{asked:#?}");
    assert!(asked.iter().all(|q| q.ends_with(&after)), "{asked:#?}");
    assert_eq!(asked[0], format!("hackathon{after}"));
    assert_eq!(asked[1], format!("\"call for\"{after}"));
    assert!(asked[6].contains(" OR "));
    assert_eq!(asked[8], format!("<@{ADA}>{after}"));

    let events = audit.events();
    let reads = |m: &str| {
        events
            .iter()
            .filter(|e| e.target.ends_with(&format!("/api/{m}")))
            .count()
    };
    assert_eq!(reads("oauth.v2.access"), 2, "two code exchanges");
    assert_eq!(reads("auth.test"), 2, "verify, then fetch");
    assert_eq!(reads("search.messages"), 9);
    assert!((1..=20).contains(&reads("users.info")));
    assert_clean(&audit);
}

#[test]
fn a_wrong_client_secret_is_a_configuration_problem() {
    let _turn = serial();
    let c = Connectors::new(eps(), vec![(Provider::Slack, app("not-the-secret"))]);
    let audit = MemoryAudit::default();
    let (url, verifier) = c.begin(Provider::Slack, "st").unwrap();
    let resp = agent().get(&url).call().unwrap();
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    let code = query_param(location, "code").unwrap();
    let err = c
        .finish(Provider::Slack, &code, &verifier, &audit)
        .unwrap_err();
    assert!(
        matches!(&err, Error::NotConfigured(m) if m.contains("bad_client_secret") && m.contains("SLACK_CLIENT_SECRET")),
        "{err:?}"
    );
    no_secrets(&err, &["not-the-secret", &code]);
    assert_clean(&audit);
}

#[test]
fn a_pasted_user_token_connects_and_fetches() {
    let _turn = serial();
    // Pasting a token needs no OAuth app on the server.
    let c = Connectors::new(eps(), vec![]);
    let audit = MemoryAudit::default();
    let cred = c
        .connect_token(Provider::Slack, &token("  xoxp-mock-ada  "), &audit)
        .unwrap();
    assert_eq!(
        (cred.provider.as_str(), cred.mode.as_str()),
        ("slack", "token")
    );
    assert_eq!(cred.access_token, "xoxp-mock-ada");
    assert_eq!(cred.scopes, s(&["search:read", "users:read"]));
    assert_eq!(
        (cred.refresh_token.as_deref(), cred.expires_at),
        (None, None)
    );
    assert_eq!(
        cred.account,
        Account {
            id: "T0FERNHILL".into(),
            name: "Fernhill Analytics".into(),
            user: "ada".into(),
            url: "https://fernhill.slack.com/".into(),
        }
    );
    let q = FetchQuery {
        max_items: 3,
        ..Default::default()
    };
    let items = c.fetch(&cred, &q, &audit).unwrap();
    assert_eq!(items.len(), 3, "{:#?}", titles(&items));
    assert!(items[0].text.contains("Call for speakers"));
    assert_eq!(c.verify(&cred, &audit).unwrap().user, "ada");

    // A stored token that turns out to belong to another workspace asks for a reconnect.
    let moved = Credential {
        account: Account {
            id: "T0ELSEWHERE".into(),
            ..cred.account.clone()
        },
        ..cred.clone()
    };
    let err = c.verify(&moved, &audit).unwrap_err();
    assert!(
        matches!(&err, Error::Unauthorized(m) if m.contains("different workspace")),
        "{err:?}"
    );
    assert_clean(&audit);
}

#[test]
fn bot_app_rotating_session_and_legacy_tokens_are_refused() {
    let _turn = serial();
    let c = Connectors::new(eps(), vec![]);
    let audit = MemoryAudit::default();
    for (t, says) in [
        ("xoxb-mock-bot", "bot token"),
        ("xapp-1-A0OPP-1234-abcdef", "app-level token"),
        ("xoxe.xoxp-1-mock-abcdef", "rotating"),
        ("xoxe-1-mock-abcdef123", "refresh token"),
        ("xoxc-1234-5678-abcdef", "legacy or browser-session"),
        ("xoxs-1234-5678-abcdef", "legacy or browser-session"),
        (
            "Bearer xoxp-mock-ada",
            "doesn't look like a Slack user token",
        ),
        ("", "doesn't look like a Slack user token"),
    ] {
        let err = c
            .connect_token(Provider::Slack, &token(t), &audit)
            .unwrap_err();
        assert!(
            matches!(&err, Error::Unsupported(m) if m.contains(says)),
            "{t}: {err:?}"
        );
        no_secrets(&err, &[t]);
    }
    assert!(
        audit.events().is_empty(),
        "refused before anything was sent"
    );

    // A legacy all-access token looks like a user token; its scopes give it away.
    let err = c
        .connect_token(Provider::Slack, &token("xoxp-mock-legacy"), &audit)
        .unwrap_err();
    assert!(
        matches!(&err, Error::Unsupported(m) if m.contains("legacy") && m.contains("post")),
        "{err:?}"
    );
    // A user token that can't search is no use.
    let err = c
        .connect_token(Provider::Slack, &token("xoxp-mock-nosearch"), &audit)
        .unwrap_err();
    assert!(
        matches!(&err, Error::Forbidden(m) if m.contains("search:read")),
        "{err:?}"
    );
    assert!(searches().is_empty());
    assert_clean(&audit);
}

#[test]
fn write_scopes_are_refused_at_connect() {
    let _turn = serial();
    let c = connectors();
    let audit = MemoryAudit::default();

    let err = c
        .connect_token(Provider::Slack, &token("xoxp-mock-writer"), &audit)
        .unwrap_err();
    assert_eq!(err, Error::WriteScopes(s(&["chat:write"])));
    assert!(err.needs_reconnect());
    no_secrets(&err, &["xoxp-mock-writer"]);

    let err = c
        .finish(Provider::Slack, "mock-code-writer", "unused", &audit)
        .unwrap_err();
    assert_eq!(err, Error::WriteScopes(s(&["chat:write"])));

    // A bot token with write scopes in the same reply: refused, and never kept.
    let err = c
        .finish(Provider::Slack, "mock-code-bot", "unused", &audit)
        .unwrap_err();
    assert_eq!(err, Error::WriteScopes(s(&["chat:write", "commands"])));
    no_secrets(&err, &["xoxb-mock", "xoxp-mock"]);

    assert!(searches().is_empty(), "nothing was read");
    assert_clean(&audit);
}

#[test]
fn a_write_scope_gained_later_is_refused_on_the_next_call() {
    let _turn = serial();
    let c = connectors();
    let audit = MemoryAudit::default();
    let cred = c
        .connect_token(Provider::Slack, &token("xoxp-mock-ada-gains"), &audit)
        .unwrap();
    assert!(
        !c.fetch(&cred, &FetchQuery::default(), &audit)
            .unwrap()
            .is_empty()
    );

    // The token is upgraded behind our back (the app was given chat:write and re-approved).
    control(
        "scopes",
        json!({"token": "xoxp-mock-ada-gains", "scopes": "search:read,users:read,chat:write"}),
    );
    let searched = searches().len();
    let err = c.verify(&cred, &audit).unwrap_err();
    assert_eq!(err, Error::WriteScopes(s(&["chat:write"])));
    assert!(err.needs_reconnect());
    let err = c.fetch(&cred, &FetchQuery::default(), &audit).unwrap_err();
    assert_eq!(err, Error::WriteScopes(s(&["chat:write"])));
    assert_eq!(searches().len(), searched, "not one search with that token");
    // The stored credential still looks read-only: only the live check on each call catches it.
    assert!(verify_scopes(Provider::Slack, &cred.scopes).is_ok());
    assert_clean(&audit);
}

#[test]
fn a_write_scope_gained_mid_fetch_stops_the_fetch() {
    let _turn = serial();
    let c = connectors();
    let audit = MemoryAudit::default();
    let cred = c
        .connect_token(Provider::Slack, &token("xoxp-mock-ada-midway"), &audit)
        .unwrap();
    // auth.test and two searches see the old scopes; the third search's reply carries chat:write.
    control(
        "scopes",
        json!({"token": "xoxp-mock-ada-midway", "scopes": "search:read,users:read,chat:write", "after": 3}),
    );
    let err = c.fetch(&cred, &FetchQuery::default(), &audit).unwrap_err();
    assert_eq!(
        err,
        Error::WriteScopes(s(&["chat:write"])),
        "what was gathered is thrown away"
    );
    assert_eq!(searches().len(), 3, "nothing was asked after that reply");
    assert!(
        !audit
            .events()
            .iter()
            .any(|e| e.target.ends_with("/users.info")),
        "no names were looked up"
    );
    assert_clean(&audit);
}

#[test]
fn a_reply_without_the_scopes_header_is_not_trusted() {
    let _turn = serial();
    let c = connectors();
    let audit = MemoryAudit::default();
    let err = c
        .connect_token(Provider::Slack, &token("xoxp-mock-noheader"), &audit)
        .unwrap_err();
    assert!(
        matches!(&err, Error::Invalid(m) if m.contains("x-oauth-scopes")),
        "{err:?}"
    );
    assert_clean(&audit);
}

#[test]
fn rotating_tokens_are_refreshed_and_each_refresh_token_works_once() {
    let _turn = serial();
    let c = connectors();
    let audit = MemoryAudit::default();
    let cred = c
        .finish(Provider::Slack, "mock-code-expiring", "unused", &audit)
        .unwrap();
    let now = now_unix();
    assert!(cred.access_token.starts_with("xoxe.xoxp-"));
    assert!(
        cred.refresh_token
            .as_deref()
            .unwrap()
            .starts_with("xoxe-1-")
    );
    assert!(cred.expires_at.unwrap() <= now + 60 && cred.needs_refresh(now));

    let fresh = c
        .refresh_if_needed(&cred, now, &audit)
        .unwrap()
        .expect("due for a refresh");
    assert!(fresh.access_token.starts_with("xoxe.xoxp-"));
    assert_ne!(fresh.access_token, cred.access_token);
    assert_ne!(fresh.refresh_token, cred.refresh_token);
    assert!(fresh.expires_at.unwrap() >= now + 43_000);
    assert_eq!(
        (fresh.provider.as_str(), fresh.mode.as_str()),
        ("slack", "oauth")
    );
    assert_eq!(fresh.scopes, cred.scopes);
    assert_eq!(fresh.account, cred.account);
    assert_eq!(
        c.refresh_if_needed(&fresh, now, &audit).unwrap(),
        None,
        "not due"
    );
    assert_eq!(c.verify(&fresh, &audit).unwrap().id, "T0FERNHILL");

    // Refresh tokens are single-use: the old one now asks for a reconnect.
    let err = c.refresh_if_needed(&cred, now, &audit).unwrap_err();
    assert!(
        matches!(&err, Error::Unauthorized(m) if m.contains("invalid_refresh_token")),
        "{err:?}"
    );
    assert!(err.needs_reconnect());
    no_secrets(
        &err,
        &[&cred.access_token, cred.refresh_token.as_deref().unwrap()],
    );

    // The next refresh uses the newest refresh token.
    let later = fresh.expires_at.unwrap();
    let third = c.refresh_if_needed(&fresh, later, &audit).unwrap().unwrap();
    assert_ne!(third.refresh_token, fresh.refresh_token);

    // A rotating token that isn't due yet isn't refreshed, and a pasted token never is.
    let not_due = c
        .finish(Provider::Slack, "mock-code-rotating", "unused", &audit)
        .unwrap();
    assert_eq!(c.refresh_if_needed(&not_due, now, &audit).unwrap(), None);
    let pasted: Credential = c
        .connect_token(Provider::Slack, &token("xoxp-mock-ada"), &audit)
        .unwrap();
    assert_eq!(
        c.refresh_if_needed(&pasted, now + 10_000_000, &audit)
            .unwrap(),
        None
    );
    assert_clean(&audit);
}

#[test]
fn a_revoked_token_asks_for_a_reconnect() {
    let _turn = serial();
    let c = connectors();
    let audit = MemoryAudit::default();
    let err = c
        .connect_token(Provider::Slack, &token("xoxp-mock-revoked"), &audit)
        .unwrap_err();
    assert!(
        matches!(&err, Error::Unauthorized(m) if m.contains("token_revoked")),
        "{err:?}"
    );

    let cred = c
        .connect_token(Provider::Slack, &token("xoxp-mock-ada-revoke"), &audit)
        .unwrap();
    control("revoke", json!({"token": "xoxp-mock-ada-revoke"}));
    let err = c.verify(&cred, &audit).unwrap_err();
    assert!(
        matches!(&err, Error::Unauthorized(m) if m.contains("token_revoked")),
        "{err:?}"
    );
    assert!(err.needs_reconnect());
    let err = c.fetch(&cred, &FetchQuery::default(), &audit).unwrap_err();
    assert!(matches!(err, Error::Unauthorized(_)), "{err:?}");
    no_secrets(&err, &["xoxp-mock-ada-revoke"]);
    assert!(searches().is_empty());
    assert_clean(&audit);
}

#[test]
fn rate_limits_keep_partial_results_or_say_when_to_retry() {
    let _turn = serial();
    let c = connectors();
    let audit = MemoryAudit::default();
    let q = FetchQuery::default();
    let connect = |t: &str| c.connect_token(Provider::Slack, &token(t), &audit).unwrap();

    // Limited from the first search: nothing to return, so say when to come back (Retry-After).
    let limited = connect("xoxp-mock-ratelimited");
    assert_eq!(
        c.fetch(&limited, &q, &audit).unwrap_err(),
        Error::RateLimited(Some(30))
    );

    // Limited after two searches: what they found is returned.
    let cred = connect("xoxp-mock-ada-rl");
    control(
        "ratelimit",
        json!({"token": "xoxp-mock-ada-rl", "after": 2, "retry_after": 7}),
    );
    let before = searches().len();
    let items = c.fetch(&cred, &q, &audit).unwrap();
    assert_eq!(
        searches().len() - before,
        3,
        "two answered, the third limited"
    );
    assert!(!items.is_empty());
    for i in &items {
        let t = i.text.to_lowercase();
        assert!(
            t.contains("hackathon") || t.contains("call for"),
            "{}",
            i.text
        );
    }
    find(&items, "Build Week internal hackathon");
    find(&items, "Call for speakers");

    // Slack's other way of saying it (HTTP 200, "ratelimited"), with and without results.
    let cred = connect("xoxp-mock-ada-rl-body");
    control(
        "ratelimit",
        json!({"token": "xoxp-mock-ada-rl-body", "after": 0, "style": "body"}),
    );
    assert_eq!(
        c.fetch(&cred, &q, &audit).unwrap_err(),
        Error::RateLimited(None)
    );
    control(
        "ratelimit",
        json!({"token": "xoxp-mock-ada-rl-body", "after": 1, "style": "body"}),
    );
    let items = c.fetch(&cred, &q, &audit).unwrap();
    find(&items, "Build Week internal hackathon");
    assert_eq!(position(&items, "Call for speakers"), None);
    assert_clean(&audit);
}

#[test]
fn fetch_respects_the_window_the_cap_and_the_keywords() {
    let _turn = serial();
    let c = connectors();
    let audit = MemoryAudit::default();
    let cred = c
        .connect_token(Provider::Slack, &token("xoxp-mock-ada-window"), &audit)
        .unwrap();

    let recent = FetchQuery {
        since_days: 3,
        ..Default::default()
    };
    let items = c.fetch(&cred, &recent, &audit).unwrap();
    let first_day = add_days(&today(), -2).unwrap();
    assert!(!items.is_empty());
    assert!(
        items
            .iter()
            .all(|i| i.date.as_deref().unwrap() >= first_day.as_str()),
        "{:#?}",
        items.iter().map(|i| &i.date).collect::<Vec<_>>()
    );
    assert_eq!(position(&items, "innovation fund"), None, "12 days old");

    let two = FetchQuery {
        max_items: 2,
        ..Default::default()
    };
    assert_eq!(c.fetch(&cred, &two, &audit).unwrap().len(), 2);

    let before = searches().len();
    let own = FetchQuery {
        keywords: s(&["Secondment", "in:#leadership"]),
        ..Default::default()
    };
    let items = c.fetch(&cred, &own, &audit).unwrap();
    find(&items, "Secondment opportunity");
    let asked = &searches()[before..];
    let after = add_days(&own.today, -30).unwrap();
    assert_eq!(
        asked,
        &[
            // A keyword of the caller's own comes before the less distinctive known ones, and
            // can't become a search modifier.
            format!("\"in leadership\" after:{after}"),
            format!("secondment after:{after}"),
            format!("<@{ADA}> after:{after}"),
        ]
    );
    assert_clean(&audit);
}

#[test]
fn the_mock_catches_write_attempts() {
    let _turn = serial();
    let base = &mock().base;
    let bearer = "Bearer xoxp-mock-ada";
    let status = |r: Result<ureq::http::Response<ureq::Body>, ureq::Error>| {
        r.map(|r| r.status().as_u16()).unwrap_or(0)
    };
    assert_eq!(
        status(
            agent()
                .post(&format!("{base}/slack/api/search.messages"))
                .header("Authorization", bearer)
                .send("query=x".as_bytes())
        ),
        405
    );
    assert_eq!(
        status(
            agent()
                .get(&format!(
                    "{base}/slack/api/chat.postMessage?channel=C1&text=hi"
                ))
                .header("Authorization", bearer)
                .call()
        ),
        405
    );
    assert_eq!(
        status(
            agent()
                .put(&format!("{base}/slack/api/users.info"))
                .header("Authorization", bearer)
                .send("user=U1".as_bytes())
        ),
        405
    );
    assert_eq!(
        status(
            agent()
                .get(&format!(
                    "{base}/slack/api/conversations.history?channel=C1"
                ))
                .header("Authorization", bearer)
                .call()
        ),
        405
    );
    let seen = violations();
    assert_eq!(seen.as_array().map(Vec::len), Some(4), "{seen}");
    assert!(
        seen.as_array()
            .unwrap()
            .iter()
            .all(|v| v["provider"] == "slack")
    );
    post_json(&format!("{base}/_reset"), &json!({}));
    assert_eq!(violations(), json!([]));
}
