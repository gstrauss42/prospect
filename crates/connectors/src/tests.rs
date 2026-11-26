use super::*;
use gateway::{path_matches, split_url};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// (status, extra headers, body)
type Scripted = (u16, Vec<(&'static str, &'static str)>, String);

/// One-connection-per-reply HTTP server: plays (status, extra headers, body) in order.
fn serve(script: Vec<Scripted>) -> (String, mpsc::Receiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (status, extra, body) in script {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let mut headers = Vec::new();
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some((k, v)) = line.split_once(':') {
                    let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
                    if k == "content-length" {
                        len = v.parse().unwrap_or(0);
                    }
                    headers.push((k, v));
                }
            }
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).unwrap();
            let _ = tx.send(Seen {
                method,
                path,
                headers,
                body: String::from_utf8_lossy(&buf).into(),
            });
            let mut resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (k, v) in extra {
                resp.push_str(&format!("{k}: {v}\r\n"));
            }
            resp.push_str("\r\n");
            resp.push_str(&body);
            stream.write_all(resp.as_bytes()).unwrap();
        }
    });
    (format!("http://{addr}"), rx)
}

fn eps_for(base: &str) -> Endpoints {
    Endpoints {
        slack_api: format!("{base}/slack"),
        slack_auth: format!("{base}/slack"),
        ms_login: format!("{base}/mslogin"),
        ms_graph: format!("{base}/msgraph"),
        atlassian_auth: format!("{base}/atlassian-auth"),
        atlassian_api: format!("{base}/atlassian-api"),
        confluence_sites: vec![
            "https://*.atlassian.net".into(),
            format!("{base}/confluence-site"),
        ],
    }
}

fn bearer() -> Auth {
    Auth::Bearer("xoxp-test".into())
}

#[test]
fn a_read_goes_out_as_get_and_is_audited() {
    let (base, rx) = serve(vec![(
        200,
        vec![("x-oauth-scopes", "search:read,users:read")],
        r#"{"ok":true,"url":"https://acme.slack.com/"}"#.into(),
    )]);
    let eps = eps_for(&base);
    let audit = MemoryAudit::default();
    let gw = Gateway::new(
        Provider::Slack,
        &eps,
        gateway::agent(Duration::from_secs(5)),
        &audit,
    );
    let reply = gw
        .get(
            &format!("{}/api/auth.test", eps.slack_api),
            &[],
            &bearer(),
            &[],
        )
        .unwrap();
    assert_eq!(reply.json["ok"], true);
    assert_eq!(
        reply.oauth_scopes.as_deref(),
        Some("search:read,users:read")
    );
    let seen = rx.recv().unwrap();
    assert_eq!(seen.method, "GET");
    assert_eq!(seen.path, "/slack/api/auth.test");
    assert!(
        seen.headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Bearer xoxp-test")
    );
    assert!(seen.body.is_empty());
    let events = audit.events();
    assert_eq!(events.len(), 1);
    assert_eq!(
        (
            events[0].action.as_str(),
            events[0].method.as_str(),
            events[0].status
        ),
        ("read", "GET", 200)
    );
    assert!(!events[0].target.contains('?'));
}

#[test]
fn query_values_are_encoded_and_kept_out_of_the_audit() {
    let (base, rx) = serve(vec![(200, vec![], r#"{"ok":true}"#.into())]);
    let eps = eps_for(&base);
    let audit = MemoryAudit::default();
    let gw = Gateway::new(
        Provider::Slack,
        &eps,
        gateway::agent(Duration::from_secs(5)),
        &audit,
    );
    gw.get(
        &format!("{}/api/search.messages", eps.slack_api),
        &[("query", "call for speakers & panel"), ("count", "20")],
        &bearer(),
        &[],
    )
    .unwrap();
    assert_eq!(
        rx.recv().unwrap().path,
        "/slack/api/search.messages?query=call%20for%20speakers%20%26%20panel&count=20"
    );
    assert!(!audit.events()[0].target.contains("speakers"));
}

#[test]
fn write_methods_on_slack_are_refused_even_as_get() {
    // Slack accepts GET for write methods, so only exact read methods may pass.
    let eps = eps_for("http://127.0.0.1:9");
    let audit = MemoryAudit::default();
    let gw = Gateway::new(
        Provider::Slack,
        &eps,
        gateway::agent(Duration::from_secs(1)),
        &audit,
    );
    for method in [
        "chat.postMessage",
        "chat.delete",
        "conversations.create",
        "files.upload",
        "reactions.add",
        "users.profile.set",
        "auth.revoke",
        "oauth.v2.access",
    ] {
        let r = gw.get(
            &format!("{}/api/{method}", eps.slack_api),
            &[("channel", "C1"), ("text", "hi")],
            &bearer(),
            &[],
        );
        assert!(
            matches!(r, Err(Error::Blocked(_))),
            "{method} must be blocked, got {r:?}"
        );
    }
    assert!(
        audit
            .events()
            .iter()
            .all(|e| e.action == "blocked" && e.status == 0),
        "nothing was sent"
    );
}

#[test]
fn foreign_hosts_paths_and_tricks_are_refused_before_sending() {
    let eps = eps_for("http://127.0.0.1:9");
    let audit = MemoryAudit::default();
    let gw = Gateway::new(
        Provider::Microsoft,
        &eps,
        gateway::agent(Duration::from_secs(1)),
        &audit,
    );
    let graph = &eps.ms_graph;
    let bad = [
        "https://graph.microsoft.com/v1.0/me/messages".to_string(), // the production origin isn't configured here
        "https://evil.example/v1.0/me".to_string(),
        format!("{graph}/v1.0/me/sendMail"),
        format!("{graph}/v1.0/me/messages/../sendMail"),
        format!("{graph}/v1.0/me/./messages"),
        format!("{graph}/v1.0/me/messages%2F..%2FsendMail"),
        format!("{graph}/v1.0//me"),
        format!("{graph}/v1.0/me;x=y"),
        format!("{graph}/v1.0/me#frag"),
        format!("{graph}/v1.0/users/someone/messages"),
        format!("{graph}/v1.0/$batch"),
        "http://user:pass@127.0.0.1:9/msgraph/v1.0/me".to_string(),
        "javascript:alert(1)".to_string(),
    ];
    for url in &bad {
        let r = gw.get(url, &[], &Auth::Bearer("t".into()), &[]);
        assert!(
            matches!(r, Err(Error::Blocked(_))),
            "{url} must be blocked, got {r:?}"
        );
    }
    assert_eq!(audit.events().len(), bad.len());
}

#[test]
fn method_override_and_unknown_headers_are_refused() {
    let eps = eps_for("http://127.0.0.1:9");
    let gw = Gateway::new(
        Provider::Microsoft,
        &eps,
        gateway::agent(Duration::from_secs(1)),
        &NoAudit,
    );
    let url = format!("{}/v1.0/me/messages", eps.ms_graph);
    for h in [
        "X-HTTP-Method-Override",
        "X-HTTP-Method",
        "X-Method-Override",
        "Content-Type",
        "Authorization",
        "Host",
    ] {
        let r = gw.get(&url, &[], &Auth::Bearer("t".into()), &[(h, "DELETE")]);
        assert!(matches!(r, Err(Error::Blocked(_))), "{h} must be blocked");
    }
    let r = gw.get(
        &url,
        &[],
        &Auth::Bearer("t".into()),
        &[("Prefer", "x\r\nX-HTTP-Method-Override: DELETE")],
    );
    assert!(
        matches!(r, Err(Error::Blocked(_))),
        "header injection must be blocked"
    );
}

#[test]
fn redirects_are_never_followed() {
    let (base, rx) = serve(vec![
        (
            302,
            vec![("location", "https://evil.example/steal")],
            "{}".into(),
        ),
        (200, vec![], r#"{"never":"reached"}"#.into()),
    ]);
    let eps = eps_for(&base);
    let gw = Gateway::new(
        Provider::Microsoft,
        &eps,
        gateway::agent(Duration::from_secs(5)),
        &NoAudit,
    );
    let r = gw.get(
        &format!("{}/v1.0/me", eps.ms_graph),
        &[],
        &Auth::Bearer("t".into()),
        &[],
    );
    assert!(matches!(r, Err(Error::Blocked(_))), "{r:?}");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        rx.try_iter().count(),
        1,
        "exactly one request: the redirect was not followed"
    );
}

#[test]
fn next_page_links_are_checked_like_any_url() {
    let (base, _rx) = serve(vec![(200, vec![], r#"{"value":[]}"#.into())]);
    let eps = eps_for(&base);
    let gw = Gateway::new(
        Provider::Microsoft,
        &eps,
        gateway::agent(Duration::from_secs(5)),
        &NoAudit,
    );
    let ok = format!("{}/v1.0/me/messages?$skip=10&$top=10", eps.ms_graph);
    assert!(gw.get(&ok, &[], &Auth::Bearer("t".into()), &[]).is_ok());
    let evil = "https://evil.example/v1.0/me/messages?$skip=10";
    assert!(matches!(
        gw.get(evil, &[], &Auth::Bearer("t".into()), &[]),
        Err(Error::Blocked(_))
    ));
}

#[test]
fn status_codes_map_to_errors() {
    let (base, _rx) = serve(vec![
        (401, vec![], r#"{"error":{"message":"expired"}}"#.into()),
        (403, vec![], r#"{"message":"no"}"#.into()),
        (429, vec![("retry-after", "7")], "{}".into()),
        (500, vec![], "oops".into()),
    ]);
    let eps = eps_for(&base);
    let gw = Gateway::new(
        Provider::Microsoft,
        &eps,
        gateway::agent(Duration::from_secs(5)),
        &NoAudit,
    );
    let url = format!("{}/v1.0/me", eps.ms_graph);
    let a = Auth::Bearer("t".into());
    assert_eq!(
        gw.get(&url, &[], &a, &[]).unwrap_err(),
        Error::Unauthorized("expired".into())
    );
    assert_eq!(
        gw.get(&url, &[], &a, &[]).unwrap_err(),
        Error::Forbidden("no".into())
    );
    assert_eq!(
        gw.get(&url, &[], &a, &[]).unwrap_err(),
        Error::RateLimited(Some(7))
    );
    assert!(matches!(
        gw.get(&url, &[], &a, &[]).unwrap_err(),
        Error::Provider(500, _)
    ));
}

#[test]
fn oversized_replies_are_rejected() {
    let big = format!(
        "{{\"x\":\"{}\"}}",
        "a".repeat(gateway::MAX_BODY as usize + 10)
    );
    let (base, _rx) = serve(vec![(200, vec![], big)]);
    let eps = eps_for(&base);
    let gw = Gateway::new(
        Provider::Microsoft,
        &eps,
        gateway::agent(Duration::from_secs(10)),
        &NoAudit,
    );
    assert!(matches!(
        gw.get(
            &format!("{}/v1.0/me", eps.ms_graph),
            &[],
            &Auth::Bearer("t".into()),
            &[]
        ),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn confluence_sites_must_be_one_atlassian_subdomain_over_https() {
    let eps = Endpoints::default();
    let gw = Gateway::new(
        Provider::Confluence,
        &eps,
        gateway::agent(Duration::from_secs(1)),
        &NoAudit,
    );
    assert!(
        gw.refusal("https://acme.atlassian.net/wiki/rest/api/search?cql=x")
            .is_none()
    );
    assert!(
        gw.refusal("https://api.atlassian.com/ex/confluence/abc-123/wiki/rest/api/search")
            .is_none()
    );
    assert!(
        gw.refusal("http://acme.atlassian.net/wiki/rest/api/search")
            .is_some(),
        "no plain http"
    );
    assert!(
        gw.refusal("https://a.b.atlassian.net/wiki/rest/api/search")
            .is_some(),
        "one label only"
    );
    assert!(
        gw.refusal("https://atlassian.net.evil.com/wiki/rest/api/search")
            .is_some()
    );
    assert!(
        gw.refusal("https://evilatlassian.net/wiki/rest/api/search")
            .is_some()
    );
    assert!(
        gw.refusal("https://acme.atlassian.net/wiki/rest/api/content")
            .is_some(),
        "content (writable) API not allow-listed"
    );
    assert!(
        gw.refusal("https://api.atlassian.com/ex/confluence/abc/wiki/rest/api/content/123")
            .is_some()
    );
}

#[test]
fn oauth_http_only_reaches_token_endpoints() {
    let (base, rx) = serve(vec![(
        200,
        vec![],
        r#"{"ok":true,"authed_user":{}}"#.into(),
    )]);
    let eps = eps_for(&base);
    let audit = MemoryAudit::default();
    let http = OAuthHttp::new(
        Provider::Slack,
        &eps,
        gateway::agent(Duration::from_secs(5)),
        &audit,
    );
    let (status, json) = http
        .post_form(
            &format!("{}/api/oauth.v2.access", eps.slack_api),
            &[("code", "c&d"), ("client_secret", "s3cret")],
            None,
        )
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(json["ok"], true);
    let seen = rx.recv().unwrap();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.body, "code=c%26d&client_secret=s3cret");
    let ev = audit.events();
    assert_eq!(ev[0].action, "token");
    assert!(
        !ev[0].note.contains("s3cret") && !ev[0].target.contains("s3cret"),
        "secrets never reach the audit log"
    );
    for url in [
        format!("{}/api/chat.postMessage", eps.slack_api),
        format!("{}/api/oauth.v2.access?x=1", eps.slack_api),
        "https://evil.example/api/oauth.v2.access".to_string(),
    ] {
        assert!(
            matches!(http.post_form(&url, &[], None), Err(Error::Blocked(_))),
            "{url}"
        );
    }
    let ms = OAuthHttp::new(
        Provider::Microsoft,
        &eps,
        gateway::agent(Duration::from_secs(1)),
        &NoAudit,
    );
    assert!(
        ms.refusal(&format!("{}/organizations/oauth2/v2.0/token", eps.ms_login))
            .is_none()
    );
    assert!(
        ms.refusal(&format!("{}/v1.0/me/sendMail", eps.ms_graph))
            .is_some(),
        "token client can't touch Graph"
    );
}

#[test]
fn scopes_must_be_read_only() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert!(
        verify_scopes(
            Provider::Slack,
            &s(&["search:read", "users:read", "team:read"])
        )
        .is_ok()
    );
    assert_eq!(
        verify_scopes(Provider::Slack, &s(&["search:read", "chat:write"])),
        Err(Error::WriteScopes(s(&["chat:write"])))
    );
    assert!(verify_scopes(Provider::Slack, &s(&["files:write", "admin"])).is_err());
    assert!(
        verify_scopes(Provider::Slack, &s(&["unknown:scope"])).is_err(),
        "unknown scopes are refused too"
    );
    assert!(
        verify_scopes(
            Provider::Microsoft,
            &s(&[
                "https://graph.microsoft.com/Mail.Read",
                "openid",
                "profile",
                "email",
                "offline_access",
                "User.Read"
            ])
        )
        .is_ok()
    );
    assert!(verify_scopes(Provider::Microsoft, &s(&["Mail.Read", "Mail.Send"])).is_err());
    assert!(verify_scopes(Provider::Microsoft, &s(&["Mail.ReadWrite"])).is_err());
    assert!(
        verify_scopes(Provider::Microsoft, &s(&["mail.read", "CALENDARS.READ"])).is_ok(),
        "case-insensitive"
    );
    assert!(
        verify_scopes(
            Provider::Confluence,
            &s(&[
                "read:confluence-content.summary",
                "search:confluence",
                "offline_access"
            ])
        )
        .is_ok()
    );
    assert!(verify_scopes(Provider::Confluence, &s(&["write:confluence-content"])).is_err());
    assert!(
        verify_scopes(Provider::Confluence, &s(&[])).is_ok(),
        "unknown (pasted token) is allowed but marked app-enforced"
    );
    assert_eq!(parse_scopes("a, b  c,d"), s(&["a", "b", "c", "d"]));
}

#[test]
fn every_policy_is_internally_read_only() {
    for p in Provider::ALL {
        let pol = p.policy();
        let allowed: Vec<String> = pol.allowed_scopes.iter().map(|s| s.to_string()).collect();
        assert!(
            verify_scopes(p, &allowed).is_ok(),
            "{p:?}: an allowed scope trips the write deny-list"
        );
        let requested: Vec<String> = pol.requested_scopes.iter().map(|s| s.to_string()).collect();
        assert!(
            verify_scopes(p, &requested).is_ok(),
            "{p:?}: requests a scope it wouldn't accept"
        );
        for rp in pol.read_paths {
            let lower = rp.to_ascii_lowercase();
            for w in [
                "post", "send", "delete", "create", "update", "write", "upload", "set", "add",
                "remove", "revoke", "batch", "content/",
            ] {
                let last = lower.rsplit('/').next().unwrap_or("");
                assert!(
                    !last.contains(w) || last == "search.messages",
                    "{p:?}: read path {rp} looks like a write ({w})"
                );
            }
            assert!(!rp.contains("**"), "{p:?}: keep read paths exact ({rp})");
        }
        assert!(
            pol.token_paths.iter().all(|t| !pol.read_paths.contains(t)),
            "{p:?}: token endpoint readable via the gateway"
        );
    }
}

#[test]
fn path_patterns() {
    assert!(path_matches("/api/auth.test", "/api/auth.test"));
    assert!(!path_matches("/api/auth.test", "/api/auth.test/x"));
    assert!(!path_matches("/api/auth.test", "/api"));
    assert!(path_matches(
        "/ex/confluence/*/wiki/rest/api/search",
        "/ex/confluence/abc/wiki/rest/api/search"
    ));
    assert!(!path_matches(
        "/ex/confluence/*/wiki/rest/api/search",
        "/ex/confluence//wiki/rest/api/search"
    ));
    assert!(!path_matches(
        "/ex/confluence/*/wiki/rest/api/search",
        "/ex/confluence/a/b/wiki/rest/api/search"
    ));
    assert!(path_matches("/files/**", "/files/a/b"));
    assert!(path_matches(
        "/*/oauth2/v2.0/token",
        "/organizations/oauth2/v2.0/token"
    ));
}

#[test]
fn url_splitting() {
    let p = split_url("HTTPS://Graph.Microsoft.com:443/v1.0/me?$top=5").unwrap();
    assert_eq!(
        (
            p.scheme.as_str(),
            p.host.as_str(),
            p.port,
            p.path.as_str(),
            p.query.as_str()
        ),
        ("https", "graph.microsoft.com", None, "/v1.0/me", "$top=5")
    );
    assert_eq!(
        split_url("http://127.0.0.1:9911/slack/api/x").unwrap().port,
        Some(9911)
    );
    assert_eq!(split_url("http://[::1]:8080/x").unwrap().host, "[::1]");
    assert!(split_url("https://a@b.com/x").is_none());
    assert!(split_url("https://b.com:notaport/x").is_none());
    assert!(split_url("ftp://b.com/x").is_none());
    assert!(split_url("https://b.com/x y").is_none());
}

#[test]
fn only_https_or_loopback_bases_are_accepted() {
    assert!(gateway::acceptable_base("https://slack.com"));
    assert!(gateway::acceptable_base("http://127.0.0.1:9911/slack"));
    assert!(gateway::acceptable_base("http://localhost:9911"));
    assert!(!gateway::acceptable_base("http://slack.com"));
    assert!(!gateway::acceptable_base("http://10.0.0.5:80"));
}

#[test]
fn facade_reports_availability_and_needs_oauth_apps() {
    let c = Connectors::new(Endpoints::default(), vec![]);
    let av = c.availability();
    assert_eq!(av.len(), 3);
    assert!(av.iter().all(|a| !a.oauth));
    assert!(
        av.iter()
            .find(|a| a.provider == "microsoft")
            .is_some_and(|a| !a.token)
    );
    assert!(matches!(
        c.begin(Provider::Slack, "state"),
        Err(Error::NotConfigured(_))
    ));
    let app = OAuthApp {
        client_id: "id".into(),
        client_secret: "s".into(),
        redirect_uri: "http://localhost:8080/api/connectors/microsoft/callback".into(),
        tenant: "organizations".into(),
    };
    let c = Connectors::new(Endpoints::default(), vec![(Provider::Microsoft, app)]);
    let (url, verifier) = c.begin(Provider::Microsoft, "st8").unwrap();
    assert!(
        url.starts_with("https://login.microsoftonline.com/organizations/oauth2/v2.0/authorize?")
    );
    assert!(url.contains("state=st8") && url.contains("code_challenge="));
    assert_eq!(verifier.len(), 43);
    let ms_token = TokenInput {
        token: "x".into(),
        ..Default::default()
    };
    assert!(matches!(
        c.connect_token(Provider::Microsoft, &ms_token, &NoAudit),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn facade_refuses_stored_credentials_with_write_scopes() {
    let c = Connectors::new(Endpoints::default(), vec![]);
    let audit = MemoryAudit::default();
    let cred = Credential {
        provider: "slack".into(),
        mode: "token".into(),
        access_token: "xoxp".into(),
        scopes: vec!["search:read".into(), "chat:write".into()],
        ..Default::default()
    };
    assert!(matches!(
        c.fetch(&cred, &FetchQuery::default(), &audit),
        Err(Error::WriteScopes(_))
    ));
    assert!(matches!(
        c.verify(&cred, &audit),
        Err(Error::WriteScopes(_))
    ));
    assert!(audit.events().iter().all(|e| e.action == "scope_rejected"));
    assert_eq!(cred.enforcement(), "provider");
    assert_eq!(Credential::default().enforcement(), "app");
}
