//! The account's AI model (the model selector): DeepSeek Flash or Pro on this server's key, or a
//! Bedrock chat model in the business's own AWS account. Stored in `users.ai_json`; Bedrock
//! secrets inside it are sealed by the vault (bound to the account) and never returned — the web
//! client only sees a masked hint. Web search always stays on DeepSeek's Search API.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app;
use crate::http::ApiError;
use crate::users::User;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    /// "deepseek" or "bedrock".
    pub provider: String,
    /// The DeepSeek model, for provider "deepseek".
    pub model: String,
    pub bedrock: Option<BedrockConfig>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BedrockConfig {
    pub region: String,
    /// Model id, inference profile id or ARN.
    pub model: String,
    /// "api_key" or "keys" (IAM access keys).
    pub auth: String,
    /// `vault::mask` of the API key or access key id.
    pub key_hint: String,
    /// The secrets, sealed (see [`Secrets`]).
    pub sealed: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Secrets {
    api_key: String,
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
}

const DEFAULT_MODEL: &str = "deepseek-flash";

fn deepseek_models() -> impl Iterator<Item = &'static str> {
    model::AI_MODELS
        .iter()
        .filter(|m| m.provider == "deepseek")
        .map(|m| m.id)
}

fn vault_context(user_id: i64) -> String {
    format!("user:{user_id}:bedrock")
}

/// The account's settings (DeepSeek Flash until changed).
pub fn config(user: &User) -> AiConfig {
    let mut c: AiConfig = user
        .ai_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    if c.provider != "bedrock" {
        c.provider = "deepseek".into();
        if !deepseek_models().any(|m| m == c.model) {
            c.model = DEFAULT_MODEL.into();
        }
    }
    c
}

/// The API's AiSettings: never a secret.
pub fn settings_json(user: &User) -> Value {
    let c = config(user);
    json!({
        "provider": c.provider,
        "model": c.model,
        "bedrock": c.bedrock.map(|b| json!({ "region": b.region, "model": b.model, "auth": b.auth, "key_hint": b.key_hint })),
    })
}

fn bedrock_config(user_id: i64, b: &BedrockConfig) -> Result<bedrock::Config, String> {
    let plain = app()
        .vault
        .unseal(&b.sealed, &vault_context(user_id))
        .map_err(|e| format!("the stored Bedrock key can't be read ({e}); enter it again"))?;
    let s: Secrets = serde_json::from_str(&plain).unwrap_or_default();
    let auth = if b.auth == "keys" {
        bedrock::Auth::Keys {
            access_key_id: s.access_key_id,
            secret_access_key: s.secret_access_key,
            session_token: Some(s.session_token).filter(|t| !t.is_empty()),
        }
    } else {
        bedrock::Auth::ApiKey(s.api_key)
    };
    Ok(bedrock::Config::new(
        b.region.clone(),
        b.model.clone(),
        auth,
    ))
}

/// The client a run uses for this account: the base DeepSeek client with the chosen model, or
/// with chat on Bedrock.
pub fn client_for(base: &deepseek::Client, user: &User) -> Result<deepseek::Client, String> {
    let c = config(user);
    if c.provider == "bedrock" {
        let b = c.bedrock.ok_or("Bedrock is selected but not set up")?;
        let backend =
            bedrock::Bedrock::new(bedrock_config(user.id, &b)?).map_err(|e| e.to_string())?;
        return Ok(base.with_chat_backend(Arc::new(backend)));
    }
    Ok(base.with_model(c.model))
}

/// Validate a `PUT /api/settings/ai` body into new settings (secrets sealed).
pub fn update(user: &User, body: &Value) -> Result<AiConfig, ApiError> {
    let current = config(user);
    let text = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let provider = body
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or(&current.provider)
        .to_string();
    match provider.as_str() {
        "deepseek" => {
            let model = match body.get("model").and_then(Value::as_str) {
                Some(m) if deepseek_models().any(|id| id == m) => m.to_string(),
                Some(_) => {
                    return Err(ApiError::validation(
                        "model",
                        "Choose one of the listed DeepSeek models.",
                    ));
                }
                None if current.provider == "deepseek" => current.model.clone(),
                None => DEFAULT_MODEL.to_string(),
            };
            // The Bedrock settings are kept, so switching back doesn't mean typing keys again.
            Ok(AiConfig {
                provider,
                model,
                bedrock: current.bedrock,
            })
        }
        "bedrock" => {
            let b = body
                .get("bedrock")
                .filter(|v| v.is_object())
                .ok_or_else(|| ApiError::validation("bedrock", "Enter the Bedrock settings."))?;
            let previous = current.bedrock.clone().unwrap_or_default();
            let model_id = Some(text(b, "model"))
                .filter(|m| !m.is_empty())
                .unwrap_or(previous.model.clone());
            if model_id.is_empty()
                || model_id.len() > 2048
                || model_id.chars().any(char::is_whitespace)
            {
                return Err(ApiError::validation(
                    "model",
                    "Enter the Bedrock model id or inference profile ARN.",
                ));
            }
            let mut region = Some(text(b, "region"))
                .filter(|r| !r.is_empty())
                .unwrap_or(previous.region.clone());
            if region.is_empty() {
                region = bedrock::region_from_arn(&model_id).unwrap_or_default();
            }
            let region_ok = !region.is_empty()
                && region.len() <= 30
                && region
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            if !region_ok {
                return Err(ApiError::validation(
                    "region",
                    "Enter the AWS region, like us-east-1.",
                ));
            }
            let api_key = text(b, "api_key");
            let (access, secret) = (text(b, "access_key_id"), text(b, "secret_access_key"));
            let (auth, secrets, hint) = if !api_key.is_empty() {
                let hint = vault::mask(&api_key);
                (
                    "api_key",
                    Secrets {
                        api_key,
                        ..Secrets::default()
                    },
                    hint,
                )
            } else if !access.is_empty() || !secret.is_empty() {
                if access.is_empty() || secret.is_empty() {
                    return Err(ApiError::validation(
                        "secret_access_key",
                        "Enter both the access key id and the secret access key.",
                    ));
                }
                let hint = vault::mask(&access);
                let session_token = text(b, "session_token");
                (
                    "keys",
                    Secrets {
                        access_key_id: access,
                        secret_access_key: secret,
                        session_token,
                        ..Secrets::default()
                    },
                    hint,
                )
            } else if !previous.sealed.is_empty() {
                let bedrock = BedrockConfig {
                    region,
                    model: model_id,
                    ..previous
                };
                return Ok(AiConfig {
                    provider,
                    model: current.model,
                    bedrock: Some(bedrock),
                });
            } else {
                return Err(ApiError::validation(
                    "api_key",
                    "Enter a Bedrock API key, or IAM access keys.",
                ));
            };
            let plain = serde_json::to_string(&secrets).map_err(|_| ApiError::internal())?;
            let sealed = app().vault.seal(&plain, &vault_context(user.id));
            let bedrock = BedrockConfig {
                region,
                model: model_id,
                auth: auth.into(),
                key_hint: hint,
                sealed,
            };
            Ok(AiConfig {
                provider,
                model: current.model,
                bedrock: Some(bedrock),
            })
        }
        _ => Err(ApiError::validation(
            "provider",
            "Choose DeepSeek or Amazon Bedrock.",
        )),
    }
}

/// Try the account's model once: `(ok, message for people)`.
pub fn test(user: &User) -> (bool, String) {
    let started = Instant::now();
    let c = config(user);
    if c.provider == "bedrock" {
        let Some(b) = c.bedrock else {
            return (false, "Bedrock isn't set up yet.".into());
        };
        let mut cfg = match bedrock_config(user.id, &b) {
            Ok(cfg) => cfg,
            Err(e) => return (false, format!("Your Bedrock settings need attention: {e}.")),
        };
        // A quick check: no long waits or retries while someone looks at the settings screen.
        cfg.timeout = Duration::from_secs(20);
        cfg.max_retries = 0;
        let result = bedrock::Bedrock::new(cfg).and_then(|backend| backend.test_connection());
        return match result {
            Ok(_) => (
                true,
                format!(
                    "Bedrock answered in {:.1} s",
                    started.elapsed().as_secs_f32()
                ),
            ),
            Err(e) => (false, format!("Bedrock didn't answer: {e}")),
        };
    }
    let Some(base) = app().ai.as_ref() else {
        return (false, crate::runs::MSG_NO_AI.into());
    };
    match base.with_model(c.model.clone()).balance_usd() {
        Ok(_) => (
            true,
            format!(
                "DeepSeek answered in {:.1} s",
                started.elapsed().as_secs_f32()
            ),
        ),
        Err(e) => (false, format!("DeepSeek didn't answer: {e}")),
    }
}
