//! Against the real mock server (`tests/mock/server.py`): its Bedrock module verifies SigV4
//! signatures with the secrets below and behaves like Bedrock's Converse and listing APIs; its
//! DeepSeek module answers search. Each test starts its own mock on a free port in 9960–9969
//! and stops it (by PID) when done; tests run one at a time.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bedrock::{Auth, Bedrock, Config, list_models};
use deepseek::{ChatBackend, ChatRequest, Error, SearchRequest, Thinking};
use serde_json::{Value, json};

const API_KEY: &str = "ABSKbW9jay1iZWRyb2NrLWFwaS1rZXk=";
const AKID: &str = "AKIDMOCKBEDROCK00001";
const SECRET: &str = "mockSecret/Key+ForBedrockTests/0000000000";
const TEMP_AKID: &str = "ASIAMOCKBEDROCK00002";
const TEMP_SECRET: &str = "tempSecret/Key+ForBedrockTests/111111111";
const TEMP_TOKEN: &str = "mock-session-token/Ab+Cd==";

const HAIKU: &str = "anthropic.claude-3-haiku-20240307-v1:0";
const SONNET: &str = "us.anthropic.claude-sonnet-4-5-20250929-v1:0";
const OPUS: &str = "global.anthropic.claude-opus-5-5-v1";
const PROFILE_ARN: &str = "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-sonnet-4-5-20250929-v1:0";
const APP_ARN: &str =
    "arn:aws:bedrock:eu-west-1:123456789012:application-inference-profile/a1b2c3d4e5f6";

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x02\0\0\0\x90wS\xde";
const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F', 0];

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into()
}

fn get_json(url: &str) -> Option<Value> {
    let mut resp = http().get(url).call().ok()?;
    if resp.status() != 200 {
        return None;
    }
    serde_json::from_str(&resp.body_mut().read_to_string().ok()?).ok()
}

fn post_json(url: &str, body: Value) -> Value {
    let mut resp = http()
        .post(url)
        .header("Content-Type", "application/json")
        .send(body.to_string().as_bytes())
        .unwrap();
    serde_json::from_str(&resp.body_mut().read_to_string().unwrap()).unwrap()
}

fn free_port() -> Option<u16> {
    (9960..=9969u16).find(|p| TcpListener::bind(("127.0.0.1", *p)).is_ok())
}

struct Mock {
    child: Child,
    base: String,
}

impl Mock {
    fn start() -> Mock {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/mock/server.py");
        for port in 9960..=9969u16 {
            if TcpListener::bind(("127.0.0.1", port)).is_err() {
                continue;
            }
            let mut child = Command::new("python3")
                .arg(script)
                .arg("--port")
                .arg(port.to_string())
                // No __pycache__ left in tests/mock.
                .env("PYTHONDONTWRITEBYTECODE", "1")
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("python3 runs");
            let base = format!("http://127.0.0.1:{port}");
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                if get_json(&format!("{base}/_health")).is_some() {
                    return Mock { child, base };
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("couldn't start the mock on a free port in 9960-9969");
    }

    fn config(&self, region: &str, model: &str, auth: Auth) -> Config {
        let mut cfg = Config::new(region, model, auth);
        cfg.runtime_endpoint = Some(format!("{}/bedrock-runtime", self.base));
        cfg.control_endpoint = Some(format!("{}/bedrock", self.base));
        cfg.retry_base = Duration::from_millis(5);
        cfg.timeout = Duration::from_secs(10);
        cfg
    }

    fn backend(&self, model: &str, auth: Auth) -> Bedrock {
        Bedrock::new(self.config("us-east-1", model, auth)).unwrap()
    }

    /// The Converse calls the Bedrock mock saw, oldest first.
    fn seen(&self) -> Vec<Value> {
        get_json(&format!("{}/bedrock-runtime/_seen", self.base))
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    }

    /// Every request the server saw (paths, stages).
    fn log(&self) -> Vec<Value> {
        get_json(&format!("{}/_log", self.base))
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    }

    fn converse_requests(&self) -> usize {
        self.log()
            .iter()
            .filter(|e| e["path"].as_str().unwrap_or("").ends_with("/converse"))
            .count()
    }

    /// The server's generic failure injection (OpenAI-style error bodies, no AWS headers).
    fn fail(&self, spec: Value) {
        post_json(&format!("{}/_fail", self.base), spec);
    }

    /// The Bedrock mock's own failure injection (AWS-shaped errors).
    fn aws_fail(&self, code: &str, times: u32) {
        post_json(
            &format!("{}/bedrock-runtime/_fail", self.base),
            json!({ "type": code, "times": times }),
        );
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn api_key() -> Auth {
    Auth::ApiKey(API_KEY.into())
}

fn keys() -> Auth {
    Auth::Keys {
        access_key_id: AKID.into(),
        secret_access_key: SECRET.into(),
        session_token: None,
    }
}

#[test]
fn chat_with_an_api_key() {
    let _g = serial();
    let mock = Mock::start();
    let b = mock.backend(SONNET, api_key());
    let out = b
        .chat(
            &ChatRequest::new("hello", "Be brief.", "Say something about grants.").max_tokens(300),
        )
        .unwrap();
    assert!(
        out.content.starts_with("Mock Claude read your message"),
        "{out:?}"
    );
    assert_eq!(out.finish_reason, "stop");
    assert!(out.input_tokens > 0 && out.output_tokens > 0);
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    let s = &seen[0];
    assert_eq!(s["auth"], "bearer");
    assert_eq!(s["status"], 200);
    assert_eq!(s["stage"], "hello", "X-Opp-Stage reaches the mock");
    assert_eq!(s["model"], SONNET);
    assert_eq!(
        s["raw_path"],
        "/bedrock-runtime/model/us.anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse"
    );
    assert_eq!(s["system"], json!(["Be brief."]));
    assert_eq!(s["max_tokens"], 300);
    assert_eq!(s["thinking"], Value::Null);
}

#[test]
fn chat_signed_with_sigv4_for_an_inference_profile_arn() {
    let _g = serial();
    let mock = Mock::start();
    // No region: it comes from the ARN.
    let b = Bedrock::new(mock.config("", PROFILE_ARN, keys())).unwrap();
    assert_eq!(b.config().region, "us-east-1");
    let out = b
        .chat(&ChatRequest::new(
            "bedrock-test",
            "You are terse.",
            "Reply with OK",
        ))
        .unwrap();
    assert_eq!(out.content, "OK");
    let s = &mock.seen()[0];
    assert_eq!(
        (s["auth"].as_str(), s["status"].as_u64()),
        (Some("sigv4"), Some(200)),
        "{s}"
    );
    assert_eq!(s["access_key"], AKID);
    assert_eq!(s["region"], "us-east-1");
    assert_eq!(s["model"], PROFILE_ARN, "the ARN arrives whole");
    assert_eq!(
        s["signed_headers"],
        json!(["content-type", "host", "x-amz-date"])
    );
    assert_eq!(
        s["raw_path"],
        "/bedrock-runtime/model/arn%3Aaws%3Abedrock%3Aus-east-1%3A123456789012%3Ainference-profile%2Fus.anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse"
    );
}

#[test]
fn chat_signed_with_a_session_token_for_an_application_profile_in_another_region() {
    let _g = serial();
    let mock = Mock::start();
    let auth = Auth::Keys {
        access_key_id: TEMP_AKID.into(),
        secret_access_key: TEMP_SECRET.into(),
        session_token: Some(TEMP_TOKEN.into()),
    };
    let b = Bedrock::new(mock.config("", APP_ARN, auth)).unwrap();
    assert_eq!(b.config().region, "eu-west-1");
    assert_eq!(
        b.chat(&ChatRequest::new("bedrock-test", "", "Reply with OK"))
            .unwrap()
            .content,
        "OK"
    );
    let s = &mock.seen()[0];
    assert_eq!(s["region"], "eu-west-1");
    assert_eq!(
        s["signed_headers"],
        json!(["content-type", "host", "x-amz-date", "x-amz-security-token"])
    );
    // The mock refuses an ARN signed for a different region, so this also proves the region
    // was taken from the ARN and used to sign.
}

#[test]
fn a_throttling_burst_is_retried_until_it_clears() {
    let _g = serial();
    let mock = Mock::start();
    let b = mock.backend(SONNET, keys());
    mock.fail(json!({ "prefix": "/bedrock-runtime/", "status": 429, "times": 2 }));
    let out = b
        .chat(&ChatRequest::new("bedrock-test", "", "Reply with OK"))
        .unwrap();
    assert_eq!(out.content, "OK");
    assert_eq!(mock.converse_requests(), 3, "two 429s, then success");
}

#[test]
fn aws_errors_are_retried_or_reported() {
    let _g = serial();
    let mock = Mock::start();
    let b = mock.backend(SONNET, api_key());
    let req = ChatRequest::new("bedrock-test", "", "Reply with OK");

    // Throttling that never clears: 1 try + 3 retries, then RateLimited.
    mock.aws_fail("ThrottlingException", 10);
    let e = b.chat(&req).unwrap_err();
    assert!(
        matches!(&e, Error::RateLimited(m) if m.contains("throttling")),
        "{e:?}"
    );
    assert_eq!(mock.seen().len(), 4);
    post_json(&format!("{}/_reset", mock.base), json!({}));

    // A model that isn't ready, then is.
    mock.aws_fail("ModelNotReadyException", 1);
    assert_eq!(b.chat(&req).unwrap().content, "OK");
    let seen = mock.seen();
    assert_eq!(
        seen.iter()
            .map(|s| s["status"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![429, 200]
    );
    assert_eq!(seen[0]["error"], "ModelNotReadyException");

    // Retryable server errors that persist come back as Server / Network errors.
    post_json(&format!("{}/_reset", mock.base), json!({}));
    mock.aws_fail("ServiceUnavailableException", 10);
    assert!(matches!(b.chat(&req), Err(Error::Server(503, _))));
    post_json(&format!("{}/_reset", mock.base), json!({}));
    mock.aws_fail("ModelTimeoutException", 10);
    assert!(matches!(b.chat(&req), Err(Error::Network(_))));
    post_json(&format!("{}/_reset", mock.base), json!({}));
    mock.aws_fail("InternalServerException", 2);
    assert_eq!(b.chat(&req).unwrap().content, "OK");

    // A daily quota isn't retried.
    post_json(&format!("{}/_reset", mock.base), json!({}));
    mock.aws_fail("ServiceQuotaExceededException", 10);
    assert!(matches!(b.chat(&req), Err(Error::RateLimited(m)) if m.contains("quota")));
    assert_eq!(mock.seen().len(), 1);
}

#[test]
fn bad_credentials_are_unauthorized_and_not_retried() {
    let _g = serial();
    let mock = Mock::start();
    let req = ChatRequest::new("bedrock-test", "", "Reply with OK");
    let cases: Vec<(Auth, &str, &str)> = vec![
        (
            Auth::Keys {
                access_key_id: AKID.into(),
                secret_access_key: "not-the-secret".into(),
                session_token: None,
            },
            "InvalidSignatureException",
            "secret access key is wrong",
        ),
        (
            Auth::Keys {
                access_key_id: "AKIDUNKNOWN000000000".into(),
                secret_access_key: SECRET.into(),
                session_token: None,
            },
            "UnrecognizedClientException",
            "doesn't recognise",
        ),
        (
            Auth::ApiKey("ABSKnot-a-real-key".into()),
            "AccessDeniedException",
            "refused access",
        ),
        (
            Auth::Keys {
                access_key_id: "ASIAMOCKEXPIRED00003".into(),
                secret_access_key: "expiredSecret/ForBedrockTests/2222222222".into(),
                session_token: Some("mock-expired-token".into()),
            },
            "ExpiredTokenException",
            "expired",
        ),
        (
            Auth::Keys {
                access_key_id: TEMP_AKID.into(),
                secret_access_key: TEMP_SECRET.into(),
                session_token: None,
            },
            "UnrecognizedClientException",
            "doesn't recognise",
        ),
    ];
    for (i, (auth, code, words)) in cases.into_iter().enumerate() {
        let b = mock.backend(SONNET, auth);
        let e = b.chat(&req).unwrap_err();
        assert!(
            matches!(&e, Error::Unauthorized(m) if m.contains(words)),
            "{code}: {e:?}"
        );
        assert!(e.is_account_problem());
        let seen = mock.seen();
        assert_eq!(seen.len(), i + 1, "{code}: not retried");
        assert_eq!(seen[i]["error"], code);
    }
}

#[test]
fn model_problems_are_explained() {
    let _g = serial();
    let mock = Mock::start();
    let req = ChatRequest::new("bedrock-test", "", "Reply with OK");
    let e = mock
        .backend("anthropic.claude-nonexistent-v9", api_key())
        .chat(&req)
        .unwrap_err();
    assert!(
        matches!(&e, Error::BadRequest(404, m) if m.contains("not found or not enabled in this region") && m.contains("us-east-1")),
        "{e:?}"
    );
    // A model that needs an inference profile.
    let e = mock
        .backend("anthropic.claude-sonnet-4-5-20250929-v1:0", api_key())
        .chat(&req)
        .unwrap_err();
    assert!(
        matches!(&e, Error::BadRequest(400, m) if m.contains("inference profile id")),
        "{e:?}"
    );
    // An ARN in one region with the region set to another is caught before calling AWS.
    assert!(matches!(
        Bedrock::new(mock.config("us-west-2", PROFILE_ARN, keys())),
        Err(Error::BadRequest(400, _))
    ));
    assert_eq!(mock.seen().len(), 2);
}

#[test]
fn json_mode_returns_bare_json_even_from_a_chatty_model() {
    let _g = serial();
    let mock = Mock::start();
    // The mock's Claude 3 Haiku wraps JSON in a sentence and a code fence.
    let b = mock.backend(HAIKU, api_key());
    let out = b
        .chat(
            &ChatRequest::new(
                "bedrock-echo",
                "Plan searches. Reply in JSON.",
                "Find grants for chefs",
            )
            .json()
            .max_tokens(2000),
        )
        .unwrap();
    let v: Value = serde_json::from_str(&out.content)
        .unwrap_or_else(|e| panic!("not bare JSON ({e}): {}", out.content));
    assert_eq!(v["ok"], true);
    // What the stage handler got: an OpenAI-shaped copy of the Converse request.
    assert_eq!(v["roles"], json!(["system", "user"]));
    assert_eq!(v["user"], "Find grants for chefs");
    assert_eq!(v["response_format"]["type"], "json_object");
    assert_eq!(v["max_tokens"], 2000);
    let system = v["system"].as_str().unwrap();
    assert!(
        system.starts_with("Plan searches. Reply in JSON.")
            && system.contains("Respond with one JSON object only"),
        "{system}"
    );
    // Without json the same reply would keep its prose.
    let raw = b
        .chat(&ChatRequest::new(
            "bedrock-echo",
            "Plan searches. Reply in JSON.",
            "x",
        ))
        .unwrap();
    assert!(
        raw.content.starts_with("Here is the JSON you asked for"),
        "{}",
        raw.content
    );
}

#[test]
fn thinking_falls_back_to_what_the_model_takes_and_remembers() {
    let _g = serial();
    let mock = Mock::start();
    let thinking = |seen: &[Value]| {
        seen.iter()
            .map(|s| {
                (
                    s["thinking"].as_str().unwrap_or("-").to_string(),
                    s["status"].as_u64().unwrap(),
                )
            })
            .collect::<Vec<_>>()
    };

    // Off by default: nothing is sent even when the request asks for thinking.
    let b = mock.backend(SONNET, api_key());
    b.chat(&ChatRequest::new("bedrock-test", "", "Reply with OK").thinking(Thinking::High))
        .unwrap();
    assert_eq!(thinking(&mock.seen()), vec![("-".to_string(), 200)]);

    // Claude Sonnet 4.5 takes a budget, not adaptive thinking.
    let mut cfg = mock.config("us-east-1", SONNET, api_key());
    cfg.thinking = true;
    let b = Bedrock::new(cfg).unwrap();
    let req = ChatRequest::new("bedrock-test", "", "Reply with OK")
        .thinking(Thinking::High)
        .max_tokens(2000);
    let out = b.chat(&req).unwrap();
    assert_eq!(out.content, "OK");
    assert!(
        !out.reasoning.is_empty(),
        "the reasoning block is kept apart from the answer"
    );
    b.chat(&req).unwrap();
    let seen = mock.seen();
    assert_eq!(
        thinking(&seen[1..]),
        vec![
            ("adaptive".into(), 400),
            ("enabled".into(), 200),
            ("enabled".into(), 200)
        ]
    );
    assert_eq!(seen[2]["budget_tokens"], 8192);
    assert_eq!(seen[2]["max_tokens"], 2000 + 8192);

    // Claude 3 Haiku: no thinking at all and a 4,096-token output limit.
    post_json(&format!("{}/_reset", mock.base), json!({}));
    let mut cfg = mock.config("us-east-1", HAIKU, api_key());
    cfg.thinking = true;
    let b = Bedrock::new(cfg).unwrap();
    let req = ChatRequest::new("bedrock-test", "", "Reply with OK")
        .thinking(Thinking::Low)
        .max_tokens(3000);
    assert_eq!(b.chat(&req).unwrap().content, "OK");
    let seen = mock.seen();
    assert_eq!(
        thinking(&seen),
        vec![
            ("adaptive".into(), 400),
            ("adaptive".into(), 400),
            ("enabled".into(), 400),
            ("-".into(), 200)
        ]
    );
    assert_eq!(seen[0]["max_tokens"], 3000 + 2048);
    assert_eq!(seen[1]["max_tokens"], 4096, "the model's limit is obeyed");
    b.chat(&req).unwrap();
    let seen = mock.seen();
    assert_eq!(seen.len(), 5, "remembered: one call this time");
    assert_eq!(
        (seen[4]["thinking"].clone(), seen[4]["max_tokens"].clone()),
        (Value::Null, json!(3000))
    );
}

#[test]
fn temperature_is_dropped_for_a_model_that_refuses_it() {
    let _g = serial();
    let mock = Mock::start();
    // The mock's Claude Opus 5.5: adaptive thinking only, no temperature, always thinks.
    let b = mock.backend(OPUS, api_key());
    let req = ChatRequest::new("bedrock-test", "", "Reply with OK").temperature(0.4);
    let out = b.chat(&req).unwrap();
    assert_eq!(
        out.content, "OK",
        "the always-on reasoning block isn't part of the answer"
    );
    b.chat(&req).unwrap();
    let seen = mock.seen();
    let temps: Vec<(Value, u64)> = seen
        .iter()
        .map(|s| (s["temperature"].clone(), s["status"].as_u64().unwrap()))
        .collect();
    assert_eq!(
        temps,
        vec![(json!(0.4), 400), (Value::Null, 200), (Value::Null, 200)]
    );

    // A model that takes temperature gets it, clamped to Claude's range.
    let b = mock.backend(SONNET, api_key());
    b.chat(&ChatRequest::new("bedrock-test", "", "Reply with OK").temperature(1.6))
        .unwrap();
    assert_eq!(mock.seen()[3]["temperature"], json!(1.0));
}

#[test]
fn a_models_output_limit_is_learned() {
    let _g = serial();
    let mock = Mock::start();
    let b = mock.backend(HAIKU, keys());
    let req = ChatRequest::new("bedrock-test", "", "Reply with OK").max_tokens(8000);
    b.chat(&req).unwrap();
    b.chat(&req).unwrap();
    let max: Vec<Value> = mock
        .seen()
        .iter()
        .map(|s| s["max_tokens"].clone())
        .collect();
    assert_eq!(max, vec![json!(8000), json!(4096), json!(4096)]);
}

#[test]
fn test_connection_makes_one_tiny_call() {
    let _g = serial();
    let mock = Mock::start();
    let note = mock.backend(OPUS, keys()).test_connection().unwrap();
    assert!(
        note.contains("OK") && note.contains(OPUS) && note.contains("us-east-1"),
        "{note}"
    );
    let s = &mock.seen()[0];
    assert_eq!(s["max_tokens"], 16);
    let e = mock
        .backend(OPUS, Auth::ApiKey("ABSKwrong".into()))
        .test_connection()
        .unwrap_err();
    assert!(matches!(e, Error::Unauthorized(_)), "{e:?}");
}

#[test]
fn list_models_filters_sorts_and_pages() {
    let _g = serial();
    let mock = Mock::start();
    // Signed GETs with a query string, including nextToken values with + / and = in them.
    let models = list_models(&mock.config("us-east-1", "", keys())).unwrap();
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "anthropic.claude-3-haiku-20240307-v1:0",
            "anthropic.claude-3-5-sonnet-20240620-v1:0",
            "global.anthropic.claude-opus-5-5-v1",
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/a1b2c3d4e5f6",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "amazon.nova-pro-v1:0",
            "meta.llama3-1-70b-instruct-v1:0",
            "us.amazon.nova-premier-v1:0",
            "us.meta.llama3-2-90b-instruct-v1:0",
        ]
    );
    assert_eq!(models[0].name, "Anthropic Claude 3 Haiku");
    assert_eq!(models[3].name, "opportunity-prod");
    let paths: Vec<String> = mock
        .log()
        .iter()
        .map(|e| e["path"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        paths
            .iter()
            .filter(|p| p.as_str() == "/bedrock/inference-profiles")
            .count(),
        4,
        "three pages of system profiles, one of application profiles"
    );

    // An API key works too, and the region can come from a model ARN.
    let models = list_models(&mock.config("", PROFILE_ARN, api_key())).unwrap();
    assert_eq!(models.len(), 9);
}

#[test]
fn list_models_without_permission_for_profiles_or_at_all() {
    let _g = serial();
    let mock = Mock::start();
    let limited = Auth::Keys {
        access_key_id: "AKIDMOCKNOPROFILES04".into(),
        secret_access_key: "noProfiles/Secret+ForBedrockTests/333333".into(),
        session_token: None,
    };
    let models = list_models(&mock.config("us-east-1", "", limited)).unwrap();
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "anthropic.claude-3-haiku-20240307-v1:0",
            "anthropic.claude-3-5-sonnet-20240620-v1:0",
            "amazon.nova-pro-v1:0",
            "meta.llama3-1-70b-instruct-v1:0"
        ]
    );

    let wrong = Auth::Keys {
        access_key_id: AKID.into(),
        secret_access_key: "wrong".into(),
        session_token: None,
    };
    let e = list_models(&mock.config("us-east-1", "", wrong)).unwrap_err();
    assert!(matches!(e, Error::Unauthorized(_)), "{e:?}");
    let e = list_models(&mock.config("", "", keys())).unwrap_err();
    assert!(
        matches!(&e, Error::BadRequest(400, m) if m.contains("region")),
        "{e:?}"
    );
}

#[test]
fn a_deepseek_client_sends_chat_to_bedrock_and_search_to_deepseek() {
    let _g = serial();
    let mock = Mock::start();
    let mut ds = deepseek::Config::new("mock-key");
    ds.base_url = mock.base.clone();
    ds.retry_base = Duration::from_millis(5);
    let bedrock = Arc::new(mock.backend(SONNET, keys()));
    let client = deepseek::Client::new(ds).with_chat_backend(bedrock);
    assert_eq!(client.chat_model(), format!("bedrock:{SONNET}"));

    let v = client
        .chat_json_value(
            &ChatRequest::new(
                "bedrock-echo",
                "Plan searches. Reply in JSON.",
                "Find grants for chefs",
            )
            .image("image/png", PNG.to_vec()),
        )
        .unwrap();
    assert_eq!(
        (v["ok"].clone(), v["images"].clone()),
        (json!(true), json!(1)),
        "{v}"
    );
    assert!(
        v["image_urls"][0]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,iVBORw0KGgo"),
        "{v}"
    );
    // A real engine stage is answered by deepseek_mock's handler for it (an object either way).
    assert!(
        client
            .chat_json_value(&ChatRequest::new(
                "plan",
                "Reply in JSON.",
                "A chef in Cape Town."
            ))
            .unwrap()
            .is_object()
    );
    let results = client
        .search(&SearchRequest::new("culinary grants", 3))
        .unwrap();
    assert!(!results.is_empty());

    let log = mock.log();
    let paths: Vec<&str> = log
        .iter()
        .map(|e| e["path"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(paths.iter().filter(|p| p.ends_with("/converse")).count(), 2);
    assert_eq!(
        paths.iter().filter(|p| **p == "/v1/tools/search").count(),
        1
    );
    assert!(
        !paths.contains(&"/v1/chat/completions"),
        "no chat went to DeepSeek: {paths:?}"
    );
    let stages: Vec<&str> = log
        .iter()
        .filter(|e| e["path"].as_str().unwrap_or("").ends_with("/converse"))
        .map(|e| e["stage"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(stages, vec!["bedrock-echo", "plan"]);

    let u = client.usage();
    assert_eq!((u.chat_calls, u.searches, u.failed_calls), (2, 1, 0));
    assert!(u.input_tokens > 1100, "image tokens are counted");
    assert!(
        (u.cost_usd - deepseek::TOOL_CALL_USD).abs() < 1e-12,
        "Bedrock chat costs us nothing: {}",
        u.cost_usd
    );
    assert_eq!(u.calls[0].note, format!("bedrock:{SONNET}"));
}

#[test]
fn images_reach_bedrock_in_the_format_their_bytes_have() {
    let _g = serial();
    let mock = Mock::start();
    let b = mock.backend(SONNET, api_key());
    // The JPEG is labelled PNG; Bedrock checks the bytes, so the backend sends what they are.
    let out = b
        .chat(
            &ChatRequest::new("bedrock-test", "Pick one.", "Which photo?")
                .image("image/png", PNG.to_vec())
                .image("image/png", JPEG.to_vec()),
        )
        .unwrap();
    assert!(out.content.contains("2 images"), "{}", out.content);
    assert_eq!(mock.seen()[0]["images"], 2);
    let e = b
        .chat(&ChatRequest::new("bedrock-test", "", "x").image("image/tiff", b"II*\0".to_vec()))
        .unwrap_err();
    assert!(matches!(e, Error::BadRequest(400, _)));
    assert_eq!(mock.seen().len(), 1, "refused before calling AWS");
}

#[test]
fn slow_or_dropped_replies_are_retried() {
    let _g = serial();
    let mock = Mock::start();
    let mut cfg = mock.config("us-east-1", SONNET, api_key());
    cfg.timeout = Duration::from_millis(700);
    let b = Bedrock::new(cfg).unwrap();
    let req = ChatRequest::new("bedrock-test", "", "Reply with OK");
    mock.fail(json!({ "prefix": "/bedrock-runtime/", "status": 200, "delay": 2.0, "times": 1 }));
    let started = Instant::now();
    assert_eq!(b.chat(&req).unwrap().content, "OK");
    assert!(
        started.elapsed() < Duration::from_millis(1900),
        "the slow attempt was abandoned at the timeout: {:?}",
        started.elapsed()
    );
    mock.fail(json!({ "prefix": "/bedrock-runtime/", "status": 0, "times": 1 }));
    assert_eq!(b.chat(&req).unwrap().content, "OK");
}

#[test]
fn an_unreachable_endpoint_is_a_network_error() {
    let _g = serial();
    let Some(port) = free_port() else { return };
    let mut cfg = Config::new("us-east-1", SONNET, api_key());
    cfg.runtime_endpoint = Some(format!("http://127.0.0.1:{port}/bedrock-runtime"));
    cfg.retry_base = Duration::from_millis(1);
    cfg.max_retries = 1;
    let e = Bedrock::new(cfg)
        .unwrap()
        .chat(&ChatRequest::new("bedrock-test", "", "x"))
        .unwrap_err();
    assert!(
        matches!(&e, Error::Network(m) if m.contains("Couldn't reach Amazon Bedrock")),
        "{e:?}"
    );
}
