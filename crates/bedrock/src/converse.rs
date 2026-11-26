//! A [`ChatRequest`] as a Converse API body, and a Converse reply as a [`ChatOutput`].
//!
//! <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html>

use base64::Engine as _;
use deepseek::{ChatOutput, ChatRequest, Error, Part, Thinking};
use serde_json::{Value, json};

use crate::error::snippet;

/// Added to the system prompt when the request wants JSON. Claude has no JSON switch in
/// Converse, and prefill isn't allowed on current models, so the instruction is firm and the
/// reply still goes through [`deepseek::extract_json`].
pub(crate) const JSON_RULE: &str = "Respond with one JSON object only. Write nothing before or after it: no explanations, no notes and no Markdown code fences. The whole reply must parse as JSON.";

/// Sent when a request has no user content at all (Converse needs at least one block).
const EMPTY_USER: &str = "Please answer as instructed.";

/// Bedrock's limits for images in one Converse request.
const MAX_IMAGES: usize = 20;
const MAX_IMAGE_BYTES: usize = 3_750_000;

/// How a request asks Claude to think. Current models (4.6 and later) take adaptive thinking
/// and reject a fixed budget; older ones (3.7 to 4.5) take only a budget; others take neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Think {
    Adaptive,
    Budget,
    Off,
}

impl Think {
    pub(crate) fn from_u8(v: u8) -> Think {
        match v {
            0 => Think::Adaptive,
            1 => Think::Budget,
            _ => Think::Off,
        }
    }
    pub(crate) fn as_u8(self) -> u8 {
        match self {
            Think::Adaptive => 0,
            Think::Budget => 1,
            Think::Off => 2,
        }
    }
}

/// The optional parts of one Converse request. When Bedrock refuses one with a
/// ValidationException, [`Plan::fallback`] drops or lowers it and the call is made again.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Plan {
    pub think: Think,
    /// Thinking tokens to allow for (a budget, or room for adaptive thinking).
    pub think_tokens: u32,
    pub temperature: Option<f32>,
    /// The answer's own token allowance (the request's `max_tokens`, or a model limit).
    pub answer_tokens: u32,
    /// An output-token limit the model announced; never ask for more.
    pub cap: Option<u32>,
}

impl Plan {
    /// `think`: the thinking mode to start from when the request asks for thinking (pass
    /// [`Think::Off`] when thinking isn't enabled). Temperature only goes with requests that have
    /// thinking off, as with DeepSeek, clamped to Claude's 0–1.
    pub(crate) fn new(
        req: &ChatRequest,
        think: Think,
        temperature_ok: bool,
        cap: Option<u32>,
    ) -> Plan {
        let think_tokens = match req.thinking {
            Thinking::Off => 0,
            Thinking::Low => 2048,
            Thinking::High => 8192,
            Thinking::Max => 16_384,
        };
        let think = if think_tokens == 0 { Think::Off } else { think };
        let temperature = if think_tokens == 0 && temperature_ok {
            req.temperature.map(|t| t.clamp(0.0, 1.0))
        } else {
            None
        };
        Plan {
            think,
            think_tokens,
            temperature,
            answer_tokens: req.max_tokens.max(1),
            cap,
        }
    }

    /// `inferenceConfig.maxTokens`: the answer plus room for thinking, within any known cap.
    pub(crate) fn max_tokens(&self) -> u32 {
        let total = match self.think {
            Think::Off => self.answer_tokens,
            _ => self.answer_tokens.saturating_add(self.think_tokens),
        };
        match self.cap {
            Some(cap) => total.min(cap).max(1),
            None => total,
        }
    }

    /// A budget that fits below `max_tokens` with room left for the answer (Claude needs
    /// 1024 ≤ budget_tokens < max_tokens). Only a model's output cap can squeeze it.
    fn budget(&self) -> Option<u32> {
        let max = self.max_tokens();
        let room_for_answer = self.answer_tokens.min(max / 2).max(1);
        let budget = self.think_tokens.min(max.saturating_sub(room_for_answer));
        (budget >= 1024).then_some(budget)
    }

    /// The next thing to try after Bedrock refused this plan with `message`, or None when there
    /// is nothing optional left to drop (the request itself is the problem).
    pub(crate) fn fallback(&self, message: &str) -> Option<Plan> {
        if let Some(limit) = crate::error::max_tokens_limit(message)
            && self.max_tokens() > limit
        {
            let mut next = self.clone();
            next.cap = Some(limit);
            next.answer_tokens = next.answer_tokens.min(limit);
            return Some(next);
        }
        match self.think {
            Think::Adaptive => {
                return Some(Plan {
                    think: Think::Budget,
                    ..self.clone()
                });
            }
            Think::Budget => {
                return Some(Plan {
                    think: Think::Off,
                    ..self.clone()
                });
            }
            Think::Off => {}
        }
        if self.temperature.is_some() {
            return Some(Plan {
                temperature: None,
                ..self.clone()
            });
        }
        None
    }
}

/// The Converse request body.
pub(crate) fn request_body(req: &ChatRequest, plan: &Plan) -> Result<Value, Error> {
    let mut content: Vec<Value> = Vec::new();
    let mut images = 0usize;
    for part in &req.user {
        match part {
            Part::Text(t) => {
                if !t.trim().is_empty() {
                    content.push(json!({ "text": t }));
                }
            }
            Part::Image { mime, bytes } => {
                images += 1;
                if images > MAX_IMAGES {
                    return Err(Error::BadRequest(
                        400,
                        format!("Amazon Bedrock takes at most {MAX_IMAGES} images in one request."),
                    ));
                }
                if bytes.is_empty() {
                    return Err(Error::BadRequest(400, format!("Image {images} is empty.")));
                }
                if bytes.len() > MAX_IMAGE_BYTES {
                    return Err(Error::BadRequest(
                        413,
                        format!(
                            "Image {images} is {:.1} MB; Amazon Bedrock takes images up to 3.75 MB.",
                            bytes.len() as f64 / 1_000_000.0
                        ),
                    ));
                }
                let Some(format) = image_format(mime, bytes) else {
                    return Err(Error::BadRequest(
                        400,
                        format!(
                            "Image {images} ({}) isn't PNG, JPEG, GIF or WebP, which is all Amazon Bedrock takes.",
                            model::text::truncate(mime, 40)
                        ),
                    ));
                };
                content.push(json!({ "image": { "format": format, "source": { "bytes": base64::engine::general_purpose::STANDARD.encode(bytes) } } }));
            }
        }
    }
    if content.is_empty() {
        content.push(json!({ "text": EMPTY_USER }));
    }
    let mut body = json!({
        "messages": [{ "role": "user", "content": content }],
        "inferenceConfig": { "maxTokens": plan.max_tokens() },
    });
    let mut system: Vec<Value> = Vec::new();
    if !req.system.trim().is_empty() {
        system.push(json!({ "text": req.system }));
    }
    if req.json {
        system.push(json!({ "text": JSON_RULE }));
    }
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    if let Some(t) = plan.temperature {
        // Through f64 with rounding, so 0.3f32 goes out as 0.3 rather than 0.30000001192092896.
        body["inferenceConfig"]["temperature"] = json!((f64::from(t) * 1000.0).round() / 1000.0);
    }
    match plan.think {
        Think::Adaptive => {
            body["additionalModelRequestFields"] = json!({ "thinking": { "type": "adaptive" } })
        }
        Think::Budget => {
            if let Some(budget) = plan.budget() {
                body["additionalModelRequestFields"] =
                    json!({ "thinking": { "type": "enabled", "budget_tokens": budget } });
            }
        }
        Think::Off => {}
    }
    Ok(body)
}

/// A Converse reply as a [`ChatOutput`]: the text blocks joined (reasoning blocks go to
/// `reasoning`), the stop reason in OpenAI's words, and the token counts. With `json`, the
/// content is the bare JSON object or array found in the reply, when there is one; otherwise
/// the text is left as it came, so the caller can see what went wrong and ask again.
pub(crate) fn parse_reply(text: &str, json: bool) -> Result<ChatOutput, Error> {
    let v: Value = serde_json::from_str(text).map_err(|e| {
        Error::InvalidResponse(format!(
            "Amazon Bedrock's reply isn't JSON ({e}): {}",
            snippet(text)
        ))
    })?;
    let Some(blocks) = v["output"]["message"]["content"].as_array() else {
        return Err(Error::InvalidResponse(format!(
            "Amazon Bedrock's reply has no message: {}",
            snippet(text)
        )));
    };
    let mut content = String::new();
    let mut reasoning: Vec<&str> = Vec::new();
    for block in blocks {
        if let Some(t) = block["text"].as_str() {
            content.push_str(t);
        } else if let Some(r) = block["reasoningContent"]["reasoningText"]["text"].as_str()
            && !r.trim().is_empty()
        {
            reasoning.push(r);
        }
    }
    let usage = &v["usage"];
    let count = |k: &str| usage[k].as_u64().unwrap_or(0);
    if json
        && let Some(found) = deepseek::extract_json(&content)
        && (found.is_object() || found.is_array())
    {
        content = found.to_string();
    }
    Ok(ChatOutput {
        content,
        reasoning: reasoning.join("\n\n"),
        finish_reason: finish_reason(v["stopReason"].as_str().unwrap_or("")),
        // Cache writes are billed as input; cache reads are the cheap "cached" tokens.
        input_tokens: count("inputTokens") + count("cacheWriteInputTokens"),
        cached_tokens: count("cacheReadInputTokens"),
        output_tokens: count("outputTokens"),
    })
}

/// Converse's `stopReason` in the words the rest of the app uses.
pub(crate) fn finish_reason(stop: &str) -> String {
    match stop {
        "end_turn" | "stop_sequence" => "stop",
        "max_tokens" | "model_context_window_exceeded" => "length",
        "content_filtered" | "guardrail_intervened" | "refusal" => "content_filter",
        "tool_use" => "tool_calls",
        other => other,
    }
    .to_string()
}

/// Converse's image format, from the bytes when they're recognisable (they're what Bedrock
/// checks), else from the MIME type.
pub(crate) fn image_format(mime: &str, bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    let mime = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match mime.as_str() {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" | "image/pjpeg" => Some("jpeg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn plan(req: &ChatRequest) -> Plan {
        Plan::new(req, Think::Adaptive, true, None)
    }

    #[test]
    fn maps_system_user_parts_and_limits() {
        let req = ChatRequest::new("plan", "You plan searches.", "Find grants")
            .text("  ")
            .text("for chefs")
            .image("image/png", PNG.to_vec())
            .max_tokens(1500);
        let body = request_body(&req, &plan(&req)).unwrap();
        assert_eq!(body["system"], json!([{ "text": "You plan searches." }]));
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(
            content.len(),
            3,
            "blank text parts are skipped: {content:?}"
        );
        assert_eq!(content[0], json!({ "text": "Find grants" }));
        assert_eq!(content[1], json!({ "text": "for chefs" }));
        assert_eq!(content[2]["image"]["format"], "png");
        assert_eq!(
            content[2]["image"]["source"]["bytes"],
            base64::engine::general_purpose::STANDARD.encode(PNG)
        );
        assert_eq!(body["inferenceConfig"], json!({ "maxTokens": 1500 }));
        assert!(body.get("additionalModelRequestFields").is_none());
        assert!(body.get("toolConfig").is_none());
    }

    #[test]
    fn json_mode_adds_a_firm_system_rule() {
        let req = ChatRequest::new("evaluate", "", "x").json();
        let body = request_body(&req, &plan(&req)).unwrap();
        assert_eq!(body["system"], json!([{ "text": JSON_RULE }]));
        let req = ChatRequest::new("evaluate", "Reply in JSON.", "x").json();
        let body = request_body(&req, &plan(&req)).unwrap();
        assert_eq!(body["system"].as_array().unwrap().len(), 2);
        assert_eq!(body["system"][1]["text"], JSON_RULE);
        let plain = ChatRequest::new("x", " ", "x");
        assert!(
            request_body(&plain, &plan(&plain))
                .unwrap()
                .get("system")
                .is_none(),
            "no empty system blocks"
        );
    }

    #[test]
    fn temperature_only_without_thinking_and_clamped() {
        let req = ChatRequest::new("plan", "s", "u").temperature(1.7);
        let body = request_body(&req, &plan(&req)).unwrap();
        assert_eq!(body["inferenceConfig"]["temperature"], json!(1.0));
        let req = ChatRequest::new("plan", "s", "u").temperature(0.3);
        assert_eq!(
            request_body(&req, &plan(&req)).unwrap()["inferenceConfig"]["temperature"],
            json!(0.3)
        );
        // Thinking asked for: no temperature, whether or not thinking is then sent.
        let req = ChatRequest::new("evaluate", "s", "u")
            .temperature(0.3)
            .thinking(Thinking::Low);
        assert!(
            request_body(&req, &plan(&req)).unwrap()["inferenceConfig"]
                .get("temperature")
                .is_none()
        );
        assert!(request_body(&req, &Plan::new(&req, Think::Off, true, None)).unwrap()["inferenceConfig"].get("temperature").is_none());
        // A model that refused temperature before doesn't get it again.
        let req = ChatRequest::new("plan", "s", "u").temperature(0.3);
        assert!(request_body(&req, &Plan::new(&req, Think::Off, false, None)).unwrap()["inferenceConfig"].get("temperature").is_none());
    }

    #[test]
    fn thinking_modes() {
        let req = ChatRequest::new("evaluate", "s", "u")
            .thinking(Thinking::High)
            .max_tokens(4000);
        let adaptive = Plan::new(&req, Think::Adaptive, true, None);
        let body = request_body(&req, &adaptive).unwrap();
        assert_eq!(
            body["additionalModelRequestFields"],
            json!({ "thinking": { "type": "adaptive" } })
        );
        assert_eq!(
            body["inferenceConfig"]["maxTokens"],
            4000 + 8192,
            "room for thinking on top of the answer"
        );

        let budget = Plan::new(&req, Think::Budget, true, None);
        let body = request_body(&req, &budget).unwrap();
        assert_eq!(
            body["additionalModelRequestFields"],
            json!({ "thinking": { "type": "enabled", "budget_tokens": 8192 } })
        );
        assert_eq!(body["inferenceConfig"]["maxTokens"], 12_192);

        // Thinking off (the default): nothing extra, the request's own max_tokens.
        let off = Plan::new(&req, Think::Off, true, None);
        let body = request_body(&req, &off).unwrap();
        assert!(body.get("additionalModelRequestFields").is_none());
        assert_eq!(body["inferenceConfig"]["maxTokens"], 4000);

        // Thinking::Off in the request means off, whatever the backend allows.
        let req = ChatRequest::new("plan", "s", "u");
        assert_eq!(
            Plan::new(&req, Think::Adaptive, true, None).think,
            Think::Off
        );

        // A budget that can't fit under a model's output cap is left out.
        let req = ChatRequest::new("evaluate", "s", "u")
            .thinking(Thinking::Low)
            .max_tokens(500);
        let capped = Plan::new(&req, Think::Budget, true, Some(1500));
        assert_eq!(capped.max_tokens(), 1500);
        assert!(
            request_body(&req, &capped)
                .unwrap()
                .get("additionalModelRequestFields")
                .is_none()
        );
    }

    #[test]
    fn fallback_ladder() {
        let req = ChatRequest::new("evaluate", "s", "u")
            .thinking(Thinking::Low)
            .max_tokens(1000);
        let p = Plan::new(&req, Think::Adaptive, true, None);
        let p = p
            .fallback("thinking.type: Input tag 'adaptive' does not match any of the expected tags")
            .unwrap();
        assert_eq!(p.think, Think::Budget);
        let p = p
            .fallback("thinking: Extra inputs are not permitted")
            .unwrap();
        assert_eq!(p.think, Think::Off);
        assert_eq!(
            p.fallback("Malformed input request"),
            None,
            "nothing left to drop"
        );

        let req = ChatRequest::new("plan", "s", "u").temperature(0.2);
        let p = Plan::new(&req, Think::Off, true, None);
        let p = p
            .fallback("temperature is not supported for this model.")
            .unwrap();
        assert_eq!(p.temperature, None);
        assert_eq!(p.fallback("anything"), None);

        // An announced output limit is obeyed first, then remembered as the cap.
        let req = ChatRequest::new("plan", "s", "u").max_tokens(8000);
        let p = Plan::new(&req, Think::Off, true, None);
        let p = p.fallback("The maximum tokens you requested exceeds the model limit of 4096. Try again with a maximum tokens value that is lower than 4096.").unwrap();
        assert_eq!((p.max_tokens(), p.cap), (4096, Some(4096)));
        assert_eq!(
            p.fallback("The maximum tokens you requested exceeds the model limit of 4096."),
            None,
            "already within it"
        );
    }

    #[test]
    fn images_are_checked() {
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2];
        let gif = b"GIF89a....".to_vec();
        let webp = b"RIFF\x10\0\0\0WEBPVP8 ".to_vec();
        assert_eq!(
            image_format("image/png", &jpeg),
            Some("jpeg"),
            "the bytes win over the label"
        );
        assert_eq!(image_format("application/octet-stream", &gif), Some("gif"));
        assert_eq!(image_format("", &webp), Some("webp"));
        assert_eq!(image_format("image/JPEG; q=1", b"????"), Some("jpeg"));
        assert_eq!(image_format("image/bmp", b"BM...."), None);

        let req = ChatRequest::new("curate", "s", "u").image("image/bmp", b"BM....".to_vec());
        assert!(
            matches!(request_body(&req, &plan(&req)), Err(Error::BadRequest(400, m)) if m.contains("PNG, JPEG, GIF or WebP"))
        );
        let req = ChatRequest::new("curate", "s", "u").image("image/png", vec![]);
        assert!(matches!(
            request_body(&req, &plan(&req)),
            Err(Error::BadRequest(400, _))
        ));
        let mut big = PNG.to_vec();
        big.resize(MAX_IMAGE_BYTES + 1, 0);
        let req = ChatRequest::new("curate", "s", "u").image("image/png", big);
        assert!(
            matches!(request_body(&req, &plan(&req)), Err(Error::BadRequest(413, m)) if m.contains("3.75 MB"))
        );
        let mut req = ChatRequest::new("curate", "s", "u");
        for _ in 0..21 {
            req = req.image("image/png", PNG.to_vec());
        }
        assert!(
            matches!(request_body(&req, &plan(&req)), Err(Error::BadRequest(400, m)) if m.contains("at most 20"))
        );
    }

    #[test]
    fn empty_user_content_gets_a_neutral_line() {
        let req = ChatRequest::new("plan", "Do the thing.", "   ");
        let body = request_body(&req, &plan(&req)).unwrap();
        assert_eq!(
            body["messages"][0]["content"],
            json!([{ "text": EMPTY_USER }])
        );
    }

    fn reply(blocks: Value, stop: &str) -> String {
        json!({
            "output": { "message": { "role": "assistant", "content": blocks } },
            "stopReason": stop,
            "usage": { "inputTokens": 120, "outputTokens": 30, "totalTokens": 150, "cacheReadInputTokens": 40, "cacheWriteInputTokens": 5 },
            "metrics": { "latencyMs": 900 }
        })
        .to_string()
    }

    #[test]
    fn parses_text_reasoning_stop_and_usage() {
        let body = reply(
            json!([
                { "reasoningContent": { "reasoningText": { "text": "Let me think.", "signature": "sig" } } },
                { "text": "Hello " },
                { "text": "there." },
                { "reasoningContent": { "redactedContent": "AAAA" } }
            ]),
            "end_turn",
        );
        let out = parse_reply(&body, false).unwrap();
        assert_eq!(out.content, "Hello there.");
        assert_eq!(out.reasoning, "Let me think.");
        assert_eq!(out.finish_reason, "stop");
        assert_eq!(
            (out.input_tokens, out.cached_tokens, out.output_tokens),
            (125, 40, 30)
        );
        for (stop, want) in [
            ("stop_sequence", "stop"),
            ("max_tokens", "length"),
            ("model_context_window_exceeded", "length"),
            ("content_filtered", "content_filter"),
            ("guardrail_intervened", "content_filter"),
            ("tool_use", "tool_calls"),
            ("malformed_model_output", "malformed_model_output"),
        ] {
            assert_eq!(finish_reason(stop), want);
        }
    }

    #[test]
    fn json_mode_returns_bare_json() {
        let fenced = reply(
            json!([{ "text": "Here is the JSON:\n```json\n{\"queries\": [\"a\", \"b\"], \"n\": 2}\n```\nDone." }]),
            "end_turn",
        );
        let out = parse_reply(&fenced, true).unwrap();
        let v: Value = serde_json::from_str(&out.content).expect("bare JSON");
        assert_eq!(v, json!({ "queries": ["a", "b"], "n": 2 }));
        // Not JSON at all, or cut off: left as it came, for chat_json_value to retry.
        let cut = reply(json!([{ "text": "{\"queries\": [\"a\"," }]), "max_tokens");
        let out = parse_reply(&cut, true).unwrap();
        assert_eq!(
            (out.content.as_str(), out.finish_reason.as_str()),
            ("{\"queries\": [\"a\",", "length")
        );
        let prose = reply(json!([{ "text": "I can't help with that." }]), "end_turn");
        assert_eq!(
            parse_reply(&prose, true).unwrap().content,
            "I can't help with that."
        );
        // Without json, prose with braces in it is left alone.
        let text = reply(json!([{ "text": "Use {name} here." }]), "end_turn");
        assert_eq!(
            parse_reply(&text, false).unwrap().content,
            "Use {name} here."
        );
    }

    #[test]
    fn broken_replies_are_invalid_responses() {
        assert!(matches!(
            parse_reply("<html>", false),
            Err(Error::InvalidResponse(_))
        ));
        assert!(
            matches!(parse_reply("{\"output\": {}}", false), Err(Error::InvalidResponse(m)) if m.contains("no message"))
        );
        let empty = reply(json!([]), "end_turn");
        assert_eq!(parse_reply(&empty, true).unwrap().content, "");
    }
}
