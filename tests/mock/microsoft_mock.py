"""Mock of the Microsoft identity platform (v2.0 OAuth) and Microsoft Graph v1.0, for the
Microsoft 365 connector. Owner: the Microsoft connector builder.

Routes (server.py sends /mslogin/... and /msgraph/... here):

    GET  /mslogin/<tenant>/oauth2/v2.0/authorize   an instant "Accept": 302 to redirect_uri?code=…&state=…
    POST /mslogin/<tenant>/oauth2/v2.0/token       authorization_code (client secret + PKCE) and refresh_token
    GET  /msgraph/v1.0/me
    GET  /msgraph/v1.0/me/messages                 every folder
    GET  /msgraph/v1.0/me/mailFolders/inbox/messages
    GET  /msgraph/v1.0/me/calendarView?startDateTime=…&endDateTime=…
    GET  /msgraph/_mock/requests                   test aid (not Graph): the Graph requests served,
                                                   as {path, query}; never tokens

Faithful where the connector depends on it:

* The token endpoint checks the client id and secret, that the code is unused, unexpired and was
  issued to the same client, redirect URI and tenant, and the PKCE verifier against the challenge
  (S256: BASE64URL(SHA256(verifier)), RFC 7636). Its reply carries the *granted* `scope`: every
  permission consented for the app, which may be more than was requested. Refresh tokens rotate
  (old ones stay valid, as Microsoft's do). Errors are AADSTS-style JSON.
* Graph is case-insensitive in paths, wants a valid bearer token (401 InvalidAuthenticationToken),
  the right permission (403 ErrorAccessDenied), and supports what the connector uses: `$select`,
  `$top` (1–1000, default 10), `$search` (KQL subset: words, "phrases", prefix*, prop:value,
  received>=date, AND/OR/NOT, parentheses; newest first; at most 1,000 results; paged with
  `$skiptoken`), `$filter` on receivedDateTime with `$orderby` (and the InefficientFilter rule:
  ordered properties must lead the filter, in the same order), paging with `$skip`, and
  `@odata.nextLink` back to this server. `$search` with `$filter`, `$orderby` or `$skip` is a 400.
* Data endpoints answer GET only: any other method, or a method-override header, is a 405 and is
  recorded with record_violation("microsoft", req).

The tenant is a fictional company, Brightwater Analytics (brightwater.example). Its mailbox and
calendar are generated relative to the time the server started.

Scenario users: pick one by adding login_hint=<user> to the authorize URL, as a person picking an
account. Without a hint you are Dana.

    dana.okafor@brightwater.example        read-only grant, normal mailbox (the default)
    admin-consented@brightwater.example    an admin consented Mail.Send and Mail.ReadWrite for the app:
                                           the code exchange (and every refresh) grants them
    consent-changed@brightwater.example    read-only at first; refreshes grant Calendars.ReadWrite
    short-lived@brightwater.example        the first access token expires in 60 s (refresh due at once)
    revoked@brightwater.example            the refresh token was revoked: refresh -> invalid_grant
    throttled@brightwater.example          every Graph request -> 429, Retry-After: 30
    busy@brightwater.example               the first request to each Graph endpoint -> 429,
                                           Retry-After: 1; then fine
    locked@brightwater.example             mail and calendar -> 403 ErrorAccessDenied
    no-search@brightwater.example          $search on messages -> 400 (the connector falls back to $filter)
    evil-link@brightwater.example          message pages link to /v1.0/me/sendMail as their next page
"""

import base64
import hashlib
import json
import re
import secrets
import threading
import time
import uuid
from datetime import datetime, timedelta, timezone
from urllib.parse import quote, urlencode, urlsplit

from mocklib import Response, json_response, record_violation

# --- the app registration and the tenant -----------------------------------------------------------

CLIENT_ID = "6731de76-14a6-49ae-97bc-6eba6914391e"
CLIENT_SECRET = "mock-ms-client-secret~not-a-real-one"
TENANT_ID = "4f0b8a77-2c1e-4d6b-9a53-8e2f7c1d9b60"
TENANT_NAME = "Brightwater Analytics"
DOMAIN = "brightwater.example"
AUTHORITIES = {"organizations", "common", TENANT_ID, DOMAIN}
CALLBACK_PATH = "/api/connectors/microsoft/callback"
GRAPH = "https://graph.microsoft.com/"
READ_SCOPES = ["User.Read", "Mail.Read", "Calendars.Read"]
OIDC_SCOPES = ["openid", "profile", "email"]
CODE_TTL = 600  # seconds; Microsoft's codes live about a minute, tests can be slower

DEFAULT_USER = "dana.okafor@" + DOMAIN


def _user(name, job, **scenario):
    oid = str(uuid.uuid5(uuid.NAMESPACE_DNS, name + "." + DOMAIN))
    return dict(name=name, job=job, oid=oid, **scenario)


USERS = {
    DEFAULT_USER: _user("Dana Okafor", "Head of Data Partnerships"),
    "admin-consented@" + DOMAIN: _user("Robin Hale", "Operations Lead", extra=["Mail.Send", "Mail.ReadWrite"]),
    "consent-changed@" + DOMAIN: _user("Alex Moreau", "Data Scientist", extra_on_refresh=["Calendars.ReadWrite"]),
    "short-lived@" + DOMAIN: _user("Kim Sato", "Analyst", first_ttl=60, short_names=True),
    "revoked@" + DOMAIN: _user("Jordan Price", "Account Director", revoked=True),
    "throttled@" + DOMAIN: _user("Sasha Ivanova", "Product Manager", graph="throttled"),
    "busy@" + DOMAIN: _user("Lee Chen", "Engineering Manager", graph="busy"),
    "locked@" + DOMAIN: _user("Morgan Blake", "Contractor", graph="locked"),
    "no-search@" + DOMAIN: _user("Chris Novak", "Partnerships Manager", graph="no-search"),
    "evil-link@" + DOMAIN: _user("Pat Quinn", "Designer", graph="evil-link"),
}

# --- state ---------------------------------------------------------------------------------------

LOCK = threading.Lock()
CODES = {}  # code -> what it was issued for
ACCESS = {}  # access token -> {user, scopes (lowercase, no resource prefix), exp}
REFRESH = {}  # refresh token -> {user, client_id}
HITS = {}  # (access token, endpoint) -> requests seen, for the "busy" user
SERVED = []  # Graph requests served: {path, query}
ANCHOR = datetime.now(timezone.utc).replace(second=0, microsecond=0)


def reset():
    with LOCK:
        CODES.clear()
        ACCESS.clear()
        REFRESH.clear()
        HITS.clear()
        SERVED.clear()


def handle(req):
    if req.path.startswith("/msgraph/"):
        return _graph(req)
    m = re.fullmatch(r"/mslogin/([^/]+)/oauth2/v2\.0/(authorize|token)", req.path)
    if not m:
        return _aad_page(404, "AADSTS90100: The request path isn't an endpoint of this identity service.")
    tenant, endpoint = m.groups()
    return _authorize(req, tenant) if endpoint == "authorize" else _token(req, tenant)


# --- identity platform ---------------------------------------------------------------------------


def _aad_page(status, message):
    """The HTML error page Microsoft shows when it can't safely redirect back."""
    html = f"<!doctype html><title>Sign in to your account</title><p>{message}</p>"
    return Response(status=status, body=html.encode("utf-8"), content_type="text/html; charset=utf-8")


def _aad_error(status, error, description, codes=None):
    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%SZ")
    trace, corr = str(uuid.uuid4()), str(uuid.uuid4())
    m = re.match(r"AADSTS(\d+)", description)
    return json_response(
        {
            "error": error,
            "error_description": f"{description}\r\nTrace ID: {trace}\r\nCorrelation ID: {corr}\r\nTimestamp: {now}",
            "error_codes": codes or ([int(m.group(1))] if m else []),
            "timestamp": now,
            "trace_id": trace,
            "correlation_id": corr,
        },
        status,
    )


def _registered(uri):
    """Redirect URIs the app registration accepts: https anywhere, or http on localhost, with the
    connector's callback path."""
    try:
        p = urlsplit(uri)
    except ValueError:
        return False
    local = p.scheme == "http" and p.hostname in ("localhost", "127.0.0.1")
    return (p.scheme == "https" or local) and p.path.endswith(CALLBACK_PATH) and not p.fragment


def _redirect(uri, params):
    sep = "&" if "?" in uri else "?"
    location = uri + sep + urlencode([(k, v) for k, v in params if v is not None], quote_via=quote)
    return Response(status=302, body=b"", content_type="text/html; charset=utf-8", headers={"Location": location})


def _authorize(req, tenant):
    if req.method != "GET":
        return _aad_page(405, "AADSTS900561: This mock's authorize endpoint only accepts GET requests.")
    q = req.query
    client_id = q.get("client_id", "")
    if client_id != CLIENT_ID:
        return _aad_page(400, f"AADSTS700016: Application with identifier '{client_id}' was not found in the directory '{TENANT_NAME}'.")
    if tenant not in AUTHORITIES:
        return _aad_page(400, f"AADSTS90002: Tenant '{tenant}' not found.")
    redirect_uri = q.get("redirect_uri", "")
    if not _registered(redirect_uri):
        return _aad_page(400, f"AADSTS50011: The redirect URI '{redirect_uri}' specified in the request does not match the redirect URIs configured for the application '{CLIENT_ID}'.")
    state = q.get("state")

    def fail(error, description):
        return _redirect(redirect_uri, [("error", error), ("error_description", description), ("state", state)])

    if q.get("response_type") != "code":
        return fail("unsupported_response_type", "AADSTS70005: response_type 'token' is not enabled for the application.")
    if q.get("response_mode", "query") != "query":
        return fail("invalid_request", "This mock answers only with response_mode=query.")
    if not q.get("scope", "").split():
        return fail("invalid_request", "AADSTS900144: The request body must contain the following parameter: 'scope'.")
    challenge = q.get("code_challenge", "")
    method = q.get("code_challenge_method", "plain" if challenge else "")
    if method not in ("", "plain", "S256"):
        return fail("invalid_request", "AADSTS501491: Invalid code_challenge_method.")
    if challenge and not 43 <= len(challenge) <= 128:
        return fail("invalid_request", "AADSTS501491: Invalid size of Code_Challenge parameter.")
    hint = q.get("login_hint", "").strip().lower() or DEFAULT_USER
    if hint not in USERS:
        return _aad_page(400, f"AADSTS50034: The user account {hint} does not exist in the {TENANT_NAME} directory.")
    code = "0.AXkA" + secrets.token_urlsafe(48)
    with LOCK:
        CODES[code] = {
            "client_id": client_id,
            "redirect_uri": redirect_uri,
            "tenant": tenant,
            "challenge": challenge,
            "method": method,
            "user": hint,
            "exp": time.time() + CODE_TTL,
            "used": False,
        }
    return _redirect(redirect_uri, [("code", code), ("state", state), ("session_state", str(uuid.uuid4()))])


def _token(req, tenant):
    if req.method != "POST":
        return _aad_error(400, "invalid_request", f"AADSTS900561: The endpoint only accepts POST, OPTIONS requests. Received a {req.method} request.")
    if tenant not in AUTHORITIES:
        return _aad_error(400, "invalid_request", f"AADSTS90002: Tenant '{tenant}' not found.")
    form = {k: v[0] for k, v in req.form().items()}
    client_id, secret = form.get("client_id", ""), form.get("client_secret")
    basic = req.headers.get("authorization", "")
    if basic.lower().startswith("basic "):
        try:
            client_id, _, secret = base64.b64decode(basic[6:].strip()).decode("utf-8").partition(":")
        except (ValueError, UnicodeDecodeError):
            return _aad_error(401, "invalid_client", "AADSTS7000216: Client authentication failed.")
    if client_id != CLIENT_ID:
        return _aad_error(400, "unauthorized_client", f"AADSTS700016: Application with identifier '{client_id}' was not found in the directory '{TENANT_NAME}'.")
    if not secret or not secrets.compare_digest(secret, CLIENT_SECRET):
        return _aad_error(401, "invalid_client", f"AADSTS7000215: Invalid client secret provided. Ensure the secret being sent in the request is the client secret value, not the client secret ID, for a secret added to app '{CLIENT_ID}'.")
    grant_type = form.get("grant_type", "")
    if grant_type == "authorization_code":
        return _redeem_code(form, tenant)
    if grant_type == "refresh_token":
        return _redeem_refresh(form, tenant)
    return _aad_error(400, "unsupported_grant_type", f"AADSTS70003: The app requested an unsupported grant type '{grant_type}'.")


def _s256(verifier):
    return base64.urlsafe_b64encode(hashlib.sha256(verifier.encode("ascii", "replace")).digest()).rstrip(b"=").decode("ascii")


def _redeem_code(form, tenant):
    code = form.get("code", "")
    with LOCK:
        c = CODES.get(code)
        used = c is not None and c["used"]
        if c is not None:
            c["used"] = True  # single use, even when this attempt fails
    if c is None:
        return _aad_error(400, "invalid_grant", "AADSTS9002313: Invalid request. Request is malformed or invalid.")
    if used:
        return _aad_error(400, "invalid_grant", "AADSTS54005: OAuth2 Authorization code was already redeemed, please retry with a new valid code or use an existing refresh token.")
    if time.time() > c["exp"]:
        return _aad_error(400, "invalid_grant", "AADSTS70008: The provided authorization code or refresh token has expired due to inactivity. Send a new interactive authorization request for this user and resource.")
    if form.get("redirect_uri") != c["redirect_uri"]:
        return _aad_error(400, "invalid_grant", "AADSTS500112: The reply address does not match the reply address provided when requesting the authorization code.")
    if tenant != c["tenant"]:
        return _aad_error(400, "invalid_grant", "AADSTS700005: Provided Authorization Code is intended to use against other tenant, thus rejected.")
    if c["challenge"]:
        verifier = form.get("code_verifier", "")
        expected = _s256(verifier) if c["method"] == "S256" else verifier
        if not verifier or not secrets.compare_digest(expected, c["challenge"]):
            return _aad_error(400, "invalid_grant", "AADSTS501481: The Code_Verifier does not match the code_challenge supplied in the authorization request.")
    user = USERS[c["user"]]
    return _issue(c["user"], _granted(user, refresh=False), user.get("first_ttl", 3599))


def _redeem_refresh(form, tenant):
    with LOCK:
        r = REFRESH.get(form.get("refresh_token", ""))
    if r is None:
        return _aad_error(400, "invalid_grant", "AADSTS9002313: Invalid request. Request is malformed or invalid.")
    user = USERS[r["user"]]
    if user.get("revoked"):
        return _aad_error(400, "invalid_grant", "AADSTS50173: The provided grant has expired due to it being revoked, a fresh auth token is needed. The user might have changed or reset their password.")
    return _issue(r["user"], _granted(user, refresh=True), 3599)


def _granted(user, refresh):
    """Every permission consented for the app for this user: what was requested plus whatever an
    administrator added. Graph scopes as Microsoft writes them (with the resource prefix, or short
    for one user), then the OpenID scopes."""
    graph = list(READ_SCOPES) + user.get("extra", []) + (user.get("extra_on_refresh", []) if refresh else [])
    prefix = "" if user.get("short_names") else GRAPH
    return [prefix + s for s in sorted(set(graph))] + OIDC_SCOPES


def _b64url(obj):
    return base64.urlsafe_b64encode(json.dumps(obj, separators=(",", ":")).encode("utf-8")).rstrip(b"=").decode("ascii")


def _issue(hint, scopes, ttl):
    user = USERS[hint]
    now = int(time.time())
    access = "eyJ0eXAiOiJKV1QiLCJub25jZSI6Im1vY2sifQ.mock." + secrets.token_urlsafe(40)
    refresh = "1.AXkA" + secrets.token_urlsafe(60)
    with LOCK:
        ACCESS[access] = {"user": hint, "scopes": [s.rsplit("/", 1)[-1].lower() for s in scopes], "exp": now + ttl}
        REFRESH[refresh] = {"user": hint, "client_id": CLIENT_ID}
    claims = {
        "aud": CLIENT_ID,
        "iss": f"https://login.microsoftonline.com/{TENANT_ID}/v2.0",
        "iat": now,
        "nbf": now,
        "exp": now + 3600,
        "email": hint,
        "name": user["name"],
        "oid": user["oid"],
        "preferred_username": hint,
        "rh": "0.AXkA-mock",
        "sub": hashlib.sha256((user["oid"] + CLIENT_ID).encode()).hexdigest()[:43],
        "tid": TENANT_ID,
        "uti": secrets.token_urlsafe(16),
        "ver": "2.0",
    }
    id_token = _b64url({"typ": "JWT", "alg": "RS256", "kid": "mock-signing-key"}) + "." + _b64url(claims) + "." + secrets.token_urlsafe(32)
    body = {
        "token_type": "Bearer",
        "scope": " ".join(scopes),
        "expires_in": ttl,
        "ext_expires_in": ttl,
        "access_token": access,
        "refresh_token": refresh,
        "id_token": id_token,
    }
    return json_response(body, headers={"Cache-Control": "no-store", "Pragma": "no-cache"})


# --- Microsoft Graph -----------------------------------------------------------------------------

OVERRIDE_HEADERS = ("x-http-method-override", "x-http-method", "x-method-override")


def _graph_error(status, code, message, headers=None):
    body = {
        "error": {
            "code": code,
            "message": message,
            "innerError": {
                "date": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S"),
                "request-id": str(uuid.uuid4()),
                "client-request-id": str(uuid.uuid4()),
            },
        }
    }
    return json_response(body, status, headers)


def _throttled(seconds):
    return _graph_error(429, "TooManyRequests", "Please retry again later.", {"Retry-After": str(seconds)})


def _graph(req):
    if req.method != "GET" or any(h in req.headers for h in OVERRIDE_HEADERS):
        record_violation("microsoft", req)
        return _graph_error(405, "Request_BadRequest", "Opportunity mock: Microsoft Graph is read-only here; only GET is allowed.")
    path = req.path[len("/msgraph"):]
    key = path.rstrip("/").lower()
    if key == "/_mock/requests":
        with LOCK:
            return json_response(list(SERVED))
    with LOCK:
        SERVED.append({"path": path, "query": dict(req.query)})
    auth = req.headers.get("authorization", "")
    token = auth[7:].strip() if auth[:7].lower() == "bearer " else ""
    with LOCK:
        t = ACCESS.get(token)
    if t is None:
        return _graph_error(401, "InvalidAuthenticationToken", "Access token validation failure. Invalid audience.", {"WWW-Authenticate": 'Bearer realm="", authorization_uri="https://login.microsoftonline.com/common/oauth2/authorize"'})
    if time.time() >= t["exp"]:
        return _graph_error(401, "InvalidAuthenticationToken", "Lifetime validation failed, the token is expired.")
    if key == "/v1.0/me":
        endpoint = "me"
    elif key in ("/v1.0/me/messages", "/v1.0/me/mailfolders/inbox/messages"):
        endpoint = "messages"
    elif key == "/v1.0/me/calendarview":
        endpoint = "calendarView"
    else:
        segment = path.rstrip("/").rsplit("/", 1)[-1]
        return _graph_error(400, "BadRequest", f"Resource not found for the segment '{segment}'.")
    hint = t["user"]
    user = USERS[hint]
    mode = user.get("graph")
    if mode == "throttled":
        return _throttled(30)
    if mode == "busy":
        with LOCK:
            seen = HITS.get((token, endpoint), 0)
            HITS[(token, endpoint)] = seen + 1
        if seen == 0:
            return _throttled(1)
    scopes = set(t["scopes"])
    if endpoint == "me":
        if not scopes & {"user.read", "user.readbasic.all", "user.read.all"}:
            return _graph_error(403, "Authorization_RequestDenied", "Insufficient privileges to complete the operation.")
        return _me(req, hint, user)
    if mode == "locked":
        return _graph_error(403, "ErrorAccessDenied", "Access is denied. Check credentials and try again.")
    if endpoint == "messages":
        if not scopes & {"mail.read", "mail.readwrite"}:
            return _graph_error(403, "ErrorAccessDenied", "Access is denied. Check credentials and try again.")
        return _messages(req, path, user, inbox_only="mailfolders" in key)
    if not scopes & {"calendars.read", "calendars.readwrite", "calendars.readbasic"}:
        return _graph_error(403, "ErrorAccessDenied", "Access is denied. Check credentials and try again.")
    return _calendar_view(req, path)


class BadQuery(ValueError):
    def __init__(self, message, code="BadRequest"):
        super().__init__(message)
        self.code = code


def _options(req, allowed):
    """Query parameters by lowercase name; unknown system query options are an error."""
    q = {}
    for k, v in req.query.items():
        lk = k.lower()
        if lk.startswith("$") and lk not in allowed:
            raise BadQuery(f"Query option '{k}' is not allowed for this request.")
        q[lk] = v
    return q


def _select(item, raw, entity):
    if raw is None:
        return dict(item)
    props = [p.strip() for p in raw.split(",") if p.strip()]
    out = {k: item[k] for k in ("@odata.etag", "id") if k in item}
    for p in props:
        match = next((k for k in item if k.lower() == p.lower()), None)
        if match is None:
            raise BadQuery(f"Could not find a property named '{p}' on type 'microsoft.graph.{entity}'.", "RequestBroker--ParseUri")
        out[match] = item[match]
    return out


def _top(q):
    raw = q.get("$top")
    if raw is None:
        return 10
    if not raw.isdigit() or not 1 <= int(raw) <= 1000:
        raise BadQuery(f"Invalid value '{raw}' for $top query option found. The $top query option requires a non-negative integer value.")
    return int(raw)


def _next_link(req, path, extra):
    """The next page, as Graph writes it: this server's origin, the request path, every original
    parameter except the paging ones, then the new paging parameter (`$` encoded as %24)."""
    host = req.headers.get("host", "127.0.0.1")
    params = [(k, v) for k, v in req.query.items() if k.lower() not in ("$skip", "$skiptoken")] + [extra]
    return f"http://{host}/msgraph{path}?" + urlencode(params, quote_via=quote, safe="")


def _me(req, hint, user):
    given, _, surname = user["name"].partition(" ")
    me = {
        "businessPhones": ["+44 20 7946 0" + str(100 + sum(map(ord, hint)) % 900)],
        "displayName": user["name"],
        "givenName": given,
        "jobTitle": user["job"],
        "mail": hint,
        "mobilePhone": None,
        "officeLocation": "London, Shoreditch",
        "preferredLanguage": "en-GB",
        "surname": surname,
        "userPrincipalName": hint,
        "id": user["oid"],
    }
    try:
        _options(req, {"$select"})
        body = _select(me, req.query.get("$select"), "user")
    except BadQuery as e:
        return _graph_error(400, e.code, str(e))
    body = {"@odata.context": "https://graph.microsoft.com/v1.0/$metadata#users/$entity", **body}
    return json_response(body)


# --- messages ------------------------------------------------------------------------------------

FOLDERS = {
    "inbox": "AAMkAGVmMDEzMTM4LTZmYWUtNDdkNC1hMDZiLTU1OGY5OTZhYmY4OAAuAAAAAAAiQ8W967B7TKBjgx9rVEURAQAiIsqMbYjsT5e-T7KzowPTAAAAAAEMAAA=",
    "sentitems": "AAMkAGVmMDEzMTM4LTZmYWUtNDdkNC1hMDZiLTU1OGY5OTZhYmY4OAAuAAAAAAAiQ8W967B7TKBjgx9rVEURAQAiIsqMbYjsT5e-T7KzowPTAAAAAAEJAAA=",
    "deleteditems": "AAMkAGVmMDEzMTM4LTZmYWUtNDdkNC1hMDZiLTU1OGY5OTZhYmY4OAAuAAAAAAAiQ8W967B7TKBjgx9rVEURAQAiIsqMbYjsT5e-T7KzowPTAAAAAAEKAAA=",
    "junkemail": "AAMkAGVmMDEzMTM4LTZmYWUtNDdkNC1hMDZiLTU1OGY5OTZhYmY4OAAuAAAAAAAiQ8W967B7TKBjgx9rVEURAQAiIsqMbYjsT5e-T7KzowPTAAAAAAEiAAA=",
}

# Opportunities, written out: (hours ago, sender name, sender address, subject, body).
OPPORTUNITY_MAIL = [
    (20, "Priya Raman", "priya.raman@datasummit-europe.example",
     "Invitation to speak at Data Summit Europe 2027 (Lisbon, 18–20 March)",
     "Hi Dana,\r\n\r\nI'm the programme chair for Data Summit Europe, and we'd love you to give a 30-minute talk on "
     "privacy-preserving analytics on the main stage in Lisbon (18–20 March 2027). Speakers receive travel, two "
     "nights' accommodation and a full conference pass.\r\n\r\nCould you let me know by 12 December whether you're "
     "interested? I'm happy to jump on a call to talk through the audience and format.\r\n\r\nBest wishes,\r\nPriya Raman\r\n"
     "Programme Chair, Data Summit Europe"),
    (36, "Jonas Weber", "jonas.weber@kestrel-capital.example",
     "Following up from Tuesday",
     "Hi Dana,\r\n\r\nGreat to meet you at the London Analytics Meetup on Tuesday. As promised, here are a few thoughts "
     "on what we discussed about your team's roadmap, the health-data market and where we see consolidation over the "
     "next two years. I've attached the deck we mentioned.\r\n\r\nSeparately, we have an opening on our portfolio "
     "advisory council and think you'd be a strong fit. It's two days a quarter, paid. Would you be open to a "
     "conversation in the new year?\r\n\r\nBest,\r\nJonas"),
    (46, "Marta Lind", "marta.lind@halden-kommune.example",
     "RFP: Open data platform for the City of Halden – responses due 15 January",
     "Dear supplier,\r\n\r\nThe City of Halden invites proposals for a cloud-based open data platform (estimated budget "
     "EUR 450,000 over three years). The request for proposal, evaluation criteria and data protection annex are "
     "attached.\r\n\r\nClarification questions close on 20 December; responses are due by 12:00 CET on 15 January 2027. "
     "A virtual supplier briefing takes place on 14 December.\r\n\r\nKind regards,\r\nMarta Lind\r\nProcurement, City of Halden"),
    (70, "Brightwater People Team", "people@brightwater.example",
     "Internal opening: Director of Analytics Partnerships (apply by 9 December)",
     "Hi all,\r\n\r\nWe're hiring internally first for a new Director of Analytics Partnerships role, reporting to the "
     "COO. The role leads our partnerships with health and public-sector clients and manages a team of four.\r\n\r\n"
     "If you'd like to be considered, apply through Workday by 9 December or speak to your manager. Interviews will "
     "run in the week of 14 December.\r\n\r\nThe People Team"),
    (95, "Tomás Herrera", "tomas.herrera@lumenhealth.example",
     "Partnership proposal: Lumen Health × Brightwater",
     "Hi Dana,\r\n\r\nWe've been following Brightwater's work on anonymised cohort analytics. Lumen Health is looking "
     "for a data partner for our NHS pilot next spring, and we'd love to explore a partnership: joint delivery, shared "
     "IP on the risk models and co-authored outputs.\r\n\r\nAre you free for an intro call next week?\r\n\r\nTomás Herrera\r\n"
     "Chief Data Officer, Lumen Health"),
    (130, "Grace Liu", "grace.liu@womenindata.example",
     "Would you join our panel on responsible AI? (Women in Data London, 4 February)",
     "Hi Dana,\r\n\r\nWe're putting together a panel on responsible AI in the public sector for Women in Data London on "
     "4 February, and your name came up more than once. It's a 45-minute moderated discussion with three other "
     "speakers, followed by audience questions.\r\n\r\nWould you be interested?\r\n\r\nGrace"),
    (160, "Analytics Excellence Awards", "awards@analyticsawards.example",
     "Nominations open: Analytics Excellence Awards 2027 – deadline 31 January",
     "Nominate your team, a project or a colleague for the Analytics Excellence Awards 2027. Categories include Data "
     "Partnership of the Year, Public Sector Impact and Analytics Leader of the Year. Shortlisted entries are invited "
     "to the awards dinner in London.\r\n\r\nNominations close on 31 January."),
    (190, "Brightwater Learning", "learning@brightwater.example",
     "Become a mentor: 2027 Brightwater mentoring programme",
     "Applications for mentors in the 2027 Brightwater mentoring programme are open until 16 December. Mentors meet "
     "their mentee once a month for six months, with training provided.\r\n\r\nIf you'd like to mentor someone early in "
     "their data career, apply on the intranet."),
    (240, "Sofia Marin", "s.marin@petfund.example",
     "Grant call: £250k for privacy-enhancing technologies (deadline 28 February)",
     "Dear Dana,\r\n\r\nFollowing our conversation at the Open Data Summit, I wanted to make sure you saw our new "
     "funding call for privacy-enhancing technologies in health and social care: grants of up to £250,000 for "
     "18-month projects. The deadline for applications is 28 February.\r\n\r\nBest regards,\r\nSofia Marin"),
    (280, "Ben Carter", "ben.carter@brightwater.example",
     "Hack Week 2027: call for project ideas",
     "Hi everyone,\r\n\r\nOur internal hackathon runs 18–22 January. We're looking for project ideas and team leads: "
     "anything from tooling to a client-facing prototype. Pitch your idea in the Hack Week channel by 20 December.\r\n\r\nBen"),
    (330, "Lucy Park", "lucy.park@brightwater.example",
     "Intro: Dana ↔ Kwame Mensah (Head of Data, Sahel Retail)",
     "Dana, meet Kwame. Kwame leads data at Sahel Retail and is looking for an analytics partner for their loyalty "
     "programme across four countries. Kwame, Dana runs partnerships at Brightwater and is the best person to talk "
     "to.\r\n\r\nI'll let you two take it from here.\r\nLucy"),
    (400, "HR Business Partner", "hrbp@brightwater.example",
     "Secondment opportunity: six months with the ONS Data Science Campus",
     "Hi Dana,\r\n\r\nThe ONS Data Science Campus has offered Brightwater a six-month secondment for a senior analyst or "
     "manager from March. Your manager suggested you might be interested. Expressions of interest by 18 December."),
    (450, "StatsConf Committee", "cfp@statsconf.example",
     "Call for papers: StatsConf 2027 – submissions close 10 January",
     "The StatsConf 2027 call for papers is open. We welcome talks and posters on applied statistics, privacy and "
     "official data. Submissions close on 10 January; speakers are notified in February."),
    (520, "Henrik Aas", "henrik.aas@odi-nordics.example",
     "Advisory board invitation – Open Data Institute Nordics",
     "Dear Dana,\r\n\r\nOn behalf of the Open Data Institute Nordics, I'd like to invite you to join our advisory board "
     "for 2027–2028. The board meets four times a year, twice in Oslo and twice online.\r\n\r\nWarm regards,\r\nHenrik Aas"),
    # Outside a 30-day window: must not come back.
    (24 * 45, "Spring Analytics Forum", "events@springforum.example",
     "Invitation to speak at the Spring Analytics Forum",
     "Dear Dana, we'd be delighted to have you as a speaker at the Spring Analytics Forum this April."),
]

# Not in the Inbox: only /me/messages sees these.
OTHER_FOLDERS_MAIL = [
    ("junkemail", 30, "Wealth Partner", "wealth.partner@quick-returns.example",
     "Investment opportunity – 300% returns guaranteed!!!",
     "Dear friend, an exclusive opportunity to grow your money fast. Reply now to secure your place."),
    ("sentitems", 18, "Dana Okafor", "dana.okafor@brightwater.example",
     "RE: Invitation to speak at Data Summit Europe 2027 (Lisbon, 18–20 March)",
     "Hi Priya, thank you so much for the invitation. I'd be delighted to speak; could we talk on Thursday?"),
    ("deleteditems", 60, "LeadGen Pro", "promo@leadgen.example",
     "Exclusive partnership offer for your business",
     "Grow your pipeline with our partnership programme. Unsubscribe at any time."),
]

# Everyday mail, generated: (sender name, address, subject template, body, count, first hours ago, spacing in hours).
NOISE_MAIL = [
    ("The Data Weekly", "newsletter@thedataweekly.example", "The Data Weekly #{n}: funding rounds, new roles and the week in AI",
     "This week: three funding rounds in health analytics, the roles everyone is hiring for, and a deep dive into synthetic data. "
     "Read online | Unsubscribe | View in browser", 7, 10, 168),
    ("SoftwareDeals", "deals@softwareoffers.example", "Don't miss this opportunity: 40% off analytics licences",
     "Limited-time opportunity for data teams. Offer ends Friday. Unsubscribe | View in browser", 4, 50, 170),
    ("Careers Network", "jobs-noreply@careersnetwork.example", "12 new Director roles that match your profile",
     "Director of Data, Head of Analytics and more roles near London. Apply in one click.", 5, 30, 140),
    ("Conference Hub", "marketing@conferencehub.example", "Early-bird tickets: Big Data Expo 2027",
     "Join 5,000 data leaders at the conference of the year. Early-bird pricing ends soon. Unsubscribe", 4, 80, 160),
    ("Cloud Vendor Partner Team", "partner-updates@cloudvendor.example", "Your monthly partner programme update",
     "New partner benefits, training credits and programme changes. Manage your preferences", 2, 100, 350),
    ("Insight Webinars", "events@insightwebinars.example", "Webinar invitation: data storytelling for executives",
     "Save your seat for a free webinar. Unsubscribe from these emails at any time.", 4, 60, 150),
    ("Grants Digest", "digest@grantsdigest.example", "This week's funding opportunities (issue {n})",
     "New grant and funding calls this week in research, innovation and the arts. Unsubscribe", 5, 20, 160),
    ("Microsoft Teams", "noreply@email.teams.example", "Lucy Park invited you to join a meeting",
     "Lucy Park has invited you to a Teams meeting. Join on your computer or mobile app.", 8, 15, 85),
    ("Facilities", "facilities@brightwater.example", "Conference room 4B booking confirmed",
     "Your booking for conference room 4B is confirmed. Please free the room if your plans change.", 4, 40, 170),
    ("Workday", "workday@brightwater.example", "Reminder: complete your annual compliance programme",
     "Your compliance programme training is due by 15 December. It takes about 40 minutes.", 3, 70, 220),
    ("Recruiting", "recruiting@brightwater.example", "Interview feedback needed: Data Engineer candidate {n}",
     "Please submit your feedback for the Data Engineer role in Workday within 48 hours of the interview.", 3, 90, 200),
    ("Benefits", "benefits@brightwater.example", "Wellbeing program: new mindfulness sessions",
     "The wellbeing program adds weekly mindfulness sessions in January. Sign up on the intranet.", 2, 110, 300),
    ("Sam Patel", "sam.patel@brightwater.example", "Partner portal access for the Lumen pilot",
     "Can you approve Lumen's access to the partner portal before Friday? Thanks, Sam", 2, 26, 240),
    ("IT Service Desk", "servicedesk@brightwater.example", "Role-based access changes to Power BI workspaces",
     "From Monday, workspace access moves to role-based groups. No action is needed for most people.", 2, 150, 260),
    # Nothing to do with opportunities, and no keyword at all.
    ("Jira", "jira@brightwater.example", "[DATA-{n}] Pipeline failure in cohort-builder",
     "The nightly cohort-builder job failed at the export step. See the run log for details.", 12, 5, 55),
    ("Concur", "concur@brightwater.example", "Expense report approved: November travel",
     "Your expense report has been approved and will be paid with your next salary.", 5, 33, 130),
    ("Sam Patel", "sam.patel@brightwater.example", "Re: Q4 numbers for the board pack",
     "Thanks Dana, I've updated slide 6 with the revised forecast. Can you check the footnotes?", 6, 8, 100),
    ("Lucy Park", "lucy.park@brightwater.example", "Lunch on Thursday?",
     "Are you around for lunch on Thursday? The new place on Rivington Street?", 3, 45, 200),
    ("Brightwater Comms", "comms@brightwater.example", "Office update: heating works on the 3rd floor",
     "Heating works continue on the third floor next week. Desks on the east side are available.", 3, 120, 190),
    # Older than a month: a search reaches them, the date window drops them.
    ("The Data Weekly", "newsletter@thedataweekly.example", "The Data Weekly #{n} (archive): grants, roles and conferences",
     "Last month's issue: grants, roles and conferences. Unsubscribe", 6, 24 * 32, 70),
]


def _item_id(kind, n):
    raw = hashlib.sha256(f"{kind}-{n}".encode()).digest() + bytes([n % 256, 0xFF, 0xFE])
    return "AAMkAGVmMDEz" + base64.b64encode(raw).decode("ascii")  # standard base64: may hold '/', '+', '='


def _message(n, folder, hours_ago, name, address, subject, body):
    at = ANCHOR - timedelta(hours=hours_ago, minutes=(n * 7) % 50)
    ts = at.strftime("%Y-%m-%dT%H:%M:%SZ")
    mid = _item_id("message", n)
    sender = {"emailAddress": {"name": name, "address": address}}
    return {
        "@odata.etag": f'W/"CQAAABYAAAAiIsqMbYjsT5e/T7KzowPTAAAA{n:05d}"',
        "id": mid,
        "createdDateTime": ts,
        "lastModifiedDateTime": ts,
        "changeKey": f"CQAAABYAAAAiIsqMbYjsT5e/T7KzowPTAAAA{n:05d}",
        "categories": [],
        "receivedDateTime": ts,
        "sentDateTime": (at - timedelta(seconds=40)).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "hasAttachments": "attached" in body,
        "internetMessageId": f"<{hashlib.md5(mid.encode()).hexdigest()}@mail.example>",
        "subject": subject,
        "bodyPreview": body[:255],
        "importance": "normal",
        "parentFolderId": FOLDERS[folder],
        "conversationId": "AAQkAGVmMDEz" + hashlib.sha1(subject.encode()).hexdigest()[:24],
        "isDeliveryReceiptRequested": False,
        "isReadReceiptRequested": False,
        "isRead": hours_ago > 48,
        "isDraft": False,
        "webLink": "https://outlook.office365.com/owa/?ItemID=" + quote(mid, safe="") + "&exvsurl=1&viewmodel=ReadMessageItem",
        "inferenceClassification": "other" if "nsubscribe" in body else "focused",
        "body": {"contentType": "html", "content": "<html><body><div>" + body.replace("\r\n", "<br>") + "</div></body></html>"},
        "_text": body,
        "sender": sender,
        "from": sender,
        "toRecipients": [{"emailAddress": {"name": "Dana Okafor", "address": DEFAULT_USER}}],
        "ccRecipients": [],
        "bccRecipients": [],
        "replyTo": [],
        "flag": {"flagStatus": "notFlagged"},
    }


def _mailbox():
    out = []
    n = 0
    for hours, name, address, subject, body in OPPORTUNITY_MAIL:
        n += 1
        out.append(_message(n, "inbox", hours, name, address, subject, body))
    for folder, hours, name, address, subject, body in OTHER_FOLDERS_MAIL:
        n += 1
        out.append(_message(n, folder, hours, name, address, subject, body))
    for name, address, subject, body, count, first, spacing in NOISE_MAIL:
        for k in range(count):
            n += 1
            out.append(_message(n, "inbox", first + k * spacing, name, address, subject.format(n=100 + n), body))
    return out


MAILBOX = _mailbox()


def _words(s):
    return [w for w in re.split(r"[^\w]+", s.lower()) if w]


class KqlError(ValueError):
    pass


def _kql_tokens(s):
    tokens, i = [], 0
    while i < len(s):
        c = s[i]
        if c.isspace():
            i += 1
        elif c in "()":
            tokens.append(c)
            i += 1
        else:
            j, buf = i, []
            while j < len(s) and not s[j].isspace() and s[j] not in "()":
                if s[j] == '"':
                    k = s.find('"', j + 1)
                    if k < 0:
                        raise KqlError("unterminated phrase")
                    buf.append(s[j : k + 1])
                    j = k + 1
                else:
                    buf.append(s[j])
                    j += 1
            tokens.append("".join(buf))
            i = j
    return tokens


TEXT_PROPS = {
    "subject": lambda m: m["subject"],
    "body": lambda m: m["_text"],
    "from": lambda m: m["from"]["emailAddress"]["name"] + " " + m["from"]["emailAddress"]["address"],
    "to": lambda m: " ".join(r["emailAddress"]["name"] + " " + r["emailAddress"]["address"] for r in m["toRecipients"]),
}
TEXT_PROPS["participants"] = lambda m: TEXT_PROPS["from"](m) + " " + TEXT_PROPS["to"](m)
TEXT_PROPS["recipients"] = TEXT_PROPS["to"]


def _word_matches(word, words, prefix):
    if prefix:
        return any(w.startswith(word) for w in words)
    return any(w == word or w == word + "s" or w == word + "es" for w in words)


def _parse_date(value):
    for fmt in ("%Y-%m-%d", "%m/%d/%Y"):
        try:
            return datetime.strptime(value, fmt).date()
        except ValueError:
            pass
    raise KqlError(f"'{value}' is not a date")


def _term(token):
    """One KQL term as a predicate on a message."""
    m = re.fullmatch(r"(\w+)(>=|<=|:|=|>|<)(.+)", token)
    if m and m.group(1).lower() in ("received", "sent"):
        prop, op, value = m.group(1).lower(), m.group(2), m.group(3).strip('"')
        day = _parse_date(value)
        field = "receivedDateTime" if prop == "received" else "sentDateTime"
        ops = {">=": lambda a: a >= day, "<=": lambda a: a <= day, ">": lambda a: a > day, "<": lambda a: a < day, ":": lambda a: a == day, "=": lambda a: a == day}
        return lambda msg: ops[op](datetime.strptime(msg[field][:10], "%Y-%m-%d").date())
    fields = list(TEXT_PROPS.values())[:3]
    if m and m.group(2) == ":" and m.group(1).lower() in TEXT_PROPS:
        fields = [TEXT_PROPS[m.group(1).lower()]]
        token = m.group(3)
    phrase = token.startswith('"') and token.endswith('"') and len(token) >= 2
    text = token[1:-1] if phrase else token
    prefix = text.endswith("*") and not phrase
    wanted = _words(text.rstrip("*"))
    if not wanted:
        raise KqlError(f"nothing to search for in '{token}'")

    def pred(msg):
        for f in fields:
            words = _words(f(msg))
            if len(wanted) > 1 or phrase:
                hay = " " + " ".join(words) + " "
                if " " + " ".join(wanted) + " " in hay:
                    return True
            elif _word_matches(wanted[0], words, prefix):
                return True
        return False

    return pred


def _parse_kql(raw):
    s = raw.strip()
    if len(s) >= 2 and s[0] == '"' and s[-1] == '"':
        s = s[1:-1]
    s = s.replace('\\"', '"')
    tokens = _kql_tokens(s)
    if not tokens:
        raise KqlError("empty search")
    pos = [0]

    def peek():
        return tokens[pos[0]] if pos[0] < len(tokens) else None

    def take():
        pos[0] += 1
        return tokens[pos[0] - 1]

    def unary():
        t = peek()
        if t is None:
            raise KqlError("the search ends too early")
        if t == "NOT":
            take()
            inner = unary()
            return lambda m: not inner(m)
        if t == "(":
            take()
            inner = disjunction()
            if peek() != ")":
                raise KqlError("missing ')'")
            take()
            return inner
        if t in (")", "AND", "OR"):
            raise KqlError(f"unexpected '{t}'")
        return _term(take())

    def conjunction():
        parts = [unary()]
        while peek() not in (None, "OR", ")"):
            if peek() == "AND":
                take()
            parts.append(unary())
        return lambda m: all(p(m) for p in parts)

    def disjunction():
        parts = [conjunction()]
        while peek() == "OR":
            take()
            parts.append(conjunction())
        return lambda m: any(p(m) for p in parts)

    expr = disjunction()
    if peek() is not None:
        raise KqlError(f"unexpected '{peek()}'")
    return expr


FILTERABLE = {"receiveddatetime": "receivedDateTime", "sentdatetime": "sentDateTime"}
SORTABLE = {"receiveddatetime": "receivedDateTime", "sentdatetime": "sentDateTime", "subject": "subject"}


def _parse_datetime(value):
    v = value.strip()
    if v.endswith("Z"):
        v = v[:-1] + "+00:00"
    d = datetime.fromisoformat(v)
    return d if d.tzinfo else d.replace(tzinfo=timezone.utc)


def _parse_filter(raw):
    preds = []
    for clause in re.split(r"\s+and\s+", raw.strip(), flags=re.IGNORECASE):
        m = re.fullmatch(r"\s*(\w+)\s+(eq|ne|ge|gt|le|lt)\s+(\S+)\s*", clause)
        if not m or m.group(1).lower() not in FILTERABLE:
            raise BadQuery(f"Invalid filter clause: {clause.strip()}")
        try:
            value = _parse_datetime(m.group(3))
        except ValueError:
            raise BadQuery(f"Invalid filter clause: '{m.group(3)}' is not a DateTimeOffset literal.")
        preds.append((FILTERABLE[m.group(1).lower()], m.group(2), value))
    return preds


def _parse_orderby(raw):
    order = []
    for part in raw.split(","):
        bits = part.split()
        if not bits or bits[0].lower() not in SORTABLE or len(bits) > 2 or (len(bits) == 2 and bits[1].lower() not in ("asc", "desc")):
            raise BadQuery(f"Could not find a property named '{part.strip()}' on type 'microsoft.graph.message' to order by.")
        order.append((SORTABLE[bits[0].lower()], len(bits) == 2 and bits[1].lower() == "desc"))
    return order


OPS = {
    "eq": lambda a, b: a == b,
    "ne": lambda a, b: a != b,
    "ge": lambda a, b: a >= b,
    "gt": lambda a, b: a > b,
    "le": lambda a, b: a <= b,
    "lt": lambda a, b: a < b,
}


def _messages(req, path, user, inbox_only):
    try:
        q = _options(req, {"$select", "$top", "$skip", "$skiptoken", "$search", "$filter", "$orderby", "$count"})
        top = _top(q)
        items = [m for m in MAILBOX if not inbox_only or m["parentFolderId"] == FOLDERS["inbox"]]
        search = q.get("$search")
        if search is not None:
            for opt, shown in (("$filter", "$filter"), ("$orderby", "$orderBy"), ("$skip", "$skip")):
                if opt in q:
                    raise BadQuery(f"The query parameter '{shown}' is not supported with '$search'.")
            if user.get("graph") == "no-search":
                raise BadQuery("Search isn't available for this mailbox right now.", "ErrorSearchNotAvailable")
            try:
                expr = _parse_kql(search)
            except KqlError as e:
                raise BadQuery(f"Syntax error in $search: {e}.")
            items = sorted((m for m in items if expr(m)), key=lambda m: m["sentDateTime"], reverse=True)[:1000]
            token = q.get("$skiptoken")
            try:
                offset = int(json.loads(base64.urlsafe_b64decode(token + "==").decode())["o"]) if token else 0
            except (ValueError, KeyError, TypeError):
                raise BadQuery("The $skiptoken is not valid.")
        else:
            if "$skiptoken" in q:
                raise BadQuery("The $skiptoken is not valid for this request.")
            preds = _parse_filter(q["$filter"]) if "$filter" in q else []
            order = _parse_orderby(q["$orderby"]) if "$orderby" in q else []
            if preds and order:
                filtered = list(dict.fromkeys(p for p, _, _ in preds))
                if filtered[: len(order)] != [p for p, _ in order]:
                    raise BadQuery("The restriction or sort order is too complex for this operation.", "InefficientFilter")
            for prop, op, value in preds:
                items = [m for m in items if OPS[op](_parse_datetime(m[prop]), value)]
            if not order:
                order = [("receivedDateTime", True)]
            for prop, desc in reversed(order):
                items = sorted(items, key=lambda m: m[prop], reverse=desc)
            raw = q.get("$skip", "0")
            if not raw.isdigit():
                raise BadQuery(f"Invalid value '{raw}' for $skip query option found.")
            offset = int(raw)
        page = [_select(_visible(m, req), q.get("$select"), "message") for m in items[offset : offset + top]]
    except BadQuery as e:
        return _graph_error(400, e.code, str(e))
    folder = "/mailFolders('inbox')" if inbox_only else ""
    body = {"@odata.context": f"https://graph.microsoft.com/v1.0/$metadata#users('{user['oid']}'){folder}/messages", "value": page}
    if offset + top < len(items):
        if user.get("graph") == "evil-link":
            host = req.headers.get("host", "127.0.0.1")
            body["@odata.nextLink"] = f"http://{host}/msgraph/v1.0/me/sendMail?%24skip={offset + top}"
        elif search is not None:
            token = base64.urlsafe_b64encode(json.dumps({"o": offset + top}).encode()).decode().rstrip("=")
            body["@odata.nextLink"] = _next_link(req, path, ("$skiptoken", token))
        else:
            body["@odata.nextLink"] = _next_link(req, path, ("$skip", str(offset + top)))
    return json_response(body)


def _visible(m, req):
    """A message as Graph returns it: no private fields, the body in the format the Prefer header
    asked for."""
    out = {k: v for k, v in m.items() if not k.startswith("_")}
    prefer = req.headers.get("prefer", "").lower()
    if 'outlook.body-content-type="text"' in prefer or "outlook.body-content-type=text" in prefer:
        out["body"] = {"contentType": "text", "content": m["_text"]}
    return out


# --- calendar ------------------------------------------------------------------------------------

TODAY = ANCHOR.replace(hour=0, minute=0)

# (days from today, start "HH:MM", minutes, subject, organizer name, organizer address, location,
#  response, preview, cancelled)
CALENDAR = [
    (3, "10:00", 45, "Lumen Health × Brightwater – partnership intro call", "Tomás Herrera", "tomas.herrera@lumenhealth.example",
     "Microsoft Teams Meeting", "accepted", "Intro call to explore a data partnership for the NHS pilot next spring.", False),
    (6, "14:00", 60, "Panel interview: Director of Analytics Partnerships", "Brightwater People Team", "people@brightwater.example",
     "Board room, 5th floor", "notResponded",
     "Panel interview for the internal Director of Analytics Partnerships role. Please bring a 10-minute presentation on your partnership strategy.", False),
    (9, "13:00", 60, "Canceled: Webinar – Data mesh in practice", "Insight Webinars", "events@insightwebinars.example",
     "Online", "notResponded", "This webinar has been cancelled.", True),
    (12, "16:00", 90, "Panel: The future of open data (ODI Nordics)", "Henrik Aas", "henrik.aas@odi-nordics.example",
     "Oslo Science Park", "notResponded",
     "You're invited to join our panel of four speakers on open data in the Nordics, followed by a reception.", False),
    (18, "11:00", 60, "City of Halden RFP – supplier clarification session", "Marta Lind", "marta.lind@halden-kommune.example",
     "Microsoft Teams Meeting", "tentativelyAccepted", "Clarification session for suppliers responding to the open data platform tender.", False),
    (25, "18:30", 150, "London Analytics Meetup: lightning talks", "London Analytics Meetup", "hello@londonanalytics.example",
     "Shoreditch Works, London", "tentativelyAccepted", "Five-minute lightning talks; the call for speakers is still open until 10 December.", False),
    (28, "09:30", 60, "Hack Week kickoff", "Ben Carter", "ben.carter@brightwater.example",
     "Atrium", "accepted", "Our internal hackathon kicks off: pitch your project in 60 seconds.", False),
    (33, "09:00", 510, "Data & AI Leaders Summit London", "Data & AI Leaders Summit", "events@dataleaders.example",
     "ExCeL London", "notResponded", "Your invitation to the leaders' summit: keynotes, roundtables and an evening networking reception.", False),
    (41, "19:00", 240, "Analytics Excellence Awards 2026 – ceremony and dinner", "Analytics Excellence Awards", "awards@analyticsawards.example",
     "The Brewery, London", "accepted", "Black tie. Your table is number 12.", False),
    (75, "09:00", 60, "Data Summit Europe 2027 – speaker briefing", "Priya Raman", "priya.raman@datasummit-europe.example",
     "Microsoft Teams Meeting", "notResponded", "Briefing for all speakers.", False),
    (-1, "15:00", 30, "Panel prep with Grace", "Grace Liu", "grace.liu@womenindata.example",
     "Microsoft Teams Meeting", "accepted", "Prep call for the responsible AI panel.", False),
    (4, "12:30", 60, "Lunch with Lucy", "Dana Okafor", DEFAULT_USER, "Rivington Street", "organizer", "", False),
    (10, "16:00", 60, "Brightwater all-hands (Q4)", "Brightwater Comms", "comms@brightwater.example", "Atrium", "accepted",
     "Quarterly update from the leadership team.", False),
    (15, "08:00", 60, "Dentist", "Dana Okafor", DEFAULT_USER, "", "organizer", "", False),
    (20, "10:00", 90, "Quarterly business review", "Sam Patel", "sam.patel@brightwater.example", "Board room, 5th floor", "accepted",
     "Q4 numbers, pipeline and hiring plan.", False),
]


def _calendar():
    rows = list(CALENDAR)
    for d in range(-3, 70):
        weekday = (TODAY + timedelta(days=d)).weekday()
        if weekday < 5:
            rows.append((d, "09:30", 15, "Team standup", "Sam Patel", "sam.patel@brightwater.example", "Microsoft Teams Meeting", "accepted",
                         "Daily stand-up for the partnerships team.", False))
        if weekday == 3:
            rows.append((d, "15:00", 30, "1:1 Dana / Sam", "Sam Patel", "sam.patel@brightwater.example", "Microsoft Teams Meeting", "accepted",
                         "Weekly one-to-one: priorities, programme updates and blockers.", False))
        if weekday in (1, 3):
            rows.append((d, "08:00", 120, "Focus time", "Dana Okafor", DEFAULT_USER, "", "organizer", "", False))
        if weekday == 0:
            rows.append((d, "11:00", 30, "Partner sync: Lumen pilot (weekly)", "Tomás Herrera", "tomas.herrera@lumenhealth.example",
                         "Microsoft Teams Meeting", "accepted", "Weekly partner sync on the Lumen pilot.", False))
    events = []
    for n, (days, hhmm, minutes, subject, org_name, org_address, location, response, preview, cancelled) in enumerate(rows, 1):
        hour, minute = map(int, hhmm.split(":"))
        start = TODAY + timedelta(days=days, hours=hour, minutes=minute)
        end = start + timedelta(minutes=minutes)
        eid = _item_id("event", n)
        recurring = subject in ("Team standup", "1:1 Dana / Sam", "Focus time", "Partner sync: Lumen pilot (weekly)")
        events.append({
            "@odata.etag": f'W/"DwAAABYAAAAiIsqMbYjsT5e/T7KzowPTAAB{n:05d}"',
            "id": eid,
            "subject": subject,
            "bodyPreview": preview,
            "body": {"contentType": "html", "content": f"<html><body>{preview}</body></html>"},
            "importance": "normal",
            "sensitivity": "private" if subject == "Dentist" else "normal",
            "isAllDay": False,
            "isCancelled": cancelled,
            "isOrganizer": response == "organizer",
            "isOnlineMeeting": location == "Microsoft Teams Meeting",
            "responseRequested": response != "organizer",
            "showAs": "free" if cancelled else "busy",
            "type": "occurrence" if recurring else "singleInstance",
            "webLink": "https://outlook.office365.com/owa/?itemid=" + quote(eid, safe="") + "&exvsurl=1&path=/calendar/item",
            "responseStatus": {"response": response, "time": "0001-01-01T00:00:00Z"},
            "start": {"dateTime": start.strftime("%Y-%m-%dT%H:%M:%S.0000000"), "timeZone": "UTC"},
            "end": {"dateTime": end.strftime("%Y-%m-%dT%H:%M:%S.0000000"), "timeZone": "UTC"},
            "location": {"displayName": location, "locationType": "default", "uniqueIdType": "unknown", "address": {}, "coordinates": {}},
            "organizer": {"emailAddress": {"name": org_name, "address": org_address}},
            "categories": [],
            "_start": start,
            "_end": end,
        })
    return events


EVENTS = _calendar()


def _calendar_view(req, path):
    try:
        q = _options(req, {"$select", "$top", "$skip", "$orderby", "$count"})
        if not q.get("startdatetime") or not q.get("enddatetime"):
            raise BadQuery("This request requires a time window specified by the query string parameters StartDateTime and EndDateTime.", "ErrorInvalidParameter")
        window = []
        for name in ("startdatetime", "enddatetime"):
            try:
                window.append(_parse_datetime(q[name]))
            except ValueError:
                raise BadQuery(f"The value '{q[name]}' of parameter '{name}' is invalid.", "ErrorInvalidParameter")
        start, end = window
        top = _top(q)
        raw = q.get("$skip", "0")
        if not raw.isdigit():
            raise BadQuery(f"Invalid value '{raw}' for $skip query option found.")
        offset = int(raw)
        items = [e for e in EVENTS if e["_start"] < end and e["_end"] > start]
        desc = False
        if "$orderby" in q:
            bits = q["$orderby"].split()
            if not bits or bits[0].lower() != "start/datetime" or len(bits) > 2 or (len(bits) == 2 and bits[1].lower() not in ("asc", "desc")):
                raise BadQuery(f"The $orderby '{q['$orderby']}' isn't supported by this mock.")
            desc = len(bits) == 2 and bits[1].lower() == "desc"
        items.sort(key=lambda e: e["_start"], reverse=desc)
        page = [_select({k: v for k, v in e.items() if not k.startswith("_")}, q.get("$select"), "event") for e in items[offset : offset + top]]
    except BadQuery as e:
        return _graph_error(400, e.code, str(e))
    body = {"@odata.context": "https://graph.microsoft.com/v1.0/$metadata#users('me')/calendarView", "value": page}
    if offset + top < len(items):
        body["@odata.nextLink"] = _next_link(req, path, ("$skip", str(offset + top)))
    headers = {}
    prefer = req.headers.get("prefer", "")
    if "outlook.timezone" in prefer.lower():
        headers["Preference-Applied"] = prefer
    return json_response(body, headers=headers)
