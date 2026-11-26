"""Shared request/response types for the mock upstream server (stdlib only)."""

import json
from dataclasses import dataclass, field


@dataclass
class Request:
    method: str
    path: str  # without the query string
    query: dict  # name -> first value
    headers: dict  # lowercase name -> value
    body: bytes

    def json(self):
        try:
            return json.loads(self.body.decode("utf-8") or "null")
        except (ValueError, UnicodeDecodeError):
            return None

    def form(self):
        """application/x-www-form-urlencoded body as name -> list of values."""
        from urllib.parse import parse_qs

        return parse_qs(self.body.decode("utf-8", "replace"), keep_blank_values=True)


@dataclass
class Response:
    status: int = 200
    body: bytes = b""
    content_type: str = "application/json"
    headers: dict = field(default_factory=dict)


def json_response(obj, status=200, headers=None):
    return Response(status=status, body=json.dumps(obj).encode("utf-8"), headers=headers or {})


def error_response(status, message, code="invalid_request_error"):
    """An OpenAI-format error body, as DeepSeek sends."""
    return json_response({"error": {"message": message, "type": code, "param": None, "code": code}}, status)


def not_found(path):
    return error_response(404, f"mock: no route for {path}", "not_found")


# --- read-only guarantee -------------------------------------------------------------------------
# Connector mocks call record_violation() when a client sends a write method (POST/PUT/PATCH/DELETE)
# to a *data* endpoint. Tests assert GET /_violations stays empty.
import threading as _threading

_VIOLATIONS = []
_VLOCK = _threading.Lock()


def record_violation(provider, req):
    with _VLOCK:
        _VIOLATIONS.append({"provider": provider, "method": req.method, "path": req.path})


def violations():
    with _VLOCK:
        return list(_VIOLATIONS)


def reset_violations():
    with _VLOCK:
        _VIOLATIONS.clear()
