"""Mock of Mailgun's messages API. Records every message so tests can read it via GET /_mail.

    POST /mailgun/v3/<domain>/messages        the US region
    POST /mailgun/eu/v3/<domain>/messages     the EU region

Like the real API: HTTP basic auth with user "api" and the key as password (a missing or wrong
key answers 401 "Forbidden"; keys starting with "bad" or "wrong" count as wrong), an
application/x-www-form-urlencoded body with `from`, `to` (repeatable or comma-separated),
`subject`, `text` and/or `html`, `o:` options (`o:tag` repeatable) and `h:` headers, and the reply
{"id": "<...@domain>", "message": "Queued. Thank you."}. A domain whose name starts with "eu." or
contains "eu-only" lives in the EU region: the US endpoint answers 404 "Domain not found", as the
real one does for a domain in the other region, and vice versa.

Point the app at it with MAILGUN_BASE_URL=http://127.0.0.1:<port>/mailgun.
"""

import base64
import binascii
import hashlib
import re
import threading
import time
from urllib.parse import unquote

from mocklib import Response, json_response, not_found

LOCK = threading.Lock()
SENT = []
COUNTER = [0]

ROUTE = re.compile(r"^/mailgun(/eu)?/v3/([^/]+)/messages$")


def reset():
    with LOCK:
        SENT.clear()
        COUNTER[0] = 0


def sent():
    with LOCK:
        return list(SENT)


def _authorized(req):
    auth = req.headers.get("authorization", "")
    if not auth.startswith("Basic "):
        return False
    try:
        user, _, key = base64.b64decode(auth[6:].strip(), validate=True).decode("utf-8").partition(":")
    except (binascii.Error, UnicodeDecodeError, ValueError):
        return False
    return user == "api" and bool(key) and not key.startswith(("bad", "wrong"))


def _first(form, name):
    values = form.get(name) or []
    return values[0] if values else None


def handle(req):
    m = ROUTE.match(req.path)
    if not m:
        return not_found(req.path)
    region = "eu" if m.group(1) else "us"
    domain = unquote(m.group(2))
    if req.method != "POST":
        return json_response({"message": "Method not allowed"}, 405)
    if not _authorized(req):
        return Response(401, b"Forbidden", "text/plain; charset=utf-8")
    in_eu = domain.startswith("eu.") or "eu-only" in domain
    if in_eu != (region == "eu"):
        return json_response({"message": f"Domain not found: {domain}"}, 404)
    if not req.headers.get("content-type", "").startswith("application/x-www-form-urlencoded"):
        return json_response({"message": "Unsupported Content-Type: this mock reads application/x-www-form-urlencoded"}, 400)
    form = req.form()
    for field in ("from", "to"):
        if not _first(form, field):
            return json_response({"message": f"'{field}' parameter is missing"}, 400)
    if not (_first(form, "text") or _first(form, "html")):
        return json_response({"message": "Need at least one of 'text', 'html', 'amp-html' or 'template' parameters specified"}, 400)
    recipients = [a.strip() for v in form.get("to", []) for a in v.split(",") if a.strip()]
    if any("@" not in r for r in recipients):
        return json_response({"message": "'to' parameter is not a valid address. please check documentation"}, 400)
    tags = form.get("o:tag", [])
    if len(tags) > 3:
        return json_response({"message": "Too many tags: at most 3 per message"}, 400)
    with LOCK:
        COUNTER[0] += 1
        n = COUNTER[0]
        stamp = time.strftime("%Y%m%d%H%M%S", time.gmtime())
        digest = hashlib.sha256(f"{n}|{_first(form, 'subject')}".encode("utf-8")).hexdigest()[:16].upper()
        message_id = f"<{stamp}.{n}.{digest}@{domain}>"
        SENT.append({
            "id": message_id,
            "domain": domain,
            "region": region,
            "from": _first(form, "from"),
            "to": recipients,
            "subject": _first(form, "subject") or "",
            "text": _first(form, "text") or "",
            "html": _first(form, "html") or "",
            "tags": tags,
            "options": {k: v[0] for k, v in form.items() if k.startswith("o:") and k != "o:tag"},
            "headers": {k[2:]: v[0] for k, v in form.items() if k.startswith("h:")},
            "variables": {k[2:]: v[0] for k, v in form.items() if k.startswith("v:")},
            "received_at": time.time(),
        })
    return json_response({"id": message_id, "message": "Queued. Thank you."})
