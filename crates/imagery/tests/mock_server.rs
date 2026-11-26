//! `find_background` end to end against `tests/mock/server.py` (Openverse, Commons, image bytes
//! and DeepSeek's chat endpoint, all mocked): the happy path, nothing suitable, a source outage,
//! a vision failure, a vision rejection, and the second search wave.

use std::io::Read;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use imagery::{Config, Error, Provider, find_background, find_background_report, sniff};
use model::ThemeHint;
use serde_json::{Value, json};

/// The ports this crate's tests may use (the imagery & mail builder owns 9950–9959).
const PORTS: [u16; 3] = [9950, 9951, 9952];

/// Runs server.py and exits when this test process exits (its stdin closes), so a crashed or
/// killed test never leaves a mock behind.
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
            // Mock bugs surface as HTTP 500s with the error in the body; keep test output clean.
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

/// One mock for the whole test binary; tests take turns (they reset and inject failures).
fn mock() -> (MutexGuard<'static, ()>, &'static str) {
    static MOCK: OnceLock<Mock> = OnceLock::new();
    static TURN: Mutex<()> = Mutex::new(());
    let guard = TURN.lock().unwrap_or_else(|e| e.into_inner());
    let m = MOCK.get_or_init(start_mock);
    post(&m.base, "/_reset", json!({}));
    (guard, m.base.as_str())
}

fn post(base: &str, path: &str, body: Value) -> Value {
    let mut r = ureq::post(&format!("{base}{path}"))
        .header("Content-Type", "application/json")
        .send(body.to_string().as_bytes())
        .unwrap();
    serde_json::from_str(&r.body_mut().read_to_string().unwrap()).unwrap()
}

fn log(base: &str) -> Vec<Value> {
    let mut r = ureq::get(&format!("{base}/_log")).call().unwrap();
    let mut s = String::new();
    r.body_mut().as_reader().read_to_string(&mut s).unwrap();
    serde_json::from_str::<Vec<Value>>(&s).unwrap()
}

fn count(log: &[Value], prefix: &str) -> usize {
    log.iter()
        .filter(|e| e["path"].as_str().unwrap_or("").starts_with(prefix))
        .count()
}

fn config(base: &str) -> Config {
    Config {
        openverse_url: format!("{base}/openverse/v1"),
        commons_api_url: format!("{base}/commons/w/api.php"),
        timeout: Duration::from_secs(10),
        ..Config::default()
    }
}

fn ai(base: &str) -> deepseek::Client {
    let mut c = deepseek::Config::new("mock");
    c.base_url = base.to_string();
    c.max_retries = 1;
    c.retry_base = Duration::from_millis(10);
    c.chat_timeout = Duration::from_secs(20);
    deepseek::Client::new(c)
}

fn hint(label: &str, queries: &[&str]) -> ThemeHint {
    ThemeHint {
        label: label.into(),
        image_queries: queries.iter().map(|q| q.to_string()).collect(),
        mood: "Warm, inviting light".into(),
        accent: "#c2410c".into(),
    }
}

#[test]
fn finds_judges_and_downloads_a_background() {
    let (_turn, base) = mock();
    let ai = ai(base);
    let (result, report) = find_background_report(
        &config(base),
        &ai,
        &hint("Cooking", &["chef plating food", "kitchen herbs"]),
        true,
    );
    let bg = result.unwrap_or_else(|e| panic!("{e}: {}", report.summary()));
    eprintln!("{}", report.summary());
    for p in &report.previews {
        eprintln!(
            "  preview {:?}/{} {}×{} score {:.2} {}",
            p.candidate.provider,
            p.candidate.source,
            p.width,
            p.height,
            p.candidate.score,
            p.candidate.title
        );
    }
    for f in &report.failed_downloads {
        eprintln!("  failed {f}");
    }

    // The bytes are a real image whose description matches them.
    let (mime, w, h) = sniff(&bg.bytes).expect("valid image");
    assert_eq!(
        (bg.image.mime.as_str(), bg.image.width, bg.image.height),
        (mime, w, h)
    );
    assert!(imagery::is_complete(&bg.bytes));
    assert!(w as f64 / h as f64 > 1.3);

    // Four searches (two photo searches on two sources) were enough; no second wave.
    let log = log(base);
    assert_eq!(report.search_requests, 4, "{}", report.summary());
    assert_eq!(
        log.iter()
            .filter(|e| e["path"] == "/openverse/v1/images/")
            .count(),
        2
    );
    assert_eq!(
        log.iter()
            .filter(|e| e["path"] == "/commons/w/api.php")
            .count(),
        2
    );
    assert!(
        report.found > report.suitable && report.suitable >= 10,
        "{}",
        report.summary()
    );

    // Junk was filtered or survived: broken previews (an HTML page served as a JPEG, a 404, a
    // portrait image behind landscape metadata) are skipped, never judged.
    assert!(!report.failed_downloads.is_empty(), "{}", report.summary());
    assert!(
        report.previews.len() >= 4 && report.previews.len() <= 8,
        "{}",
        report.previews.len()
    );
    for p in &report.previews {
        assert_eq!(sniff(&p.bytes), Some((p.mime, p.width, p.height)));
        assert!(p.width as f64 / p.height as f64 >= 1.2);
        assert!(p.candidate.license_ok && !p.candidate.mature && p.candidate.width >= 1600);
    }
    // Both sources were judged.
    assert!(
        report
            .previews
            .iter()
            .any(|p| p.candidate.provider == Provider::Openverse)
    );
    assert!(
        report
            .previews
            .iter()
            .any(|p| p.candidate.provider == Provider::Commons)
    );

    // One vision call, and its pick (the mock always picks photo 2) won.
    let chats: Vec<&Value> = log
        .iter()
        .filter(|e| e["path"] == "/v1/chat/completions")
        .collect();
    assert_eq!(chats.len(), 1);
    assert_eq!(chats[0]["stage"], "curate");
    assert_eq!(report.decision, "vision");
    assert_eq!(report.verdicts.len(), 1);
    assert_eq!(report.verdicts[0].best, Some(1));
    let winner = report.winner.clone().unwrap();
    assert_eq!(winner, report.previews[1].candidate);
    assert_eq!(ai.usage().chat_calls, 1);

    // The large version replaced the preview when the source has one.
    let preview = &report.previews[1];
    if report.used_full_size {
        assert!(
            bg.image.width > preview.width,
            "{} vs {}",
            bg.image.width,
            preview.width
        );
        assert_eq!(bg.image.width, 1920);
    } else {
        assert_eq!(bg.image.width, preview.width);
    }

    // Credit and source link.
    let place = if winner.provider == Provider::Commons || winner.source == "wikimedia" {
        "Wikimedia Commons"
    } else {
        "via Openverse"
    };
    assert!(
        bg.image.credit.starts_with("Photo: "),
        "{}",
        bg.image.credit
    );
    assert!(bg.image.credit.ends_with(place), "{}", bg.image.credit);
    assert!(
        bg.image.credit.contains(&winner.license),
        "{}",
        bg.image.credit
    );
    assert!(
        !bg.image.credit.contains('<'),
        "HTML stripped: {}",
        bg.image.credit
    );
    assert!(
        bg.image.source_url.starts_with("https://"),
        "{}",
        bg.image.source_url
    );
}

#[test]
fn nothing_suitable_makes_no_vision_call() {
    let (_turn, base) = mock();
    let ai = ai(base);
    let (r, report) = find_background_report(
        &config(base),
        &ai,
        &hint("Nothing", &["nothing here", "nothing there"]),
        false,
    );
    assert_eq!(r, Err(Error::NothingSuitable));
    assert_eq!(report.found, 0);
    // Too little came back, so a smaller second wave was tried before giving up.
    assert!(
        report.search_requests > 4 && report.search_requests <= 8,
        "{}",
        report.search_requests
    );
    let log = log(base);
    assert_eq!(count(&log, "/v1/chat/completions"), 0);
    assert_eq!(count(&log, "/img/"), 0);
}

#[test]
fn one_source_down_uses_the_other() {
    let (_turn, base) = mock();
    post(
        base,
        "/_fail",
        json!({ "prefix": "/openverse/", "status": 503, "times": 100 }),
    );
    let (r, report) = find_background_report(
        &config(base),
        &ai(base),
        &hint("Jazz", &["saxophone close up", "jazz club stage"]),
        false,
    );
    let bg = r.unwrap_or_else(|e| panic!("{e}: {}", report.summary()));
    assert!(
        report
            .source_errors
            .iter()
            .all(|e| e.starts_with("Openverse"))
            && report.source_errors.len() == 2,
        "{:?}",
        report.source_errors
    );
    assert!(
        report
            .previews
            .iter()
            .all(|p| p.candidate.provider == Provider::Commons)
    );
    assert_eq!(report.winner.as_ref().unwrap().provider, Provider::Commons);
    assert!(
        bg.image.credit.ends_with("Wikimedia Commons"),
        "{}",
        bg.image.credit
    );
    assert!(report.used_full_size, "Commons serves a 1920 px thumbnail");
    assert_eq!(bg.image.width, 1920);

    // And the other way round.
    post(base, "/_reset", json!({}));
    post(
        base,
        "/_fail",
        json!({ "prefix": "/commons/", "status": 0, "times": 100 }),
    );
    let (r, report) = find_background_report(
        &config(base),
        &ai(base),
        &hint("Jazz", &["saxophone close up", "jazz club stage"]),
        false,
    );
    assert!(r.is_ok(), "{}", report.summary());
    assert_eq!(report.winner.unwrap().provider, Provider::Openverse);
}

#[test]
fn both_sources_down_is_a_network_error() {
    let (_turn, base) = mock();
    post(
        base,
        "/_fail",
        json!({ "prefix": "/openverse/", "status": 502, "times": 100 }),
    );
    post(
        base,
        "/_fail",
        json!({ "prefix": "/commons/", "status": 500, "times": 100 }),
    );
    let r = find_background(&config(base), &ai(base), &hint("Jazz", &["saxophone"]));
    assert!(
        matches!(r, Err(Error::Network(ref m)) if m.contains("Openverse") && m.contains("Commons")),
        "{r:?}"
    );
}

#[test]
fn vision_failure_falls_back_to_the_best_ranked_photo() {
    let (_turn, base) = mock();
    post(
        base,
        "/_fail",
        json!({ "prefix": "/v1/chat/completions", "status": 500, "times": 100 }),
    );
    let (r, report) = find_background_report(
        &config(base),
        &ai(base),
        &hint("Cooking", &["chef plating food", "kitchen herbs"]),
        false,
    );
    let bg = r.unwrap_or_else(|e| panic!("{e}: {}", report.summary()));
    assert_eq!(report.decision, "ranking");
    assert!(report.vision_error.is_some());
    assert_eq!(report.winner.as_ref(), Some(&report.previews[0].candidate));
    assert!(sniff(&bg.bytes).is_some());
}

#[test]
fn vision_rejecting_everything_tries_one_more_batch_then_gives_up() {
    let (_turn, base) = mock();
    let ai = ai(base);
    let (r, report) = find_background_report(
        &config(base),
        &ai,
        &hint("Reject", &["chef plating food", "kitchen herbs"]),
        false,
    );
    assert_eq!(r, Err(Error::NothingSuitable));
    assert_eq!(report.verdicts.len(), 2, "{}", report.summary());
    assert!(report.verdicts.iter().all(|v| v.best.is_none()));
    assert_eq!(count(&log(base), "/v1/chat/completions"), 2);
    // No photo judged twice.
    let mut urls: Vec<&str> = report.previews.iter().map(|p| p.url.as_str()).collect();
    let n = urls.len();
    urls.sort();
    urls.dedup();
    assert_eq!(urls.len(), n);
}

#[test]
fn nothing_beautiful_still_uses_the_best_decent_photo_instead_of_none() {
    let (_turn, base) = mock();
    let (r, report) = find_background_report(
        &config(base),
        &ai(base),
        &hint("Lacklustre", &["chef plating food", "kitchen herbs"]),
        false,
    );
    let bg = r.unwrap_or_else(|e| panic!("{e}: {}", report.summary()));
    assert_eq!(report.decision, "best available", "{}", report.summary());
    assert!(report.verdicts.iter().all(|v| v.best.is_none()));
    // The highest-scoring photo of the first batch (the 5), not just the first one.
    let first_batch = report.verdicts[0].scores.len();
    assert_eq!(
        report.winner.as_ref(),
        Some(&report.previews[first_batch / 2].candidate)
    );
    assert!(sniff(&bg.bytes).is_some());
}

#[test]
fn too_few_results_trigger_a_second_wave() {
    let (_turn, base) = mock();
    let (r, report) = find_background_report(
        &config(base),
        &ai(base),
        &hint("Sparse", &["sparse one", "sparse two", "sparse three"]),
        false,
    );
    assert!(r.is_ok(), "{}", report.summary());
    assert!(report.search_requests > 4, "{}", report.summary());
    assert!(report.search_requests <= 9, "{}", report.summary());
}
