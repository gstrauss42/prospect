"""Mock of Amazon Bedrock: the Converse API and model listing, strict enough to prove that the
client signs, encodes and parses the way real Bedrock needs. Owner: the Bedrock & vault builder.

Routes (server.py sends /bedrock-runtime/ and /bedrock/ here):

    POST /bedrock-runtime/model/<percent-encoded model id>/converse    Converse
    GET  /bedrock/foundation-models                                    ListFoundationModels
    GET  /bedrock/inference-profiles                                   ListInferenceProfiles
    GET  /bedrock-runtime/_seen      the recent Converse calls, as the mock understood them
    POST /bedrock-runtime/_fail      {"type": "ThrottlingException", "times": 2}: the next
                                     Converse calls fail with that AWS error, AWS-shaped

Point a client at it with runtime endpoint http://127.0.0.1:<port>/bedrock-runtime and control
endpoint http://127.0.0.1:<port>/bedrock.

Auth works as on AWS:
  * `Authorization: Bearer <Bedrock API key>`: the keys in API_KEYS.
  * SigV4: the signature is recomputed with the secret in ACCESS_KEYS and must match exactly
    (service `bedrock`, the raw path URI-encoded once more, query re-encoded and sorted, the
    signed headers, the payload hash), within 5 minutes of now, with the session token a
    temporary key needs. A model ARN's region must match the signing region.
Wrong or missing credentials get AWS's own error codes, in AWS's format (`x-amzn-ErrorType`
header, JSON body with a message).

Models: MODELS maps ids and ARNs to a GENERATION with the quirks of real Claude generations: which
thinking style it takes, whether it takes temperature, its output-token limit, whether it always
thinks, whether it wraps JSON in prose. An unknown model id gets ResourceNotFoundException.

With an X-Opp-Stage header (the client sends one only to non-AWS endpoints), the reply is the JSON
that deepseek_mock's handler for that stage makes from an OpenAI-shaped copy of the request, so
the whole engine can run on "Bedrock". A stage with no handler gets `{}` when the prompt asks for
JSON, like the DeepSeek mock. Without a JSON request the reply is a short sensible text.
"""

import base64
import binascii
import hashlib
import hmac
import json
import math
import re
import threading
import time
import uuid
from datetime import datetime, timezone
from urllib.parse import quote, unquote

import deepseek_mock
from mocklib import Response, json_response

ACCOUNT = "123456789012"

# Bedrock API keys (bearer tokens).
API_KEYS = {"ABSKbW9jay1iZWRyb2NrLWFwaS1rZXk=": "mock-user"}

# IAM access keys. Tests sign with these secrets; the mock verifies with them.
ACCESS_KEYS = {
    "AKIDMOCKBEDROCK00001": {"secret": "mockSecret/Key+ForBedrockTests/0000000000"},
    "ASIAMOCKBEDROCK00002": {"secret": "tempSecret/Key+ForBedrockTests/111111111", "token": "mock-session-token/Ab+Cd=="},
    "ASIAMOCKEXPIRED00003": {"secret": "expiredSecret/ForBedrockTests/2222222222", "token": "mock-expired-token", "expired": True},
    # May call models and list foundation models, but not bedrock:ListInferenceProfiles.
    "AKIDMOCKNOPROFILES04": {"secret": "noProfiles/Secret+ForBedrockTests/333333", "no_profiles": True},
}

SIGNATURE_WINDOW_SECS = 300

# Quirks of real Claude generations on Bedrock.
#   thinking: the additionalModelRequestFields.thinking types it takes
#   temperature: whether inferenceConfig.temperature is allowed at all
#   max_output: its output-token limit
#   always_thinks: thinks even when not asked (current models), returning a reasoning block
#   chatty: wraps JSON in a sentence and a code fence
GENERATIONS = {
    "claude-3-haiku": {"thinking": (), "temperature": True, "max_output": 4096, "always_thinks": False, "chatty": True},
    "claude-sonnet-4-5": {"thinking": ("enabled",), "temperature": True, "max_output": 64000, "always_thinks": False, "chatty": False},
    "claude-opus-5-5": {"thinking": ("adaptive",), "temperature": False, "max_output": 128000, "always_thinks": True, "chatty": False},
}

MODELS = {
    "anthropic.claude-3-haiku-20240307-v1:0": "claude-3-haiku",
    "arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-3-haiku-20240307-v1:0": "claude-3-haiku",
    "us.anthropic.claude-sonnet-4-5-20250929-v1:0": "claude-sonnet-4-5",
    f"arn:aws:bedrock:us-east-1:{ACCOUNT}:inference-profile/us.anthropic.claude-sonnet-4-5-20250929-v1:0": "claude-sonnet-4-5",
    f"arn:aws:bedrock:eu-west-1:{ACCOUNT}:application-inference-profile/a1b2c3d4e5f6": "claude-sonnet-4-5",
    "global.anthropic.claude-opus-5-5-v1": "claude-opus-5-5",
}

# Models that are only reachable through an inference profile.
PROFILE_ONLY = {"anthropic.claude-sonnet-4-5-20250929-v1:0", "anthropic.claude-opus-5-5-v1"}

IMAGE_FORMATS = ("png", "jpeg", "gif", "webp")
MAX_IMAGE_BYTES = 3_750_000

ERROR_STATUS = {
    "ThrottlingException": 429,
    "ModelNotReadyException": 429,
    "ServiceUnavailableException": 503,
    "InternalServerException": 500,
    "ModelTimeoutException": 408,
    "ModelErrorException": 424,
    "ServiceQuotaExceededException": 400,
    "AccessDeniedException": 403,
    "ValidationException": 400,
    "ResourceNotFoundException": 404,
}

ERROR_MESSAGES = {
    "ThrottlingException": "Too many requests, please wait before trying again.",
    "ModelNotReadyException": "Model is not ready for inference. Wait and try your request again.",
    "ServiceUnavailableException": "Service is temporarily unavailable. Try again later.",
    "InternalServerException": "The system encountered an unexpected error during processing. Try your request again.",
    "ModelTimeoutException": "Model has timed out in processing the request. Try your request again.",
    "ModelErrorException": "The model failed to process the request.",
    "ServiceQuotaExceededException": "You have exceeded the number of tokens allowed per day. Try again tomorrow.",
}

LOCK = threading.Lock()
STATE = {"seen": [], "fail": []}


def reset():
    with LOCK:
        STATE["seen"].clear()
        STATE["fail"].clear()


def seen():
    with LOCK:
        return list(STATE["seen"])


def _record(entry):
    with LOCK:
        STATE["seen"].append(entry)
        del STATE["seen"][:-200]


def _take_failure():
    with LOCK:
        for f in STATE["fail"]:
            if f["times"] > 0:
                f["times"] -= 1
                return dict(f)
    return None


# --- AWS-shaped replies ---------------------------------------------------------------------------


def aws_error(status, code, message, key="message", namespace="com.amazon.bedrock"):
    body = json.dumps({key: message}).encode("utf-8")
    headers = {"x-amzn-ErrorType": f"{code}:http://internal.amazon.com/coral/{namespace}/", "x-amzn-RequestId": str(uuid.uuid4())}
    return Response(status=status, body=body, headers=headers)


def aws_json(obj):
    return json_response(obj, headers={"x-amzn-RequestId": str(uuid.uuid4())})


def validation(message):
    return aws_error(400, "ValidationException", message)


def _auth_error(status, code, message, key="message"):
    return aws_error(status, code, message, key=key, namespace="com.amazon.coral.service")


# --- auth -------------------------------------------------------------------------------------------

AUTH_RE = re.compile(
    r"^AWS4-HMAC-SHA256 Credential=([^/,\s]+)/(\d{8})/([^/,\s]+)/([^/,\s]+)/aws4_request,\s*"
    r"SignedHeaders=([^,\s]+),\s*Signature=([0-9a-f]{64})$"
)


def _uri_encode(s):
    return quote(s, safe="-_.~")


def canonical_uri(raw_path):
    """The raw path normalised as for every service but S3, each segment encoded once more."""
    segments = []
    for seg in raw_path.split("/"):
        if seg in ("", "."):
            continue
        if seg == "..":
            if segments:
                segments.pop()
            continue
        segments.append(seg)
    out = "/" + "/".join(_uri_encode(s) for s in segments)
    if segments and raw_path.endswith("/"):
        out += "/"
    return out


def canonical_query(query):
    pairs = sorted((_uri_encode(k), _uri_encode(v)) for k, v in query.items())
    return "&".join(f"{k}={v}" for k, v in pairs)


def canonical_request(req, signed_names):
    headers = "".join(f"{name}:{' '.join((req.headers.get(name) or '').split())}\n" for name in signed_names)
    return "\n".join([
        req.method.upper(),
        canonical_uri(req.path),
        canonical_query(req.query),
        headers,
        ";".join(signed_names),
        hashlib.sha256(req.body).hexdigest(),
    ])


def _hmac(key, msg):
    return hmac.new(key, msg.encode("utf-8"), hashlib.sha256).digest()


def verify_sigv4(req):
    """(identity, None) for a valid signature, else (None, an AWS error reply)."""
    m = AUTH_RE.match(req.headers.get("authorization", ""))
    if not m:
        return None, _auth_error(403, "IncompleteSignatureException", "Authorization header requires 'Credential', 'Signature' and 'SignedHeaders' parameters.")
    access_key, date, region, service, signed, signature = m.groups()
    key = ACCESS_KEYS.get(access_key)
    if key is None:
        return None, _auth_error(403, "UnrecognizedClientException", "The security token included in the request is invalid.")
    if key.get("expired"):
        return None, _auth_error(403, "ExpiredTokenException", "The security token included in the request is expired")
    amz_date = req.headers.get("x-amz-date", "")
    if not re.fullmatch(r"\d{8}T\d{6}Z", amz_date):
        return None, _auth_error(403, "IncompleteSignatureException", "Authorization header requires existence of either a 'X-Amz-Date' or a 'Date' header.")
    if service != "bedrock":
        return None, _auth_error(403, "InvalidSignatureException", "Credential should be scoped to correct service: 'bedrock'. ")
    if date != amz_date[:8]:
        return None, _auth_error(403, "InvalidSignatureException", "Date in Credential scope does not match YYYYMMDD from ISO-8601 version of date from HTTP.")
    if not re.fullmatch(r"[a-z]{2}(-[a-z]+)+-\d+", region):
        return None, _auth_error(403, "InvalidSignatureException", "Credential should be scoped to a valid region. ")
    names = signed.split(";")
    if "host" not in names or "x-amz-date" not in names:
        return None, _auth_error(403, "InvalidSignatureException", "'Host' and 'X-Amz-Date' must be SignedHeaders in the AWS Authorization.")
    signed_at = datetime.strptime(amz_date, "%Y%m%dT%H%M%SZ").replace(tzinfo=timezone.utc).timestamp()
    now = time.time()
    if signed_at < now - SIGNATURE_WINDOW_SECS:
        stamp = datetime.fromtimestamp(now, timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        return None, _auth_error(403, "InvalidSignatureException", f"Signature expired: {amz_date} is now earlier than ({stamp} - 5 min.)")
    if signed_at > now + SIGNATURE_WINDOW_SECS:
        stamp = datetime.fromtimestamp(now, timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        return None, _auth_error(403, "InvalidSignatureException", f"Signature not yet current: {amz_date} is still later than ({stamp} + 5 min.)")
    token = req.headers.get("x-amz-security-token")
    if key.get("token") is not None and (token != key["token"] or "x-amz-security-token" not in names):
        return None, _auth_error(403, "UnrecognizedClientException", "The security token included in the request is invalid.")
    if key.get("token") is None and token:
        return None, _auth_error(403, "UnrecognizedClientException", "The security token included in the request is invalid.")
    canonical = canonical_request(req, names)
    to_sign = "\n".join(["AWS4-HMAC-SHA256", amz_date, f"{date}/{region}/{service}/aws4_request", hashlib.sha256(canonical.encode("utf-8")).hexdigest()])
    k = _hmac(("AWS4" + key["secret"]).encode("utf-8"), date)
    for part in (region, service, "aws4_request"):
        k = _hmac(k, part)
    expected = hmac.new(k, to_sign.encode("utf-8"), hashlib.sha256).hexdigest()
    if not hmac.compare_digest(expected, signature):
        return None, _auth_error(
            403,
            "InvalidSignatureException",
            "The request signature we calculated does not match the signature you provided. Check your AWS Secret Access Key and signing method. "
            "Consult the service documentation for details.\n\nThe Canonical String for this request should have been\n'" + canonical
            + "'\n\nThe String-to-Sign should have been\n'" + to_sign + "'\n",
        )
    return {"auth": "sigv4", "access_key": access_key, "region": region, "signed_headers": names}, None


def authenticate(req):
    auth = req.headers.get("authorization", "")
    if not auth:
        return None, _auth_error(403, "MissingAuthenticationTokenException", "Missing Authentication Token")
    if auth.startswith("Bearer "):
        if auth[7:].strip() in API_KEYS:
            return {"auth": "bearer", "access_key": None, "region": None, "signed_headers": []}, None
        return None, aws_error(403, "AccessDeniedException", "Authentication failed: Please make sure your API Key is valid.", key="Message")
    if auth.startswith("AWS4-HMAC-SHA256"):
        return verify_sigv4(req)
    return None, _auth_error(403, "IncompleteSignatureException", "Authorization header requires 'Credential', 'Signature' and 'SignedHeaders' parameters.")


# --- Converse -----------------------------------------------------------------------------------------


def _tokens(text):
    return math.ceil(len(text) / 4) if text else 0


def _magic_format(data):
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return "png"
    if data.startswith(b"\xff\xd8\xff"):
        return "jpeg"
    if data.startswith((b"GIF87a", b"GIF89a")):
        return "gif"
    if len(data) >= 12 and data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "webp"
    return None


def _check_blocks(blocks, where):
    """An error message for a malformed content list, or None. Returns (error, images)."""
    if not isinstance(blocks, list) or not blocks:
        return f"The content field in the Message object at {where} is empty. Add a ContentBlock object to the content field and try again.", 0
    images = 0
    for i, block in enumerate(blocks):
        at = f"{where}.content.{i}"
        if not isinstance(block, dict) or len(block) != 1:
            return f"The ContentBlock object at {at} must set exactly one of the content types.", images
        if "text" in block:
            if not isinstance(block["text"], str) or not block["text"].strip():
                return f"The text field in the ContentBlock object at {at} is blank. Add text to the text field, and try again.", images
        elif "image" in block:
            img = block["image"]
            if not isinstance(img, dict) or img.get("format") not in IMAGE_FORMATS:
                return f"The format field in the ImageBlock object at {at} must be one of png, jpeg, gif, webp.", images
            raw = (img.get("source") or {}).get("bytes")
            try:
                data = base64.b64decode(raw or "", validate=True)
            except (binascii.Error, ValueError):
                return f"Invalid base64 in the image source at {at}.", images
            if not data:
                return f"The image at {at} is empty.", images
            if len(data) > MAX_IMAGE_BYTES:
                return f"The image at {at} exceeds the 3.75 MB limit.", images
            actual = _magic_format(data)
            if actual and actual != img["format"]:
                return f"The model returned the following errors: Image does not match the provided media type image/{img['format']}", images
            images += 1
        else:
            return f"The ContentBlock object at {at} has an unsupported content type.", images
    return None, images


def _model_errors(body, gen):
    """The ValidationException message for what this model generation refuses, or None."""
    caps = GENERATIONS[gen]
    inference = body.get("inferenceConfig") or {}
    if not isinstance(inference, dict):
        return "The inferenceConfig field must be an object."
    max_tokens = inference.get("maxTokens", caps["max_output"])
    if not isinstance(max_tokens, int) or isinstance(max_tokens, bool) or max_tokens < 1:
        return "The maxTokens field in the InferenceConfiguration object must be at least 1."
    if max_tokens > caps["max_output"]:
        limit = caps["max_output"]
        return f"The maximum tokens you requested exceeds the model limit of {limit}. Try again with a maximum tokens value that is lower than {limit}."
    extra = body.get("additionalModelRequestFields") or {}
    if not isinstance(extra, dict):
        return "The additionalModelRequestFields field must be a JSON object."
    thinking = extra.get("thinking")
    if "temperature" in inference:
        t = inference["temperature"]
        if not caps["temperature"]:
            return "The model returned the following errors: temperature is deprecated for this model."
        if not isinstance(t, (int, float)) or isinstance(t, bool) or not 0 <= t <= 1:
            return "The model returned the following errors: temperature: Input should be less than or equal to 1"
        if thinking and t != 1:
            return "The model returned the following errors: temperature may only be set to 1 when thinking is enabled."
    if thinking is not None:
        kind = thinking.get("type") if isinstance(thinking, dict) else None
        if not caps["thinking"]:
            return "The model returned the following errors: thinking: Extra inputs are not permitted"
        if kind not in caps["thinking"]:
            if "adaptive" in caps["thinking"]:
                return f"The model returned the following errors: thinking.type.{kind} is not supported for this model. Use thinking.type.adaptive and output_config.effort to control thinking behavior."
            return f"The model returned the following errors: thinking.type: Input tag '{kind}' found using 'type' does not match any of the expected tags: 'disabled', 'enabled'"
        if kind == "enabled":
            budget = thinking.get("budget_tokens")
            if not isinstance(budget, int) or budget < 1024:
                return "The model returned the following errors: thinking.enabled.budget_tokens: Input should be greater than or equal to 1024"
            if budget >= max_tokens:
                return "The model returned the following errors: `max_tokens` must be greater than `thinking.budget_tokens`."
    return None


def openai_body(body, model_id):
    """The Converse request as the OpenAI-shaped chat body deepseek_mock's stage handlers read."""
    system = "\n\n".join(b["text"] for b in body.get("system") or [] if isinstance(b, dict) and isinstance(b.get("text"), str))
    messages = [{"role": "system", "content": system}]
    for m in body.get("messages") or []:
        parts = []
        for b in m.get("content") or []:
            if "text" in b:
                parts.append({"type": "text", "text": b["text"]})
            elif "image" in b:
                img = b["image"]
                url = f"data:image/{img.get('format')};base64,{(img.get('source') or {}).get('bytes', '')}"
                parts.append({"type": "image_url", "image_url": {"url": url, "detail": "low"}})
        content = "\n\n".join(p["text"] for p in parts) if all(p["type"] == "text" for p in parts) else parts
        messages.append({"role": m.get("role", "user"), "content": content})
    inference = body.get("inferenceConfig") or {}
    thinking = (body.get("additionalModelRequestFields") or {}).get("thinking")
    out = {
        "model": model_id,
        "messages": messages,
        "max_tokens": inference.get("maxTokens", 4096),
        "stream": False,
        "response_format": {"type": "json_object"},
        "thinking": {"type": "enabled" if thinking else "disabled"},
    }
    if "temperature" in inference:
        out["temperature"] = inference["temperature"]
    return out


def _echo_stage(body):
    """A stage for tests: what a stage handler received, to check the Converse → OpenAI copy."""
    msgs = body.get("messages") or []
    user = msgs[-1].get("content", "") if msgs else ""
    parts = user if isinstance(user, list) else [{"type": "text", "text": user}]
    images = [p for p in parts if p.get("type") == "image_url"]
    return {
        "ok": True,
        "model": body.get("model"),
        "roles": [m.get("role") for m in msgs],
        "system": (msgs[0].get("content") or "")[:400] if msgs else "",
        "user": "\n\n".join(p.get("text", "") for p in parts if p.get("type") == "text")[:400],
        "images": len(images),
        "image_urls": [p["image_url"]["url"][:40] for p in images],
        "max_tokens": body.get("max_tokens"),
        "response_format": body.get("response_format"),
        "thinking": body.get("thinking"),
    }


LOCAL_STAGES = {"bedrock-echo": _echo_stage}


def _stage_handler(stage):
    if not stage:
        return None
    return deepseek_mock._stage_fn(stage) or LOCAL_STAGES.get(stage)


def converse(req, encoded_id):
    model_id = unquote(encoded_id)
    entry = {"model": model_id, "raw_path": req.path, "stage": req.headers.get("x-opp-stage", ""), "status": 200, "error": None}
    who, err = authenticate(req)
    if who:
        entry.update({"auth": who["auth"], "access_key": who["access_key"], "region": who["region"], "signed_headers": who["signed_headers"]})
    reply = err or _converse(req, model_id, who, entry)
    entry["status"] = reply.status
    if reply.status != 200:
        entry["error"] = (reply.headers.get("x-amzn-ErrorType") or "").split(":")[0]
    _record(entry)
    return reply


def _converse(req, model_id, who, entry):
    body = req.json()
    if not isinstance(body, dict):
        return validation("Malformed input request, please reformat your input and try again.")
    inference = body.get("inferenceConfig") or {}
    thinking = (body.get("additionalModelRequestFields") or {}).get("thinking") if isinstance(body.get("additionalModelRequestFields"), dict) else None
    entry.update({
        "max_tokens": inference.get("maxTokens") if isinstance(inference, dict) else None,
        "temperature": inference.get("temperature") if isinstance(inference, dict) else None,
        "thinking": thinking.get("type") if isinstance(thinking, dict) else None,
        "budget_tokens": thinking.get("budget_tokens") if isinstance(thinking, dict) else None,
        "system": [b.get("text") for b in body.get("system") or [] if isinstance(b, dict)],
    })
    if model_id in PROFILE_ONLY:
        return validation(
            f"Invocation of model ID {model_id} with on-demand throughput isn’t supported. Retry your request with the ID or ARN of an inference profile that contains this model."
        )
    gen = MODELS.get(model_id)
    if gen is None:
        return aws_error(404, "ResourceNotFoundException", "Could not resolve the foundation model from the provided model identifier.")
    if model_id.startswith("arn:") and who["region"] and model_id.split(":")[3] != who["region"]:
        return validation(f"The provided ARN region ({model_id.split(':')[3]}) does not match the request region ({who['region']}).")
    messages = body.get("messages")
    if not isinstance(messages, list) or not messages:
        return validation("A conversation must start with a user message. Try again with a conversation that starts with a user message.")
    if not isinstance(messages[0], dict) or messages[0].get("role") != "user":
        return validation("A conversation must start with a user message. Try again with a conversation that starts with a user message.")
    images = 0
    for i, m in enumerate(messages):
        problem, n = _check_blocks(m.get("content") if isinstance(m, dict) else None, f"messages.{i}")
        if problem:
            return validation(problem)
        images += n
    for i, b in enumerate(body.get("system") or []):
        if not isinstance(b, dict) or not isinstance(b.get("text"), str) or not b["text"].strip():
            return validation(f"The text field in the SystemContentBlock object at system.{i} is blank. Add text to the text field, and try again.")
    problem = _model_errors(body, gen)
    if problem:
        return validation(problem)
    failure = _take_failure()
    if failure:
        return aws_error(failure["status"], failure["type"], failure["message"])

    caps = GENERATIONS[gen]
    entry["images"] = images
    system_text = "\n\n".join(b["text"] for b in body.get("system") or [])
    user_blocks = messages[-1]["content"]
    user_text = "\n\n".join(b["text"] for b in user_blocks if "text" in b)
    stage = req.headers.get("x-opp-stage", "")
    handler = _stage_handler(stage)
    if handler is not None or (stage and "JSON" in system_text):
        obj = handler(openai_body(body, model_id)) if handler else {}
        text = json.dumps(obj)
        if caps["chatty"]:
            text = "Here is the JSON you asked for:\n```json\n" + text + "\n```"
    elif "Reply with OK" in user_text:
        text = "OK"
    else:
        noun = f", {images} image{'s' if images != 1 else ''}" if images else ""
        text = f"Mock Claude read your message ({len(user_text)} characters{noun}): {user_text[:80]}"

    blocks = []
    think_tokens = 0
    if thinking or caps["always_thinks"]:
        think_tokens = 12
        # Current models omit the reasoning text unless asked to summarise it.
        summary = "Weighing the request against the instructions." if thinking else ""
        blocks.append({"reasoningContent": {"reasoningText": {"text": summary, "signature": "mock-" + hashlib.sha256(text.encode()).hexdigest()[:16]}}})
    max_tokens = inference.get("maxTokens", caps["max_output"])
    room = max_tokens - think_tokens
    stop = "end_turn"
    if room <= 0:
        text, stop = "", "max_tokens"
    elif _tokens(text) > room:
        text, stop = text[: room * 4], "max_tokens"
    if text:
        blocks.append({"text": text})
    input_tokens = 8 + _tokens(system_text) + _tokens(user_text) + 1100 * images
    output_tokens = think_tokens + _tokens(text)
    return aws_json({
        "output": {"message": {"role": "assistant", "content": blocks}},
        "stopReason": stop,
        "usage": {"inputTokens": input_tokens, "outputTokens": output_tokens, "totalTokens": input_tokens + output_tokens, "cacheReadInputTokens": 0, "cacheWriteInputTokens": 0},
        "metrics": {"latencyMs": 42},
    })


# --- listing ------------------------------------------------------------------------------------------


def _fm(model_id, name, provider, inference=("ON_DEMAND",), status="ACTIVE", output=("TEXT",), inputs=("TEXT",)):
    return {
        "modelArn": f"arn:aws:bedrock:us-east-1::foundation-model/{model_id}",
        "modelId": model_id,
        "modelName": name,
        "providerName": provider,
        "inputModalities": list(inputs),
        "outputModalities": list(output),
        "responseStreamingSupported": True,
        "customizationsSupported": [],
        "inferenceTypesSupported": list(inference),
        "modelLifecycle": {"status": status},
    }


FOUNDATION_MODELS = [
    _fm("amazon.nova-pro-v1:0", "Nova Pro", "Amazon", inputs=("TEXT", "IMAGE")),
    _fm("anthropic.claude-3-haiku-20240307-v1:0", "Claude 3 Haiku", "Anthropic", inference=("ON_DEMAND", "PROVISIONED"), inputs=("TEXT", "IMAGE")),
    _fm("anthropic.claude-sonnet-4-5-20250929-v1:0", "Claude Sonnet 4.5", "Anthropic", inference=("INFERENCE_PROFILE",), inputs=("TEXT", "IMAGE")),
    _fm("anthropic.claude-v2:1", "Claude", "Anthropic", status="LEGACY"),
    _fm("meta.llama3-1-70b-instruct-v1:0", "Llama 3.1 70B Instruct", "Meta"),
    _fm("amazon.titan-embed-text-v2:0", "Titan Text Embeddings V2", "Amazon", output=("EMBEDDING",)),
    _fm("stability.sd3-5-large-v1:0", "Stable Diffusion 3.5 Large", "Stability AI", output=("IMAGE",)),
    _fm("anthropic.claude-3-5-sonnet-20240620-v1:0", "Claude 3.5 Sonnet", "Anthropic", inputs=("TEXT", "IMAGE")),
]


def _profile(pid, name, models, kind="SYSTEM_DEFINED", status="ACTIVE", region="us-east-1"):
    resource = "inference-profile" if kind == "SYSTEM_DEFINED" else "application-inference-profile"
    return {
        "inferenceProfileName": name,
        "description": f"Routes requests to {name}.",
        "createdAt": "2025-10-01T00:00:00Z",
        "updatedAt": "2025-10-01T00:00:00Z",
        "inferenceProfileArn": f"arn:aws:bedrock:{region}:{ACCOUNT}:{resource}/{pid}",
        "models": [{"modelArn": f"arn:aws:bedrock:{region}::foundation-model/{m}"} for m in models],
        "inferenceProfileId": pid,
        "status": status,
        "type": kind,
    }


PROFILES = [
    _profile("us.anthropic.claude-sonnet-4-5-20250929-v1:0", "US Anthropic Claude Sonnet 4.5", ["anthropic.claude-sonnet-4-5-20250929-v1:0"]),
    _profile("us.meta.llama3-2-90b-instruct-v1:0", "US Meta Llama 3.2 90B Instruct", ["meta.llama3-2-90b-instruct-v1:0"]),
    _profile("global.anthropic.claude-opus-5-5-v1", "Global Anthropic Claude Opus 5.5", ["anthropic.claude-opus-5-5-v1"]),
    _profile("us.anthropic.claude-3-opus-20240229-v1:0", "US Anthropic Claude 3 Opus", ["anthropic.claude-3-opus-20240229-v1:0"], status="DELETING"),
    _profile("us.amazon.nova-premier-v1:0", "US Amazon Nova Premier", ["amazon.nova-premier-v1:0"]),
    _profile("a1b2c3d4e5f6", "opportunity-prod", ["anthropic.claude-sonnet-4-5-20250929-v1:0"], kind="APPLICATION"),
]

PAGE_SIZE = 2  # small pages, so a client has to follow nextToken


def foundation_models(req):
    who, err = authenticate(req)
    if err:
        return err
    items = FOUNDATION_MODELS
    q = req.query
    if "byOutputModality" in q:
        if q["byOutputModality"] not in ("TEXT", "IMAGE", "EMBEDDING"):
            return validation(f"1 validation error detected: Value '{q['byOutputModality']}' at 'byOutputModality' failed to satisfy constraint: Member must satisfy enum value set: [TEXT, IMAGE, EMBEDDING]")
        items = [m for m in items if q["byOutputModality"] in m["outputModalities"]]
    if "byProvider" in q:
        items = [m for m in items if m["providerName"].lower() == q["byProvider"].lower()]
    if "byInferenceType" in q:
        items = [m for m in items if q["byInferenceType"] in m["inferenceTypesSupported"]]
    return aws_json({"modelSummaries": items})


def inference_profiles(req):
    who, err = authenticate(req)
    if err:
        return err
    if ACCESS_KEYS.get(who.get("access_key") or "", {}).get("no_profiles"):
        return aws_error(
            403,
            "AccessDeniedException",
            f"User: arn:aws:iam::{ACCOUNT}:user/mock is not authorized to perform: bedrock:ListInferenceProfiles because no identity-based policy allows the bedrock:ListInferenceProfiles action",
            key="Message",
        )
    q = req.query
    try:
        max_results = int(q.get("maxResults", "1000"))
    except ValueError:
        return validation("1 validation error detected: Value at 'maxResults' failed to satisfy constraint: Member must be a number")
    if not 1 <= max_results <= 1000:
        return validation(f"1 validation error detected: Value '{max_results}' at 'maxResults' failed to satisfy constraint: Member must have value between 1 and 1000")
    kind = q.get("type", "SYSTEM_DEFINED")
    if kind not in ("SYSTEM_DEFINED", "APPLICATION"):
        return validation(f"1 validation error detected: Value '{kind}' at 'typeEquals' failed to satisfy constraint: Member must satisfy enum value set: [SYSTEM_DEFINED, APPLICATION]")
    items = [p for p in PROFILES if p["type"] == kind]
    start = 0
    token = q.get("nextToken")
    if token is not None:
        m = re.fullmatch(r"tok\+(\d+)/([A-Z_]+)==", token)
        if not m or m.group(2) != kind:
            return validation("The pagination token is invalid.")
        start = int(m.group(1))
    size = min(max_results, PAGE_SIZE)
    out = {"inferenceProfileSummaries": items[start:start + size]}
    if start + size < len(items):
        # '+', '/' and '=' on purpose: a client must encode them in the query, and sign them so.
        out["nextToken"] = f"tok+{start + size}/{kind}=="
    return aws_json(out)


# --- routing ------------------------------------------------------------------------------------------


def handle(req):
    path = req.path
    if path == "/bedrock-runtime/_seen":
        return json_response(seen())
    if path == "/bedrock-runtime/_fail" and req.method == "POST":
        spec = req.json() or {}
        code = spec.get("type", "ThrottlingException")
        f = {"type": code, "status": int(spec.get("status", ERROR_STATUS.get(code, 400))), "times": int(spec.get("times", 1)), "message": spec.get("message") or ERROR_MESSAGES.get(code, "Injected by the mock.")}
        with LOCK:
            STATE["fail"].append(f)
        return json_response({"ok": True, "failure": f})
    if path.startswith("/bedrock-runtime/"):
        parts = path.split("/")
        if req.method == "POST" and len(parts) == 5 and parts[2] == "model" and parts[3] and parts[4] == "converse":
            return converse(req, parts[3])
        return aws_error(404, "UnknownOperationException", "The requested operation is not recognized by the service.")
    if path == "/bedrock/foundation-models" and req.method == "GET":
        return foundation_models(req)
    if path == "/bedrock/inference-profiles" and req.method == "GET":
        return inference_profiles(req)
    return aws_error(404, "UnknownOperationException", "The requested operation is not recognized by the service.")
