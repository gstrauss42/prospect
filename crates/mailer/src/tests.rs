use super::*;
use model::Opportunity;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

/// A request one of the test servers received.
#[derive(Debug)]
struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    /// The form body as (name, value) pairs, decoded.
    fn form(&self) -> Vec<(String, String)> {
        self.body
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (form_decode(k), form_decode(v))
            })
            .collect()
    }
    fn field(&self, name: &str) -> Option<String> {
        self.form()
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }
}

fn form_decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// An HTTP server that answers every request with `status` and `body` (closing the connection
/// each time) and reports what it saw.
fn serve(replies: Vec<(u16, &'static str)>) -> (String, mpsc::Receiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut replies = replies.into_iter();
        let mut last = (500, "{}");
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let (status, body) = replies.next().unwrap_or(last);
            last = (status, body);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).is_err() {
                continue;
            }
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
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}"), rx)
}

const QUEUED: &str =
    r#"{"id": "<20261126091530.1.ABCD@mg.example.com>", "message": "Queued. Thank you."}"#;
const NOT_FOUND: &str = r#"{"message": "Domain not found: mg.example.com"}"#;
const FORBIDDEN: &str = r#"{"message": "Invalid private key"}"#;

fn live_config(bases: Vec<String>) -> Config {
    Config {
        api_key: Some("key-test".into()),
        domain: "mg.example.com".into(),
        sender: "hello@mg.example.com".into(),
        base_urls: bases,
        outbox_dir: std::env::temp_dir().join("opportunity-mailer-unused"),
        public_url: "https://opportunity.example.org".into(),
        app_name: "Opportunity".into(),
    }
}

fn simple_email() -> Email {
    Email {
        to: "jane@example.com".into(),
        subject: "Hello".into(),
        text: "Hi Jane".into(),
        html: "<p>Hi Jane</p>".into(),
        tag: "welcome".into(),
    }
}

#[test]
fn address_validation() {
    for ok in [
        "jane@example.com",
        "a.b+tag@sub.example.co.za",
        "o'neil@example.ie",
        "x@xn--bcher-kva.example",
        "zoë@exämple.de",
        "1@2.io",
    ] {
        assert!(is_valid_email(ok), "{ok}");
    }
    for bad in [
        "",
        "jane",
        "jane@",
        "@example.com",
        "jane@example",
        "jane@@example.com",
        "jane@example..com",
        ".jane@example.com",
        "jane.@example.com",
        "ja..ne@example.com",
        "jane doe@example.com",
        "jane@exa mple.com",
        "<jane@example.com>",
        "jane@example.com\r\nBcc: x@y.z",
        "jane@-example.com",
        "jane@example.c",
        "jane@example.123",
        "\"jane\"@example.com",
        "jane@example.com,bob@example.com",
    ] {
        assert!(!is_valid_email(bad), "{bad:?}");
    }
    assert!(!is_valid_email(&format!("{}@example.com", "a".repeat(65))));
    assert!(!is_valid_email(&format!("a@{}.com", "b".repeat(250))));
}

#[test]
fn invalid_emails_are_refused_before_anything_is_sent() {
    let m = Mailer::new(live_config(vec!["http://127.0.0.1:9".into()]));
    let bad_to = Email {
        to: "not-an-address".into(),
        ..simple_email()
    };
    assert!(matches!(m.send(&bad_to), Err(Error::Invalid(_))));
    let no_subject = Email {
        subject: " \r\n ".into(),
        ..simple_email()
    };
    assert!(matches!(m.send(&no_subject), Err(Error::Invalid(_))));
    let no_body = Email {
        text: " ".into(),
        html: String::new(),
        ..simple_email()
    };
    assert!(matches!(m.send(&no_body), Err(Error::Invalid(_))));
    // "Name <addr>" recipients are fine.
    let named = Email {
        to: "Jane Doe <jane@example.com>".into(),
        ..simple_email()
    };
    assert_eq!(recipient(&named.to), "jane@example.com");
}

#[test]
fn mailgun_request_is_formed_like_the_api_expects() {
    let (base, rx) = serve(vec![(200, QUEUED)]);
    let m = Mailer::new(live_config(vec![base.clone()]));
    assert!(m.is_live());
    let email = digest(
        &m.config().clone(),
        "Acme Cleaning",
        "jane@example.com",
        &[],
        3,
        "Cooking",
        "#c2410c",
    );
    let d = m.send(&email).unwrap();
    assert_eq!(
        d,
        Delivery::Mailgun {
            id: "<20261126091530.1.ABCD@mg.example.com>".into()
        }
    );
    let seen = rx.recv().unwrap();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path, "/v3/mg.example.com/messages");
    // base64("api:key-test")
    assert_eq!(seen.header("authorization"), Some("Basic YXBpOmtleS10ZXN0"));
    assert!(
        seen.header("content-type")
            .unwrap()
            .starts_with("application/x-www-form-urlencoded")
    );
    assert_eq!(
        seen.field("from").as_deref(),
        Some("Opportunity <hello@mg.example.com>")
    );
    assert_eq!(seen.field("to").as_deref(), Some("jane@example.com"));
    assert_eq!(
        seen.field("subject").as_deref(),
        Some("No new opportunities for Acme Cleaning this time")
    );
    assert_eq!(seen.field("text").as_deref(), Some(email.text.as_str()));
    assert_eq!(seen.field("html").as_deref(), Some(email.html.as_str()));
    assert_eq!(seen.field("o:tag").as_deref(), Some("digest"));
    assert_eq!(seen.field("o:tracking-clicks").as_deref(), Some("no"));
    assert_eq!(
        seen.field("h:List-Unsubscribe").as_deref(),
        Some("<https://opportunity.example.org/settings>")
    );
    // A named sender is used as given.
    let (base2, rx2) = serve(vec![(200, QUEUED)]);
    let mut cfg = live_config(vec![base2]);
    cfg.sender = "Opportunity Digest <digest@mg.example.com>".into();
    Mailer::new(cfg).send(&simple_email()).unwrap();
    let seen = rx2.recv().unwrap();
    assert_eq!(
        seen.field("from").as_deref(),
        Some("Opportunity Digest <digest@mg.example.com>")
    );
    assert_eq!(
        seen.field("h:List-Unsubscribe"),
        None,
        "only digests carry List-Unsubscribe"
    );
}

#[test]
fn falls_back_to_the_eu_region_and_remembers_it() {
    let (us, us_rx) = serve(vec![(404, NOT_FOUND)]);
    let (eu, eu_rx) = serve(vec![(200, QUEUED), (200, QUEUED)]);
    let m = Mailer::new(live_config(vec![us.clone(), eu.clone()]));
    assert_eq!(m.endpoint(), None);
    assert!(matches!(
        m.send(&simple_email()),
        Ok(Delivery::Mailgun { .. })
    ));
    assert_eq!(us_rx.try_iter().count(), 1, "US tried first");
    assert_eq!(eu_rx.try_iter().count(), 1, "then EU");
    assert_eq!(m.endpoint().as_deref(), Some(eu.as_str()));
    // Next time the EU endpoint goes first and the US one isn't bothered.
    assert!(matches!(
        m.send(&simple_email()),
        Ok(Delivery::Mailgun { .. })
    ));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(us_rx.try_iter().count(), 0);
    assert_eq!(eu_rx.try_iter().count(), 1);
}

#[test]
fn shared_across_threads() {
    let (us, _us_rx) = serve(vec![(403, FORBIDDEN)]);
    let (eu, eu_rx) = serve(vec![(200, QUEUED); 9]);
    let m = std::sync::Arc::new(Mailer::new(live_config(vec![us, eu.clone()])));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let m = m.clone();
            std::thread::spawn(move || m.send(&simple_email()))
        })
        .collect();
    for h in handles {
        assert!(matches!(h.join().unwrap(), Ok(Delivery::Mailgun { .. })));
    }
    assert_eq!(m.endpoint().as_deref(), Some(eu.as_str()));
    assert_eq!(eu_rx.try_iter().count(), 4);
}

#[test]
fn errors_name_every_endpoint_tried() {
    let (us, _a) = serve(vec![(401, FORBIDDEN)]);
    let (eu, _b) = serve(vec![(401, FORBIDDEN)]);
    let m = Mailer::new(live_config(vec![us.clone(), eu.clone()]));
    let Err(Error::Mailgun(msg)) = m.send(&simple_email()) else {
        panic!("expected a Mailgun error")
    };
    assert!(
        msg.contains(&format!("{us} answered 401: Invalid private key")),
        "{msg}"
    );
    assert!(msg.contains(&format!("{eu} answered 401")), "{msg}");
    assert_eq!(m.endpoint(), None, "nothing remembered after a failure");
}

#[test]
fn a_bad_request_does_not_try_the_other_region() {
    let (us, _a) = serve(vec![(
        400,
        r#"{"message": "'to' parameter is not a valid address. please check documentation"}"#,
    )]);
    let (eu, eu_rx) = serve(vec![(200, QUEUED)]);
    let m = Mailer::new(live_config(vec![us, eu]));
    let Err(Error::Mailgun(msg)) = m.send(&simple_email()) else {
        panic!("expected a Mailgun error")
    };
    assert!(
        msg.contains("400") && msg.contains("not a valid address"),
        "{msg}"
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(eu_rx.try_iter().count(), 0);
}

#[test]
fn transient_failures_are_retried_once() {
    let (base, rx) = serve(vec![(503, r#"{"message": "busy"}"#), (200, QUEUED)]);
    let m = Mailer::new(live_config(vec![base]));
    assert!(matches!(
        m.send(&simple_email()),
        Ok(Delivery::Mailgun { .. })
    ));
    assert_eq!(rx.try_iter().count(), 2);
    let (base, rx) = serve(vec![(503, "{}"), (503, "{}"), (200, QUEUED)]);
    let m = Mailer::new(live_config(vec![base]));
    assert!(
        matches!(m.send(&simple_email()), Err(Error::Mailgun(_))),
        "only one retry"
    );
    assert_eq!(rx.try_iter().count(), 2);
}

#[test]
fn unreachable_mailgun_is_a_clear_error() {
    let m = Mailer::new(live_config(vec!["http://127.0.0.1:9".into()]));
    let Err(Error::Mailgun(msg)) = m.send(&simple_email()) else {
        panic!("expected a Mailgun error")
    };
    assert!(msg.contains("couldn't reach Mailgun"), "{msg}");
}

fn outbox_config(dir: &Path) -> Config {
    Config {
        api_key: None,
        domain: String::new(),
        sender: String::new(),
        base_urls: vec![MAILGUN_US.into(), MAILGUN_EU.into()],
        outbox_dir: dir.to_path_buf(),
        public_url: "https://opportunity.example.org".into(),
        app_name: "Opportunity".into(),
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "opportunity-mailer-{name}-{}-{}",
        std::process::id(),
        mime::token()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Decode a quoted-printable body.
fn qp_decode(s: &str) -> String {
    let s = s.replace("=\r\n", "");
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'=' {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

#[test]
fn outbox_writes_a_real_eml_and_an_html_preview() {
    let dir = temp_dir("outbox");
    let m = Mailer::new(outbox_config(&dir));
    assert!(!m.is_live());
    let items = vec![Opportunity {
        url: "https://example.org/a".into(),
        title: "Café bursary — R35 000".into(),
        category: "funding".into(),
        score: 90,
        ..Default::default()
    }];
    let email = digest(
        m.config(),
        "Acme Cleaning",
        "zoe@example.com",
        &items,
        4,
        "Cooking",
        "#c2410c",
    );
    let Delivery::Outbox { path } = m.send(&email).unwrap() else {
        panic!("expected the outbox")
    };
    assert_eq!(path.extension().unwrap(), "eml");
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    assert!(name.ends_with("-digest.eml"), "{name}");
    assert!(
        name.starts_with(&model::date::today().replace('-', "")),
        "{name}"
    );
    let html_path = path.with_extension("html");
    assert_eq!(std::fs::read_to_string(&html_path).unwrap(), email.html);

    let eml = std::fs::read_to_string(&path).unwrap();
    let (head, body) = eml.split_once("\r\n\r\n").unwrap();
    assert!(
        head.contains("\r\nFrom: Opportunity <no-reply@opportunity.example.org>\r\n"),
        "{head}"
    );
    assert!(head.contains("\r\nTo: zoe@example.com\r\n"));
    assert!(head.contains("Subject: A new opportunity for Acme Cleaning\r\n"));
    assert!(head.contains("MIME-Version: 1.0"));
    assert!(head.contains("X-Mailgun-Tag: digest"));
    assert!(head.contains("List-Unsubscribe: <https://opportunity.example.org/settings>"));
    assert!(head.contains("Message-ID: <") && head.contains("@opportunity.example.org>"));
    assert!(head.starts_with("Date: "));
    assert!(head.contains("multipart/alternative"));
    let boundary = head
        .split("boundary=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let parts: Vec<&str> = body.split(&format!("--{boundary}")).collect();
    assert_eq!(parts.len(), 4, "preamble, text, html, epilogue");
    let decode = |p: &str| {
        qp_decode(p.split_once("\r\n\r\n").unwrap().1.trim_end_matches("\r\n"))
            .replace("\r\n", "\n")
    };
    assert!(parts[1].contains("Content-Type: text/plain; charset=utf-8"));
    assert_eq!(decode(parts[1]), email.text.trim_end_matches('\n'));
    assert!(parts[2].contains("Content-Type: text/html; charset=utf-8"));
    assert_eq!(decode(parts[2]), email.html.trim_end_matches('\n'));
    assert!(eml.split("\r\n").all(|l| l.len() <= 998));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn outbox_names_sort_by_time_and_never_collide() {
    let dir = temp_dir("names");
    let m = std::sync::Arc::new(Mailer::new(outbox_config(&dir)));
    let mut paths: Vec<PathBuf> = Vec::new();
    for _ in 0..5 {
        let Delivery::Outbox { path } = m.send(&simple_email()).unwrap() else {
            panic!()
        };
        paths.push(path);
    }
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(sorted, paths, "names sort in sending order");
    // Many threads at once: every email gets its own file.
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let m = m.clone();
            std::thread::spawn(move || {
                for _ in 0..5 {
                    m.send(&simple_email()).unwrap();
                }
                5
            })
        })
        .collect();
    let sent: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let emls = std::fs::read_dir(&dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "eml")
        })
        .count();
    assert_eq!(emls, sent + 5);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn outbox_errors_are_reported() {
    let file = std::env::temp_dir().join(format!("opportunity-mailer-file-{}", mime::token()));
    std::fs::write(&file, "not a directory").unwrap();
    let m = Mailer::new(outbox_config(&file.join("outbox")));
    assert!(matches!(m.send(&simple_email()), Err(Error::Io(_))));
    let _ = std::fs::remove_file(&file);
}

#[test]
fn helpers() {
    assert_eq!(
        compact_timestamp(Duration::from_millis(1_795_682_592_123)),
        "20261126T084312.123Z"
    );
    assert_eq!(sanitize_tag(" Password-Reset\r\n"), "password-reset");
    assert_eq!(path_escape("mg.example.com"), "mg.example.com");
    assert_eq!(path_escape("a/b c"), "a%2Fb%20c");
    assert_eq!(mailgun_message(r#"{"message": "Forbidden"}"#), "Forbidden");
    assert_eq!(
        mailgun_message("<html>Bad gateway</html>"),
        "<html>Bad gateway</html>"
    );
    assert_eq!(mailgun_message(""), "no details");
    let cfg = outbox_config(Path::new("/tmp"));
    assert_eq!(
        cfg.settings_url(),
        "https://opportunity.example.org/settings"
    );
}
