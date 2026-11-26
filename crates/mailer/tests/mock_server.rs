//! The mailer against `tests/mock/server.py`'s Mailgun mock, and the outbox checked with a real
//! mail parser (Python's `email` package).

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use mailer::{Config, Delivery, Error, Mailer, digest, password_reset, welcome};
use model::Opportunity;
use serde_json::{Value, json};

/// The ports this crate's tests may use (the imagery & mail builder owns 9950–9959).
const PORTS: [u16; 3] = [9955, 9956, 9957];

/// Runs server.py and exits when this test process exits (its stdin closes).
const WRAPPER: &str = r#"
import os, runpy, sys, threading
script, port = sys.argv[1], sys.argv[2]
def watch():
    sys.stdin.read()
    os._exit(0)
threading.Thread(target=watch, daemon=True).start()
sys.path.insert(0, os.path.dirname(script))
sys.argv = [script, "--port", port]
runpy.run_path(script, run_name="__main__")
"#;

struct Mock {
    _child: Child,
    base: String,
}

fn http_ok(url: &str) -> bool {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(500)))
        .build()
        .into();
    agent
        .get(url)
        .call()
        .map(|r| r.status().as_u16() == 200)
        .unwrap_or(false)
}

fn start_mock() -> Mock {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/mock/server.py");
    for port in PORTS {
        if TcpListener::bind(("127.0.0.1", port)).is_err() {
            continue;
        }
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(WRAPPER)
            .arg(&script)
            .arg(port.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("python3 runs");
        let base = format!("http://127.0.0.1:{port}");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if http_ok(&format!("{base}/_health")) {
                return Mock {
                    _child: child,
                    base,
                };
            }
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    panic!("couldn't start the mock server on any of {PORTS:?}");
}

fn mock() -> (MutexGuard<'static, ()>, &'static str) {
    static MOCK: OnceLock<Mock> = OnceLock::new();
    static TURN: Mutex<()> = Mutex::new(());
    let guard = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let m = MOCK.get_or_init(start_mock);
    let _ = ureq::post(&format!("{}/_reset", m.base))
        .header("Content-Type", "application/json")
        .send("{}".as_bytes())
        .unwrap();
    (guard, m.base.as_str())
}

fn get_json(url: &str) -> Value {
    let mut r = ureq::get(url).call().unwrap();
    serde_json::from_str(&r.body_mut().read_to_string().unwrap()).unwrap()
}

fn config(bases: Vec<String>, key: &str, domain: &str) -> Config {
    Config {
        api_key: Some(key.into()),
        domain: domain.into(),
        sender: "Prospect <hello@mg.example.com>".into(),
        base_urls: bases,
        outbox_dir: std::env::temp_dir().join("opportunity-mailer-unused"),
        public_url: "https://prospect.example.org".into(),
        app_name: "Prospect".into(),
    }
}

fn items() -> Vec<Opportunity> {
    vec![
        Opportunity {
            url: "https://example.org/bursary".into(),
            title: "Culinary bursary: R35 000 towards tuition".into(),
            organization: "SA Chefs".into(),
            category: "funding".into(),
            why: "You're training as a chef in Cape Town.".into(),
            deadline: Some("2027-01-15".into()),
            value: "R35 000".into(),
            score: 92,
            ..Default::default()
        },
        Opportunity {
            url: "https://example.org/contest".into(),
            title: "Young Chef of the Year".into(),
            category: "visibility".into(),
            score: 80,
            ..Default::default()
        },
    ]
}

#[test]
fn sends_every_kind_through_the_mailgun_mock() {
    let (_turn, base) = mock();
    let m = Mailer::new(config(
        vec![format!("{base}/mailgun")],
        "key-test",
        "mg.example.com",
    ));
    assert!(m.is_live());
    let cfg = m.config().clone();
    let emails = [
        welcome(&cfg, "Acme Cleaning", "gabriel@example.com"),
        digest(
            &cfg,
            "Acme Cleaning",
            "gabriel@example.com",
            &items(),
            12,
            "Cooking",
            "#c2410c",
        ),
        password_reset(
            &cfg,
            "Acme Cleaning",
            "gabriel@example.com",
            "https://prospect.example.org/reset?token=abc",
        ),
    ];
    for e in &emails {
        match m.send(e).unwrap() {
            Delivery::Mailgun { id } => assert!(
                id.starts_with('<') && id.ends_with("@mg.example.com>"),
                "{id}"
            ),
            other => panic!("expected Mailgun, got {other:?}"),
        }
    }
    let sent = get_json(&format!("{base}/_mail"));
    let sent = sent.as_array().unwrap();
    assert_eq!(sent.len(), 3);
    for (msg, e) in sent.iter().zip(&emails) {
        assert_eq!(msg["domain"], "mg.example.com");
        assert_eq!(msg["region"], "us");
        assert_eq!(msg["from"], "Prospect <hello@mg.example.com>");
        assert_eq!(msg["to"], json!(["gabriel@example.com"]));
        assert_eq!(msg["subject"], e.subject.as_str());
        assert_eq!(msg["text"], e.text.as_str());
        assert_eq!(msg["html"], e.html.as_str());
        assert_eq!(msg["tags"], json!([e.tag]));
        assert_eq!(msg["options"]["o:tracking-clicks"], "no");
    }
    assert_eq!(sent[1]["subject"], "2 new opportunities for Acme Cleaning");
    assert_eq!(
        sent[1]["headers"]["List-Unsubscribe"],
        "<https://prospect.example.org/settings>"
    );
    assert_eq!(sent[1]["tags"], json!(["digest"]));
    assert_eq!(sent[2]["tags"], json!(["password-reset"]));
}

#[test]
fn a_wrong_key_is_refused() {
    let (_turn, base) = mock();
    let m = Mailer::new(config(
        vec![format!("{base}/mailgun")],
        "bad-key",
        "mg.example.com",
    ));
    let r = m.send(&welcome(m.config(), "Acme Cleaning", "gabriel@example.com"));
    assert!(
        matches!(&r, Err(Error::Mailgun(msg)) if msg.contains("401") && msg.contains("Forbidden")),
        "{r:?}"
    );
    assert_eq!(get_json(&format!("{base}/_mail")), json!([]));
}

#[test]
fn an_eu_domain_falls_back_to_the_eu_endpoint_and_stays_there() {
    let (_turn, base) = mock();
    let m = Mailer::new(config(
        vec![format!("{base}/mailgun"), format!("{base}/mailgun/eu")],
        "key-test",
        "eu.mg.example.com",
    ));
    let e = welcome(m.config(), "Acme Cleaning", "gabriel@example.com");
    assert!(matches!(m.send(&e), Ok(Delivery::Mailgun { .. })));
    assert!(matches!(m.send(&e), Ok(Delivery::Mailgun { .. })));
    let log = get_json(&format!("{base}/_log"));
    let paths: Vec<&str> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        vec![
            "/mailgun/v3/eu.mg.example.com/messages",
            "/mailgun/eu/v3/eu.mg.example.com/messages",
            "/mailgun/eu/v3/eu.mg.example.com/messages"
        ],
        "US once, then EU, then straight to EU"
    );
    let sent = get_json(&format!("{base}/_mail"));
    assert!(sent.as_array().unwrap().iter().all(|m| m["region"] == "eu"));
}

#[test]
fn mailgun_outage_is_an_error_not_a_panic() {
    let (_turn, base) = mock();
    let _ = ureq::post(&format!("{base}/_fail"))
        .header("Content-Type", "application/json")
        .send(
            json!({ "prefix": "/mailgun/", "status": 503, "times": 10 })
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    let m = Mailer::new(config(
        vec![format!("{base}/mailgun")],
        "key-test",
        "mg.example.com",
    ));
    let r = m.send(&welcome(m.config(), "Acme Cleaning", "gabriel@example.com"));
    assert!(
        matches!(&r, Err(Error::Mailgun(msg)) if msg.contains("503")),
        "{r:?}"
    );
}

/// Parse an .eml with Python's standard `email` package (policy.default, the strict modern
/// parser) and compare what a mail client would show with what we meant to send.
const CHECK_EML: &str = r#"
import sys, email
from email import policy
from email.utils import parsedate_to_datetime
eml, text_path, html_path, subject, to = sys.argv[1:6]
msg = email.message_from_bytes(open(eml, "rb").read(), policy=policy.default)
for part in msg.walk():
    assert not part.defects, part.defects
assert msg["Subject"] == subject, (msg["Subject"], subject)
assert msg["To"].addresses[0].addr_spec == to, msg["To"]
assert msg["From"].addresses[0].display_name == "Prospect", msg["From"]
assert parsedate_to_datetime(msg["Date"]).year >= 2026
assert msg.get_content_type() == "multipart/alternative"
norm = lambda s: s.replace("\r\n", "\n").rstrip("\n")
text = msg.get_body(preferencelist=("plain",)).get_content()
html = msg.get_body(preferencelist=("html",)).get_content()
assert norm(text) == norm(open(text_path, encoding="utf-8").read()), "text differs"
assert norm(html) == norm(open(html_path, encoding="utf-8").read()), "html differs"
print("OK")
"#;

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "opportunity-mailer-it-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn check_with_python(eml: &Path, email: &mailer::Email, dir: &Path) {
    let text_path = dir.join("expected.txt");
    let html_path = dir.join("expected.html");
    std::fs::write(&text_path, &email.text).unwrap();
    std::fs::write(&html_path, &email.html).unwrap();
    let out = Command::new("python3")
        .arg("-I")
        .arg("-c")
        .arg(CHECK_EML)
        .arg(eml)
        .arg(&text_path)
        .arg(&html_path)
        .arg(&email.subject)
        .arg(&email.to)
        .output()
        .expect("python3 runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.trim() == "OK",
        "{}\n{stdout}\n{}",
        eml.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn outbox_emails_parse_cleanly_with_a_real_mail_parser() {
    let dir = temp_dir("outbox");
    let mut cfg = config(vec![], "", "");
    cfg.api_key = None;
    cfg.outbox_dir = dir.join("outbox");
    let m = Mailer::new(cfg);
    assert!(!m.is_live());
    let cfg = m.config().clone();
    let mut unicode = items();
    unicode[0].title = "Bourse « Cuisine & Café » — 3 500 € pour Zoë".into();
    let emails = [
        welcome(&cfg, "Pâtisserie Zoë", "zoe@example.com"),
        digest(
            &cfg,
            "Pâtisserie Zoë",
            "zoe@example.com",
            &unicode,
            9,
            "Pâtisserie",
            "#9d174d",
        ),
        password_reset(
            &cfg,
            "Pâtisserie Zoë",
            "zoe@example.com",
            "https://prospect.example.org/reset?token=x%2By&z=1",
        ),
    ];
    for e in &emails {
        let Delivery::Outbox { path } = m.send(e).unwrap() else {
            panic!("expected the outbox")
        };
        assert!(path.with_extension("html").exists());
        check_with_python(&path, e, &dir);
    }
    // A non-ASCII subject survives the encoded-word round trip too.
    let mut e = welcome(&cfg, "Pâtisserie Zoë", "zoe@example.com");
    e.subject = "Bienvenue chez Prospect, Zoë — c’est parti".into();
    let Delivery::Outbox { path } = m.send(&e).unwrap() else {
        panic!()
    };
    check_with_python(&path, &e, &dir);
    let _ = std::fs::remove_dir_all(&dir);
}
