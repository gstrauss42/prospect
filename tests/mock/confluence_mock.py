"""Mock of Atlassian Cloud for the Confluence connector (stdlib only).

Three upstreams in one module, each under its own prefix:

    /atlassian-auth/...    auth.atlassian.com   OAuth 2.0 (3LO): /authorize, /oauth/token
    /atlassian-api/...     api.atlassian.com    /oauth/token/accessible-resources, and Confluence's
                                                REST API through the gateway:
                                                /ex/confluence/<cloudId>/wiki/rest/api/{search,user/current}
    /confluence-site/...   <name>.atlassian.net the site's own REST API, /wiki/rest/api/{search,user/current}
                                                (API tokens, basic auth), and /_edge/tenant_info

The shapes follow Atlassian's documentation: the token endpoint takes JSON (as documented) or a
form, rotates refresh tokens and answers 403 invalid_grant for a bad one; accessible-resources lists
{id, name, url, scopes, avatarUrl}; search takes CQL and answers results with content._links.webui,
@@@hl@@@ highlight markup, resultGlobalContainer, lastModified and a cursor in _links.next;
user/current answers {"type": "known", accountId, displayName, ...}; 429s carry Retry-After.
The company, people and pages are fictional (Lumenfield).

Read-only guarantee: every data endpoint answers GET only; any other method gets 405 and is
recorded with record_violation("confluence", req), so tests can assert GET /_violations is [].
Asking the authorize page for a write-capable scope is recorded as a violation too.

The OAuth app the mock knows: client_id "mock-atlassian-client-id", secret
"mock-atlassian-client-secret", configured with exactly the read scopes in KNOWN_SCOPES.

Behaviours for edge cases: append &mock_behavior=<name> to the authorize URL; the code, its tokens
and every refresh of them carry it.

    normal               a read-only grant; tokens last an hour
    expiring             the first access token lasts 60 s, so it needs refreshing at once
    write_scope          the token reply's scope includes write:confluence-content
    write_on_refresh     read-only at first (60 s); the refresh reply adds write:confluence-content
    revoked              the refresh token is already revoked (60 s token)
    no_scope             the token reply leaves out "scope"
    rate_limited         the first search request is answered 429 (Retry-After: 30)
    rate_limited_page2   search requests after the first page are answered 429
    resource_write       accessible-resources lists write:confluence-content for the site
    two_sites            accessible-resources lists a sandbox site first, then the real one

Refresh tokens rotate strictly: a used one is dead, and using it again revokes its whole family
(Atlassian's breach detection; its 10-minute reuse leeway is left out on purpose, so a client that
keeps an old refresh token fails here at once).

API tokens (basic auth, email:token):
    sam.okafor@lumenfield.example : mock-classic-api-token   classic: the site's own API only
    sam.okafor@lumenfield.example : mock-scoped-api-token    scoped:  the gateway only
    anonymous.fallback@lumenfield.example : anything         the site answers as a signed-out visitor
"""

import base64
import hashlib
import html
import re
import secrets
import threading
import time
from urllib.parse import quote, urlencode

from mocklib import Response, json_response, not_found, record_violation

CLIENT_ID = "mock-atlassian-client-id"
CLIENT_SECRET = "mock-atlassian-client-secret"
CLOUD_ID = "4f8b2c1a-7d3e-4b6a-9c0f-1e2d3c4b5a69"
SANDBOX_CLOUD_ID = "9a0b1c2d-3e4f-4a5b-8c6d-7e8f9a0b1c2d"
SITE_NAME = "lumenfield"
SANDBOX_URL = "https://lumenfield-sandbox.atlassian.net"
AVATAR = "https://site-admin-avatar-cdn.prod.public.atl-paas.net/avatars/240/rocket.png"
WRITE_SCOPE = "write:confluence-content"
# The scopes this mock's OAuth app was given in the developer console: read only.
KNOWN_SCOPES = {
    "search:confluence",
    "read:confluence-content.summary",
    "read:confluence-space.summary",
    "read:confluence-user",
    "offline_access",
}
WRITE_MARKERS = ("write", "manage", "delete", "admin", "create", "update")
ACCESS_TTL = 3600
SHORT_TTL = 60
CODE_TTL = 600
RETRY_AFTER = "30"
BEHAVIOURS = {
    "normal",
    "expiring",
    "write_scope",
    "write_on_refresh",
    "revoked",
    "no_scope",
    "rate_limited",
    "rate_limited_page2",
    "resource_write",
    "two_sites",
}
SHORT_LIVED = {"expiring", "write_on_refresh", "revoked"}

OAUTH_USER = {
    "accountId": "712020:6b1f3c2e-8a4d-4e5f-9b0c-1d2e3f4a5b6c",
    "displayName": "Dana Whitfield",
    "email": "dana.whitfield@lumenfield.example",
}
TOKEN_USER = {
    "accountId": "712020:0c9d8e7f-6a5b-4c3d-2e1f-0a9b8c7d6e5f",
    "displayName": "Sam Okafor",
    "email": "sam.okafor@lumenfield.example",
}
API_TOKENS = {
    ("sam.okafor@lumenfield.example", "mock-classic-api-token"): "classic",
    ("sam.okafor@lumenfield.example", "mock-scoped-api-token"): "scoped",
}
ANONYMOUS_EMAIL = "anonymous.fallback@lumenfield.example"

SPACES = {
    "INNO": "Innovation Lab",
    "PEOPLE": "People & Culture",
    "ENG": "Engineering",
    "STRAT": "Strategy & Planning",
    "IT": "IT Help Desk",
    "MKT": "Marketing",
    "OPS": "Workplace",
}

# (id, type, space, title, body, days since last modified, author)
PAGES = [
    (393217, "page", "INNO", "Innovation Week 2026: call for project ideas",
     "Innovation Week runs 8 to 12 December. Every team is invited to pitch a hackathon project, from internal tooling to a new product bet. "
     "Submit your idea on this page by 1 December; teams of up to five can apply, and the three winning projects get two sprints and a "
     "budget of 10,000 euros to build a prototype, with mentoring from the leadership team.", 3, "Priya Raman"),
    (393218, "page", "PEOPLE", "Mentoring programme: applications open for the spring cohort",
     "Our mentoring programme pairs colleagues across departments for six months. We're looking for mentors with three or more years of "
     "experience, and for mentees who want to grow into leadership or change track. Applications close on 15 January.", 6, "Marta Kowalczyk"),
    (393219, "page", "ENG", "Call for proposals: represent Lumenfield at DevConf Europe 2027",
     "The conference team is collecting talk proposals for DevConf Europe (Lisbon, April 2027). Lumenfield sponsors the event and has three "
     "speaking slots. Send a 200-word abstract by 9 January; we help with slides and rehearsals, and first-time speakers are encouraged.",
     10, "Tom Achterberg"),
    (393220, "page", "PEOPLE", "Open roles: internal applications welcome",
     "Current openings: Staff Data Engineer (Platform), Product Manager (Payments) and Engineering Manager (Edge Devices). Internal "
     "candidates are guaranteed a first interview: talk to your manager and apply through Workday before the posting closes.", 2, "Hiring Team"),
    (393221, "page", "STRAT", "2027 strategy: where Lumenfield is heading",
     "Next year we enter industrial IoT and open an engineering hub in Lisbon. We will be hiring about 40 engineers and creating a "
     "partnerships team to work with integrators. Each department will put people forward for a secondment to the new hub.", 12, "Elena Duarte"),
    (393222, "blogpost", "MKT", "We're sponsoring the City Robotics Challenge: volunteers wanted",
     "Lumenfield is the main sponsor of this year's City Robotics Challenge for secondary schools. We need volunteer judges and mentors "
     "on 6 December; sign up by 28 November. It's a great way to meet the local engineering community.", 4, "Jonas Berg"),
    (393223, "page", "STRAT", "R&D grants: internal funding round for Q1",
     "The R&D board has opened its quarterly funding round: up to 25,000 euros per project. Proposals are reviewed on 20 December. "
     "Tip: \"small, measurable\" beats \"big & vague\" <every time>.", 8, "Elena Duarte"),
    (393224, "page", "PEOPLE", "Secondment opportunity: six months with the Singapore office",
     "The Singapore team is looking for an engineer to join them for six months from February to set up our APAC support tooling. "
     "Relocation is covered. Expressions of interest by 10 December.", 5, "Marta Kowalczyk"),
    (393225, "page", "OPS", "Café Lumen partnership: baristas wanted for the opening week",
     "Our new partnership with Café Lumen starts on 1 December. Volunteers from every floor can help run the opening week and pick the menu.",
     9, "Workplace Team"),
    # Routine pages that mention nothing the search asks for.
    (393226, "page", "ENG", "Q4 platform team retro",
     "What went well: the migration finished on time. What to improve: on-call handover notes, flaky integration tests. "
     "Actions: Alex writes the runbook; Sam fixes the test harness.", 7, "Alex Chen"),
    (393227, "page", "IT", "VPN setup guide",
     "Install the client from the self-service portal, sign in with your SSO account and pick the nearest gateway. If the connection "
     "drops, restart the client before raising a ticket.", 9, "IT Help Desk"),
    (393228, "page", "OPS", "Office move: parking and access cards",
     "From 1 December the Riverside office moves to floor 4. Collect your new access card at reception; parking permits carry over "
     "automatically.", 11, "Workplace Team"),
    # Relevant, but too old for a 30-day search.
    (393229, "page", "INNO", "Summer hackathon 2026: results",
     "Thanks to everyone who took part in the summer hackathon. The winning team built a predictive maintenance demo.", 120, "Priya Raman"),
    (393230, "page", "ENG", "Autumn conference recap",
     "Notes from the conference: three talks by our speakers and a panel on edge computing.", 45, "Tom Achterberg"),
]
# Routine pages that do match a keyword ("deadline"), enough to need a second page of results.
_COMPONENTS = ["billing", "edge agent", "data platform", "mobile app", "search"]
PAGES += [
    (393300 + i, "page", "ENG", f"Sprint {40 + i} planning notes",
     f"Sprint goals, capacity and the release deadline for the {_COMPONENTS[i % len(_COMPONENTS)]} work. Carry-over items are listed below.",
     1 + (i * 27) // 29, "Alex Chen")
    for i in range(30)
]

LOCK = threading.Lock()
CODES = {}  # code -> {redirect_uri, scopes, behaviour, exp, used}
ACCESS = {}  # access token -> {scopes, exp, behaviour}
REFRESH = {}  # refresh token -> {family, active, scopes, behaviour}
FAMILIES = {}  # family id -> {"revoked": bool}


def reset():
    with LOCK:
        CODES.clear()
        ACCESS.clear()
        REFRESH.clear()
        FAMILIES.clear()


def handle(req):
    path = req.path
    if path.startswith("/atlassian-auth/"):
        return _auth(req, path[len("/atlassian-auth"):])
    if path.startswith("/atlassian-api/"):
        return _api(req, path[len("/atlassian-api"):])
    if path.startswith("/confluence-site/"):
        return _site(req, path[len("/confluence-site"):])
    return not_found(path)


# --- replies ---------------------------------------------------------------------------------------


def _oauth_error(status, error, description):
    return json_response({"error": error, "error_description": description}, status)


def _gateway_error(status, message):
    return json_response({"code": status, "message": message}, status)


def _site_error(status, message):
    body = {"statusCode": status, "data": {"authorized": False, "valid": True, "errors": [], "successful": False}, "message": message}
    return json_response(body, status)


def _not_get(req):
    """405 for anything but GET on a data endpoint; a write method is a violation."""
    if req.method not in ("GET", "HEAD"):
        record_violation("confluence", req)
    return json_response({"code": 405, "message": "Method Not Allowed"}, 405, {"Allow": "GET"})


def _rate_limited():
    return json_response({"code": 429, "message": "Too Many Requests"}, 429, {"Retry-After": RETRY_AFTER, "RateLimit-Reason": "confluence-burst-based"})


def _credentials(req):
    """("bearer", token), ("basic", (user, secret)) or (None, None)."""
    header = req.headers.get("authorization", "")
    scheme, _, value = header.partition(" ")
    if scheme.lower() == "bearer" and value.strip():
        return "bearer", value.strip()
    if scheme.lower() == "basic":
        try:
            user, _, secret = base64.b64decode(value.strip(), validate=True).decode("utf-8").partition(":")
        except (ValueError, UnicodeDecodeError):
            return None, None
        return "basic", (user, secret)
    return None, None


def _site_base(req):
    return f"http://{req.headers.get('host', '127.0.0.1')}/confluence-site"


def _write_capable(scope):
    s = scope.lower()
    return any(m in s for m in WRITE_MARKERS)


# --- auth.atlassian.com ----------------------------------------------------------------------------


def _auth(req, path):
    if path == "/authorize":
        return _authorize(req)
    if path == "/oauth/token":
        return _token(req)
    return not_found(req.path)


def _authorize(req):
    if req.method != "GET":
        return json_response({"error": "method_not_allowed"}, 405)
    q = req.query
    problems = []
    if q.get("audience") != "api.atlassian.com":
        problems.append("audience must be api.atlassian.com")
    if q.get("client_id") != CLIENT_ID:
        problems.append("unknown client_id")
    if q.get("response_type") != "code":
        problems.append("response_type must be code")
    if q.get("prompt") != "consent":
        problems.append("prompt must be consent")
    redirect = q.get("redirect_uri", "")
    if not redirect.startswith(("http://", "https://")):
        problems.append("redirect_uri is missing")
    if not q.get("state"):
        problems.append("state is missing")
    scopes = q.get("scope", "").split()
    if not scopes:
        problems.append("scope is missing")
    if any(_write_capable(s) for s in scopes):
        record_violation("confluence", req)
        problems.append("a write scope was requested")
    unknown = [s for s in scopes if s not in KNOWN_SCOPES]
    if unknown:
        problems.append("scopes not added to the app: " + " ".join(unknown))
    behaviour = q.get("mock_behavior", "normal")
    if behaviour not in BEHAVIOURS:
        problems.append("unknown mock_behavior")
    if problems:
        return _oauth_error(400, "invalid_request", "; ".join(problems))
    code = "mock-code-" + secrets.token_urlsafe(18)
    with LOCK:
        CODES[code] = {"redirect_uri": redirect, "scopes": scopes, "behaviour": behaviour, "exp": time.time() + CODE_TTL, "used": False}
    sep = "&" if "?" in redirect else "?"
    location = redirect + sep + urlencode({"code": code, "state": q["state"]})
    return Response(status=302, body=b"", content_type="text/html", headers={"Location": location})


def _token(req):
    if req.method != "POST":
        return json_response({"error": "method_not_allowed"}, 405)
    if "json" in req.headers.get("content-type", ""):
        body = req.json()
        body = body if isinstance(body, dict) else {}
    else:
        body = {k: v[0] for k, v in req.form().items()}
    client_id, client_secret = body.get("client_id"), body.get("client_secret")
    kind, cred = _credentials(req)
    if kind == "basic":
        client_id, client_secret = cred
    if client_id != CLIENT_ID or client_secret != CLIENT_SECRET:
        return _oauth_error(401, "access_denied", "Unauthorized")
    grant = body.get("grant_type")
    if grant == "authorization_code":
        return _exchange(body)
    if grant == "refresh_token":
        return _refresh(body)
    return _oauth_error(400, "unsupported_grant_type", "Unsupported grant type")


def _exchange(body):
    code, redirect = body.get("code") or "", body.get("redirect_uri") or ""
    with LOCK:
        entry = CODES.get(code)
        if not entry or entry["used"] or entry["exp"] < time.time() or entry["redirect_uri"] != redirect:
            if entry:
                entry["used"] = True  # a replayed or misdirected code is burnt
            return _oauth_error(403, "invalid_grant", "Invalid authorization code")
        entry["used"] = True
        behaviour, scopes = entry["behaviour"], list(entry["scopes"])
        if behaviour == "write_scope":
            scopes.append(WRITE_SCOPE)
        family = secrets.token_hex(8)
        FAMILIES[family] = {"revoked": behaviour == "revoked"}
        ttl = SHORT_TTL if behaviour in SHORT_LIVED else ACCESS_TTL
        return _issue(scopes, behaviour, family, ttl)


def _refresh(body):
    token = body.get("refresh_token") or ""
    with LOCK:
        entry = REFRESH.get(token)
        family = FAMILIES.get(entry["family"]) if entry else None
        if not entry or not family or family["revoked"]:
            return _oauth_error(403, "invalid_grant", "Unknown or invalid refresh token.")
        if not entry["active"]:
            family["revoked"] = True  # reuse of a rotated-out token: breach detection
            return _oauth_error(403, "invalid_grant", "Unknown or invalid refresh token.")
        entry["active"] = False
        behaviour, scopes = entry["behaviour"], list(entry["scopes"])
        if behaviour == "write_on_refresh" and WRITE_SCOPE not in scopes:
            scopes.append(WRITE_SCOPE)
        return _issue(scopes, behaviour, entry["family"], ACCESS_TTL)


def _issue(scopes, behaviour, family, ttl):
    """A token reply (caller holds LOCK)."""
    access = "mock-access-" + secrets.token_urlsafe(24)
    ACCESS[access] = {"scopes": scopes, "exp": time.time() + ttl, "behaviour": behaviour}
    reply = {"access_token": access, "expires_in": ttl, "token_type": "Bearer"}
    if behaviour != "no_scope":
        reply["scope"] = " ".join(scopes)
    if "offline_access" in scopes:
        refresh = "mock-refresh-" + secrets.token_urlsafe(24)
        REFRESH[refresh] = {"family": family, "active": True, "scopes": scopes, "behaviour": behaviour}
        reply["refresh_token"] = refresh
    return json_response(reply)


# --- api.atlassian.com -----------------------------------------------------------------------------


def _access(req):
    """The live access-token entry of a bearer request, or None."""
    kind, token = _credentials(req)
    if kind != "bearer":
        return None
    with LOCK:
        entry = ACCESS.get(token)
        if not entry or entry["exp"] < time.time():
            return None
        return dict(entry)


def _api(req, path):
    if req.method != "GET":
        return _not_get(req)
    if path == "/oauth/token/accessible-resources":
        return _accessible_resources(req)
    m = re.fullmatch(r"/ex/confluence/([^/]+)(/wiki/rest/api/[a-z/]+)", path)
    if not m:
        return _gateway_error(404, "Not Found")
    cloud, rest = m.groups()
    kind, cred = _credentials(req)
    if kind == "bearer":
        entry = _access(req)
        if not entry:
            return _gateway_error(401, "Unauthorized")
        who = {"user": OAUTH_USER, "scopes": entry["scopes"], "behaviour": entry["behaviour"]}
    elif kind == "basic" and API_TOKENS.get(cred) == "scoped":
        who = {"user": TOKEN_USER, "scopes": None, "behaviour": "normal"}
    else:
        # Bad credentials, and (as assumed here) classic API tokens, which belong on the site.
        return _gateway_error(401, "Unauthorized")
    if cloud not in (CLOUD_ID, SANDBOX_CLOUD_ID) or (cloud == SANDBOX_CLOUD_ID and who["behaviour"] != "two_sites"):
        return _gateway_error(404, "Not Found")
    need = {"/wiki/rest/api/search": "search:confluence", "/wiki/rest/api/user/current": "read:confluence-user"}.get(rest)
    if need is None:
        return _gateway_error(404, "Not Found")
    if who["scopes"] is not None and need not in who["scopes"]:
        return _gateway_error(401, "Unauthorized; scope does not match")
    if rest.endswith("/user/current"):
        return json_response(_user_json(who["user"], _site_base(req)))
    pages = PAGES if cloud == CLOUD_ID else []
    return _search(req, who["behaviour"], pages, _site_base(req))


def _accessible_resources(req):
    entry = _access(req)
    if not entry:
        return _gateway_error(401, "Unauthorized")
    product = [s for s in entry["scopes"] if s != "offline_access"]
    if entry["behaviour"] == "resource_write" and WRITE_SCOPE not in product:
        product.append(WRITE_SCOPE)
    site = {"id": CLOUD_ID, "name": SITE_NAME, "url": _site_base(req), "scopes": product, "avatarUrl": AVATAR}
    if entry["behaviour"] == "two_sites":
        sandbox = {"id": SANDBOX_CLOUD_ID, "name": "lumenfield-sandbox", "url": SANDBOX_URL, "scopes": product, "avatarUrl": AVATAR}
        return json_response([sandbox, site])
    return json_response([site])


# --- <name>.atlassian.net --------------------------------------------------------------------------


def _site(req, path):
    if req.method != "GET":
        return _not_get(req)
    if path == "/_edge/tenant_info":
        return json_response({"cloudId": CLOUD_ID})
    if path not in ("/wiki/rest/api/search", "/wiki/rest/api/user/current"):
        return _site_error(404, "Not Found")
    kind, cred = _credentials(req)
    if kind == "basic" and cred[0] == ANONYMOUS_EMAIL:
        # The credentials failed, and the site carried on as a signed-out visitor.
        if path.endswith("/user/current"):
            return json_response(_anonymous_json(_site_base(req)))
        return _search(req, "normal", [], _site_base(req))
    if kind != "basic" or API_TOKENS.get(cred) != "classic":
        # Bad credentials, OAuth tokens and scoped API tokens (which only work through the gateway).
        return _site_error(401, "Client must be authenticated to access this resource.")
    if path.endswith("/user/current"):
        return json_response(_user_json(TOKEN_USER, _site_base(req)))
    return _search(req, "normal", PAGES, _site_base(req))


def _user_json(user, site_base):
    return {
        "type": "known",
        "accountId": user["accountId"],
        "accountType": "atlassian",
        "email": user["email"],
        "publicName": user["displayName"],
        "profilePicture": {"path": "/wiki/aa-avatar/" + user["accountId"], "width": 48, "height": 48, "isDefault": False},
        "displayName": user["displayName"],
        "timeZone": "Europe/London",
        "isExternalCollaborator": False,
        "isGuest": False,
        "externalCollaborator": False,
        "operations": None,
        "_expandable": {"operations": "", "personalSpace": ""},
        "_links": {"self": f"{site_base}/wiki/rest/api/user?accountId={quote(user['accountId'])}", "base": f"{site_base}/wiki", "context": "/wiki"},
    }


def _anonymous_json(site_base):
    return {
        "type": "anonymous",
        "profilePicture": {"path": "/wiki/s/-1/default-avatar.png", "width": 48, "height": 48, "isDefault": True},
        "displayName": "Anonymous",
        "operations": None,
        "isExternalCollaborator": False,
        "_expandable": {"operations": ""},
        "_links": {"base": f"{site_base}/wiki", "context": "/wiki"},
    }


# --- search ----------------------------------------------------------------------------------------


def _bad_cql(cql, why):
    message = f"com.atlassian.confluence.api.service.exceptions.BadRequestException: Could not parse cql : {cql} ({why})"
    return _site_error(400, message)


def _search(req, behaviour, pages, site_base):
    q = req.query
    if behaviour == "rate_limited" and not q.get("cursor"):
        return _rate_limited()
    if behaviour == "rate_limited_page2" and q.get("cursor"):
        return _rate_limited()
    cql = q.get("cql", "")
    if not cql:
        return _site_error(400, "cql is required")
    try:
        tree, order = parse_cql(cql)
    except CqlError as e:
        return _bad_cql(cql, str(e))
    try:
        limit = min(max(int(q.get("limit", "25")), 0), 100)
        offset = _cursor_offset(q["cursor"], cql) if q.get("cursor") else max(int(q.get("start", "0")), 0)
    except (ValueError, CqlError) as e:
        return _site_error(400, f"bad paging parameters ({e})")
    mode = q.get("excerpt", "highlight")
    if mode not in ("highlight", "indexed", "none", "highlight_unescaped", "indexed_unescaped"):
        return _site_error(400, "unknown excerpt strategy")
    expand = {e.strip() for e in q.get("expand", "").split(",") if e.strip()}
    today = time.time()
    docs = [_doc(p, today) for p in pages]
    matched = [d for d in docs if _eval(tree, d)]
    for field, direction in reversed(order):
        key = {"lastmodified": "stamp", "created": "stamp", "title": "title"}.get(field, "id")
        matched.sort(key=lambda d: d[key], reverse=direction == "DESC")
    window = matched[offset:offset + limit]
    terms = _text_terms(tree)
    results = [_result(d, terms, mode, expand, site_base) for d in window]
    links = {"base": f"{site_base}/wiki", "context": "/wiki", "self": f"{site_base}/wiki/rest/api/search?" + urlencode({"cql": cql})}
    if offset + limit < len(matched) and limit > 0:
        nxt = offset + limit
        params = [("next", "true"), ("cursor", _cursor(nxt, cql)), ("limit", str(limit)), ("start", str(nxt)), ("cql", cql)]
        if "excerpt" in q:
            params.append(("excerpt", mode))
        links["next"] = "/rest/api/search?" + urlencode(params)
    if offset > 0:
        prev = max(offset - limit, 0)
        links["prev"] = "/rest/api/search?" + urlencode([("prev", "true"), ("cursor", _cursor(prev, cql)), ("limit", str(limit)), ("start", str(prev)), ("cql", cql)])
    return json_response({
        "results": results,
        "start": offset,
        "limit": limit,
        "size": len(results),
        "totalSize": len(matched),
        "cqlQuery": cql,
        "searchDuration": 37,
        "archivedResultCount": 0,
        "_links": links,
    })


def _cursor(offset, cql):
    """Opaque, bound to the query it came from (Atlassian's look like _f_<base64>_sa_<...>)."""
    raw = base64.b64encode(str(offset).encode()).decode()
    return f"_f_{raw}_sa_{hashlib.sha256(cql.encode()).hexdigest()[:12]}"


def _cursor_offset(cursor, cql):
    m = re.fullmatch(r"_f_([A-Za-z0-9+/=]+)_sa_([0-9a-f]{12})", cursor)
    if not m:
        raise CqlError("invalid cursor")
    if m.group(2) != hashlib.sha256(cql.encode()).hexdigest()[:12]:
        raise CqlError("the cursor belongs to another query")
    return int(base64.b64decode(m.group(1)).decode())


def _doc(page, now):
    pid, kind, space, title, body, days, author = page
    at = now - days * 86400 - 3 * 3600
    stamp = time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime(at))
    return {"id": pid, "type": kind, "space": space, "title": title, "body": body, "author": author,
            "lastmodified": stamp[:10], "created": stamp[:10], "stamp": stamp, "friendly": time.strftime("%b %d, %Y", time.gmtime(at))}


def _slug(title):
    words = re.findall(r"\w+", title)
    return quote("+".join(words), safe="+")


def _result(d, terms, mode, expand, site_base):
    if d["type"] == "blogpost":
        y, mth, day = d["lastmodified"].split("-")
        webui = f"/spaces/{d['space']}/blog/{y}/{mth}/{day}/{d['id']}/{_slug(d['title'])}"
    else:
        webui = f"/spaces/{d['space']}/pages/{d['id']}/{_slug(d['title'])}"
    content = {
        "id": str(d["id"]),
        "type": d["type"],
        "status": "current",
        "title": d["title"],
        "childTypes": {},
        "macroRenderedOutput": {},
        "restrictions": {},
        "_expandable": {"container": "", "metadata": "", "extensions": "", "operations": "", "children": "",
                        "history": f"/rest/api/content/{d['id']}/history", "ancestors": "", "body": "", "version": "",
                        "descendants": "", "space": f"/rest/api/space/{d['space']}"},
        "_links": {"webui": webui, "self": f"{site_base}/wiki/rest/api/content/{d['id']}",
                   "tinyui": "/x/" + base64.urlsafe_b64encode(str(d["id"]).encode()).decode().rstrip("=")},
    }
    if "content.history" in expand:
        content["history"] = {"latest": True, "createdBy": {"type": "known", "displayName": d["author"]}, "createdDate": d["stamp"]}
        content["_expandable"].pop("history")
    highlight = mode.startswith("highlight")
    escaped = not mode.endswith("_unescaped")
    excerpt = "" if mode == "none" else _mark(_window(d["body"], terms), terms, highlight, escaped)
    return {
        "content": content,
        "title": _mark(d["title"], terms, highlight, escaped),
        "excerpt": excerpt,
        "url": webui,
        "resultGlobalContainer": {"title": SPACES[d["space"]], "displayUrl": f"/spaces/{d['space']}"},
        "breadcrumbs": [],
        "entityType": "content",
        "iconCssClass": "aui-icon content-type-" + d["type"],
        "lastModified": d["stamp"],
        "friendlyLastModified": d["friendly"],
        "score": round(1.0 + len(terms) / 100.0, 3),
    }


def _matched_spans(text, terms):
    """(start, end) of the words of `text` that a search term matches."""
    words = [(m.start(), m.end(), m.group().lower()) for m in re.finditer(r"\w+", text)]
    spans = []
    for kind, value in terms:
        if kind == "term":
            spans += [(s, e) for s, e, w in words if w.startswith(value)]
        else:
            n = len(value)
            for i in range(len(words) - n + 1):
                if [w for _, _, w in words[i:i + n]] == value:
                    spans.append((words[i][0], words[i + n - 1][1]))
    merged = []
    for s, e in sorted(spans):
        if merged and s <= merged[-1][1]:
            merged[-1] = (merged[-1][0], max(e, merged[-1][1]))
        else:
            merged.append((s, e))
    return merged


def _window(body, terms, size=280):
    if len(body) <= size:
        return body
    spans = _matched_spans(body, terms)
    first = spans[0][0] if spans else 0
    start = max(0, first - 80)
    while start > 0 and not body[start - 1].isspace():
        start -= 1
    end = min(len(body), start + size)
    while end < len(body) and not body[end].isspace():
        end += 1
    return body[start:end].strip()


def _mark(text, terms, highlight, escaped):
    esc = html.escape if escaped else (lambda s: s)
    if not highlight:
        return esc(text)
    out, at = [], 0
    for s, e in _matched_spans(text, terms):
        out += [esc(text[at:s]), "@@@hl@@@", esc(text[s:e]), "@@@endhl@@@"]
        at = e
    out.append(esc(text[at:]))
    return "".join(out)


# --- a small CQL --------------------------------------------------------------------------------------


class CqlError(ValueError):
    pass


FIELDS = {"type", "lastmodified", "created", "text", "title", "space", "id"}
KEYWORDS = {"AND", "OR", "NOT", "IN", "ORDER", "BY", "ASC", "DESC"}
TOKEN = re.compile(r'''\s*(?:(?P<str>"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')|(?P<op>!=|!~|>=|<=|=|~|>|<)|(?P<punc>[(),])|(?P<word>[A-Za-z0-9_.:\-]+))''', re.S)
LUCENE_SPECIAL = set('+-&|!(){}[]^"~*?:/\\')


def _unescape(s):
    out, i = [], 0
    while i < len(s):
        if s[i] == "\\" and i + 1 < len(s):
            out.append(s[i + 1])
            i += 2
        else:
            out.append(s[i])
            i += 1
    return "".join(out)


def _tokens(cql):
    out, pos = [], 0
    while pos < len(cql):
        if not cql[pos:].strip():
            break
        m = TOKEN.match(cql, pos)
        if not m or m.end() == pos:
            raise CqlError(f"unexpected character at {pos}")
        pos = m.end()
        if m.group("str") is not None:
            out.append(("str", _unescape(m.group("str")[1:-1])))
        elif m.group("op"):
            out.append(("op", m.group("op")))
        elif m.group("punc"):
            out.append((m.group("punc"), m.group("punc")))
        elif m.group("word").upper() in KEYWORDS:
            out.append((m.group("word").upper(), None))
        else:
            out.append(("word", m.group("word")))
    return out


class _Parser:
    def __init__(self, tokens):
        self.t, self.i = tokens, 0

    def peek(self):
        return self.t[self.i][0] if self.i < len(self.t) else None

    def take(self, kind):
        if self.peek() != kind:
            raise CqlError(f"expected {kind}, found {self.peek()}")
        self.i += 1
        return self.t[self.i - 1][1]

    def query(self):
        tree = self.expr()
        order = []
        if self.peek() == "ORDER":
            self.take("ORDER")
            self.take("BY")
            while True:
                field = self.take("word").lower()
                if field not in FIELDS:
                    raise CqlError(f"can't order by {field}")
                direction = "ASC"
                if self.peek() in ("ASC", "DESC"):
                    direction = self.peek()
                    self.i += 1
                order.append((field, direction))
                if self.peek() != ",":
                    break
                self.take(",")
        if self.i != len(self.t):
            raise CqlError("unexpected text after the query")
        return tree, order

    def expr(self):
        node = self.term()
        while self.peek() == "OR":
            self.take("OR")
            node = ("or", node, self.term())
        return node

    def term(self):
        node = self.factor()
        while self.peek() == "AND":
            self.take("AND")
            node = ("and", node, self.factor())
        return node

    def factor(self):
        if self.peek() == "NOT":
            self.take("NOT")
            return ("not", self.factor())
        if self.peek() == "(":
            self.take("(")
            node = self.expr()
            self.take(")")
            return node
        return self.clause()

    def clause(self):
        field = self.take("word").lower()
        if field not in FIELDS:
            raise CqlError(f"unknown field {field}")
        if self.peek() in ("IN", "NOT"):
            negate = self.peek() == "NOT"
            if negate:
                self.take("NOT")
            self.take("IN")
            self.take("(")
            values = [self.value()]
            while self.peek() == ",":
                self.take(",")
                values.append(self.value())
            self.take(")")
            if field in ("text", "title"):
                raise CqlError(f"{field} can't be used with IN")
            return ("in", field, values, negate)
        op = self.take("op")
        value = self.value()
        if field in ("text", "title"):
            if op not in ("~", "!~") or value[0] != "lit":
                raise CqlError(f"{field} needs ~ and a string")
            return ("text", field, op, _lucene(value[1]))
        if op in ("~", "!~"):
            raise CqlError(f"{field} can't be used with ~")
        if field in ("lastmodified", "created"):
            return ("date", field, op, _date_value(value))
        if op not in ("=", "!=") or value[0] != "lit":
            raise CqlError(f"{field} supports = and != with a value only")
        return ("cmp", field, op, value)

    def value(self):
        kind = self.peek()
        if kind == "str":
            return ("lit", self.take("str"))
        if kind == "word":
            word = self.take("word")
            if self.peek() == "(":
                self.take("(")
                args = []
                if self.peek() != ")":
                    args.append(self.value())
                self.take(")")
                return ("fn", word.lower(), args)
            return ("lit", word)
        raise CqlError("expected a value")


def parse_cql(cql):
    return _Parser(_tokens(cql)).query()


def _lucene(value):
    """A text-search value: ("phrase", [words]) or ("term", word). Reserved characters must be
    escaped (Atlassian: precede them with two backslashes), so an unescaped one is an error."""
    v = value.strip()
    if len(v) >= 2 and v[0] == '"' and v[-1] == '"':
        inner = v[1:-1]
        if '"' in re.sub(r"\\.", "", inner):
            raise CqlError("unbalanced quotes in a text search")
        words = re.findall(r"\w+", _unescape(inner).lower())
        if not words:
            raise CqlError("an empty phrase")
        return ("phrase", words)
    bare = re.sub(r"\\.", "", v)
    if any(c in LUCENE_SPECIAL or c.isspace() for c in bare):
        raise CqlError(f"unescaped reserved character in text search {value!r}")
    words = re.findall(r"\w+", _unescape(v).lower())
    if len(words) != 1:
        raise CqlError(f"a text-search term must be one word: {value!r}")
    return ("term", words[0])


def _date_value(value):
    if value[0] == "lit":
        m = re.fullmatch(r"(\d{4})[-/](\d{2})[-/](\d{2})(?: \d{2}:\d{2})?", value[1])
        if not m:
            raise CqlError(f"bad date {value[1]!r}")
        return "-".join(m.groups())
    if value[0] == "fn" and value[1] in ("now", "startofday"):
        shift = 0
        if value[2]:
            m = re.fullmatch(r"([+-]?)(\d+)d", value[2][0][1])
            if not m:
                raise CqlError("only day offsets are supported here")
            shift = int(m.group(2)) * (-1 if m.group(1) == "-" else 1)
        return time.strftime("%Y-%m-%d", time.gmtime(time.time() + shift * 86400))
    raise CqlError("unsupported date value")


def _text_terms(node):
    if node[0] in ("or", "and"):
        return _text_terms(node[1]) + _text_terms(node[2])
    if node[0] == "not":
        return []
    if node[0] == "text" and node[2] == "~":
        return [node[3]]
    return []


def _eval(node, d):
    kind = node[0]
    if kind == "or":
        return _eval(node[1], d) or _eval(node[2], d)
    if kind == "and":
        return _eval(node[1], d) and _eval(node[2], d)
    if kind == "not":
        return not _eval(node[1], d)
    if kind == "text":
        _, field, op, term = node
        hay = d["title"] if field == "title" else d["title"] + " " + d["body"]
        hit = bool(_matched_spans(hay, [term]))
        return hit if op == "~" else not hit
    if kind == "date":
        _, field, op, day = node
        got = d[field]
        return {">=": got >= day, ">": got > day, "<=": got <= day, "<": got < day, "=": got == day, "!=": got != day}[op]
    if kind == "in":
        _, field, values, negate = node
        hit = any(str(d[field]).lower() == str(v[1]).lower() for v in values if v[0] == "lit")
        return hit != negate
    if kind == "cmp":
        _, field, op, value = node
        hit = str(d[field]).lower() == str(value[1]).lower()
        return hit if op == "=" else not hit
    raise CqlError("unsupported clause")
