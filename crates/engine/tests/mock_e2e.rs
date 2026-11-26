//! End-to-end tests of discover, research and theme_hint against the mock upstream server
//! (tests/mock/server.py). No network beyond localhost, no cost.
//!
//! Each test starts its own mock on a free port in 9920–9929, so failure injection in one test
//! never leaks into another; the port goes back to the pool when the test's mock is dropped.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use engine::{BusinessInput, Ctx, DiscoverInput, DiscoverOutput, Error, Limits, ResearchInput};
use model::{ContextItem, KnownOpportunity, Profile, Progress, Steering};
use serde_json::{Value, json};

const TODAY: &str = "2026-11-26";
const FIRST_PORT: u16 = 9920;
static PORTS_IN_USE: Mutex<[bool; 10]> = Mutex::new([false; 10]);

struct Mock {
    child: Child,
    port: u16,
}

impl Mock {
    fn start() -> Mock {
        let port = take_port();
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/mock/server.py");
        let child = Command::new("python3")
            .arg(script)
            .arg("--port")
            .arg(port.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the mock server (python3)");
        let mock = Mock { child, port };
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if mock
                .try_http("GET", "/_health", "")
                .is_some_and(|b| b.contains("true"))
            {
                return mock;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the mock server didn't come up on port {port}");
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn try_http(&self, method: &str, path: &str, body: &str) -> Option<String> {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).ok()?;
        let mut out = String::new();
        s.read_to_string(&mut out).ok()?;
        out.split_once("\r\n\r\n").map(|(_, b)| b.to_string())
    }

    fn fail(&self, prefix: &str, status: u16, times: u32) {
        let body = json!({ "prefix": prefix, "status": status, "times": times }).to_string();
        self.try_http("POST", "/_fail", &body)
            .expect("inject a failure");
    }

    /// Every request the mock saw: [{method, path, stage, body}].
    fn log(&self) -> Vec<Value> {
        let body = self
            .try_http("GET", "/_log", "")
            .expect("read the mock log");
        serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    }

    fn client(&self, max_retries: u32) -> deepseek::Client {
        let mut cfg = deepseek::Config::new("mock-key");
        cfg.base_url = self.base();
        cfg.retry_base = Duration::from_millis(5);
        cfg.max_retries = max_retries;
        cfg.chat_timeout = Duration::from_secs(30);
        cfg.tool_timeout = Duration::from_secs(15);
        deepseek::Client::new(cfg)
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let mut used = PORTS_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
        used[(self.port - FIRST_PORT) as usize] = false;
    }
}

/// A port from 9920–9929 that nobody is listening on, waiting for one to come free if needed.
fn take_port() -> u16 {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        {
            let mut used = PORTS_IN_USE.lock().unwrap_or_else(|e| e.into_inner());
            for (i, slot) in used.iter_mut().enumerate() {
                let port = FIRST_PORT + i as u16;
                if !*slot && TcpListener::bind(("127.0.0.1", port)).is_ok() {
                    *slot = true;
                    return port;
                }
            }
        }
        assert!(Instant::now() < deadline, "no free port in 9920-9929");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn entries<'a>(log: &'a [Value], path: &str) -> Vec<&'a Value> {
    log.iter().filter(|e| e["path"] == path).collect()
}

fn search_queries(log: &[Value]) -> Vec<String> {
    entries(log, "/v1/tools/search")
        .iter()
        .filter_map(|e| serde_json::from_str::<Value>(e["body"].as_str().unwrap_or("")).ok())
        .filter_map(|b| b["query"].as_str().map(str::to_string))
        .collect()
}

fn fetched_urls(log: &[Value]) -> Vec<String> {
    entries(log, "/v1/tools/fetch")
        .iter()
        .filter_map(|e| serde_json::from_str::<Value>(e["body"].as_str().unwrap_or("")).ok())
        .filter_map(|b| b["url"].as_str().map(str::to_string))
        .collect()
}

fn chat_stages(log: &[Value]) -> Vec<String> {
    entries(log, "/v1/chat/completions")
        .iter()
        .map(|e| e["stage"].as_str().unwrap_or("").to_string())
        .collect()
}

/// The mock's rule for closed pages (see tests/mock/deepseek_mock.py `is_closed_url`).
fn mock_says_closed(url: &str) -> bool {
    matches!(url.trim_end_matches('/').chars().last(), Some('0'..='7'))
}

fn business_profile() -> Profile {
    Profile {
        headline: "Family-run steel fabrication company with 45 staff".into(),
        industry: "Steel fabrication".into(),
        stage: "established".into(),
        organization: "Khumalo Precision".into(),
        website: "https://www.khumaloprecision.example".into(),
        location: "Durban, South Africa".into(),
        reach: "national".into(),
        seeking: vec!["sales".into(), "funding".into(), "partners".into()],
        interests: vec!["mining".into(), "ports".into()],
        skills: vec![
            "steel fabrication".into(),
            "industrial maintenance".into(),
            "CNC machining".into(),
        ],
        ..Default::default()
    }
}

/// A cleaning company in Cape Town (the tests' second business).
fn chef_profile() -> Profile {
    Profile {
        headline: "Commercial cleaning for offices and schools".into(),
        industry: "Commercial cleaning".into(),
        stage: "established".into(),
        organization: "Sparkle Office Care".into(),
        location: "Cape Town, South Africa".into(),
        seeking: vec!["sales".into(), "funding".into(), "visibility".into()],
        skills: vec!["office cleaning".into(), "deep cleaning".into()],
        ..Default::default()
    }
}

struct Run<T> {
    result: T,
    progress: Vec<Progress>,
    usage: deepseek::Usage,
}

fn with_ctx<T>(
    client: &deepseek::Client,
    cancel_at: Option<&str>,
    f: impl FnOnce(&Ctx) -> T,
) -> Run<T> {
    let log: Mutex<Vec<Progress>> = Mutex::new(Vec::new());
    let cancel = AtomicBool::new(false);
    let cancel_stage = cancel_at.map(str::to_string);
    let progress = |p: Progress| {
        if cancel_stage.as_deref() == Some(p.stage.as_str()) {
            cancel.store(true, Ordering::Relaxed);
        }
        log.lock().unwrap().push(p);
    };
    let ctx = Ctx {
        ai: client,
        progress: &progress,
        today: TODAY.into(),
        localization: Some("102.65.1.2".into()),
        cancel: &cancel,
    };
    let result = f(&ctx);
    Run {
        result,
        progress: log.into_inner().unwrap(),
        usage: client.usage(),
    }
}

fn discover(
    client: &deepseek::Client,
    input: &DiscoverInput,
) -> Run<Result<DiscoverOutput, Error>> {
    with_ctx(client, None, |ctx| engine::discover(ctx, input))
}

fn assert_progress_is_a_good_feed(progress: &[Progress]) {
    assert!(!progress.is_empty());
    assert!(
        progress.windows(2).all(|w| w[0].percent <= w[1].percent),
        "percent must only rise: {progress:?}"
    );
    assert!(
        progress.iter().all(|p| !p.message.trim().is_empty()
            && p.message.chars().count() <= 120
            && !p.message.contains('!')),
        "{progress:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// discover
// ---------------------------------------------------------------------------------------------

#[test]
fn discover_end_to_end() {
    let mock = Mock::start();
    let client = mock.client(2);
    let input = DiscoverInput {
        profile: business_profile(),
        steering: Steering {
            notes: vec!["more maintenance contracts".into()],
            liked: vec!["Port maintenance tender".into()],
            disliked: vec![],
        },
        known: vec![
            KnownOpportunity {
                key: "old.example/stale".into(),
                title: "A stale listing".into(),
                category: "sales".into(),
                status: "active".into(),
                deadline: None,
            },
            KnownOpportunity {
                key: "kept.example/saved".into(),
                title: "Saved but stale-looking".into(),
                category: "funding".into(),
                status: "saved".into(),
                deadline: None,
            },
        ],
        limits: Limits::default(),
        ..Default::default()
    };
    let run = discover(&client, &input);
    let out = run.result.expect("discover works against the mock");
    let log = mock.log();

    // Opportunities: real candidates, current, valid, unique, best first.
    let opps = &out.opportunities;
    assert!(
        !opps.is_empty() && opps.len() <= 24,
        "{} opportunities",
        opps.len()
    );
    for o in opps {
        assert!(
            o.url.starts_with("https://example-") && o.url.contains(".org/opportunities/"),
            "URL must be a search result: {}",
            o.url
        );
        assert!(model::is_opportunity_type(&o.category));
        assert!(
            o.deadline.as_deref().is_none_or(|d| d >= TODAY),
            "{:?}",
            o.deadline
        );
        assert!(
            !o.title.contains("must be"),
            "invalid model entries are ignored: {}",
            o.title
        );
        assert!(!o.why.is_empty() && !o.action.is_empty() && !o.summary.is_empty());
        assert_eq!(o.published.as_deref(), Some("2026-11-01"));
        assert!(o.score <= 100);
    }
    assert!(
        opps.windows(2).all(|w| w[0].score >= w[1].score),
        "best first"
    );
    let keys: std::collections::HashSet<String> = opps.iter().filter_map(|o| o.key()).collect();
    assert_eq!(keys.len(), opps.len(), "unique by key");
    assert!(opps.iter().any(|o| o.verified), "top picks are verified");
    assert!(
        opps.iter()
            .filter(|o| o.verified)
            .all(|o| o.value == "$5,000 prize"),
        "verified picks take the page's value"
    );
    assert_eq!(
        out.stale,
        vec!["old.example/stale"],
        "only active known items can be stale"
    );

    // Stats tell the story.
    let s = &out.stats;
    assert_eq!(s.queries, 14);
    assert_eq!(s.results, 140);
    assert_eq!(s.candidates, 70);
    assert!(s.selected >= opps.len());
    assert_eq!(s.failed_searches, 0);
    assert!(!s.plan_fallback);
    assert!(
        s.verified >= 1 && s.verified + s.rejected_on_verify + s.unverified <= 8,
        "{s:?}"
    );

    // The calls: one plan, 14 searches, two review batches, ≤ 8 page reads each checked.
    let stages = chat_stages(&log);
    assert_eq!(stages.iter().filter(|s| *s == "plan").count(), 1);
    assert_eq!(
        stages.iter().filter(|s| *s == "evaluate").count(),
        1,
        "70 candidates are reviewed in one call"
    );
    assert!(
        !stages.contains(&"context".to_string()),
        "no context, no context call"
    );
    let fetched = fetched_urls(&log);
    assert!(!fetched.is_empty() && fetched.len() <= 8);
    assert_eq!(
        stages.iter().filter(|s| *s == "verify").count(),
        fetched.len()
    );
    let closed = fetched.iter().filter(|u| mock_says_closed(u)).count();
    assert_eq!(
        s.rejected_on_verify, closed,
        "closed pages are dropped: {fetched:?}"
    );
    assert!(
        closed >= 1,
        "the mock data should exercise the closed-page path"
    );
    for u in fetched.iter().filter(|u| mock_says_closed(u)) {
        assert!(
            !opps.iter().any(|o| &o.url == u),
            "a closed page never comes back: {u}"
        );
    }

    // Queries: steering shaped the plan; nothing identifying was searched.
    let queries = search_queries(&log);
    assert_eq!(queries.len(), 14);
    assert!(
        queries
            .iter()
            .any(|q| q.contains("more maintenance contracts")),
        "steering reaches the plan: {queries:?}"
    );
    assert!(
        queries
            .iter()
            .all(|q| !q.to_lowercase().contains("khumalo")),
        "the business's own name is never searched: {queries:?}"
    );
    let first_search: Value = serde_json::from_str(
        entries(&log, "/v1/tools/search")[0]["body"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(first_search["count"], 10);
    assert_eq!(first_search["localization"], "102.65.1.2");
    assert_eq!(first_search["need_content"], false);

    // The plan prompt describes the business and carries today's date, never its website.
    let plan_body: Value = serde_json::from_str(
        entries(&log, "/v1/chat/completions")[0]["body"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let plan_user = plan_body["messages"][1]["content"].as_str().unwrap();
    assert!(plan_user.contains("Today is 2026-11-26"));
    assert!(plan_user.contains("Industry: Steel fabrication"));
    assert!(
        !plan_user.contains("khumaloprecision.example"),
        "the website never reaches discovery prompts"
    );

    // The live feed.
    let p = &run.progress;
    assert_progress_is_a_good_feed(p);
    assert_eq!(p.first().unwrap().stage, "planning");
    assert!(
        p.iter()
            .any(|x| x.message.starts_with("Planning 14 searches across")),
        "{p:?}"
    );
    assert!(
        p.iter()
            .any(|x| x.stage == "searching" && x.message.starts_with("Searching: "))
    );
    assert!(
        p.iter()
            .any(|x| x.message == "Found 140 leads, reviewing the best 70"),
        "{p:?}"
    );
    assert!(
        p.iter()
            .any(|x| x.stage == "verifying" && x.message.starts_with("Reading "))
    );
    let last = p.last().unwrap();
    assert_eq!(last.percent, 96);
    assert!(last.message.ends_with("opportunities ready"), "{last:?}");

    // Usage was counted.
    assert_eq!(run.usage.searches, 14);
    assert!(run.usage.chat_calls >= 4);

    // A dismissed opportunity never comes back, even when found again.
    let gone = opps[0].key().unwrap();
    let mut again = input.clone();
    again.known.push(KnownOpportunity {
        key: gone.clone(),
        title: opps[0].title.clone(),
        category: opps[0].category.clone(),
        status: "dismissed".into(),
        deadline: None,
    });
    let second = discover(&mock.client(2), &again)
        .result
        .expect("second run");
    assert!(!second.opportunities.is_empty());
    assert!(
        second
            .opportunities
            .iter()
            .all(|o| o.key().as_deref() != Some(gone.as_str())),
        "dismissed key came back"
    );
}

#[test]
fn discover_survives_search_rate_limits_and_unreadable_pages() {
    let mock = Mock::start();
    // One retry per call: 20 rate-limited attempts sink at least 6 of the 14 searches.
    mock.fail("/v1/tools/search", 429, 20);
    mock.fail("/v1/tools/fetch", 404, 3);
    let run = discover(
        &mock.client(1),
        &DiscoverInput {
            profile: chef_profile(),
            ..Default::default()
        },
    );
    let out = run.result.expect("partial failures are tolerated");
    let s = &out.stats;
    assert!((6..=10).contains(&s.failed_searches), "{s:?}");
    assert!(!out.opportunities.is_empty());
    assert!(
        s.unverified >= 3,
        "pages that can't be read keep their pick, unverified: {s:?}"
    );
    assert!(out.opportunities.iter().any(|o| !o.verified));
    assert_progress_is_a_good_feed(&run.progress);
}

#[test]
fn discover_stops_on_account_problems() {
    // Out of credit on the very first call: nothing else is attempted.
    let mock = Mock::start();
    mock.fail("/v1/chat/completions", 402, 1);
    let r = discover(
        &mock.client(2),
        &DiscoverInput {
            profile: chef_profile(),
            ..Default::default()
        },
    )
    .result;
    assert!(
        matches!(r, Err(Error::Ai(deepseek::Error::InsufficientBalance(_)))),
        "{r:?}"
    );
    assert!(
        search_queries(&mock.log()).is_empty(),
        "no searches after an account error"
    );
    drop(mock);

    // A rejected key on search stops the remaining searches.
    let mock = Mock::start();
    mock.fail("/v1/tools/search", 401, 100);
    let r = discover(
        &mock.client(2),
        &DiscoverInput {
            profile: chef_profile(),
            ..Default::default()
        },
    )
    .result;
    assert!(
        matches!(r, Err(Error::Ai(deepseek::Error::Unauthorized(_)))),
        "{r:?}"
    );
    let log = mock.log();
    assert!(
        search_queries(&log).len() < 14,
        "the rest of the searches were skipped"
    );
    assert!(!chat_stages(&log).contains(&"evaluate".to_string()));
    drop(mock);

    // Out of credit while reading pages.
    let mock = Mock::start();
    mock.fail("/v1/tools/fetch", 402, 1);
    let r = discover(
        &mock.client(2),
        &DiscoverInput {
            profile: chef_profile(),
            ..Default::default()
        },
    )
    .result;
    assert!(
        matches!(r, Err(Error::Ai(deepseek::Error::InsufficientBalance(_)))),
        "{r:?}"
    );
}

#[test]
fn discover_falls_back_to_a_deterministic_plan_and_handles_total_search_failure() {
    let mock = Mock::start();
    mock.fail("/v1/chat/completions", 500, 2);
    let run = discover(
        &mock.client(1),
        &DiscoverInput {
            profile: chef_profile(),
            ..Default::default()
        },
    );
    let out = run.result.expect("a failed plan falls back");
    assert!(out.stats.plan_fallback);
    assert_eq!(out.stats.queries, 14);
    assert!(!out.opportunities.is_empty());
    let queries = search_queries(&mock.log());
    assert!(
        queries.iter().any(|q| q.contains("after:2026-07-29")),
        "{queries:?}"
    );
    drop(mock);

    let mock = Mock::start();
    mock.fail("/v1/tools/search", 503, 1000);
    let r = discover(
        &mock.client(1),
        &DiscoverInput {
            profile: chef_profile(),
            ..Default::default()
        },
    )
    .result;
    assert!(
        matches!(r, Err(Error::Ai(deepseek::Error::Server(503, _)))),
        "every search failing fails the run: {r:?}"
    );
}

#[test]
fn discover_cancels_and_refuses_empty_profiles() {
    let mock = Mock::start();
    let client = mock.client(2);
    let run = with_ctx(&client, Some("searching"), |ctx| {
        engine::discover(
            ctx,
            &DiscoverInput {
                profile: chef_profile(),
                ..Default::default()
            },
        )
    });
    assert!(
        matches!(run.result, Err(Error::Cancelled)),
        "{:?}",
        run.result
    );
    assert!(
        !chat_stages(&mock.log()).contains(&"evaluate".to_string()),
        "nothing runs after the cancel"
    );

    let empty = discover(
        &client,
        &DiscoverInput {
            profile: Profile {
                location: "Cape Town".into(),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    assert!(matches!(empty.result, Err(Error::EmptyProfile)));
}

// ---------------------------------------------------------------------------------------------
// research
// ---------------------------------------------------------------------------------------------

#[test]
fn research_end_to_end() {
    let mock = Mock::start();
    let client = mock.client(2);
    let input = ResearchInput {
        first_name: "Jane".into(),
        last_name: "Doe".into(),
        email: "jane@heyhi.co.za".into(),
        hints: "Cape Town · product lead · https://heyhi.co.za/team".into(),
    };
    let run = with_ctx(&client, None, |ctx| engine::research(ctx, &input));
    let p = run.result.expect("research works against the mock");
    let r = p.research.clone().expect("research notes are attached");
    assert!(r.confidence >= 0.6, "{r:?}");
    assert!(r.summary.starts_with("We found you"));
    assert!(!r.sources.is_empty());
    assert!(
        r.sources
            .iter()
            .all(|s| s.starts_with("https://example-") || s == "https://heyhi.co.za/team"),
        "{:?}",
        r.sources
    );
    assert!(
        r.sources.contains(&"https://heyhi.co.za/team".to_string()),
        "the linked page is a source"
    );
    assert_eq!(
        p.headline,
        "Product lead at Mock Org, building tools for restaurants"
    );
    assert_eq!(p.stage, "growth");
    assert_eq!(
        p.seeking,
        vec!["sales", "partners", "visibility"],
        "unknown kinds are dropped"
    );
    assert!(
        !p.interests
            .iter()
            .any(|i| i.to_lowercase().contains("religio")),
        "sensitive traits are dropped: {:?}",
        p.interests
    );
    assert!(p.links.contains(&"https://heyhi.co.za/team".to_string()));
    assert!(
        !p.links.iter().any(|l| l.contains("invented.example")),
        "links must come from matched results: {:?}",
        p.links
    );
    assert!(
        p.links
            .iter()
            .all(|l| l.starts_with("https://example-") || l == "https://heyhi.co.za/team")
    );

    let log = mock.log();
    let queries = search_queries(&log);
    for q in [
        "\"Jane Doe\"",
        "\"Jane Doe\" heyhi",
        "site:heyhi.co.za",
        "\"Jane Doe\" Cape Town product lead",
    ] {
        assert!(queries.contains(&q.to_string()), "missing {q}: {queries:?}");
    }
    assert_eq!(fetched_urls(&log), vec!["https://heyhi.co.za/team"]);
    assert_eq!(chat_stages(&log), vec!["research"]);
    let research_body: Value = serde_json::from_str(
        entries(&log, "/v1/chat/completions")[0]["body"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let user = research_body["messages"][1]["content"].as_str().unwrap();
    assert!(
        user.contains("Today is 2026-11-26")
            && user.contains("Name: Jane Doe")
            && user.contains("probably their organisation")
    );
    assert_progress_is_a_good_feed(&run.progress);
    assert!(run.progress.iter().all(|p| p.stage == "researching"));
    assert_eq!(run.progress.last().unwrap().percent, 95);
}

#[test]
fn research_with_nothing_found_is_honest() {
    let mock = Mock::start();
    let client = mock.client(2);
    // A free-mail address: no domain search. Nothing is found; the draft comes from the hints.
    let input = ResearchInput {
        first_name: "Nobody".into(),
        last_name: "Atall".into(),
        email: "nobody.atall@gmail.com".into(),
        hints: "Durban pastry chef".into(),
    };
    let p = with_ctx(&client, None, |ctx| engine::research(ctx, &input))
        .result
        .expect("research");
    let r = p.research.clone().unwrap();
    assert!(r.confidence <= 0.2, "{r:?}");
    assert!(r.summary.starts_with("We couldn't find"), "{}", r.summary);
    assert!(r.sources.is_empty());
    assert_eq!(p.headline, "Durban pastry chef");
    let log = mock.log();
    assert!(
        search_queries(&log).iter().all(|q| !q.starts_with("site:")),
        "no domain search for gmail"
    );
    assert_eq!(chat_stages(&log), vec!["research"]);
    drop(mock);

    // No hints either: no model call at all, a blank draft with an honest note.
    let mock = Mock::start();
    let input = ResearchInput {
        first_name: "Nobody".into(),
        last_name: "Atall".into(),
        email: "nobody.atall@gmail.com".into(),
        hints: String::new(),
    };
    let p = with_ctx(&mock.client(2), None, |ctx| engine::research(ctx, &input))
        .result
        .expect("research");
    let r = p.research.clone().unwrap();
    assert_eq!(r.confidence, 0.0);
    assert!(r.summary.starts_with("We couldn't find"));
    assert!(p.headline.is_empty() && p.interests.is_empty());
    assert!(chat_stages(&mock.log()).is_empty());
}

#[test]
fn research_stops_on_account_problems() {
    let mock = Mock::start();
    mock.fail("/v1/tools/search", 402, 100);
    let input = ResearchInput {
        first_name: "Jane".into(),
        last_name: "Doe".into(),
        email: "jane@heyhi.co.za".into(),
        hints: String::new(),
    };
    let r = with_ctx(&mock.client(2), None, |ctx| engine::research(ctx, &input)).result;
    assert!(
        matches!(r, Err(Error::Ai(deepseek::Error::InsufficientBalance(_)))),
        "{r:?}"
    );
    assert!(chat_stages(&mock.log()).is_empty());
}

// ---------------------------------------------------------------------------------------------
// theme
// ---------------------------------------------------------------------------------------------

#[test]
fn theme_from_the_model_and_its_fallback() {
    let mock = Mock::start();
    let client = mock.client(1);
    let chef = chef_profile();
    let t = with_ctx(&client, None, |ctx| engine::theme_hint(ctx, &chef)).result;
    assert_eq!(t.label, "Commercial Cleaning");
    assert_eq!(t.accent, "#c2410c");
    assert_eq!(t.image_queries.len(), 2);
    assert!(!t.mood.is_empty());
    let user = serde_json::from_str::<Value>(
        entries(&mock.log(), "/v1/chat/completions")[0]["body"]
            .as_str()
            .unwrap(),
    )
    .unwrap()["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(user.contains("Today is 2026-11-26"));

    // The model failing never fails the theme.
    mock.fail("/v1/chat/completions", 500, 10);
    let t = with_ctx(&client, None, |ctx| engine::theme_hint(ctx, &chef)).result;
    assert_eq!(t.label, "Cleaning & Facilities");
    assert_eq!(t.accent, "#0e7490");
    assert!((2..=4).contains(&t.image_queries.len()));

    // Nor does an unreachable API, or an account problem.
    let mut cfg = deepseek::Config::new("mock-key");
    cfg.base_url = "http://127.0.0.1:9".into();
    cfg.max_retries = 0;
    let dead = deepseek::Client::new(cfg);
    let t = with_ctx(&dead, None, |ctx| {
        engine::theme_hint(ctx, &business_profile())
    })
    .result;
    assert!(!t.label.is_empty() && model::sanitize_hex_color(&t.accent, "") == t.accent);
}

// ---------------------------------------------------------------------------------------------
// business research and context
// ---------------------------------------------------------------------------------------------

#[test]
fn research_business_end_to_end() {
    let mock = Mock::start();
    let client = mock.client(2);
    let input = BusinessInput {
        business_name: "Acme Cleaning".into(),
        website: "www.acmeclean.example".into(),
        hints: "Cape Town".into(),
    };
    let run = with_ctx(&client, None, |ctx| engine::research_business(ctx, &input));
    let (profile, intel) = run
        .result
        .expect("research_business works against the mock");

    // The profile keeps the name and website it was given and only the site's own links.
    assert_eq!(profile.organization, "Acme Cleaning");
    assert_eq!(profile.website, "https://www.acmeclean.example");
    assert_eq!(profile.industry, "Commercial cleaning");
    assert_eq!(profile.stage, "established");
    assert_eq!(
        profile.seeking,
        vec!["sales", "funding", "regulation"],
        "unknown focus areas dropped"
    );
    assert_eq!(
        profile.links,
        vec!["https://linkedin.com/company/mock-business"],
        "only links found on the site"
    );
    let r = profile.research.clone().expect("research notes");
    assert!(r.confidence >= 0.5 && !r.summary.is_empty());
    assert!(
        r.sources
            .contains(&"https://www.acmeclean.example".to_string())
    );

    // The dossier: every item sourced from what was read or searched; inventions dropped.
    let log = mock.log();
    let fetched = fetched_urls(&log);
    let mut known_urls: Vec<String> = fetched.clone();
    for e in entries(&log, "/v1/tools/search") {
        let _ = e;
    }
    assert!(intel.is_ready());
    assert_eq!(
        intel.overview.sources.len(),
        1,
        "the unknown S999 source is dropped"
    );
    assert_eq!(
        intel.offerings.len(),
        1,
        "an offering citing a missing source is dropped"
    );
    assert!(
        intel.segments[0].examples.is_empty(),
        "customer names the source doesn't show are dropped"
    );
    assert!(!intel.competitors.is_empty());
    assert!(
        intel
            .competitors
            .iter()
            .all(|c| c.name.ends_with(" Services")),
        "a competitor no source names is dropped: {:?}",
        intel.competitors
    );
    assert!(
        intel
            .competitors
            .iter()
            .all(|c| c.source.url.starts_with("https://example-"))
    );
    assert_eq!(intel.regulation.len(), 1);
    assert_eq!(intel.regulation[0].effective.as_deref(), Some("2027-03-01"));
    assert_eq!(intel.trends.len(), 1);
    assert_eq!(intel.news.len(), 1);
    known_urls.extend(intel.competitors.iter().map(|c| c.source.url.clone()));
    for src in intel
        .overview
        .sources
        .iter()
        .chain(intel.offerings.iter().map(|o| &o.source))
    {
        assert!(
            fetched.contains(&src.url),
            "own-site facts cite pages that were read: {}",
            src.url
        );
    }

    // What was read and searched.
    assert!(fetched.contains(&"https://www.acmeclean.example".to_string()));
    for page in ["/about", "/services", "/case-studies"] {
        assert!(
            fetched.iter().any(|u| u.ends_with(page)),
            "key page {page} read: {fetched:?}"
        );
    }
    assert!(
        !fetched
            .iter()
            .any(|u| u.contains("linkedin") || u.starts_with("mailto")),
        "only the business's own pages are fetched"
    );
    let queries = search_queries(&log);
    assert!(
        queries.contains(&"site:acmeclean.example".to_string()),
        "{queries:?}"
    );
    assert!(
        queries.contains(&"\"Acme Cleaning\" after:2025-11-26".to_string()),
        "news from the last year: {queries:?}"
    );
    assert!(queries.contains(&"\"Acme Cleaning\" competitors".to_string()));
    assert!(
        queries.contains(&"commercial cleaning companies Cape Town".to_string()),
        "follow-up searches from the profile step"
    );
    assert!(queries.iter().any(|q| q.contains("minimum wage")));
    assert_eq!(
        chat_stages(&log),
        vec!["business_profile", "business_intel"]
    );

    // The live feed.
    assert_progress_is_a_good_feed(&run.progress);
    let msgs: Vec<&str> = run.progress.iter().map(|p| p.message.as_str()).collect();
    assert!(msgs.contains(&"Reading acmeclean.example"), "{msgs:?}");
    assert!(
        msgs.iter()
            .any(|m| m.starts_with("Checking regulation in South Africa")),
        "{msgs:?}"
    );
    assert!(msgs.iter().any(|m| m.starts_with("Research complete")));
}

#[test]
fn research_business_stops_on_account_problems() {
    let mock = Mock::start();
    mock.fail("/v1/tools/fetch", 402, 100);
    let input = BusinessInput {
        business_name: "Acme Cleaning".into(),
        website: "https://www.acmeclean.example".into(),
        hints: String::new(),
    };
    let r = with_ctx(&mock.client(2), None, |ctx| {
        engine::research_business(ctx, &input)
    })
    .result;
    assert!(
        matches!(r, Err(Error::Ai(deepseek::Error::InsufficientBalance(_)))),
        "{r:?}"
    );
    assert!(chat_stages(&mock.log()).is_empty());
}

#[test]
fn discover_uses_intel_and_team_context() {
    let mock = Mock::start();
    let mut intel = model::Intel::default();
    intel.overview.summary = "Sparkle cleans offices and schools in Cape Town.".into();
    intel.overview.sources = vec![model::Source {
        url: "https://sparkle.example".into(),
        title: "Sparkle".into(),
        date: None,
    }];
    let context = vec![
        ContextItem {
            source: "slack".into(),
            kind: "message".into(),
            title: "Quote request from Growthpoint".into(),
            text: "Can you quote for a deep clean of Century City offices?".into(),
            author: "Thandi".into(),
            container: "#sales".into(),
            url: "https://sparkle.slack.com/archives/C1/p1".into(),
            date: Some("2026-11-20".into()),
        },
        ContextItem {
            source: "microsoft".into(),
            kind: "event".into(),
            title: "Team lunch".into(),
            text: "Friday lunch".into(),
            author: String::new(),
            container: String::new(),
            url: "https://outlook.office.com/calendar/item/2".into(),
            date: Some("2026-11-21".into()),
        },
    ];
    let input = DiscoverInput {
        profile: chef_profile(),
        intel,
        context,
        ..Default::default()
    };
    let run = discover(&mock.client(2), &input);
    let out = run.result.expect("discover with context");
    let internal: Vec<&model::Opportunity> = out
        .opportunities
        .iter()
        .filter(|o| o.source == "slack")
        .collect();
    assert_eq!(
        internal.len(),
        1,
        "the quote request becomes an internal opportunity"
    );
    assert_eq!(internal[0].url, "https://sparkle.slack.com/archives/C1/p1");
    assert_eq!(internal[0].site_name, "Slack");
    assert_eq!(out.stats.internal, 1);
    assert!(
        out.opportunities
            .iter()
            .filter(|o| o.source.is_empty())
            .count()
            > 0,
        "web finds too"
    );
    let log = mock.log();
    assert_eq!(
        chat_stages(&log).iter().filter(|s| *s == "context").count(),
        1
    );
    let bodies: Vec<String> = entries(&log, "/v1/chat/completions")
        .iter()
        .map(|e| e["body"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        bodies
            .iter()
            .any(|b| b.contains("X-Opp") || b.contains("What research established about it")),
        "the dossier reaches the prompts"
    );
    assert!(
        bodies
            .iter()
            .any(|b| b.contains("Already in the team's own tools")),
        "the review sees what the team is on"
    );
    assert_progress_is_a_good_feed(&run.progress);
}

#[test]
fn discover_runs_a_bounded_second_wave_when_the_first_pass_is_thin() {
    let mock = Mock::start();
    // The mock gives a business named "Thin …" only two picks from the first pass.
    let mut profile = chef_profile();
    profile.organization = "Thin Cleaning Co".into();
    let run = discover(
        &mock.client(2),
        &DiscoverInput {
            profile,
            ..Default::default()
        },
    );
    let out = run.result.expect("discover with a second wave");
    let s = &out.stats;
    assert!(
        s.second_wave_queries >= 6 && s.second_wave_queries <= 8,
        "{s:?}"
    );
    assert!(
        out.opportunities.len() > 2,
        "the second wave adds opportunities: {}",
        out.opportunities.len()
    );
    assert!(
        out.opportunities.iter().any(|o| o.title.contains("Wider")),
        "picks from the second wave are kept"
    );
    assert!(
        out.opportunities
            .windows(2)
            .all(|w| w[0].score >= w[1].score)
    );
    let log = mock.log();
    let stages = chat_stages(&log);
    assert_eq!(
        stages.iter().filter(|x| *x == "plan_more").count(),
        1,
        "exactly one extra round"
    );
    assert_eq!(stages.iter().filter(|x| *x == "evaluate").count(), 2);
    let queries = search_queries(&log);
    assert_eq!(queries.len(), 14 + s.second_wave_queries);
    assert!(
        queries
            .iter()
            .filter(|q| q.starts_with("wider "))
            .all(|q| !q.contains("after:")),
        "broader queries drop operators"
    );
    assert!(
        queries
            .iter()
            .all(|q| !q.to_lowercase().contains("thin cleaning")),
        "the business itself is never searched"
    );
    assert_progress_is_a_good_feed(&run.progress);
    assert!(
        run.progress
            .iter()
            .any(|p| p.message == "Too few strong leads yet, so searching wider")
    );

    // A healthy first pass never triggers it.
    let mock = Mock::start();
    let out = discover(
        &mock.client(2),
        &DiscoverInput {
            profile: chef_profile(),
            ..Default::default()
        },
    )
    .result
    .expect("discover");
    assert_eq!(out.stats.second_wave_queries, 0);
    assert!(!chat_stages(&mock.log()).contains(&"plan_more".to_string()));
}
