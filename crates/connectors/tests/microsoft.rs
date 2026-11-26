//! The Microsoft 365 connector end to end, through the public facade only, against the mock
//! Microsoft identity platform and Microsoft Graph (`tests/mock/server.py` →
//! `tests/mock/microsoft_mock.py`).
//!
//! Every test starts its own mock server on a free port in 9980–9989 and stops it when it ends
//! (even on failure). Tests run one at a time: the port range is shared with other suites.
//! Each one finishes by checking the read-only guarantee from both sides: the mock saw no write
//! (`GET /_violations` is `[]`) and the audit holds only GET reads and token-endpoint POSTs.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use connectors::gateway::encode;
use connectors::{
    Connectors, ContextItem, Credential, Endpoints, Error, FetchQuery, MemoryAudit, OAuthApp,
    Provider,
};
use serde_json::{Value, json};

/// The app registration the mock knows.
const CLIENT_ID: &str = "6731de76-14a6-49ae-97bc-6eba6914391e";
const CLIENT_SECRET: &str = "mock-ms-client-secret~not-a-real-one";
const TENANT_ID: &str = "4f0b8a77-2c1e-4d6b-9a53-8e2f7c1d9b60";
const REDIRECT: &str = "http://localhost:8080/api/connectors/microsoft/callback";
const DANA: &str = "dana.okafor@brightwater.example";
const INBOX: &str = "/v1.0/me/mailFolders/inbox/messages";
const CALENDAR: &str = "/v1.0/me/calendarView";

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Runs the shared mock server, and ends it when the test process goes away however that happens
/// (stdin closes): no orphaned servers.
const LAUNCHER: &str = r#"
import os, runpy, sys, threading
mock_dir, port = sys.argv[1], sys.argv[2]
sys.path.insert(0, mock_dir)
sys.argv = [os.path.join(mock_dir, "server.py"), "--port", port]
def serve():
    try:
        runpy.run_path(sys.argv[0], run_name="__main__")
    finally:
        os._exit(1)
threading.Thread(target=serve, daemon=True).start()
sys.stdin.read()
"#;

struct Mock {
    child: Child,
    stdin: Option<ChildStdin>,
    _stdout: BufReader<ChildStdout>,
    base: String,
}

impl Mock {
    fn start() -> Mock {
        let mock_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/mock");
        for port in 9980..=9989u16 {
            if std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
                continue;
            }
            // -B: leave no bytecode caches in the repository.
            let mut child = Command::new("python3")
                .args(["-B", "-c", LAUNCHER, mock_dir, &port.to_string()])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("python3 runs the mock server");
            let stdin = child.stdin.take();
            let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
            let mut line = String::new();
            let _ = stdout.read_line(&mut line);
            if line.contains("listening") {
                return Mock {
                    child,
                    stdin,
                    _stdout: stdout,
                    base: format!("http://127.0.0.1:{port}"),
                };
            }
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("no free port in 9980–9989 for the mock server");
    }

    fn connectors(&self, tenant: &str) -> Connectors {
        self.connectors_with(tenant, CLIENT_SECRET)
    }

    fn connectors_with(&self, tenant: &str, secret: &str) -> Connectors {
        let eps = Endpoints {
            ms_login: format!("{}/mslogin", self.base),
            ms_graph: format!("{}/msgraph", self.base),
            ..Endpoints::default()
        };
        let app = OAuthApp {
            client_id: CLIENT_ID.into(),
            client_secret: secret.into(),
            redirect_uri: REDIRECT.into(),
            tenant: tenant.into(),
        };
        Connectors::new(eps, vec![(Provider::Microsoft, app)])
    }

    fn get_json(&self, path: &str) -> Value {
        let (status, _, body) = http_get(&format!("{}{path}", self.base));
        assert_eq!(status, 200, "{path}: {body}");
        serde_json::from_str(&body).expect("JSON")
    }

    /// The Graph requests the mock served: `{path, query}`.
    fn served(&self) -> Vec<Value> {
        self.get_json("/msgraph/_mock/requests")
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn served_to(&self, path: &str) -> Vec<Value> {
        self.served()
            .into_iter()
            .filter(|r| r["path"] == path)
            .collect()
    }

    /// The next `times` requests under `prefix` fail with `status` (server.py's injection).
    fn fail(&self, prefix: &str, status: u16, times: u32) {
        let body = json!({"prefix": prefix, "status": status, "times": times}).to_string();
        let mut resp = agent()
            .post(&format!("{}/_fail", self.base))
            .header("Content-Type", "application/json")
            .send(body.as_bytes())
            .expect("POST /_fail");
        let status = resp.status().as_u16();
        let reply = resp.body_mut().read_to_string().unwrap_or_default();
        assert_eq!(status, 200, "{reply}");
    }

    /// The read-only guarantee, seen from the provider's side.
    fn assert_no_violations(&self) {
        assert_eq!(self.get_json("/_violations"), json!([]), "write attempts");
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into()
}

/// (status, Location header, body), without following redirects.
fn http_get(url: &str) -> (u16, Option<String>, String) {
    let mut resp = agent().get(url).call().expect("GET");
    let status = resp.status().as_u16();
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    (status, location, body)
}

/// Percent-decoding of a query component (`+` is a space).
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(Ok(v)) = s.get(i + 1..i + 3).map(|h| u8::from_str_radix(h, 16))
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_params(url: &str) -> HashMap<String, String> {
    url.split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

/// The person goes to Microsoft (as `user`; Dana without one), says yes, and comes back: the
/// code from the callback, and the PKCE verifier kept since `begin`.
fn authorize(c: &Connectors, user: Option<&str>) -> (String, String) {
    let state = "st8-4e1f9a";
    let (url, verifier) = c.begin(Provider::Microsoft, state).expect("begin");
    let url = match user {
        Some(u) => format!("{url}&login_hint={}", encode(u)),
        None => url,
    };
    let (status, location, body) = http_get(&url);
    assert_eq!(status, 302, "authorize: {body}");
    let location = location.expect("a redirect back");
    assert!(location.starts_with(&format!("{REDIRECT}?")), "{location}");
    let params = query_params(&location);
    assert_eq!(params.get("state").map(String::as_str), Some(state));
    let code = params.get("code").cloned().expect("a code");
    (code, verifier)
}

fn sign_in(c: &Connectors, user: Option<&str>, audit: &MemoryAudit) -> Result<Credential, Error> {
    let (code, verifier) = authorize(c, user);
    c.finish(Provider::Microsoft, &code, &verifier, audit)
}

fn query(max_items: usize) -> FetchQuery {
    FetchQuery {
        max_items,
        ..FetchQuery::default()
    }
}

fn now() -> i64 {
    model::date::now_unix()
}

/// The read-only guarantee, seen from our side: only GET reads of Graph and POSTs to the token
/// endpoint (plus scope refusals, which send nothing), no query strings, no secrets.
fn assert_audit_read_only(audit: &MemoryAudit, secrets: &[&str]) {
    let events = audit.events();
    assert!(!events.is_empty());
    for e in &events {
        match (e.action.as_str(), e.method.as_str()) {
            ("read", "GET") => assert!(e.target.contains("/msgraph/v1.0/me"), "{e:?}"),
            ("token", "POST") => assert!(e.target.ends_with("/oauth2/v2.0/token"), "{e:?}"),
            ("scope_rejected", "") => assert_eq!(e.status, 0),
            _ => panic!("not a read or a token request: {e:?}"),
        }
        assert!(!e.target.contains('?'), "no query in the audit: {e:?}");
        for s in secrets {
            assert!(
                !e.target.contains(s) && !e.note.contains(s),
                "a secret reached the audit: {e:?}"
            );
        }
    }
}

fn titles(items: &[ContextItem]) -> Vec<&str> {
    items.iter().map(|i| i.title.as_str()).collect()
}

fn has(items: &[ContextItem], needle: &str) -> bool {
    items.iter().any(|i| i.title.contains(needle))
}

#[test]
fn sign_in_verify_and_fetch_opportunities() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start();
    let c = mock.connectors("organizations");
    let audit = MemoryAudit::default();

    // The authorize URL asks for read-only scopes, with PKCE (S256), back to our callback.
    let (url, verifier) = c.begin(Provider::Microsoft, "state-1").unwrap();
    assert!(url.starts_with(&format!(
        "{}/mslogin/organizations/oauth2/v2.0/authorize?",
        mock.base
    )));
    let p = query_params(&url);
    assert_eq!(p["client_id"], CLIENT_ID);
    assert_eq!(p["response_type"], "code");
    assert_eq!(p["response_mode"], "query");
    assert_eq!(p["redirect_uri"], REDIRECT);
    assert_eq!(
        p["scope"],
        "openid profile email offline_access User.Read Mail.Read Calendars.Read"
    );
    assert_eq!(p["code_challenge_method"], "S256");
    assert_eq!(p["code_challenge"].len(), 43);
    assert_eq!(verifier.len(), 43);

    let cred = sign_in(&c, None, &audit).expect("signed in");
    assert_eq!(
        (cred.provider.as_str(), cred.mode.as_str()),
        ("microsoft", "oauth")
    );
    assert!(!cred.access_token.is_empty());
    let refresh_token = cred
        .refresh_token
        .clone()
        .expect("offline_access gives a refresh token");
    let expires_in = cred.expires_at.unwrap() - now();
    assert!((3500..=3600).contains(&expires_in), "{expires_in}");
    assert_eq!(
        cred.scopes,
        vec![
            "https://graph.microsoft.com/Calendars.Read",
            "https://graph.microsoft.com/Mail.Read",
            "https://graph.microsoft.com/User.Read",
            "openid",
            "profile",
            "email"
        ],
        "exactly what Microsoft granted"
    );
    assert_eq!(cred.enforcement(), "provider");
    assert!(
        cred.account.id.starts_with(&format!("{TENANT_ID}:")),
        "{:?}",
        cred.account
    );
    assert_eq!(cred.account.user, DANA);
    assert_eq!(cred.account.name, "brightwater.example");
    assert!(mock.served().is_empty(), "signing in reads no data");

    let account = c.verify(&cred, &audit).expect("verified");
    assert_eq!(account.id, cred.account.id);
    assert_eq!(account.user, DANA);
    let me = mock.served_to("/v1.0/me");
    assert_eq!(
        me[0]["query"]["$select"],
        "id,displayName,mail,userPrincipalName"
    );

    let items = c.fetch(&cred, &query(40), &audit).expect("fetched");
    let shown = titles(&items);
    assert!(!items.is_empty() && items.len() <= 40, "{shown:#?}");
    let today = model::date::today();
    let since = model::date::add_days(&today, -30).unwrap();
    for i in &items {
        assert_eq!(i.source, "microsoft");
        match i.kind.as_str() {
            "email" => assert_eq!(i.container, "Inbox"),
            "event" => assert_eq!(i.container, "Calendar"),
            other => panic!("kind {other}"),
        }
        assert!(
            i.url.starts_with("https://outlook.office365.com/owa/?"),
            "{i:?}"
        );
        assert!(!i.title.is_empty() && !i.author.is_empty(), "{i:?}");
        assert!(!i.text.contains('\n') && !i.text.contains('\r'), "{i:?}");
        assert!(i.text.chars().count() <= 300, "previews only: {i:?}");
        let date = i.date.as_deref().expect("a date");
        if i.kind == "event" {
            assert!(date >= today.as_str(), "upcoming events only: {i:?}");
        } else {
            assert!(date >= since.as_str(), "the last 30 days only: {i:?}");
        }
    }
    // The opportunities Dana would want to hear about.
    for wanted in [
        "Invitation to speak at Data Summit Europe 2027",
        "RFP: Open data platform for the City of Halden",
        "Internal opening: Director of Analytics Partnerships",
        "Partnership proposal: Lumen Health",
        "Would you join our panel on responsible AI?",
        "Panel interview: Director of Analytics Partnerships",
        "Panel: The future of open data (ODI Nordics)",
    ] {
        assert!(has(&items, wanted), "missing {wanted:?} in {shown:#?}");
    }
    // Ranking: every one of them comes before any newsletter or marketing mail that made the cut.
    let position = |needle: &str| shown.iter().position(|t| t.contains(needle));
    let last_opportunity = [
        "Invitation to speak at Data Summit Europe 2027",
        "RFP: Open data platform",
        "Internal opening",
        "Partnership proposal",
        "Would you join our panel",
    ]
    .iter()
    .filter_map(|w| position(w))
    .max()
    .unwrap();
    for (i, t) in shown.iter().enumerate() {
        let bulk = [
            "The Data Weekly",
            "new Director roles",
            "partner programme update",
            "funding opportunities (issue",
        ]
        .iter()
        .any(|b| t.contains(b));
        assert!(
            !bulk || i > last_opportunity,
            "{t:?} outranks an opportunity: {shown:#?}"
        );
    }
    assert!(
        has(&items, "Following up from Tuesday"),
        "the search reaches past the preview (an opening mentioned further down): {shown:#?}"
    );
    // What it must leave out: other folders, old mail, routine and cancelled events, marketing.
    for unwanted in [
        "Investment opportunity",      // Junk Email
        "RE: Invitation to speak",     // Sent Items
        "Exclusive partnership offer", // Deleted Items
        "Spring Analytics Forum",      // 45 days old
        "Team standup",
        "1:1 Dana / Sam",
        "Partner sync",
        "Data mesh in practice", // cancelled
        "speaker briefing",      // beyond 60 days
        "Panel prep",            // yesterday
        "Don't miss this opportunity",
    ] {
        assert!(
            !has(&items, unwanted),
            "{unwanted:?} shouldn't be in {shown:#?}"
        );
    }

    // How it asked: the Inbox only, only the fields it needs, search without filter or order, pages.
    assert!(
        mock.served_to("/v1.0/me/messages").is_empty(),
        "the Inbox only"
    );
    let mail = mock.served_to(INBOX);
    assert_eq!(mail.len(), 2, "two pages of matches: {mail:#?}");
    let first = &mail[0]["query"];
    assert_eq!(
        first["$select"],
        "subject,from,receivedDateTime,bodyPreview,webLink"
    );
    assert_eq!(first["$top"], "50");
    let search = first["$search"].as_str().expect("a KQL search");
    assert!(
        search.starts_with("\"opportunity OR opening OR hiring"),
        "{search}"
    );
    assert!(first.get("$filter").is_none() && first.get("$orderby").is_none());
    assert!(
        mail[1]["query"]["$skiptoken"].is_string(),
        "the next page came from nextLink"
    );
    let cal = mock.served_to(CALENDAR);
    assert_eq!(cal.len(), 2, "two pages of events: {cal:#?}");
    let q = &cal[0]["query"];
    assert_eq!(
        q["$select"],
        "subject,organizer,start,location,bodyPreview,webLink,isCancelled,responseStatus"
    );
    assert_eq!(q["startDateTime"], format!("{today}T00:00:00Z"));
    assert_eq!(q["$orderby"], "start/dateTime");
    assert_eq!(cal[1]["query"]["$skip"], "50");

    let token = cred.access_token.as_str();
    assert_audit_read_only(
        &audit,
        &[token, refresh_token.as_str(), CLIENT_SECRET, "Lisbon"],
    );
    let events = audit.events();
    assert_eq!(events.iter().filter(|e| e.action == "token").count(), 1);
    assert_eq!(
        events.iter().filter(|e| e.action == "read").count(),
        1 + 2 + 2
    );
    mock.assert_no_violations();
}

#[test]
fn the_mock_catches_writes_so_an_empty_violation_list_means_something() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start();
    let graph = format!("{}/msgraph/v1.0/me", mock.base);
    let read = |mut r: ureq::http::Response<ureq::Body>| {
        let status = r.status().as_u16();
        let _ = r.body_mut().read_to_string();
        status
    };
    let sent = agent()
        .post(&format!("{graph}/sendMail"))
        .header("Content-Type", "application/json")
        .send(br#"{"message":{"subject":"hi"}}"#.as_slice())
        .expect("POST");
    assert_eq!(read(sent), 405);
    let deleted = agent()
        .delete(&format!("{graph}/messages"))
        .call()
        .expect("DELETE");
    assert_eq!(read(deleted), 405);
    let smuggled = agent()
        .get(&format!("{graph}/messages"))
        .header("X-HTTP-Method-Override", "DELETE")
        .call()
        .expect("GET");
    assert_eq!(read(smuggled), 405);
    let v = mock.get_json("/_violations");
    let seen: Vec<(String, String)> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            assert_eq!(x["provider"], "microsoft");
            (
                x["method"].as_str().unwrap_or("").to_string(),
                x["path"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            ("POST".to_string(), "/msgraph/v1.0/me/sendMail".to_string()),
            (
                "DELETE".to_string(),
                "/msgraph/v1.0/me/messages".to_string()
            ),
            ("GET".to_string(), "/msgraph/v1.0/me/messages".to_string()),
        ]
    );
}

#[test]
fn a_single_tenant_app_signs_in_against_its_tenant() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start();
    let c = mock.connectors(TENANT_ID);
    let (url, _) = c.begin(Provider::Microsoft, "s").unwrap();
    assert!(url.contains(&format!("/mslogin/{TENANT_ID}/oauth2/v2.0/authorize?")));
    let audit = MemoryAudit::default();
    let cred = sign_in(&c, None, &audit).expect("signed in");
    assert!(
        c.fetch(&cred, &query(10), &audit)
            .is_ok_and(|i| !i.is_empty())
    );
    assert!(audit.events().iter().any(|e| {
        e.target
            .ends_with(&format!("/mslogin/{TENANT_ID}/oauth2/v2.0/token"))
    }));
    assert_audit_read_only(&audit, &[&cred.access_token]);
    mock.assert_no_violations();
}

#[test]
fn pkce_codes_and_the_client_secret_are_enforced() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start();
    let c = mock.connectors("organizations");
    let audit = MemoryAudit::default();

    // A wrong verifier: refused, and the code is spent.
    let (code, verifier) = authorize(&c, None);
    let wrong = "A".repeat(43);
    match c.finish(Provider::Microsoft, &code, &wrong, &audit) {
        Err(Error::Unauthorized(m)) => assert!(m.contains("AADSTS501481"), "{m}"),
        other => panic!("{other:?}"),
    }
    match c.finish(Provider::Microsoft, &code, &verifier, &audit) {
        Err(Error::Unauthorized(m)) => assert!(m.contains("AADSTS54005"), "{m}"),
        other => panic!("{other:?}"),
    }

    // A code works once.
    let (code, verifier) = authorize(&c, None);
    assert!(
        c.finish(Provider::Microsoft, &code, &verifier, &audit)
            .is_ok()
    );
    let again = c.finish(Provider::Microsoft, &code, &verifier, &audit);
    assert!(matches!(&again, Err(Error::Unauthorized(_))), "{again:?}");
    assert!(again.unwrap_err().needs_reconnect());

    // A wrong client secret is the administrator's problem, not the person's.
    let bad = mock.connectors_with("organizations", "wrong-secret");
    match sign_in(&bad, None, &audit) {
        Err(Error::NotConfigured(m)) => {
            assert!(
                m.contains("AADSTS7000215") && m.contains("MS_CLIENT_SECRET"),
                "{m}"
            );
            assert!(!m.contains("wrong-secret"));
        }
        other => panic!("{other:?}"),
    }
    assert!(mock.served().is_empty(), "no data was read");
    assert_audit_read_only(&audit, &[CLIENT_SECRET, "wrong-secret", &verifier]);
    mock.assert_no_violations();
}

#[test]
fn a_grant_with_write_permissions_is_refused_at_sign_in() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start();
    let c = mock.connectors("organizations");
    let audit = MemoryAudit::default();
    // An administrator consented Mail.Send and Mail.ReadWrite for the app: Microsoft puts them in
    // the token although only read scopes were requested.
    let err = sign_in(&c, Some("admin-consented@brightwater.example"), &audit).unwrap_err();
    match &err {
        Error::WriteScopes(s) => assert_eq!(
            s,
            &vec![
                "https://graph.microsoft.com/Mail.ReadWrite".to_string(),
                "https://graph.microsoft.com/Mail.Send".to_string()
            ]
        ),
        other => panic!("{other:?}"),
    }
    assert!(err.needs_reconnect());
    assert!(err.to_string().contains("Mail.Send"));
    assert!(mock.served().is_empty(), "the token was never used");
    let events = audit.events();
    assert_eq!(
        events.iter().map(|e| e.action.as_str()).collect::<Vec<_>>(),
        vec!["token", "scope_rejected"]
    );
    assert_audit_read_only(&audit, &[]);
    mock.assert_no_violations();
}

#[test]
fn a_refresh_that_gains_write_permissions_is_refused() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start();
    let c = mock.connectors("organizations");
    let audit = MemoryAudit::default();
    let cred = sign_in(&c, Some("consent-changed@brightwater.example"), &audit)
        .expect("read-only at first");
    // Later the administrator consents Calendars.ReadWrite: the next refresh carries it.
    let due = cred.expires_at.unwrap();
    match c.refresh_if_needed(&cred, due, &audit) {
        Err(Error::WriteScopes(s)) => {
            assert_eq!(
                s,
                vec!["https://graph.microsoft.com/Calendars.ReadWrite".to_string()]
            )
        }
        other => panic!("{other:?}"),
    }
    // A stored credential that somehow gained a write scope isn't used at all.
    let mut tampered = cred.clone();
    tampered.scopes.push("Mail.Send".into());
    assert!(matches!(
        c.fetch(&tampered, &query(10), &audit),
        Err(Error::WriteScopes(_))
    ));
    assert!(matches!(
        c.verify(&tampered, &audit),
        Err(Error::WriteScopes(_))
    ));
    assert!(
        mock.served().is_empty(),
        "nothing was read with either token"
    );
    assert_eq!(
        audit
            .events()
            .iter()
            .filter(|e| e.action == "scope_rejected")
            .count(),
        3
    );
    assert_audit_read_only(&audit, &[&cred.access_token]);
    mock.assert_no_violations();
}

#[test]
fn an_expiring_token_is_refreshed_and_the_new_refresh_token_kept() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start();
    let c = mock.connectors("organizations");
    let audit = MemoryAudit::default();
    let cred = sign_in(&c, Some("short-lived@brightwater.example"), &audit).expect("signed in");
    let t = now();
    assert!(cred.expires_at.unwrap() - t <= 60);
    assert!(cred.needs_refresh(t));

    let fresh = c
        .refresh_if_needed(&cred, t, &audit)
        .expect("refreshed")
        .expect("it was due");
    assert_ne!(fresh.access_token, cred.access_token);
    assert!(fresh.refresh_token.is_some());
    assert_ne!(
        fresh.refresh_token, cred.refresh_token,
        "the rotated refresh token is kept"
    );
    assert!(fresh.expires_at.unwrap() - t >= 3500);
    assert_eq!(
        fresh.scopes,
        vec![
            "Calendars.Read",
            "Mail.Read",
            "User.Read",
            "openid",
            "profile",
            "email"
        ],
        "re-read from the refresh reply"
    );
    assert_eq!(fresh.account, cred.account);
    assert_eq!(
        (fresh.provider.as_str(), fresh.mode.as_str()),
        ("microsoft", "oauth")
    );

    let tokens_before = audit
        .events()
        .iter()
        .filter(|e| e.action == "token")
        .count();
    assert_eq!(
        c.refresh_if_needed(&fresh, t, &audit),
        Ok(None),
        "not due yet"
    );
    assert_eq!(
        audit
            .events()
            .iter()
            .filter(|e| e.action == "token")
            .count(),
        tokens_before,
        "nothing sent when no refresh is due"
    );
    // The new refresh token works in turn.
    let later = c
        .refresh_if_needed(&fresh, fresh.expires_at.unwrap(), &audit)
        .expect("refreshed again")
        .expect("due");
    assert_ne!(later.access_token, fresh.access_token);

    assert!(
        c.fetch(&fresh, &query(10), &audit)
            .is_ok_and(|i| !i.is_empty())
    );
    let secrets = [
        cred.access_token.as_str(),
        fresh.access_token.as_str(),
        cred.refresh_token.as_deref().unwrap_or("-"),
        fresh.refresh_token.as_deref().unwrap_or("-"),
    ];
    assert_audit_read_only(&audit, &secrets);
    mock.assert_no_violations();
}

#[test]
fn a_revoked_or_rejected_token_means_reconnecting() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start();
    let c = mock.connectors("organizations");
    let audit = MemoryAudit::default();
    let cred = sign_in(&c, Some("revoked@brightwater.example"), &audit).expect("signed in");
    let refresh_token = cred.refresh_token.clone().unwrap();
    let err = c
        .refresh_if_needed(&cred, cred.expires_at.unwrap(), &audit)
        .unwrap_err();
    match &err {
        Error::Unauthorized(m) => {
            assert!(
                m.contains("invalid_grant") && m.contains("AADSTS50173"),
                "{m}"
            );
            assert!(
                !m.contains(&refresh_token) && !m.contains("Trace ID"),
                "{m}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(err.needs_reconnect());

    // Graph rejects an access token: the fetch stops at once, without trying the calendar.
    let mut rejected = cred.clone();
    rejected.access_token = "no-longer-valid".into();
    assert!(matches!(
        c.verify(&rejected, &audit),
        Err(Error::Unauthorized(_))
    ));
    let fetched = c.fetch(&rejected, &query(10), &audit);
    assert!(
        matches!(&fetched, Err(Error::Unauthorized(_))),
        "{fetched:?}"
    );
    assert!(fetched.unwrap_err().needs_reconnect());
    assert_eq!(mock.served_to(INBOX).len(), 1);
    assert!(mock.served_to(CALENDAR).is_empty());
    assert_audit_read_only(&audit, &[&refresh_token, &cred.access_token]);
    mock.assert_no_violations();
}

#[test]
fn throttling_is_waited_out_briefly_or_reported() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start();
    let c = mock.connectors("organizations");
    let audit = MemoryAudit::default();

    // 429 with Retry-After: 1 on the first request to each endpoint: waited out, nothing lost.
    let busy = sign_in(&c, Some("busy@brightwater.example"), &audit).expect("signed in");
    let started = Instant::now();
    let items = c
        .fetch(&busy, &query(25), &audit)
        .expect("fetched after waiting");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "it waited as asked"
    );
    assert!(items.iter().any(|i| i.kind == "email") && items.iter().any(|i| i.kind == "event"));
    assert!(has(
        &items,
        "Invitation to speak at Data Summit Europe 2027"
    ));
    let first_pages = |path: &str| {
        mock.served_to(path)
            .iter()
            .filter(|r| r["query"].get("$skip").is_none() && r["query"].get("$skiptoken").is_none())
            .count()
    };
    assert_eq!(first_pages(INBOX), 2, "retried once");
    assert_eq!(first_pages(CALENDAR), 2, "retried once");
    let throttled = audit.events().iter().filter(|e| e.status == 429).count();
    assert_eq!(throttled, 2);

    // 429 with Retry-After: 30: too long to wait inside a run, so it's reported, and the calendar
    // isn't asked at all (Graph throttles per app and mailbox).
    let mailbox_before = mock.served_to(INBOX).len();
    let calendar_before = mock.served_to(CALENDAR).len();
    let slow = sign_in(&c, Some("throttled@brightwater.example"), &audit).expect("signed in");
    let started = Instant::now();
    assert_eq!(
        c.fetch(&slow, &query(25), &audit),
        Err(Error::RateLimited(Some(30)))
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "didn't sit out a long Retry-After"
    );
    assert_eq!(mock.served_to(INBOX).len(), mailbox_before + 1);
    assert_eq!(mock.served_to(CALENDAR).len(), calendar_before);
    assert_eq!(c.verify(&slow, &audit), Err(Error::RateLimited(Some(30))));
    assert_audit_read_only(&audit, &[&busy.access_token, &slow.access_token]);
    mock.assert_no_violations();
}

#[test]
fn one_failing_source_still_returns_the_other() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start();
    let c = mock.connectors("organizations");
    let audit = MemoryAudit::default();
    let cred = sign_in(&c, None, &audit).expect("signed in");

    // The calendar is unavailable (503, then 503 again on the retry): mail still comes back.
    mock.fail("/msgraph/v1.0/me/calendarView", 503, 2);
    let items = c.fetch(&cred, &query(25), &audit).expect("partial results");
    assert!(!items.is_empty());
    assert!(
        items.iter().all(|i| i.kind == "email"),
        "{:#?}",
        titles(&items)
    );
    assert_eq!(audit.events().iter().filter(|e| e.status == 503).count(), 2);

    // Mail and calendar both refused for this account: that's an error, not an empty list.
    let locked = sign_in(&c, Some("locked@brightwater.example"), &audit).expect("signed in");
    assert!(matches!(
        c.fetch(&locked, &query(25), &audit),
        Err(Error::Forbidden(_))
    ));
    assert_audit_read_only(&audit, &[&cred.access_token, &locked.access_token]);
    mock.assert_no_violations();
}

#[test]
fn a_mailbox_without_search_falls_back_to_a_date_filter() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start();
    let c = mock.connectors("organizations");
    let audit = MemoryAudit::default();
    let cred = sign_in(&c, Some("no-search@brightwater.example"), &audit).expect("signed in");
    let items = c.fetch(&cred, &query(25), &audit).expect("fetched");
    for wanted in [
        "Invitation to speak at Data Summit Europe 2027",
        "RFP: Open data platform for the City of Halden",
        "Internal opening: Director of Analytics Partnerships",
        "Partnership proposal: Lumen Health",
    ] {
        assert!(
            has(&items, wanted),
            "missing {wanted:?} in {:#?}",
            titles(&items)
        );
    }
    assert!(!has(&items, "Don't miss this opportunity"));
    assert!(!has(&items, "Spring Analytics Forum"));
    let mail = mock.served_to(INBOX);
    assert!(mail[0]["query"]["$search"].is_string(), "search first");
    let filtered = &mail[1]["query"];
    assert!(filtered.get("$search").is_none());
    let q = FetchQuery::default();
    let day = model::date::add_days(&q.today, -i64::from(q.since_days)).unwrap();
    assert_eq!(
        filtered["$filter"],
        format!("receivedDateTime ge {day}T00:00:00Z")
    );
    // The mock enforces Graph's rule (the ordered property leads the filter): a page back proves it.
    assert_eq!(filtered["$orderby"], "receivedDateTime desc");
    assert_eq!(mail.len(), 3, "the filtered Inbox in two pages: {mail:#?}");
    assert_eq!(mail[2]["query"]["$skip"], "50");
    assert_audit_read_only(&audit, &[&cred.access_token]);
    mock.assert_no_violations();
}

#[test]
fn a_next_link_off_the_allow_list_is_refused_and_audited() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mock = Mock::start();
    let c = mock.connectors("organizations");
    let audit = MemoryAudit::default();
    let cred = sign_in(&c, Some("evil-link@brightwater.example"), &audit).expect("signed in");
    // The first page of mail points its "next page" at /v1.0/me/sendMail.
    let items = c
        .fetch(&cred, &query(25), &audit)
        .expect("what was read is kept");
    assert!(has(
        &items,
        "Invitation to speak at Data Summit Europe 2027"
    ));
    let events = audit.events();
    let blocked: Vec<_> = events.iter().filter(|e| e.action == "blocked").collect();
    assert_eq!(blocked.len(), 1, "{events:#?}");
    assert!(
        blocked[0].target.ends_with("/msgraph/v1.0/me/sendMail"),
        "{:?}",
        blocked[0]
    );
    assert_eq!(
        (blocked[0].method.as_str(), blocked[0].status),
        ("GET", 0),
        "never sent"
    );
    assert!(
        mock.served()
            .iter()
            .all(|r| !r["path"].as_str().unwrap_or("").contains("sendMail")),
        "the mock never saw it"
    );
    assert_eq!(mock.served_to(INBOX).len(), 1, "paging stopped there");
    assert!(
        !mock.served_to(CALENDAR).is_empty(),
        "the calendar was still read"
    );
    for e in events.iter().filter(|e| e.action != "blocked") {
        assert!(matches!(
            (e.action.as_str(), e.method.as_str()),
            ("read", "GET") | ("token", "POST")
        ));
    }
    mock.assert_no_violations();
}
