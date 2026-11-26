//! Bedrock's failures, read from `x-amzn-ErrorType` and the body's `message`, mapped onto
//! [`deepseek::Error`] with messages a customer can act on.

use deepseek::Error;
use serde_json::Value;

/// What went wrong with one HTTP exchange, after retries.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Failure {
    /// AWS answered with an error.
    Aws(AwsError),
    /// No usable answer: couldn't connect, timed out, or the reply was cut off.
    Network(String),
    /// A success status, but not the reply that was asked for.
    Invalid(String),
}

/// An AWS error reply.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AwsError {
    pub status: u16,
    /// The bare error code, e.g. "ThrottlingException" (empty when AWS didn't say).
    pub code: String,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Throttled,
    QuotaExceeded,
    AccessDenied,
    UnknownCredentials,
    ExpiredCredentials,
    BadSignature,
    NoCredentials,
    AuthOther,
    Validation,
    NotFound,
    UnknownOperation,
    TooLarge,
    ModelNotReady,
    Unavailable,
    Internal,
    ModelFailed,
    Timeout,
    Other,
}

/// Which call failed, for the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Converse,
    ListModels,
}

pub(crate) struct Ctx<'a> {
    pub op: Op,
    pub model: &'a str,
    pub region: &'a str,
}

impl AwsError {
    /// Reads an error reply. The code comes from `x-amzn-ErrorType` (or the body's `__type` or
    /// `code`), cut down to the bare name as the AWS JSON protocols specify: anything after the
    /// first `:` and before a `#` is dropped.
    pub(crate) fn parse(status: u16, error_type: Option<&str>, body: &str) -> AwsError {
        let json: Option<Value> = serde_json::from_str(body).ok();
        let code = error_type
            .map(bare_code)
            .filter(|c| !c.is_empty())
            .or_else(|| {
                let v = json.as_ref()?;
                v["__type"]
                    .as_str()
                    .or_else(|| v["code"].as_str())
                    .map(bare_code)
            })
            .unwrap_or_default();
        let message = json
            .as_ref()
            .and_then(|v| {
                ["message", "Message", "errorMessage"]
                    .iter()
                    .find_map(|k| v[*k].as_str())
                    // An OpenAI-style body (the mock's injected failures, or a proxy in between).
                    .or_else(|| v["error"]["message"].as_str())
                    .map(str::to_string)
            })
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| {
                if body.trim().is_empty() {
                    format!("HTTP {status}")
                } else {
                    body.to_string()
                }
            });
        AwsError {
            status,
            code,
            message: snippet(&message),
        }
    }

    pub(crate) fn kind(&self) -> Kind {
        let code = self
            .code
            .strip_suffix("Exception")
            .unwrap_or(&self.code)
            .to_ascii_lowercase();
        match code.as_str() {
            "throttling"
            | "throttled"
            | "toomanyrequests"
            | "requestlimitexceeded"
            | "slowdown" => Kind::Throttled,
            "servicequotaexceeded" => Kind::QuotaExceeded,
            "accessdenied" | "notauthorized" | "optinrequired" | "authfailure" => {
                Kind::AccessDenied
            }
            "unrecognizedclient" | "invalidclienttokenid" => Kind::UnknownCredentials,
            "expiredtoken" | "requestexpired" => Kind::ExpiredCredentials,
            "invalidsignature" | "signaturedoesnotmatch" | "incompletesignature" => {
                Kind::BadSignature
            }
            "missingauthenticationtoken" => Kind::NoCredentials,
            "validation" | "validationerror" | "serialization" => Kind::Validation,
            "resourcenotfound" => Kind::NotFound,
            "unknownoperation" => Kind::UnknownOperation,
            "requestentitytoolarge" => Kind::TooLarge,
            "modelnotready" => Kind::ModelNotReady,
            "serviceunavailable" => Kind::Unavailable,
            "internalserver" | "internalfailure" => Kind::Internal,
            "modelerror" => Kind::ModelFailed,
            "modeltimeout" | "requesttimeout" => Kind::Timeout,
            _ => match self.status {
                401 | 403 => Kind::AuthOther,
                408 => Kind::Timeout,
                413 => Kind::TooLarge,
                429 => Kind::Throttled,
                503 => Kind::Unavailable,
                500..=599 => Kind::Internal,
                _ => Kind::Other,
            },
        }
    }

    /// Worth trying again after a pause.
    pub(crate) fn retryable(&self) -> bool {
        matches!(
            self.kind(),
            Kind::Throttled
                | Kind::ModelNotReady
                | Kind::Unavailable
                | Kind::Internal
                | Kind::ModelFailed
                | Kind::Timeout
        )
    }

    pub(crate) fn is_validation(&self) -> bool {
        self.kind() == Kind::Validation
    }

    pub(crate) fn to_error(&self, ctx: &Ctx) -> Error {
        let Ctx { op, model, region } = *ctx;
        let msg = self.message.trim_end_matches('.');
        match self.kind() {
            Kind::Throttled => Error::RateLimited(format!(
                "Amazon Bedrock is throttling requests in {region} ({msg}). It works again after a short wait; for steady use, ask AWS for a higher quota."
            )),
            Kind::QuotaExceeded => Error::RateLimited(format!(
                "This AWS account has reached an Amazon Bedrock quota in {region}: {msg}."
            )),
            Kind::AccessDenied => Error::Unauthorized(match op {
                Op::Converse => format!(
                    "Amazon Bedrock refused access to {model} in {region}: {msg}. Check that these credentials allow bedrock:InvokeModel on it and that access to the model is enabled in the Bedrock console for {region}."
                ),
                Op::ListModels => format!(
                    "Amazon Bedrock refused to list the models in {region}: {msg}. The credentials need bedrock:ListFoundationModels and bedrock:ListInferenceProfiles."
                ),
            }),
            Kind::UnknownCredentials => Error::Unauthorized(format!(
                "AWS doesn't recognise these credentials: {msg}. Check the access key ID or API key, and that it hasn't been deleted or deactivated."
            )),
            Kind::ExpiredCredentials => Error::Unauthorized(format!(
                "The AWS credentials have expired: {msg}. Temporary credentials only last a few hours; use long-lived access keys or a Bedrock API key instead."
            )),
            Kind::BadSignature => {
                let hint = if msg.contains("expired") || msg.contains("not yet current") {
                    "This server's clock is off, so the signature looked stale to AWS"
                } else {
                    "This usually means the secret access key is wrong"
                };
                Error::Unauthorized(format!(
                    "AWS rejected the request signature: {msg}. {hint}."
                ))
            }
            Kind::NoCredentials => {
                Error::Unauthorized(format!("No credentials reached AWS: {msg}."))
            }
            Kind::AuthOther => Error::Unauthorized(format!(
                "Amazon Bedrock rejected the credentials ({}): {msg}.",
                self.status
            )),
            Kind::Validation => {
                if msg.contains("model identifier is invalid") {
                    Error::BadRequest(
                        400,
                        format!(
                            "Amazon Bedrock doesn't recognise the model \"{model}\" in {region}: {msg}."
                        ),
                    )
                } else if msg.contains("on-demand throughput") || msg.contains("inference profile")
                {
                    Error::BadRequest(
                        400,
                        format!(
                            "Amazon Bedrock refused the request for {model}: {msg}. Use an inference profile id (such as us.anthropic.…) or its ARN instead of the bare model id."
                        ),
                    )
                } else {
                    Error::BadRequest(
                        400,
                        format!("Amazon Bedrock refused the request for {model}: {msg}."),
                    )
                }
            }
            Kind::NotFound => Error::BadRequest(
                404,
                format!(
                    "Model not found or not enabled in this region: {model} in {region} ({msg})."
                ),
            ),
            Kind::UnknownOperation => Error::BadRequest(
                404,
                format!("Amazon Bedrock doesn't know this request ({msg}). Check the endpoint."),
            ),
            Kind::TooLarge => Error::BadRequest(
                413,
                format!("The request is too large for Amazon Bedrock: {msg}."),
            ),
            // Not 5xx on the wire (429 and 424), but they mean the service side couldn't answer.
            Kind::ModelNotReady => {
                Error::Server(503, format!("{model} isn't ready to answer yet: {msg}."))
            }
            Kind::ModelFailed => {
                Error::Server(502, format!("{model} failed while answering: {msg}."))
            }
            Kind::Unavailable => Error::Server(
                503,
                format!("Amazon Bedrock is unavailable in {region} right now: {msg}."),
            ),
            Kind::Internal => Error::Server(
                if self.status >= 500 { self.status } else { 500 },
                format!("Amazon Bedrock had an internal error: {msg}."),
            ),
            Kind::Timeout => Error::Network(format!("{model} took too long to answer: {msg}.")),
            Kind::Other => Error::BadRequest(
                self.status,
                format!(
                    "Amazon Bedrock refused the request ({}): {msg}.",
                    self.status
                ),
            ),
        }
    }
}

impl Failure {
    pub(crate) fn to_error(&self, ctx: &Ctx) -> Error {
        match self {
            Failure::Aws(e) => e.to_error(ctx),
            Failure::Network(m) => Error::Network(format!(
                "Couldn't reach Amazon Bedrock in {}: {m}",
                ctx.region
            )),
            Failure::Invalid(m) => {
                Error::InvalidResponse(format!("Unexpected reply from Amazon Bedrock: {m}"))
            }
        }
    }
}

/// `ValidationException:http://internal.amazon.com/coral/…` → `ValidationException`;
/// `aws.protocoltests.restjson#FooError` → `FooError`.
fn bare_code(raw: &str) -> String {
    let before_colon = raw.split(':').next().unwrap_or("");
    let after_hash = before_colon
        .split_once('#')
        .map(|(_, b)| b)
        .unwrap_or(before_colon);
    after_hash.trim().to_string()
}

/// The output-token limit a ValidationException names, e.g. "…exceeds the model limit of 4096.
/// Try again with a maximum tokens value that is lower than 4096."
pub(crate) fn max_tokens_limit(message: &str) -> Option<u32> {
    let lower = message.to_ascii_lowercase();
    if !(lower.contains("token")
        && (lower.contains("limit") || lower.contains("maximum") || lower.contains("max_tokens")))
    {
        return None;
    }
    for marker in ["model limit of ", "lower than ", "less than or equal to "] {
        if let Some(i) = lower.find(marker) {
            let digits: String = lower[i + marker.len()..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<u32>()
                && n >= 1
            {
                return Some(n);
            }
        }
    }
    // "max_tokens: 8192 > 4096, which is the maximum allowed number of output tokens for …"
    if let Some(i) = lower.find(", which is the maximum") {
        let before = &lower[..i];
        let digits: String = before
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if let Ok(n) = digits.parse::<u32>()
            && n >= 1
        {
            return Some(n);
        }
    }
    None
}

pub(crate) fn snippet(s: &str) -> String {
    model::text::truncate(&model::text::squash(s), 300)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(status: u16, header: Option<&str>, body: &str) -> AwsError {
        AwsError::parse(status, header, body)
    }

    const CTX: Ctx = Ctx {
        op: Op::Converse,
        model: "us.anthropic.claude-x",
        region: "us-east-1",
    };

    #[test]
    fn reads_code_and_message() {
        let e = err(
            400,
            Some("ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/"),
            r#"{"message":"Malformed input request."}"#,
        );
        assert_eq!(e.code, "ValidationException");
        assert_eq!(e.message, "Malformed input request.");
        assert!(e.is_validation());
        assert!(!e.retryable());

        let e = err(
            403,
            None,
            r#"{"__type":"com.amazon.coral.service#UnrecognizedClientException","Message":"The security token included in the request is invalid."}"#,
        );
        assert_eq!(e.code, "UnrecognizedClientException");
        assert_eq!(
            e.message,
            "The security token included in the request is invalid."
        );

        let e = err(
            403,
            Some("AccessDeniedException"),
            r#"{"Message":"User: arn:aws:iam::1:user/x is not authorized to perform: bedrock:InvokeModel"}"#,
        );
        assert_eq!(e.kind(), Kind::AccessDenied);
        assert!(e.message.starts_with("User: arn:aws:iam::1:user/x"));

        assert_eq!(
            bare_code(
                "aws.protocoltests.restjson#FooError:http://internal.amazon.com/coral/com.amazon.coral.validate/"
            ),
            "FooError"
        );

        // No code and no JSON: the status decides and the body is the message.
        let e = err(502, None, "<html>Bad Gateway</html>");
        assert_eq!((e.code.as_str(), e.kind()), ("", Kind::Internal));
        assert!(e.retryable());
        assert_eq!(e.message, "<html>Bad Gateway</html>");
        // The mock's injected failures use an OpenAI-style body.
        let e = err(
            429,
            None,
            r#"{"error":{"message":"mock: injected 429","type":"x"}}"#,
        );
        assert_eq!(
            (e.kind(), e.message.as_str()),
            (Kind::Throttled, "mock: injected 429")
        );
        assert!(e.retryable());
        assert_eq!(err(500, None, "").message, "HTTP 500");
    }

    #[test]
    fn maps_onto_deepseek_errors() {
        let map =
            |status: u16, code: &str| err(status, Some(code), r#"{"message":"m"}"#).to_error(&CTX);
        assert!(matches!(
            map(429, "ThrottlingException"),
            Error::RateLimited(_)
        ));
        assert!(matches!(
            map(400, "ServiceQuotaExceededException"),
            Error::RateLimited(_)
        ));
        for code in [
            "AccessDeniedException",
            "UnrecognizedClientException",
            "ExpiredTokenException",
            "InvalidSignatureException",
            "IncompleteSignatureException",
            "MissingAuthenticationTokenException",
        ] {
            let e = map(403, code);
            assert!(matches!(e, Error::Unauthorized(_)), "{code}: {e:?}");
            assert!(e.is_account_problem());
        }
        assert!(matches!(
            map(400, "ValidationException"),
            Error::BadRequest(400, _)
        ));
        let nf = map(404, "ResourceNotFoundException");
        assert!(
            matches!(&nf, Error::BadRequest(404, m) if m.contains("not found or not enabled in this region") && m.contains("us.anthropic.claude-x")),
            "{nf:?}"
        );
        assert!(matches!(
            map(429, "ModelNotReadyException"),
            Error::Server(503, _)
        ));
        assert!(matches!(
            map(503, "ServiceUnavailableException"),
            Error::Server(503, _)
        ));
        assert!(matches!(
            map(500, "InternalServerException"),
            Error::Server(500, _)
        ));
        assert!(matches!(
            map(424, "ModelErrorException"),
            Error::Server(502, _)
        ));
        assert!(matches!(
            map(408, "ModelTimeoutException"),
            Error::Network(_)
        ));
        assert!(matches!(
            map(413, "RequestEntityTooLargeException"),
            Error::BadRequest(413, _)
        ));
        assert!(matches!(
            map(404, "UnknownOperationException"),
            Error::BadRequest(404, _)
        ));
        // Unknown codes fall back on the status.
        assert!(matches!(map(401, "Mystery"), Error::Unauthorized(_)));
        assert!(matches!(map(418, "Mystery"), Error::BadRequest(418, _)));
        assert!(matches!(map(504, "Mystery"), Error::Server(504, _)));
    }

    #[test]
    fn what_is_retried() {
        let retry = |status: u16, code: &str| err(status, Some(code), "{}").retryable();
        for (s, c) in [
            (429, "ThrottlingException"),
            (429, "ModelNotReadyException"),
            (503, "ServiceUnavailableException"),
            (500, "InternalServerException"),
            (408, "ModelTimeoutException"),
            (424, "ModelErrorException"),
            (503, ""),
            (429, ""),
        ] {
            assert!(retry(s, c), "{s} {c}");
        }
        for (s, c) in [
            (400, "ValidationException"),
            (403, "AccessDeniedException"),
            (404, "ResourceNotFoundException"),
            (400, "ServiceQuotaExceededException"),
            (403, "ExpiredTokenException"),
            (400, ""),
        ] {
            assert!(!retry(s, c), "{s} {c}");
        }
    }

    #[test]
    fn helpful_messages() {
        let e = err(400, Some("ValidationException"), r#"{"message":"Invocation of model ID anthropic.claude-sonnet-4-5-20250929-v1:0 with on-demand throughput isn't supported. Retry your request with the ID or ARN of an inference profile that contains this model."}"#)
            .to_error(&CTX);
        assert!(
            matches!(&e, Error::BadRequest(400, m) if m.contains("Use an inference profile id")),
            "{e:?}"
        );
        let e = err(
            400,
            Some("ValidationException"),
            r#"{"message":"The provided model identifier is invalid."}"#,
        )
        .to_error(&CTX);
        assert!(
            matches!(&e, Error::BadRequest(400, m) if m.contains("doesn't recognise the model \"us.anthropic.claude-x\" in us-east-1")),
            "{e:?}"
        );
        let e = err(403, Some("InvalidSignatureException"), r#"{"message":"Signature expired: 20261126T100000Z is now earlier than 20261126T100500Z (20261126T101000Z - 5 min.)"}"#).to_error(&CTX);
        assert!(
            matches!(&e, Error::Unauthorized(m) if m.contains("clock is off")),
            "{e:?}"
        );
        let e = err(403, Some("InvalidSignatureException"), r#"{"message":"The request signature we calculated does not match the signature you provided."}"#).to_error(&CTX);
        assert!(
            matches!(&e, Error::Unauthorized(m) if m.contains("secret access key is wrong")),
            "{e:?}"
        );
        let list = Ctx {
            op: Op::ListModels,
            ..CTX
        };
        let e = err(403, Some("AccessDeniedException"), r#"{"message":"no"}"#).to_error(&list);
        assert!(
            matches!(&e, Error::Unauthorized(m) if m.contains("bedrock:ListFoundationModels")),
            "{e:?}"
        );
        let e = Failure::Network("timeout: global".into()).to_error(&CTX);
        assert!(
            matches!(&e, Error::Network(m) if m.contains("us-east-1")),
            "{e:?}"
        );
        // Long bodies are cut down.
        let e = err(
            400,
            Some("ValidationException"),
            &format!(r#"{{"message":"{}"}}"#, "x".repeat(5000)),
        );
        assert!(e.message.chars().count() <= 300);
    }

    #[test]
    fn output_token_limits() {
        assert_eq!(
            max_tokens_limit(
                "The maximum tokens you requested exceeds the model limit of 4096. Try again with a maximum tokens value that is lower than 4096."
            ),
            Some(4096)
        );
        assert_eq!(
            max_tokens_limit(
                "max_tokens: 8192 > 4096, which is the maximum allowed number of output tokens for claude-3-haiku"
            ),
            Some(4096)
        );
        assert_eq!(
            max_tokens_limit(
                "The model returned the following errors: max_tokens: Input should be less than or equal to 64000"
            ),
            Some(64000)
        );
        assert_eq!(
            max_tokens_limit("temperature: Input should be less than or equal to 1"),
            None
        );
        assert_eq!(max_tokens_limit("Malformed input request"), None);
    }
}
