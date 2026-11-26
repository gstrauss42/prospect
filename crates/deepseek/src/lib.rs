//! DeepSeek API client: the v1 endpoints Opportunity is built on.
//!
//! | What              | Endpoint                         | Billing                          |
//! |-------------------|----------------------------------|----------------------------------|
//! | Chat (JSON/vision)| `POST /v1/chat/completions`      | tokens                           |
//! | Search API        | `POST /v1/tools/search`          | $0.75 per 1,000 non-empty results|
//! | Fetch API         | `POST /v1/tools/fetch`           | $0.75 per 1,000 pages            |
//! | Balance           | `GET /user/balance`              | free                             |
//!
//! Blocking and thread-safe: clone a [`Client`] freely, call it from many threads. Every call
//! retries what is worth retrying (408, 429, 5xx, network errors, blank JSON), never retries what
//! isn't (400, 401, 402, 403, 404), and is counted in [`Usage`] with an estimated cost, so a run
//! can report exactly what it spent. [`Client::fork`] gives a client with its own counters that
//! shares the connection pool.
//!
//! Every request carries an `X-Opp-Stage` header naming the pipeline step that made it. DeepSeek
//! ignores it; the mock server in `tests/mock` uses it to answer each step with the right shape.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
pub const DEFAULT_MODEL: &str = "deepseek-flash";

/// Price of one successful Search or Fetch API call, in USD.
pub const TOOL_CALL_USD: f64 = 0.75 / 1000.0;

/// How many call records a [`Usage`] keeps (oldest dropped first).
const MAX_CALL_RECORDS: usize = 200;

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Config {
    pub api_key: String,
    /// `https://api.deepseek.com`, or a mock server in tests. No trailing slash.
    pub base_url: String,
    /// `deepseek-flash` unless told otherwise.
    pub model: String,
    /// Longest one chat call may take. Thinking calls can be slow.
    pub chat_timeout: Duration,
    /// Longest one Search or Fetch call may take.
    pub tool_timeout: Duration,
    /// Extra attempts after a retryable failure.
    pub max_retries: u32,
    /// First retry delay; doubles each attempt. Tests set it tiny.
    pub retry_base: Duration,
}

impl Config {
    pub fn new(api_key: impl Into<String>) -> Config {
        Config {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            chat_timeout: Duration::from_secs(300),
            tool_timeout: Duration::from_secs(45),
            max_retries: 3,
            retry_base: Duration::from_millis(1000),
        }
    }

    /// `DEEPSEEK_API_KEY` (required), `DEEPSEEK_BASE_URL`, `DEEPSEEK_MODEL`. None without a key.
    pub fn from_env() -> Option<Config> {
        let key = std::env::var("DEEPSEEK_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())?;
        let mut cfg = Config::new(key.trim());
        if let Ok(url) = std::env::var("DEEPSEEK_BASE_URL")
            && !url.trim().is_empty()
        {
            cfg.base_url = url.trim().trim_end_matches('/').to_string();
        }
        if let Ok(model) = std::env::var("DEEPSEEK_MODEL")
            && !model.trim().is_empty()
        {
            cfg.model = model.trim().to_string();
        }
        Some(cfg)
    }
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// 401: the API key is missing or wrong.
    Unauthorized(String),
    /// 402: the DeepSeek account is out of credit.
    InsufficientBalance(String),
    /// 429, still, after retrying.
    RateLimited(String),
    /// A 4xx that retrying won't fix (400, 413, 422…).
    BadRequest(u16, String),
    /// Fetch couldn't get usable content: blocked by policy (403) or nothing to read (404).
    Unavailable(u16, String),
    /// 5xx, still, after retrying.
    Server(u16, String),
    /// Couldn't connect, or timed out, still, after retrying.
    Network(String),
    /// The reply wasn't what was asked for (e.g. not JSON), still, after retrying.
    InvalidResponse(String),
}

impl Error {
    /// True when nothing will work until someone fixes the account (bad key, no credit), so a
    /// pipeline should stop rather than carry on with other calls.
    pub fn is_account_problem(&self) -> bool {
        matches!(self, Error::Unauthorized(_) | Error::InsufficientBalance(_))
    }

    /// The HTTP status behind the error, 0 when there was none.
    pub fn status(&self) -> u16 {
        match self {
            Error::Unauthorized(_) => 401,
            Error::InsufficientBalance(_) => 402,
            Error::RateLimited(_) => 429,
            Error::BadRequest(s, _) | Error::Unavailable(s, _) | Error::Server(s, _) => *s,
            Error::Network(_) | Error::InvalidResponse(_) => 0,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unauthorized(m) => write!(f, "The AI service rejected the credentials: {m}"),
            Error::InsufficientBalance(m) => {
                write!(f, "The AI account is out of credit: {m}")
            }
            Error::RateLimited(m) => write!(f, "The AI service is limiting requests: {m}"),
            Error::BadRequest(s, m) => write!(f, "The AI service refused the request ({s}): {m}"),
            Error::Unavailable(s, m) => write!(f, "Page unavailable ({s}): {m}"),
            Error::Server(s, m) => write!(f, "The AI service had an error ({s}): {m}"),
            Error::Network(m) => write!(f, "Couldn't reach the AI service: {m}"),
            Error::InvalidResponse(m) => write!(f, "Unexpected reply from the AI service: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------------------------
// Usage and cost
// ---------------------------------------------------------------------------------------------

/// What a client has used so far, with an estimated cost.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub chat_calls: u32,
    /// Input tokens that missed the cache.
    pub input_tokens: u64,
    /// Input tokens served from the cache (much cheaper).
    pub cached_tokens: u64,
    /// Output tokens, including thinking.
    pub output_tokens: u64,
    /// Billable (non-empty) searches.
    pub searches: u32,
    /// Billable (non-empty) fetches.
    pub fetches: u32,
    /// Calls that ended in an error after retries.
    pub failed_calls: u32,
    /// Estimated spend in USD (peak/off-peak aware).
    pub cost_usd: f64,
    /// The most recent calls, oldest first.
    pub calls: Vec<CallRecord>,
}

/// One call, for logs and run diagnostics.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CallRecord {
    /// "chat", "search" or "fetch".
    pub kind: String,
    /// The pipeline step that made it.
    pub stage: String,
    /// HTTP status of the last attempt, 0 for a network error.
    pub status: u16,
    pub attempts: u32,
    pub ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Search query or fetched URL (truncated), empty for chat.
    pub note: String,
}

impl Usage {
    /// Add another usage (e.g. a forked client's) into this one.
    pub fn absorb(&mut self, other: &Usage) {
        self.chat_calls += other.chat_calls;
        self.input_tokens += other.input_tokens;
        self.cached_tokens += other.cached_tokens;
        self.output_tokens += other.output_tokens;
        self.searches += other.searches;
        self.fetches += other.fetches;
        self.failed_calls += other.failed_calls;
        self.cost_usd += other.cost_usd;
        for c in &other.calls {
            self.push_call(c.clone());
        }
    }

    fn push_call(&mut self, c: CallRecord) {
        if self.calls.len() >= MAX_CALL_RECORDS {
            self.calls.remove(0);
        }
        self.calls.push(c);
    }
}

/// USD per million tokens at off-peak rates: (cache miss, cache hit, output).
fn token_prices(model: &str) -> (f64, f64, f64) {
    if model.contains("pro") {
        (0.66, 0.022, 1.98)
    } else {
        (0.15, 0.003, 0.6)
    }
}

/// DeepSeek peak hours: 01:00–04:00 and 06:00–10:00 UTC, Monday to Friday. Tokens cost double.
pub fn is_peak(unix: i64) -> bool {
    let h = model::date::hour(unix);
    model::date::weekday(unix) < 5 && ((1..4).contains(&h) || (6..10).contains(&h))
}

/// Estimated USD for one chat call's tokens.
pub fn chat_cost(model: &str, miss: u64, hit: u64, out: u64, peak: bool) -> f64 {
    let (pm, ph, po) = token_prices(model);
    let k = if peak { 2.0 } else { 1.0 };
    k * (miss as f64 * pm + hit as f64 * ph + out as f64 * po) / 1_000_000.0
}

// ---------------------------------------------------------------------------------------------
// Chat
// ---------------------------------------------------------------------------------------------

/// Thinking mode. Off is fastest and cheapest; Low/High/Max think first, then answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Thinking {
    Off,
    Low,
    High,
    Max,
}

/// A piece of the user message.
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Text(String),
    /// An image sent inline (base64). `mime` is image/jpeg, image/png, image/gif or image/webp.
    Image {
        mime: String,
        bytes: Vec<u8>,
    },
}

/// One chat completion: a system prompt and one user message.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatRequest {
    /// The pipeline step ("plan", "evaluate", "curate"…), for logs, usage and the mock server.
    pub stage: String,
    pub system: String,
    pub user: Vec<Part>,
    /// Ask for a JSON object (`response_format: json_object`). The prompt must mention JSON.
    pub json: bool,
    pub thinking: Thinking,
    /// Cap on output tokens, thinking included.
    pub max_tokens: u32,
    /// Only applies with thinking off.
    pub temperature: Option<f32>,
    /// Send images at low detail (512×512, far fewer tokens). On by default.
    pub low_detail_images: bool,
}

impl ChatRequest {
    pub fn new(
        stage: impl Into<String>,
        system: impl Into<String>,
        user: impl Into<String>,
    ) -> ChatRequest {
        ChatRequest {
            stage: stage.into(),
            system: system.into(),
            user: vec![Part::Text(user.into())],
            json: false,
            thinking: Thinking::Off,
            max_tokens: 4096,
            temperature: None,
            low_detail_images: true,
        }
    }
    pub fn json(mut self) -> Self {
        self.json = true;
        self
    }
    pub fn thinking(mut self, t: Thinking) -> Self {
        self.thinking = t;
        self
    }
    pub fn max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n.max(16);
        self
    }
    pub fn temperature(mut self, t: f32) -> Self {
        self.temperature = Some(t.clamp(0.0, 2.0));
        self
    }
    pub fn text(mut self, t: impl Into<String>) -> Self {
        self.user.push(Part::Text(t.into()));
        self
    }
    pub fn image(mut self, mime: impl Into<String>, bytes: Vec<u8>) -> Self {
        self.user.push(Part::Image {
            mime: mime.into(),
            bytes,
        });
        self
    }

    fn body(&self, model: &str) -> Value {
        let only_text = self.user.iter().all(|p| matches!(p, Part::Text(_)));
        let user_content = if only_text {
            let texts: Vec<&str> = self
                .user
                .iter()
                .filter_map(|p| {
                    if let Part::Text(t) = p {
                        Some(t.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            Value::String(texts.join("\n\n"))
        } else {
            let detail = if self.low_detail_images {
                "low"
            } else {
                "auto"
            };
            Value::Array(
                self.user
                    .iter()
                    .map(|p| match p {
                        Part::Text(t) => json!({ "type": "text", "text": t }),
                        Part::Image { mime, bytes } => json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)),
                                "detail": detail,
                            }
                        }),
                    })
                    .collect(),
            )
        };
        let mut body = json!({
            "model": model,
            "messages": [
                { "role": "system", "content": self.system },
                { "role": "user", "content": user_content },
            ],
            "max_tokens": self.max_tokens,
            "stream": false,
        });
        match self.thinking {
            Thinking::Off => {
                body["thinking"] = json!({ "type": "disabled" });
                if let Some(t) = self.temperature {
                    body["temperature"] = json!(t);
                }
            }
            t => {
                body["thinking"] = json!({ "type": "enabled" });
                body["reasoning_effort"] = json!(match t {
                    Thinking::Low => "low",
                    Thinking::Max => "max",
                    _ => "high",
                });
            }
        }
        if self.json {
            body["response_format"] = json!({ "type": "json_object" });
        }
        body
    }
}

/// A chat completion's answer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatOutput {
    pub content: String,
    /// The model's thinking, when thinking was on.
    pub reasoning: String,
    /// "stop", "length", "content_filter"…
    pub finish_reason: String,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
}

// ---------------------------------------------------------------------------------------------
// Search and Fetch
// ---------------------------------------------------------------------------------------------

/// A Search API request. `query` supports operators: `site:`, `-site:`, `"exact"`, `-term`,
/// `lang:en`, `after:2026-01-01`, `before:2026-06-01`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchRequest {
    /// 1–128 characters; longer queries are cut at a word boundary.
    pub query: String,
    /// 1–50 results.
    pub count: u32,
    /// Also return each page's full content (much bigger replies).
    pub need_content: bool,
    /// The question behind the search, to help the engine understand it (≤128 chars).
    pub ori_question: Option<String>,
    /// An IP address to localise results to.
    pub localization: Option<String>,
}

impl SearchRequest {
    pub fn new(query: impl Into<String>, count: u32) -> SearchRequest {
        SearchRequest {
            query: query.into(),
            count,
            ..Default::default()
        }
    }
}

/// One Search API result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchResult {
    /// 0–3; 1.5 or more is a high-quality match.
    pub rerank_score: f64,
    pub title: String,
    pub url: String,
    /// 100–200 characters, for display.
    pub snippet: String,
    /// 600–800 characters, for a model to read.
    pub summary: String,
    /// Full page text when `need_content` was set.
    pub content: String,
    pub site_name: Option<String>,
    pub site_icon: Option<String>,
    /// RFC 3339 publication time, if known.
    pub date: Option<String>,
    /// 0–4 (3+ is highly authoritative), −1 unknown.
    pub authority: i32,
}

/// A page read by the Fetch API, as Markdown.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Page {
    pub title: String,
    pub content: String,
}

// ---------------------------------------------------------------------------------------------
// Other chat providers
// ---------------------------------------------------------------------------------------------

/// A model other than DeepSeek's that answers chat requests — e.g. Claude on Amazon Bedrock,
/// in the customer's own cloud account. A [`Client`] with a backend sends every chat call to it;
/// search and fetch always stay on DeepSeek's Search and Fetch APIs.
///
/// Implementations must honour [`ChatRequest::json`] (answer with a JSON object as text, with no
/// prose around it), accept [`Part::Image`]s or fail clearly, map their failures onto [`Error`]
/// (401/403 → `Unauthorized`, throttling → `RateLimited`, …) and retry what is retryable
/// themselves.
pub trait ChatBackend: Send + Sync {
    /// Short and stable, for logs and usage records: "bedrock:us.anthropic.claude-sonnet-4-5…".
    fn name(&self) -> String;
    /// Answer one request.
    fn chat(&self, req: &ChatRequest) -> Result<ChatOutput>;
    /// Estimated USD for one answer. 0.0 when it is billed elsewhere (the customer's account).
    fn cost_usd(&self, out: &ChatOutput) -> f64 {
        let _ = out;
        0.0
    }
}

// ---------------------------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct Client {
    cfg: Arc<Config>,
    agent: ureq::Agent,
    usage: Arc<Mutex<Usage>>,
    backend: Option<Arc<dyn ChatBackend>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.cfg.base_url)
            .field("chat_model", &self.chat_model())
            .finish()
    }
}

/// What one HTTP exchange produced.
struct Reply {
    status: u16,
    body: String,
    attempts: u32,
    ms: u64,
}

impl Client {
    pub fn new(cfg: Config) -> Client {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(concat!("opportunity/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(15)))
            .build()
            .into();
        Client {
            cfg: Arc::new(cfg),
            agent,
            usage: Arc::new(Mutex::new(Usage::default())),
            backend: None,
        }
    }

    /// A client that shares this one's configuration, connections and chat backend but counts
    /// usage separately.
    pub fn fork(&self) -> Client {
        Client {
            cfg: self.cfg.clone(),
            agent: self.agent.clone(),
            usage: Arc::new(Mutex::new(Usage::default())),
            backend: self.backend.clone(),
        }
    }

    /// A fork whose chat calls go to `backend` (search and fetch stay on DeepSeek).
    pub fn with_chat_backend(&self, backend: Arc<dyn ChatBackend>) -> Client {
        let mut c = self.fork();
        c.backend = Some(backend);
        c
    }

    /// A fork that uses another DeepSeek chat model (e.g. "deepseek-v4-pro") and no backend.
    pub fn with_model(&self, model: impl Into<String>) -> Client {
        let mut cfg = (*self.cfg).clone();
        cfg.model = model.into();
        Client {
            cfg: Arc::new(cfg),
            agent: self.agent.clone(),
            usage: Arc::new(Mutex::new(Usage::default())),
            backend: None,
        }
    }

    /// What answers chat calls: the DeepSeek model, or the backend's name.
    pub fn chat_model(&self) -> String {
        match &self.backend {
            Some(b) => b.name(),
            None => self.cfg.model.clone(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// A snapshot of what this client (not its forks) has used.
    pub fn usage(&self) -> Usage {
        self.usage.lock().map(|u| u.clone()).unwrap_or_default()
    }

    fn record(&self, f: impl FnOnce(&mut Usage)) {
        if let Ok(mut u) = self.usage.lock() {
            f(&mut u);
        }
    }

    // ----- chat ---------------------------------------------------------------------------------

    /// One chat completion (by the backend, when there is one).
    pub fn chat(&self, req: &ChatRequest) -> Result<ChatOutput> {
        if let Some(backend) = &self.backend {
            return self.chat_via(backend.as_ref(), req);
        }
        let body = req.body(&self.cfg.model);
        let peak = is_peak(model::date::now_unix());
        let reply = match self.post(
            "/v1/chat/completions",
            &req.stage,
            &body,
            self.cfg.chat_timeout,
        ) {
            Ok(r) => r,
            Err((e, attempts, ms)) => {
                self.record(|u| {
                    u.failed_calls += 1;
                    u.push_call(CallRecord {
                        kind: "chat".into(),
                        stage: req.stage.clone(),
                        status: e.status(),
                        attempts,
                        ms,
                        ..Default::default()
                    });
                });
                return Err(e);
            }
        };
        let v: Value = serde_json::from_str(&reply.body).map_err(|e| {
            Error::InvalidResponse(format!(
                "chat reply is not JSON ({e}): {}",
                snippet(&reply.body)
            ))
        })?;
        let msg = &v["choices"][0]["message"];
        let usage = &v["usage"];
        let total_in = usage["prompt_tokens"].as_u64().unwrap_or(0);
        let hit = usage["prompt_cache_hit_tokens"]
            .as_u64()
            .or_else(|| usage["prompt_tokens_details"]["cached_tokens"].as_u64())
            .unwrap_or(0);
        let miss = usage["prompt_cache_miss_tokens"]
            .as_u64()
            .unwrap_or(total_in.saturating_sub(hit));
        let out_tokens = usage["completion_tokens"].as_u64().unwrap_or(0);
        let out = ChatOutput {
            content: msg["content"].as_str().unwrap_or("").to_string(),
            reasoning: msg["reasoning_content"].as_str().unwrap_or("").to_string(),
            finish_reason: v["choices"][0]["finish_reason"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            input_tokens: miss,
            cached_tokens: hit,
            output_tokens: out_tokens,
        };
        let cost = chat_cost(&self.cfg.model, miss, hit, out_tokens, peak);
        self.record(|u| {
            u.chat_calls += 1;
            u.input_tokens += miss;
            u.cached_tokens += hit;
            u.output_tokens += out_tokens;
            u.cost_usd += cost;
            u.push_call(CallRecord {
                kind: "chat".into(),
                stage: req.stage.clone(),
                status: reply.status,
                attempts: reply.attempts,
                ms: reply.ms,
                tokens_in: miss + hit,
                tokens_out: out_tokens,
                note: String::new(),
            });
        });
        if v["choices"][0].is_null() {
            return Err(Error::InvalidResponse(format!(
                "chat reply has no choices: {}",
                snippet(&reply.body)
            )));
        }
        Ok(out)
    }

    fn chat_via(&self, backend: &dyn ChatBackend, req: &ChatRequest) -> Result<ChatOutput> {
        let started = Instant::now();
        let result = backend.chat(req);
        let ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(out) => {
                let cost = backend.cost_usd(&out);
                self.record(|u| {
                    u.chat_calls += 1;
                    u.input_tokens += out.input_tokens;
                    u.cached_tokens += out.cached_tokens;
                    u.output_tokens += out.output_tokens;
                    u.cost_usd += cost;
                    u.push_call(CallRecord {
                        kind: "chat".into(),
                        stage: req.stage.clone(),
                        status: 200,
                        attempts: 1,
                        ms,
                        tokens_in: out.input_tokens + out.cached_tokens,
                        tokens_out: out.output_tokens,
                        note: backend.name(),
                    });
                });
                Ok(out)
            }
            Err(e) => {
                self.record(|u| {
                    u.failed_calls += 1;
                    u.push_call(CallRecord {
                        kind: "chat".into(),
                        stage: req.stage.clone(),
                        status: e.status(),
                        attempts: 1,
                        ms,
                        note: backend.name(),
                        ..Default::default()
                    });
                });
                Err(e)
            }
        }
    }

    /// A chat completion in JSON mode, parsed. Retries (up to twice) when the reply is blank,
    /// isn't valid JSON or was cut off, doubling `max_tokens` after a cut-off.
    pub fn chat_json_value(&self, req: &ChatRequest) -> Result<Value> {
        let mut req = req.clone();
        req.json = true;
        let mut last = String::new();
        for _ in 0..3 {
            let out = self.chat(&req)?;
            if let Some(v) = extract_json(&out.content)
                && (v.is_object() || v.is_array())
            {
                return Ok(v);
            }
            last = if out.content.trim().is_empty() {
                format!("blank reply (finish_reason {})", out.finish_reason)
            } else {
                format!(
                    "not JSON (finish_reason {}): {}",
                    out.finish_reason,
                    snippet(&out.content)
                )
            };
            if out.finish_reason == "length" {
                req.max_tokens = (req.max_tokens.saturating_mul(2)).min(65_536);
            }
        }
        Err(Error::InvalidResponse(format!(
            "stage {}: {last}",
            req.stage
        )))
    }

    /// A chat completion in JSON mode, deserialised into `T`. Use `#[serde(default)]` types so a
    /// missing field doesn't fail the whole reply.
    pub fn chat_json<T: DeserializeOwned>(&self, req: &ChatRequest) -> Result<T> {
        let v = self.chat_json_value(req)?;
        serde_json::from_value(v.clone()).map_err(|e| {
            Error::InvalidResponse(format!(
                "stage {}: JSON has the wrong shape ({e}): {}",
                req.stage,
                snippet(&v.to_string())
            ))
        })
    }

    // ----- search and fetch ---------------------------------------------------------------------

    /// Search the web with the Search API.
    pub fn search(&self, req: &SearchRequest) -> Result<Vec<SearchResult>> {
        let query = clamp_query(&req.query)
            .ok_or_else(|| Error::BadRequest(400, "empty search query".into()))?;
        let mut body = json!({
            "query": query,
            "count": req.count.clamp(1, 50),
            "need_content": req.need_content,
        });
        if let Some(q) = req.ori_question.as_deref().and_then(clamp_query) {
            body["ori_question"] = json!(q);
        }
        if let Some(ip) = req
            .localization
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            body["localization"] = json!(ip);
        }
        let note = model::text::truncate(&query, 120);
        let reply = match self.post("/v1/tools/search", "search", &body, self.cfg.tool_timeout) {
            Ok(r) => r,
            Err((e, attempts, ms)) => {
                self.record(|u| {
                    u.failed_calls += 1;
                    u.push_call(CallRecord {
                        kind: "search".into(),
                        stage: "search".into(),
                        status: e.status(),
                        attempts,
                        ms,
                        note: note.clone(),
                        ..Default::default()
                    });
                });
                return Err(e);
            }
        };
        #[derive(Deserialize)]
        struct Results {
            #[serde(default)]
            results: Vec<SearchResult>,
        }
        let parsed: Results = serde_json::from_str(&reply.body).map_err(|e| {
            Error::InvalidResponse(format!("search reply ({e}): {}", snippet(&reply.body)))
        })?;
        let results: Vec<SearchResult> = parsed
            .results
            .into_iter()
            .filter(|r| model::links::is_http_url(&r.url))
            .collect();
        let billable = !results.is_empty();
        self.record(|u| {
            if billable {
                u.searches += 1;
                u.cost_usd += TOOL_CALL_USD;
            }
            u.push_call(CallRecord {
                kind: "search".into(),
                stage: "search".into(),
                status: reply.status,
                attempts: reply.attempts,
                ms: reply.ms,
                note,
                ..Default::default()
            });
        });
        Ok(results)
    }

    /// Read one page with the Fetch API, as Markdown.
    pub fn fetch(&self, url: &str) -> Result<Page> {
        let url = url.trim();
        if !model::links::is_http_url(url) || url.len() > 8192 {
            return Err(Error::BadRequest(
                400,
                format!("not a fetchable URL: {}", model::text::truncate(url, 100)),
            ));
        }
        let note = model::text::truncate(url, 160);
        let reply = match self.post(
            "/v1/tools/fetch",
            "fetch",
            &json!({ "url": url }),
            self.cfg.tool_timeout,
        ) {
            Ok(r) => r,
            Err((e, attempts, ms)) => {
                self.record(|u| {
                    u.failed_calls += 1;
                    u.push_call(CallRecord {
                        kind: "fetch".into(),
                        stage: "fetch".into(),
                        status: e.status(),
                        attempts,
                        ms,
                        note: note.clone(),
                        ..Default::default()
                    });
                });
                return Err(e);
            }
        };
        let page: Page = serde_json::from_str(&reply.body).map_err(|e| {
            Error::InvalidResponse(format!("fetch reply ({e}): {}", snippet(&reply.body)))
        })?;
        let billable = !page.content.trim().is_empty();
        self.record(|u| {
            if billable {
                u.fetches += 1;
                u.cost_usd += TOOL_CALL_USD;
            }
            u.push_call(CallRecord {
                kind: "fetch".into(),
                stage: "fetch".into(),
                status: reply.status,
                attempts: reply.attempts,
                ms: reply.ms,
                note,
                ..Default::default()
            });
        });
        if !billable {
            return Err(Error::Unavailable(
                404,
                "the page has no readable content".into(),
            ));
        }
        Ok(page)
    }

    /// The account's total balance in USD (for health checks and budget guards).
    pub fn balance_usd(&self) -> Result<f64> {
        let url = format!("{}/user/balance", self.cfg.base_url);
        let resp = self
            .agent
            .get(&url)
            .config()
            .timeout_global(Some(self.cfg.tool_timeout))
            .build()
            .header("Authorization", &format!("Bearer {}", self.cfg.api_key))
            .call();
        let mut resp = resp.map_err(|e| Error::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::Network(e.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(status_error(status, api_error_message(&body)));
        }
        let v: Value =
            serde_json::from_str(&body).map_err(|e| Error::InvalidResponse(e.to_string()))?;
        let infos = v["balance_infos"].as_array().cloned().unwrap_or_default();
        let usd = infos
            .iter()
            .find(|i| i["currency"] == "USD")
            .or_else(|| infos.first())
            .and_then(|i| {
                i["total_balance"]
                    .as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .or_else(|| i["total_balance"].as_f64())
            })
            .unwrap_or(0.0);
        Ok(usd)
    }

    // ----- transport ----------------------------------------------------------------------------

    /// POST JSON with retries. Errors carry the attempts and elapsed time for the call record.
    fn post(
        &self,
        path: &str,
        stage: &str,
        body: &Value,
        timeout: Duration,
    ) -> std::result::Result<Reply, (Error, u32, u64)> {
        let url = format!("{}{}", self.cfg.base_url, path);
        let payload = body.to_string();
        let started = Instant::now();
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let result = self
                .agent
                .post(&url)
                .config()
                .timeout_global(Some(timeout))
                .build()
                .header("Authorization", &format!("Bearer {}", self.cfg.api_key))
                .header("Content-Type", "application/json")
                .header("X-Opp-Stage", stage)
                .send(payload.as_bytes());
            let elapsed = || started.elapsed().as_millis() as u64;
            let retry_left = attempt <= self.cfg.max_retries;
            match result {
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    let retry_after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.trim().parse::<u64>().ok());
                    let text = match resp
                        .body_mut()
                        .with_config()
                        .limit(64 * 1024 * 1024)
                        .read_to_string()
                    {
                        Ok(t) => t,
                        Err(e) => {
                            if retry_left {
                                std::thread::sleep(self.backoff(attempt, None));
                                continue;
                            }
                            return Err((
                                Error::Network(format!("reading the reply: {e}")),
                                attempt,
                                elapsed(),
                            ));
                        }
                    };
                    if (200..300).contains(&status) {
                        return Ok(Reply {
                            status,
                            body: text,
                            attempts: attempt,
                            ms: elapsed(),
                        });
                    }
                    if is_retryable(status) && retry_left {
                        std::thread::sleep(self.backoff(attempt, retry_after));
                        continue;
                    }
                    return Err((
                        status_error(status, api_error_message(&text)),
                        attempt,
                        elapsed(),
                    ));
                }
                Err(e) => {
                    if retry_left {
                        std::thread::sleep(self.backoff(attempt, None));
                        continue;
                    }
                    return Err((Error::Network(e.to_string()), attempt, elapsed()));
                }
            }
        }
    }

    fn backoff(&self, attempt: u32, retry_after_secs: Option<u64>) -> Duration {
        if let Some(s) = retry_after_secs {
            return Duration::from_secs(s.min(20));
        }
        let base = self
            .cfg
            .retry_base
            .saturating_mul(1 << (attempt - 1).min(6));
        // Cheap jitter: up to a quarter of the delay, from the clock's nanoseconds.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0) as u64;
        let quarter = (base.as_millis() as u64 / 4).max(1);
        base + Duration::from_millis(nanos % quarter)
    }
}

fn is_retryable(status: u16) -> bool {
    matches!(status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504)
}

fn status_error(status: u16, msg: String) -> Error {
    match status {
        401 => Error::Unauthorized(msg),
        402 => Error::InsufficientBalance(msg),
        429 => Error::RateLimited(msg),
        403 | 404 => Error::Unavailable(status, msg),
        500..=599 => Error::Server(status, msg),
        408 => Error::Network(format!("timed out: {msg}")),
        _ => Error::BadRequest(status, msg),
    }
}

/// The `error.message` of an OpenAI-format error body, or a snippet of whatever came back.
fn api_error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| snippet(body))
}

fn snippet(s: &str) -> String {
    model::text::truncate(&model::text::squash(s), 300)
}

/// A search query trimmed to 1–128 characters, cut at a word boundary. None if blank.
pub fn clamp_query(q: &str) -> Option<String> {
    let q = model::text::squash(q);
    if q.is_empty() {
        return None;
    }
    if q.chars().count() <= 128 {
        return Some(q);
    }
    let cut: String = q.chars().take(128).collect();
    let at_word = match cut.rfind(' ') {
        Some(i) if i > 64 => cut[..i].to_string(),
        _ => cut,
    };
    Some(at_word.trim().to_string())
}

/// The JSON value in a model reply: the whole text, or inside ``` fences, or the outermost
/// {...} / [...] span. None when there is none.
pub fn extract_json(text: &str) -> Option<Value> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return Some(v);
    }
    if let Some(start) = t.find("```") {
        let after = &t[start + 3..];
        let after = after
            .strip_prefix("json")
            .or_else(|| after.strip_prefix("JSON"))
            .unwrap_or(after);
        if let Some(end) = after.find("```")
            && let Ok(v) = serde_json::from_str::<Value>(after[..end].trim())
        {
            return Some(v);
        }
    }
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let (Some(s), Some(e)) = (t.find(open), t.rfind(close))
            && s < e
            && let Ok(v) = serde_json::from_str::<Value>(&t[s..=e])
        {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests;
