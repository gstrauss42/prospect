use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

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
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

/// A one-connection-per-reply HTTP server that plays `script` in order and reports every
/// request it saw.
fn serve(script: Vec<(u16, String)>) -> (String, mpsc::Receiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (status, body) in script {
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
            let resp = format!(
                "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
    });
    (format!("http://{addr}"), rx)
}

fn client(base: &str) -> Client {
    let mut cfg = Config::new("sk-test");
    cfg.base_url = base.into();
    cfg.retry_base = Duration::from_millis(5);
    cfg.max_retries = 3;
    Client::new(cfg)
}

fn chat_reply(content: &str) -> String {
    json!({
        "choices": [{ "message": { "role": "assistant", "content": content }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 100, "completion_tokens": 20, "prompt_cache_hit_tokens": 40, "prompt_cache_miss_tokens": 60 }
    })
    .to_string()
}

fn err_body(msg: &str) -> String {
    json!({ "error": { "message": msg, "type": "x", "param": null, "code": "x" } }).to_string()
}

#[test]
fn chat_json_parses_counts_and_sends_the_right_request() {
    let (base, rx) = serve(vec![(200, chat_reply("{\"a\": 1}"))]);
    let c = client(&base);
    let v = c
        .chat_json_value(&ChatRequest::new("plan", "Reply in JSON.", "hi"))
        .unwrap();
    assert_eq!(v, json!({ "a": 1 }));
    let seen = rx.recv().unwrap();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path, "/v1/chat/completions");
    assert_eq!(seen.header("authorization"), Some("Bearer sk-test"));
    assert_eq!(seen.header("x-opp-stage"), Some("plan"));
    let body = seen.json();
    assert_eq!(body["model"], "deepseek-flash");
    assert_eq!(body["thinking"]["type"], "disabled");
    assert_eq!(body["response_format"]["type"], "json_object");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["content"], "hi");
    let u = c.usage();
    assert_eq!(
        (
            u.chat_calls,
            u.input_tokens,
            u.cached_tokens,
            u.output_tokens
        ),
        (1, 60, 40, 20)
    );
    assert!(u.cost_usd > 0.0);
    assert_eq!(u.calls.len(), 1);
    assert_eq!(u.calls[0].stage, "plan");
}

#[test]
fn chat_json_deserialises_into_a_type() {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Plan {
        queries: Vec<String>,
        note: String,
    }
    let (base, _rx) = serve(vec![(
        200,
        chat_reply("```json\n{\"queries\": [\"a\", \"b\"]}\n```"),
    )]);
    let p: Plan = client(&base)
        .chat_json(&ChatRequest::new("plan", "JSON", "x"))
        .unwrap();
    assert_eq!(p.queries, vec!["a", "b"]);
    assert_eq!(p.note, "");
}

#[test]
fn retries_429_and_5xx_then_succeeds() {
    let (base, rx) = serve(vec![
        (429, err_body("slow down")),
        (503, err_body("busy")),
        (200, chat_reply("{\"ok\": true}")),
    ]);
    let c = client(&base);
    let v = c
        .chat_json_value(&ChatRequest::new("evaluate", "JSON", "x"))
        .unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(rx.try_iter().count(), 3);
    let u = c.usage();
    assert_eq!(u.calls.last().unwrap().attempts, 3);
    assert_eq!(u.failed_calls, 0);
}

#[test]
fn account_errors_are_not_retried() {
    let (base, rx) = serve(vec![
        (402, err_body("Insufficient Balance")),
        (200, chat_reply("{}")),
    ]);
    let c = client(&base);
    let e = c.chat(&ChatRequest::new("plan", "s", "u")).unwrap_err();
    assert!(
        matches!(e, Error::InsufficientBalance(ref m) if m == "Insufficient Balance"),
        "{e:?}"
    );
    assert!(e.is_account_problem());
    assert_eq!(e.status(), 402);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(rx.try_iter().count(), 1);
    assert_eq!(c.usage().failed_calls, 1);

    let (base, _rx) = serve(vec![(401, err_body("Authentication Fails"))]);
    assert!(matches!(
        client(&base).chat(&ChatRequest::new("plan", "s", "u")),
        Err(Error::Unauthorized(_))
    ));

    let (base, _rx) = serve(vec![(400, err_body("bad field"))]);
    assert!(matches!(
        client(&base).chat(&ChatRequest::new("plan", "s", "u")),
        Err(Error::BadRequest(400, _))
    ));
}

#[test]
fn rate_limit_that_never_clears_is_reported() {
    let (base, rx) = serve(vec![
        (429, err_body("a")),
        (429, err_body("b")),
        (429, err_body("c")),
        (429, err_body("d")),
    ]);
    let e = client(&base)
        .chat(&ChatRequest::new("plan", "s", "u"))
        .unwrap_err();
    assert!(matches!(e, Error::RateLimited(_)), "{e:?}");
    assert_eq!(rx.try_iter().count(), 4, "1 try + 3 retries");
}

#[test]
fn blank_or_broken_json_is_retried_then_given_up() {
    let (base, _rx) = serve(vec![
        (200, chat_reply("   ")),
        (200, chat_reply("{\"ok\": 1}")),
    ]);
    assert_eq!(
        client(&base)
            .chat_json_value(&ChatRequest::new("plan", "JSON", "x"))
            .unwrap()["ok"],
        1
    );

    let (base, _rx) = serve(vec![
        (200, chat_reply("nope")),
        (200, chat_reply("still no")),
        (200, chat_reply("{oops")),
    ]);
    let e = client(&base)
        .chat_json_value(&ChatRequest::new("plan", "JSON", "x"))
        .unwrap_err();
    assert!(
        matches!(e, Error::InvalidResponse(ref m) if m.contains("stage plan")),
        "{e:?}"
    );
}

#[test]
fn truncated_json_doubles_max_tokens() {
    let cut = json!({
        "choices": [{ "message": { "content": "{\"a\": [1, 2" }, "finish_reason": "length" }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
    })
    .to_string();
    let (base, rx) = serve(vec![(200, cut), (200, chat_reply("{\"a\": [1, 2]}"))]);
    let v = client(&base)
        .chat_json_value(&ChatRequest::new("plan", "JSON", "x").max_tokens(1000))
        .unwrap();
    assert_eq!(v["a"][1], 2);
    let first = rx.recv().unwrap().json();
    let second = rx.recv().unwrap().json();
    assert_eq!(first["max_tokens"], 1000);
    assert_eq!(second["max_tokens"], 2000);
}

#[test]
fn thinking_and_images_shape_the_body() {
    let body = ChatRequest::new("evaluate", "sys", "look")
        .thinking(Thinking::Low)
        .temperature(0.3)
        .image("image/png", vec![1, 2, 3])
        .body("deepseek-flash");
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["reasoning_effort"], "low");
    assert!(
        body.get("temperature").is_none(),
        "temperature is ignored in thinking mode, so not sent"
    );
    let content = body["messages"][1]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AQID");
    assert_eq!(content[1]["image_url"]["detail"], "low");

    let off = ChatRequest::new("plan", "s", "u")
        .temperature(0.2)
        .body("deepseek-flash");
    assert_eq!(off["thinking"]["type"], "disabled");
    assert!((off["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
    assert!(off.get("reasoning_effort").is_none());
    assert!(off.get("response_format").is_none());
}

#[test]
fn search_parses_filters_and_bills() {
    let reply = json!({ "results": [
        { "rerank_score": 2.1, "title": "Grant", "url": "https://example.org/grant", "snippet": "s", "summary": "long", "site_name": "Ex", "site_icon": null, "date": "2026-09-22T16:00:00+00:00", "authority": 3 },
        { "title": "junk", "url": "not a url" }
    ]})
    .to_string();
    let (base, rx) = serve(vec![(200, reply)]);
    let c = client(&base);
    let mut req = SearchRequest::new(format!("culinary grants {}", "x".repeat(200)), 99);
    req.localization = Some("102.0.0.1".into());
    let results = c.search(&req).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].site_name.as_deref(), Some("Ex"));
    assert_eq!(results[0].authority, 3);
    let seen = rx.recv().unwrap();
    assert_eq!(seen.path, "/v1/tools/search");
    let body = seen.json();
    assert_eq!(body["count"], 50);
    assert!(body["query"].as_str().unwrap().chars().count() <= 128);
    assert_eq!(body["localization"], "102.0.0.1");
    assert!(body.get("ori_question").is_none());
    let u = c.usage();
    assert_eq!(u.searches, 1);
    assert!((u.cost_usd - TOOL_CALL_USD).abs() < 1e-12);
}

#[test]
fn empty_search_is_free_and_blank_query_is_refused() {
    let (base, _rx) = serve(vec![(200, json!({ "results": [] }).to_string())]);
    let c = client(&base);
    assert!(
        c.search(&SearchRequest::new("nothing here", 5))
            .unwrap()
            .is_empty()
    );
    assert_eq!(c.usage().searches, 0);
    assert_eq!(c.usage().cost_usd, 0.0);
    assert!(matches!(
        c.search(&SearchRequest::new("   ", 5)),
        Err(Error::BadRequest(400, _))
    ));
}

#[test]
fn fetch_ok_and_unavailable() {
    let (base, rx) = serve(vec![
        (
            200,
            json!({ "title": "Page", "content": "# Page\n\nText" }).to_string(),
        ),
        (404, err_body("no content")),
        (200, json!({ "title": "", "content": "  " }).to_string()),
    ]);
    let c = client(&base);
    let p = c.fetch("https://example.org/a").unwrap();
    assert_eq!(p.title, "Page");
    assert_eq!(rx.recv().unwrap().json()["url"], "https://example.org/a");
    assert!(matches!(
        c.fetch("https://example.org/b"),
        Err(Error::Unavailable(404, _))
    ));
    assert!(matches!(
        c.fetch("https://example.org/c"),
        Err(Error::Unavailable(404, _))
    ));
    assert!(matches!(
        c.fetch("javascript:alert(1)"),
        Err(Error::BadRequest(400, _))
    ));
    assert_eq!(c.usage().fetches, 1);
}

#[test]
fn unreachable_server_is_a_network_error() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut cfg = Config::new("k");
    cfg.base_url = format!("http://127.0.0.1:{port}");
    cfg.retry_base = Duration::from_millis(1);
    cfg.max_retries = 1;
    let e = Client::new(cfg)
        .search(&SearchRequest::new("x", 1))
        .unwrap_err();
    assert!(matches!(e, Error::Network(_)), "{e:?}");
}

#[test]
fn balance_is_read() {
    let (base, rx) = serve(vec![(200, json!({ "is_available": true, "balance_infos": [{ "currency": "USD", "total_balance": "2.28" }] }).to_string())]);
    assert!((client(&base).balance_usd().unwrap() - 2.28).abs() < 1e-9);
    assert_eq!(rx.recv().unwrap().path, "/user/balance");
}

#[test]
fn forks_count_separately() {
    let (base, _rx) = serve(vec![(200, chat_reply("{}")), (200, chat_reply("{}"))]);
    let a = client(&base);
    let b = a.fork();
    a.chat(&ChatRequest::new("x", "s", "u")).unwrap();
    b.chat(&ChatRequest::new("y", "s", "u")).unwrap();
    assert_eq!(a.usage().chat_calls, 1);
    assert_eq!(b.usage().chat_calls, 1);
    let mut total = a.usage();
    total.absorb(&b.usage());
    assert_eq!(total.chat_calls, 2);
    assert_eq!(total.calls.len(), 2);
}

#[test]
fn json_extraction() {
    assert_eq!(extract_json("{\"a\":1}"), Some(json!({ "a": 1 })));
    assert_eq!(
        extract_json("  ```json\n{\"a\":1}\n```  "),
        Some(json!({ "a": 1 }))
    );
    assert_eq!(
        extract_json("Sure! {\"a\": {\"b\": 2}} hope that helps"),
        Some(json!({ "a": { "b": 2 } }))
    );
    assert_eq!(extract_json("[1,2]"), Some(json!([1, 2])));
    assert_eq!(extract_json("   "), None);
    assert_eq!(extract_json("no json"), None);
}

#[test]
fn query_clamping() {
    assert_eq!(clamp_query("  a   b "), Some("a b".into()));
    assert_eq!(clamp_query("\n\t "), None);
    let long = "word ".repeat(60);
    let q = clamp_query(&long).unwrap();
    assert!(q.chars().count() <= 128);
    assert!(!q.ends_with(' '));
    assert!(q.ends_with("word"));
}

#[test]
fn peak_hours_and_cost() {
    // 2026-11-26 (Thursday) 00:00 UTC
    let thu = model::date::days_from_civil(2026, 11, 26) * 86_400;
    assert!(!is_peak(thu));
    assert!(is_peak(thu + 2 * 3600));
    assert!(!is_peak(thu + 5 * 3600));
    assert!(is_peak(thu + 7 * 3600));
    assert!(!is_peak(thu + 12 * 3600));
    let sat = model::date::days_from_civil(2026, 11, 28) * 86_400;
    assert!(!is_peak(sat + 7 * 3600));
    let off = chat_cost("deepseek-flash", 1_000_000, 0, 0, false);
    assert!((off - 0.15).abs() < 1e-9);
    assert!((chat_cost("deepseek-flash", 1_000_000, 0, 0, true) - 0.30).abs() < 1e-9);
    assert!((chat_cost("deepseek-flash", 0, 0, 1_000_000, false) - 0.6).abs() < 1e-9);
    assert!((chat_cost("deepseek-v4-pro", 1_000_000, 0, 0, false) - 0.66).abs() < 1e-9);
}

struct FakeBackend {
    reply: String,
    seen: Mutex<Vec<ChatRequest>>,
}

impl ChatBackend for FakeBackend {
    fn name(&self) -> String {
        "fake:claude".into()
    }
    fn chat(&self, req: &ChatRequest) -> Result<ChatOutput> {
        self.seen.lock().unwrap().push(req.clone());
        if self.reply == "fail" {
            return Err(Error::Unauthorized("bad keys".into()));
        }
        Ok(ChatOutput {
            content: self.reply.clone(),
            finish_reason: "stop".into(),
            input_tokens: 10,
            cached_tokens: 0,
            output_tokens: 5,
            ..Default::default()
        })
    }
    fn cost_usd(&self, out: &ChatOutput) -> f64 {
        out.output_tokens as f64 * 0.001
    }
}

#[test]
fn chat_backend_takes_chat_but_search_stays_on_deepseek() {
    let (base, rx) = serve(vec![(
        200,
        json!({ "results": [{ "title": "t", "url": "https://x.org/a" }] }).to_string(),
    )]);
    let backend = Arc::new(FakeBackend {
        reply: "{\"ok\": true}".into(),
        seen: Mutex::new(vec![]),
    });
    let c = client(&base).with_chat_backend(backend.clone());
    assert_eq!(c.chat_model(), "fake:claude");
    let v = c
        .chat_json_value(&ChatRequest::new("plan", "JSON please", "hi").image("image/png", vec![1]))
        .unwrap();
    assert_eq!(v["ok"], true);
    let seen = backend.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].json, "chat_json asks the backend for JSON");
    assert_eq!(seen[0].user.len(), 2, "images reach the backend");
    // Search still goes to DeepSeek (the scripted server), and only it hit the network.
    assert_eq!(c.search(&SearchRequest::new("grants", 3)).unwrap().len(), 1);
    assert_eq!(rx.recv().unwrap().path, "/v1/tools/search");
    let u = c.usage();
    assert_eq!(
        (u.chat_calls, u.input_tokens, u.output_tokens, u.searches),
        (1, 10, 5, 1)
    );
    assert!((u.cost_usd - (0.005 + TOOL_CALL_USD)).abs() < 1e-9);
    assert_eq!(u.calls[0].note, "fake:claude");
    // Forks keep the backend; with_model drops it.
    assert_eq!(c.fork().chat_model(), "fake:claude");
    let pro = c.with_model("deepseek-v4-pro");
    assert_eq!(pro.chat_model(), "deepseek-v4-pro");
    assert_eq!(pro.config().model, "deepseek-v4-pro");
}

#[test]
fn chat_backend_errors_count_and_propagate() {
    let backend = Arc::new(FakeBackend {
        reply: "fail".into(),
        seen: Mutex::new(vec![]),
    });
    let c = client("http://127.0.0.1:9").with_chat_backend(backend);
    let e = c.chat(&ChatRequest::new("plan", "s", "u")).unwrap_err();
    assert!(e.is_account_problem());
    assert_eq!(c.usage().failed_calls, 1);
}
