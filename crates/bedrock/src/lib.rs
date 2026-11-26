//! Claude (or another Bedrock chat model) in the customer's own AWS account, as a
//! [`deepseek::ChatBackend`]: give a `deepseek::Client` this backend with `with_chat_backend` and
//! every chat call of the engine and the imagery pick goes to Bedrock, while web search stays on
//! DeepSeek's Search API.
//!
//! * Converse API: `POST https://bedrock-runtime.{region}.amazonaws.com/model/{modelId}/converse`,
//!   where `modelId` is a model id (`anthropic.claude-…`), an inference profile id
//!   (`us.anthropic.claude-…`) or an ARN (foundation model, inference profile, application
//!   inference profile, provisioned throughput), percent-encoded as one path segment.
//! * Auth: a Bedrock API key (`Authorization: Bearer …`), or IAM access keys signed with SigV4
//!   (service `bedrock`; the path is signed URI-encoded once more, as for every service but S3).
//! * Listing models for the selector: `GET https://bedrock.{region}.amazonaws.com/foundation-models`
//!   and `/inference-profiles`.
//!
//! Every call retries what is worth retrying (throttling, a model that isn't ready, 5xx, model
//! timeouts, network errors) with exponential backoff, and maps Bedrock's errors onto
//! [`deepseek::Error`] with messages a customer can act on. When Bedrock refuses an optional
//! part of a request (thinking, temperature, too many output tokens for the model), the call is
//! made again without it, and the backend remembers, so later calls don't ask again.
//!
//! Owner: the Bedrock & vault builder.

mod converse;
mod error;
mod sigv4;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deepseek::{ChatOutput, ChatRequest};
use serde_json::Value;

use converse::{Plan, Think};
use error::{AwsError, Ctx, Failure, Op};

#[derive(Clone, PartialEq)]
pub enum Auth {
    /// A Bedrock API key (sent as a bearer token).
    ApiKey(String),
    /// IAM credentials, signed with SigV4.
    Keys {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
}

/// Secrets never show in logs: only the end of the access key ID, which identifies a key but
/// grants nothing.
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::ApiKey(_) => f.debug_tuple("ApiKey").field(&"<redacted>").finish(),
            Auth::Keys {
                access_key_id,
                session_token,
                ..
            } => {
                let tail: String = access_key_id
                    .chars()
                    .rev()
                    .take(4)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                f.debug_struct("Keys")
                    .field("access_key_id", &format!("…{tail}"))
                    .field("secret_access_key", &"<redacted>")
                    .field(
                        "session_token",
                        &session_token.as_ref().map(|_| "<redacted>"),
                    )
                    .finish()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// e.g. "us-east-1". Taken from the ARN when the model is an ARN and this is empty.
    pub region: String,
    /// Model id, inference profile id, or ARN.
    pub model: String,
    pub auth: Auth,
    /// Longest one attempt may take.
    pub timeout: Duration,
    /// Extra attempts after a retryable failure.
    pub max_retries: u32,
    /// First retry delay; doubles each attempt, plus jitter. Tests set it tiny.
    pub retry_base: Duration,
    /// Let requests that ask for thinking use Claude's extended thinking. Off by default: Claude
    /// is strong without it, and an arbitrary model or ARN may not support it. When on, adaptive
    /// thinking is tried first (Claude 4.6 and later), then a thinking budget (Claude 3.7 to
    /// 4.5), then none; whatever the model accepts is remembered.
    pub thinking: bool,
    /// Replaces `https://bedrock-runtime.{region}.amazonaws.com` (tests point it at the mock).
    pub runtime_endpoint: Option<String>,
    /// Replaces `https://bedrock.{region}.amazonaws.com` for listing models (tests).
    pub control_endpoint: Option<String>,
}

impl Config {
    pub fn new(region: impl Into<String>, model: impl Into<String>, auth: Auth) -> Config {
        Config {
            region: region.into(),
            model: model.into(),
            auth,
            timeout: Duration::from_secs(180),
            max_retries: 3,
            retry_base: Duration::from_millis(1000),
            thinking: false,
            runtime_endpoint: std::env::var("BEDROCK_RUNTIME_ENDPOINT")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            control_endpoint: std::env::var("BEDROCK_CONTROL_ENDPOINT")
                .ok()
                .filter(|s| !s.trim().is_empty()),
        }
    }
}

/// A model the account can use, for the selector.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ModelChoice {
    pub id: String,
    pub name: String,
}

/// The region in a Bedrock ARN (`arn:aws:bedrock:us-east-1:…` → `us-east-1`).
pub fn region_from_arn(arn: &str) -> Option<String> {
    let parts: Vec<&str> = arn.split(':').collect();
    (parts.len() >= 6 && parts[0] == "arn" && parts[2] == "bedrock" && !parts[3].is_empty())
        .then(|| parts[3].to_string())
}

pub struct Bedrock {
    cfg: Config,
    http: Http,
    /// Send `X-Opp-Stage` (to the mock only, never to AWS).
    stage_header: bool,
    /// What the model accepted for thinking, as [`Think::as_u8`].
    think: AtomicU8,
    /// False once the model refused `temperature`.
    temperature_ok: AtomicBool,
    /// An output-token limit the model announced, 0 while none is known.
    max_tokens_cap: AtomicU32,
}

impl Bedrock {
    /// Checks the config (region, model, credentials present) without calling AWS. The region
    /// comes from the model's ARN when the config's region is empty.
    pub fn new(cfg: Config) -> Result<Bedrock, deepseek::Error> {
        let cfg = normalize(cfg, Purpose::Chat)?;
        let http = Http::new(&cfg)?;
        let stage_header = cfg.runtime_endpoint.is_some() && !http.runtime.is_aws();
        Ok(Bedrock {
            cfg,
            http,
            stage_header,
            think: AtomicU8::new(Think::Adaptive.as_u8()),
            temperature_ok: AtomicBool::new(true),
            max_tokens_cap: AtomicU32::new(0),
        })
    }

    /// The checked config: trimmed, with the region filled in.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// One tiny real call, for the "Test connection" button. Ok with a short note on success.
    pub fn test_connection(&self) -> Result<String, deepseek::Error> {
        let req = ChatRequest::new("connection-test", "", "Reply with OK").max_tokens(16);
        let out = deepseek::ChatBackend::chat(self, &req)?;
        let reply = model::text::truncate(&model::text::squash(&out.content), 40);
        let place = format!("{} in {}", self.cfg.model, self.cfg.region);
        Ok(if reply.is_empty() {
            format!("Connected: {place} answered, with an empty reply.")
        } else {
            format!("Connected: {place} answered \u{201c}{reply}\u{201d}.")
        })
    }

    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            op: Op::Converse,
            model: &self.cfg.model,
            region: &self.cfg.region,
        }
    }

    /// Remember what Bedrock refused, so later calls skip it.
    fn learn(&self, first: &Plan, last: &Plan) {
        if first.think != Think::Off && last.think != first.think {
            self.think.store(last.think.as_u8(), Ordering::Relaxed);
        }
        if first.temperature.is_some() && last.temperature.is_none() {
            self.temperature_ok.store(false, Ordering::Relaxed);
        }
        if let Some(cap) = last.cap
            && first.cap != last.cap
        {
            self.max_tokens_cap.store(cap, Ordering::Relaxed);
        }
    }
}

impl deepseek::ChatBackend for Bedrock {
    fn name(&self) -> String {
        format!("bedrock:{}", self.cfg.model)
    }

    fn chat(&self, req: &ChatRequest) -> Result<ChatOutput, deepseek::Error> {
        let think = if self.cfg.thinking {
            Think::from_u8(self.think.load(Ordering::Relaxed))
        } else {
            Think::Off
        };
        let cap = Some(self.max_tokens_cap.load(Ordering::Relaxed)).filter(|&c| c > 0);
        let first = Plan::new(req, think, self.temperature_ok.load(Ordering::Relaxed), cap);
        let path = converse_path(&self.cfg.model);
        let stage = self.stage_header.then_some(req.stage.as_str());
        let mut plan = first.clone();
        let mut tries = 0u32;
        loop {
            tries += 1;
            let body = converse::request_body(req, &plan)?.to_string();
            match self
                .http
                .send(Target::Runtime, &path, &[], Some(body.as_bytes()), stage)
            {
                Ok(text) => {
                    self.learn(&first, &plan);
                    return converse::parse_reply(&text, req.json);
                }
                // Each fallback drops or lowers something, so this ends; the cap is a backstop.
                Err(Failure::Aws(e)) if e.is_validation() => match plan.fallback(&e.message) {
                    Some(next) if tries < 8 => plan = next,
                    _ => return Err(e.to_error(&self.ctx())),
                },
                Err(f) => return Err(f.to_error(&self.ctx())),
            }
        }
    }

    /// Always 0.0: Bedrock bills the customer's own AWS account, not ours.
    fn cost_usd(&self, out: &ChatOutput) -> f64 {
        let _ = out;
        0.0
    }
}

/// The chat models (Anthropic first) and inference profiles the account can use in its region:
/// foundation models with text output and on-demand throughput that aren't legacy, and active
/// inference profiles (system-defined ones by id, application ones by ARN, since that is how
/// they are called). The profiles need `bedrock:ListInferenceProfiles`; without it, the
/// foundation models alone come back.
pub fn list_models(cfg: &Config) -> Result<Vec<ModelChoice>, deepseek::Error> {
    let cfg = normalize(cfg.clone(), Purpose::List)?;
    let http = Http::new(&cfg)?;
    let ctx = Ctx {
        op: Op::ListModels,
        model: &cfg.model,
        region: &cfg.region,
    };
    let found = http
        .get_json(
            "/foundation-models",
            &params(&[("byOutputModality", "TEXT")]),
        )
        .map_err(|f| f.to_error(&ctx))?;
    // (Anthropic?, choice)
    let mut choices: Vec<(bool, ModelChoice)> = Vec::new();
    for m in found["modelSummaries"].as_array().into_iter().flatten() {
        let id = m["modelId"].as_str().unwrap_or("").trim();
        let has = |field: &str, value: &str| {
            m[field]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(value)))
        };
        let legacy = m["modelLifecycle"]["status"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("LEGACY"));
        let text_out =
            m["outputModalities"].as_array().is_none() || has("outputModalities", "TEXT");
        if id.is_empty() || legacy || !text_out || !has("inferenceTypesSupported", "ON_DEMAND") {
            continue;
        }
        let provider = m["providerName"].as_str().unwrap_or("").trim();
        let name = m["modelName"]
            .as_str()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or(id);
        let name = if provider.is_empty() {
            name.to_string()
        } else {
            format!("{provider} {name}")
        };
        let anthropic = provider.eq_ignore_ascii_case("anthropic") || id.contains("anthropic.");
        choices.push((
            anthropic,
            ModelChoice {
                id: id.to_string(),
                name,
            },
        ));
    }
    for kind in ["SYSTEM_DEFINED", "APPLICATION"] {
        let mut token: Option<String> = None;
        for _page in 0..20 {
            let mut query = params(&[("maxResults", "1000"), ("type", kind)]);
            if let Some(t) = &token {
                query.push(("nextToken".into(), t.clone()));
            }
            let Ok(page) = http.get_json("/inference-profiles", &query) else {
                break;
            };
            for p in page["inferenceProfileSummaries"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if p["status"]
                    .as_str()
                    .is_some_and(|s| !s.eq_ignore_ascii_case("ACTIVE"))
                {
                    continue;
                }
                let application = p["type"].as_str() == Some("APPLICATION");
                let id = if application {
                    p["inferenceProfileArn"].as_str()
                } else {
                    p["inferenceProfileId"].as_str()
                }
                .unwrap_or("")
                .trim();
                if id.is_empty() {
                    continue;
                }
                let name = p["inferenceProfileName"]
                    .as_str()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .unwrap_or(id);
                let lower = name.to_ascii_lowercase();
                let anthropic = id.contains("anthropic")
                    || lower.contains("anthropic")
                    || lower.contains("claude")
                    || p["models"].as_array().is_some_and(|ms| {
                        ms.iter()
                            .any(|m| m["modelArn"].as_str().unwrap_or("").contains("anthropic"))
                    });
                choices.push((
                    anthropic,
                    ModelChoice {
                        id: id.to_string(),
                        name: name.to_string(),
                    },
                ));
            }
            token = page["nextToken"]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            if token.is_none() {
                break;
            }
        }
    }
    choices.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()))
            .then_with(|| a.1.id.cmp(&b.1.id))
    });
    let mut seen = HashSet::new();
    Ok(choices
        .into_iter()
        .map(|(_, c)| c)
        .filter(|c| seen.insert(c.id.clone()))
        .collect())
}

/// `/model/{modelId}/converse` with the model id percent-encoded as one path segment, so an
/// ARN's `:` and `/` go out as `%3A` and `%2F`.
pub(crate) fn converse_path(model: &str) -> String {
    format!("/model/{}/converse", sigv4::uri_encode(model))
}

fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Config checks
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Chat,
    List,
}

fn bad(msg: impl Into<String>) -> deepseek::Error {
    deepseek::Error::BadRequest(400, msg.into())
}

/// The config trimmed and checked, with the region resolved. Listing models doesn't need a
/// model, and when the config names a region it lists that region even if the model is an ARN
/// elsewhere (the selector may be mid-change).
fn normalize(mut cfg: Config, purpose: Purpose) -> Result<Config, deepseek::Error> {
    cfg.model = cfg.model.trim().to_string();
    if purpose == Purpose::Chat {
        if cfg.model.is_empty() {
            return Err(bad(
                "Enter a Bedrock model id, inference profile id or ARN.",
            ));
        }
        if cfg.model.len() > 2048 {
            return Err(bad(
                "That model id is too long: Bedrock takes up to 2,048 characters.",
            ));
        }
        if cfg
            .model
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
            || cfg.model == "."
            || cfg.model == ".."
        {
            return Err(bad(
                "The model id has spaces or characters in it that a model id can't have; copy it again.",
            ));
        }
    }
    let region = cfg.region.trim().to_ascii_lowercase();
    cfg.region = match (region.is_empty(), region_from_arn(&cfg.model)) {
        (true, Some(from_arn)) => from_arn,
        (true, None) => {
            return Err(bad(
                "Choose the AWS region the model runs in, such as us-east-1.",
            ));
        }
        (false, Some(from_arn)) if purpose == Purpose::Chat && from_arn != region => {
            return Err(bad(format!(
                "The model's ARN is in {from_arn}, but the region is set to {region}. Use {from_arn}, or a model in {region}."
            )));
        }
        (false, _) => region,
    };
    if !valid_region(&cfg.region) {
        return Err(bad(format!(
            "\"{}\" isn't an AWS region name; they look like us-east-1.",
            model::text::truncate(&cfg.region, 40)
        )));
    }
    cfg.auth = normalize_auth(cfg.auth)?;
    if cfg.timeout.is_zero() {
        cfg.timeout = Duration::from_secs(180);
    }
    for endpoint in [&mut cfg.runtime_endpoint, &mut cfg.control_endpoint] {
        *endpoint = endpoint
            .take()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty());
        if let Some(url) = endpoint.as_deref()
            && Endpoint::parse(url).is_none()
        {
            return Err(bad(format!(
                "Not an http(s) endpoint: {}",
                model::text::truncate(url, 80)
            )));
        }
    }
    Ok(cfg)
}

/// Letters, digits and hyphens, starting with a letter, with at least one hyphen: every AWS
/// region (us-east-1, us-gov-west-1, ap-southeast-4…), and nothing that could change the host
/// the credentials are sent to.
fn valid_region(r: &str) -> bool {
    r.len() <= 32
        && r.starts_with(|c: char| c.is_ascii_lowercase())
        && r.contains('-')
        && !r.ends_with('-')
        && !r.contains("--")
        && r.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn normalize_auth(auth: Auth) -> Result<Auth, deepseek::Error> {
    fn check(value: &str, what: &str) -> Result<(), deepseek::Error> {
        if value.is_empty() {
            return Err(bad(format!("Enter the {what}.")));
        }
        if value.len() > 8192 || !value.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(bad(format!(
                "The {what} has spaces or characters in it that it can't have; copy it again."
            )));
        }
        Ok(())
    }
    match auth {
        Auth::ApiKey(key) => {
            let key = key.trim().to_string();
            check(&key, "Bedrock API key")?;
            Ok(Auth::ApiKey(key))
        }
        Auth::Keys {
            access_key_id,
            secret_access_key,
            session_token,
        } => {
            let access_key_id = access_key_id.trim().to_string();
            let secret_access_key = secret_access_key.trim().to_string();
            let session_token = session_token
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty());
            check(&access_key_id, "AWS access key ID")?;
            check(&secret_access_key, "AWS secret access key")?;
            if let Some(t) = &session_token {
                check(t, "AWS session token")?;
            }
            Ok(Auth::Keys {
                access_key_id,
                secret_access_key,
                session_token,
            })
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------------------------

/// Bedrock replies are small; anything bigger is a mistake.
const MAX_REPLY_BYTES: u64 = 16 * 1024 * 1024;

/// An http(s) base URL, split for signing.
#[derive(Clone, Debug, PartialEq)]
struct Endpoint {
    scheme: &'static str,
    /// Lowercase `host[:port]`.
    authority: String,
    /// A path prefix without a trailing slash (`/bedrock-runtime` on the mock), usually empty.
    base_path: String,
}

impl Endpoint {
    fn parse(url: &str) -> Option<Endpoint> {
        let url = url.trim().trim_end_matches('/');
        let lower = url.to_ascii_lowercase();
        let (scheme, rest) = if lower.starts_with("https://") {
            ("https", &url[8..])
        } else if lower.starts_with("http://") {
            ("http", &url[7..])
        } else {
            return None;
        };
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let authority_ok = !authority.is_empty()
            && authority
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
        let path_ok = path
            .chars()
            .all(|c| c.is_ascii_graphic() && c != '?' && c != '#');
        (authority_ok && path_ok).then(|| Endpoint {
            scheme,
            authority: authority.to_ascii_lowercase(),
            base_path: path.to_string(),
        })
    }

    /// The `Host` header: the authority without the scheme's default port (as HTTP clients send).
    fn host_header(&self) -> &str {
        let default_port = if self.scheme == "https" {
            ":443"
        } else {
            ":80"
        };
        self.authority
            .strip_suffix(default_port)
            .unwrap_or(&self.authority)
    }

    fn hostname(&self) -> &str {
        match self.authority.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or(""),
            None => self.authority.split(':').next().unwrap_or(""),
        }
    }

    fn is_aws(&self) -> bool {
        let host = self.hostname();
        ["amazonaws.com", "amazonaws.com.cn", "api.aws"]
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}")))
    }
}

fn aws_domain(region: &str) -> &'static str {
    if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    }
}

#[derive(Clone, Copy)]
enum Target {
    Runtime,
    Control,
}

struct Http {
    agent: ureq::Agent,
    runtime: Endpoint,
    control: Endpoint,
    region: String,
    auth: Auth,
    timeout: Duration,
    max_retries: u32,
    retry_base: Duration,
}

/// What one HTTP exchange produced.
struct Reply {
    status: u16,
    error_type: Option<String>,
    retry_after: Option<u64>,
    body: String,
}

impl Http {
    fn new(cfg: &Config) -> Result<Http, deepseek::Error> {
        let domain = aws_domain(&cfg.region);
        let runtime_url = cfg
            .runtime_endpoint
            .clone()
            .unwrap_or_else(|| format!("https://bedrock-runtime.{}.{domain}", cfg.region));
        let control_url = cfg
            .control_endpoint
            .clone()
            .unwrap_or_else(|| format!("https://bedrock.{}.{domain}", cfg.region));
        let runtime = Endpoint::parse(&runtime_url)
            .ok_or_else(|| bad(format!("Not an http(s) endpoint: {runtime_url}")))?;
        let control = Endpoint::parse(&control_url)
            .ok_or_else(|| bad(format!("Not an http(s) endpoint: {control_url}")))?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // A redirect would carry the request (and a bearer key) somewhere else; AWS never sends one.
            .max_redirects(0)
            .user_agent(concat!("prospect/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(15)))
            .build()
            .into();
        Ok(Http {
            agent,
            runtime,
            control,
            region: cfg.region.clone(),
            auth: cfg.auth.clone(),
            timeout: cfg.timeout,
            max_retries: cfg.max_retries,
            retry_base: cfg.retry_base,
        })
    }

    fn get_json(&self, path: &str, query: &[(String, String)]) -> Result<Value, Failure> {
        let text = self.send(Target::Control, path, query, None, None)?;
        serde_json::from_str(&text)
            .map_err(|e| Failure::Invalid(format!("not JSON ({e}): {}", error::snippet(&text))))
    }

    /// One call with retries: POST JSON when there's a body, GET otherwise. Every attempt is
    /// signed afresh.
    fn send(
        &self,
        target: Target,
        path: &str,
        query: &[(String, String)],
        body: Option<&[u8]>,
        stage: Option<&str>,
    ) -> Result<String, Failure> {
        let endpoint = match target {
            Target::Runtime => &self.runtime,
            Target::Control => &self.control,
        };
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let retry_left = attempt <= self.max_retries;
            match self.attempt(endpoint, path, query, body, stage) {
                Ok(reply) if (200..300).contains(&reply.status) => return Ok(reply.body),
                Ok(reply) => {
                    let e = AwsError::parse(reply.status, reply.error_type.as_deref(), &reply.body);
                    if e.retryable() && retry_left {
                        std::thread::sleep(self.backoff(attempt, reply.retry_after));
                        continue;
                    }
                    return Err(Failure::Aws(e));
                }
                Err(problem) => {
                    if retry_left {
                        std::thread::sleep(self.backoff(attempt, None));
                        continue;
                    }
                    return Err(Failure::Network(problem));
                }
            }
        }
    }

    fn attempt(
        &self,
        endpoint: &Endpoint,
        path: &str,
        query: &[(String, String)],
        body: Option<&[u8]>,
        stage: Option<&str>,
    ) -> Result<Reply, String> {
        let wire_path = format!("{}{path}", endpoint.base_path);
        let query_string = sigv4::query_string(query);
        let url = if query_string.is_empty() {
            format!("{}://{}{wire_path}", endpoint.scheme, endpoint.authority)
        } else {
            format!(
                "{}://{}{wire_path}?{query_string}",
                endpoint.scheme, endpoint.authority
            )
        };
        let host = endpoint.host_header();
        // The Host header is set here, so what is signed is exactly what is sent.
        let mut headers: Vec<(&str, String)> = vec![
            ("host", host.to_string()),
            ("accept", "application/json".to_string()),
        ];
        if body.is_some() {
            headers.push(("content-type", "application/json".to_string()));
        }
        match &self.auth {
            Auth::ApiKey(key) => headers.push(("authorization", format!("Bearer {key}"))),
            Auth::Keys {
                access_key_id,
                secret_access_key,
                session_token,
            } => {
                let signed: &[(&str, &str)] = if body.is_some() {
                    &[("content-type", "application/json")]
                } else {
                    &[]
                };
                let req = sigv4::Request {
                    method: if body.is_some() { "POST" } else { "GET" },
                    host,
                    path: &wire_path,
                    query,
                    headers: signed,
                    payload: body.unwrap_or_default(),
                };
                let creds = sigv4::Credentials {
                    access_key_id,
                    secret_access_key,
                    session_token: session_token.as_deref(),
                };
                headers.extend(sigv4::sign(
                    &req,
                    &creds,
                    &self.region,
                    "bedrock",
                    &sigv4::amz_date(model::date::now_unix()),
                ));
            }
        }
        if let Some(stage) = stage {
            headers.push(("x-opp-stage", stage.to_string()));
        }
        let result = match body {
            Some(bytes) => {
                let mut rb = self
                    .agent
                    .post(&url)
                    .config()
                    .timeout_global(Some(self.timeout))
                    .build();
                for (name, value) in &headers {
                    rb = rb.header(*name, value);
                }
                rb.send(bytes)
            }
            None => {
                let mut rb = self
                    .agent
                    .get(&url)
                    .config()
                    .timeout_global(Some(self.timeout))
                    .build();
                for (name, value) in &headers {
                    rb = rb.header(*name, value);
                }
                rb.call()
            }
        };
        let mut resp = result.map_err(|e| match e {
            ureq::Error::Timeout(_) => format!("no answer within {} s", self.timeout.as_secs_f32()),
            e => e.to_string(),
        })?;
        let status = resp.status().as_u16();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let error_type = header("x-amzn-errortype");
        let retry_after = header("retry-after").and_then(|s| s.trim().parse::<u64>().ok());
        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_REPLY_BYTES)
            .read_to_string()
            .map_err(|e| format!("reading the reply: {e}"))?;
        Ok(Reply {
            status,
            error_type,
            retry_after,
            body,
        })
    }

    fn backoff(&self, attempt: u32, retry_after_secs: Option<u64>) -> Duration {
        if let Some(s) = retry_after_secs {
            return Duration::from_secs(s.min(20));
        }
        let base = self
            .retry_base
            .saturating_mul(1 << (attempt.saturating_sub(1)).min(6));
        // Cheap jitter: up to a quarter of the delay, from the clock's nanoseconds.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0) as u64;
        let quarter = (base.as_millis() as u64 / 4).max(1);
        (base + Duration::from_millis(nanos % quarter)).min(Duration::from_secs(20))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Auth {
        Auth::Keys {
            access_key_id: "AKIAEXAMPLE1234".into(),
            secret_access_key: "secret/key+value".into(),
            session_token: None,
        }
    }

    fn cfg(region: &str, model: &str) -> Config {
        let mut c = Config::new(region, model, keys());
        c.runtime_endpoint = None;
        c.control_endpoint = None;
        c
    }

    #[test]
    fn arn_regions() {
        assert_eq!(
            region_from_arn(
                "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-x"
            )
            .as_deref(),
            Some("us-east-1")
        );
        assert_eq!(
            region_from_arn("arn:aws:bedrock:eu-west-1::foundation-model/anthropic.claude-x")
                .as_deref(),
            Some("eu-west-1")
        );
        assert_eq!(region_from_arn("anthropic.claude-x"), None);
    }

    #[test]
    fn region_comes_from_the_arn_and_must_agree() {
        let arn =
            "arn:aws:bedrock:eu-west-1:123456789012:application-inference-profile/a1b2c3d4e5f6";
        let b = Bedrock::new(cfg("", arn)).unwrap();
        assert_eq!(b.config().region, "eu-west-1");
        assert_eq!(
            b.http.runtime.authority,
            "bedrock-runtime.eu-west-1.amazonaws.com"
        );
        assert_eq!(b.http.control.authority, "bedrock.eu-west-1.amazonaws.com");
        assert_eq!(
            Bedrock::new(cfg(" EU-WEST-1 ", arn))
                .unwrap()
                .config()
                .region,
            "eu-west-1"
        );
        let e = Bedrock::new(cfg("us-east-1", arn)).err().unwrap();
        assert!(
            matches!(&e, deepseek::Error::BadRequest(400, m) if m.contains("eu-west-1") && m.contains("us-east-1")),
            "{e:?}"
        );
        // Listing lists the region asked for.
        assert_eq!(
            normalize(cfg("us-east-1", arn), Purpose::List)
                .unwrap()
                .region,
            "us-east-1"
        );
        let e = Bedrock::new(cfg("", "anthropic.claude-3-haiku-20240307-v1:0"))
            .err()
            .unwrap();
        assert!(
            matches!(&e, deepseek::Error::BadRequest(400, m) if m.contains("region")),
            "{e:?}"
        );
        assert_eq!(
            Bedrock::new(cfg("cn-north-1", "m.x"))
                .unwrap()
                .http
                .runtime
                .authority,
            "bedrock-runtime.cn-north-1.amazonaws.com.cn"
        );
    }

    #[test]
    fn config_is_checked() {
        let err = |c: Config| {
            Bedrock::new(c)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        };
        assert!(err(cfg("us-east-1", "  ")).contains("model id"));
        assert!(err(cfg("us-east-1", "anthropic claude")).contains("spaces"));
        assert!(err(cfg("us-east-1", &"m".repeat(2049))).contains("too long"));
        assert!(err(cfg("us-east-1", "..")).contains("model id"));
        for region in [
            "evil.com/x",
            "us-east-1.attacker.net",
            "us_east_1",
            "useast",
            "us-east-1-",
            "1us-east",
            "us--east-1",
        ] {
            assert!(err(cfg(region, "m.x")).contains("region"), "{region}");
        }
        for region in [
            "us-east-1",
            "us-gov-west-1",
            "ap-southeast-4",
            "eu-central-2",
            "il-central-1",
        ] {
            assert!(Bedrock::new(cfg(region, "m.x")).is_ok(), "{region}");
        }
        let mut c = cfg("us-east-1", "m.x");
        c.auth = Auth::ApiKey("   ".into());
        assert!(err(c.clone()).contains("Enter the Bedrock API key"));
        c.auth = Auth::ApiKey("abc\r\nX-Evil: 1".into());
        assert!(err(c.clone()).contains("copy it again"));
        c.auth = Auth::Keys {
            access_key_id: "AKIA".into(),
            secret_access_key: "".into(),
            session_token: None,
        };
        assert!(err(c.clone()).contains("secret access key"));
        // Pasted credentials are trimmed; a blank session token is no token.
        c.auth = Auth::Keys {
            access_key_id: " AKIA123 \n".into(),
            secret_access_key: "\tsecret ".into(),
            session_token: Some("  ".into()),
        };
        let b = Bedrock::new(c.clone()).unwrap();
        assert_eq!(
            b.config().auth,
            Auth::Keys {
                access_key_id: "AKIA123".into(),
                secret_access_key: "secret".into(),
                session_token: None
            }
        );
        c.runtime_endpoint = Some("ftp://example.com".into());
        assert!(err(c.clone()).contains("endpoint"));
        c.runtime_endpoint = Some("http://127.0.0.1:9960/bedrock-runtime/".into());
        let b = Bedrock::new(c).unwrap();
        assert_eq!(
            b.config().runtime_endpoint.as_deref(),
            Some("http://127.0.0.1:9960/bedrock-runtime")
        );
        assert_eq!(
            b.http.runtime,
            Endpoint {
                scheme: "http",
                authority: "127.0.0.1:9960".into(),
                base_path: "/bedrock-runtime".into()
            }
        );
    }

    #[test]
    fn stage_header_only_goes_to_a_test_endpoint() {
        let mut c = cfg("us-east-1", "m.x");
        assert!(
            !Bedrock::new(c.clone()).unwrap().stage_header,
            "real AWS: never"
        );
        c.runtime_endpoint = Some("http://127.0.0.1:9960/bedrock-runtime".into());
        assert!(Bedrock::new(c.clone()).unwrap().stage_header);
        // A VPC endpoint is still AWS.
        c.runtime_endpoint =
            Some("https://vpce-0abc-xyz.bedrock-runtime.us-east-1.vpce.amazonaws.com".into());
        assert!(!Bedrock::new(c.clone()).unwrap().stage_header);
        c.runtime_endpoint = Some("https://bedrock-runtime.us-east-1.api.aws".into());
        assert!(!Bedrock::new(c).unwrap().stage_header);
    }

    #[test]
    fn endpoints_and_host_headers() {
        let e = Endpoint::parse("HTTPS://Bedrock-Runtime.us-east-1.amazonaws.com:443/").unwrap();
        assert_eq!(
            (e.scheme, e.host_header(), e.base_path.as_str()),
            ("https", "bedrock-runtime.us-east-1.amazonaws.com", "")
        );
        assert!(e.is_aws());
        let e = Endpoint::parse("http://[::1]:9960/x").unwrap();
        assert_eq!((e.host_header(), e.hostname()), ("[::1]:9960", "::1"));
        assert!(!e.is_aws());
        assert!(
            !Endpoint::parse("http://amazonaws.com.evil.net")
                .unwrap()
                .is_aws()
        );
        for bad in [
            "",
            "localhost:9960",
            "http://",
            "http://user@host",
            "http://h/a?b",
            "http://h /x",
        ] {
            assert!(Endpoint::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn secrets_stay_out_of_debug_output() {
        let c = Config::new(
            "us-east-1",
            "m.x",
            Auth::Keys {
                access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
                secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
                session_token: Some("FwoGZXIvYXdzEBYaD".into()),
            },
        );
        let shown = format!("{c:?}");
        assert!(
            !shown.contains("wJalr") && !shown.contains("FwoG") && !shown.contains("AKIAIOSF"),
            "{shown}"
        );
        assert!(shown.contains("…MPLE"));
        let shown = format!("{:?}", Auth::ApiKey("ABSKQmVkcm9ja0FQSUtleS1secret".into()));
        assert_eq!(shown, "ApiKey(\"<redacted>\")");
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let mut c = cfg("us-east-1", "m.x");
        c.retry_base = Duration::from_millis(100);
        let b = Bedrock::new(c).unwrap();
        let d1 = b.http.backoff(1, None);
        let d3 = b.http.backoff(3, None);
        assert!(
            d1 >= Duration::from_millis(100) && d1 <= Duration::from_millis(125),
            "{d1:?}"
        );
        assert!(
            d3 >= Duration::from_millis(400) && d3 <= Duration::from_millis(500),
            "{d3:?}"
        );
        assert!(
            b.http.backoff(30, None) <= Duration::from_millis(8000),
            "doubling stops after 6 steps"
        );
        assert_eq!(b.http.backoff(1, Some(99)), Duration::from_secs(20));
        assert_eq!(b.http.backoff(1, Some(2)), Duration::from_secs(2));
        let mut slow = cfg("us-east-1", "m.x");
        slow.retry_base = Duration::from_secs(10);
        assert_eq!(
            Bedrock::new(slow).unwrap().http.backoff(3, None),
            Duration::from_secs(20),
            "never more than 20 s"
        );
    }

    #[test]
    fn names_and_cost() {
        use deepseek::ChatBackend;
        let b = Bedrock::new(cfg(
            "us-east-1",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        ))
        .unwrap();
        assert_eq!(
            b.name(),
            "bedrock:us.anthropic.claude-sonnet-4-5-20250929-v1:0"
        );
        assert_eq!(
            b.cost_usd(&ChatOutput {
                output_tokens: 1_000_000,
                ..Default::default()
            }),
            0.0
        );
    }
}
