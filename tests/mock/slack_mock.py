"""Mock of Slack for the read-only Slack connector: OAuth v2 and the three Web API read methods the
connector uses, over a fictional workspace (Fernhill Analytics) whose channels hold genuine
opportunities among everyday noise. Owner: the Slack connector builder.

Slack routes (all under /slack):

    GET  /slack/oauth/v2/authorize      an instant "Allow": 302 to redirect_uri?code=...&state=...
                                        (add mock_deny=1 for error=access_denied&state=...)
    POST /slack/api/oauth.v2.access     code exchange, and refresh with grant_type=refresh_token;
                                        client id/secret in HTTP Basic auth (or in the form)
    GET  /slack/api/auth.test           who the token is
    GET  /slack/api/search.messages     Slack's search syntax: words (all must match), "phrases",
                                        a OR b, -exclusions, <@U...> mentions, after:/before:/on:
                                        (YYYY-MM-DD; after and before exclusive), in:, from:
    GET  /slack/api/users.info          a member's profile

Like Slack, every Web API reply carries x-oauth-scopes (the scopes the token really has; empty for
a token Slack doesn't know) and x-accepted-oauth-scopes, and errors are HTTP 200
{"ok": false, "error": ...} except rate limits (HTTP 429 with Retry-After). The token must be a
Bearer Authorization header; a token in the query string is refused.

The read-only guarantee: the read methods answer GET only. Any other HTTP method on them, and any
request at all to another Web API method (chat.postMessage, conversations.history, auth.revoke,
team.info...), gets 405 and is recorded with record_violation("slack", ...), so GET /_violations
must stay [].

Tokens to paste:
    xoxp-mock-ada, xoxp-mock-ada-<anything>   Ada Okafor, the person: search:read,users:read
                                              (each distinct token has its own state)
    xoxp-mock-writer          also has chat:write
    xoxp-mock-legacy          a legacy all-access token: identify,read,post,client
    xoxp-mock-revoked         revoked (token_revoked)
    xoxp-mock-noheader        replies come without x-oauth-scopes (a proxy that strips it)
    xoxp-mock-ratelimited     search.messages is always rate limited (429, Retry-After: 30)
    xoxp-mock-nosearch        users:read only
    xoxb-mock-bot             a bot token

Codes for oauth.v2.access, besides the ones the authorize page issues (client mock-client-id,
secret mock-client-secret):
    mock-code-writer          grants chat:write as well
    mock-code-bot             also returns a bot token with chat:write,commands
    mock-code-rotating        token rotation: xoxe.xoxp-... with a refresh token, expires_in 43200
    mock-code-expiring        the same with expires_in 60, so it needs refreshing at once

Test controls (not Slack):
    POST /slack/_mock/scopes     {"token", "scopes", "after": 0}   the token's scopes become
                                 `scopes` after `after` more Web API calls
    POST /slack/_mock/revoke     {"token"}
    POST /slack/_mock/ratelimit  {"token", "after": 0, "retry_after": 30, "style": "429"|"body"}
                                 search.messages is rate limited after `after` more searches
    GET  /slack/_mock/searches   the queries search.messages received, in order
    GET  /slack/_mock/info       {"instance": $SLACK_MOCK_INSTANCE}, so a test knows the server
                                 is the one it started
"""

import base64
import calendar
import copy
import math
import os
import re
import secrets
import threading
import time
from urllib.parse import urlencode

from mocklib import Response, json_response, not_found, record_violation

CLIENT_ID = "mock-client-id"
CLIENT_SECRET = "mock-client-secret"
APP_ID = "A0OPPORTUN1"
TEAM_ID = "T0FERNHILL"
TEAM_NAME = "Fernhill Analytics"
TEAM_URL = "https://fernhill.slack.com/"
ADA = "U0ADA0001"
READ_SCOPES = "search:read,users:read"
DAY = 86400

# The read methods the connector may call, and the scope each needs (as x-accepted-oauth-scopes).
READ_METHODS = {"auth.test": "", "search.messages": "search:read", "users.info": "users:read"}

# --- the workspace -------------------------------------------------------------------------------

USERS = {
    # id: (handle, real name, display name, title)
    "U0ADA0001": ("ada", "Ada Okafor", "Ada", "Senior Data Engineer"),
    "U0PRI0002": ("priya", "Priya Natarajan", "Priya", "People Partner"),
    "U0MAR0003": ("marco", "Marco Bianchi", "Marco", "APAC Lead"),
    "U0SAM0004": ("sam", "Sam Whitfield", "Sam", "Developer Relations"),
    "U0LIN0005": ("lin", "Lin Zhao", "Lin", "Partnerships Manager"),
    "U0TOM0006": ("tom", "Tom Reyes", "Tom", "Software Engineer"),
    "U0JOR0007": ("jordan", "Jordan Ellis", "Jordan", "Head of Innovation"),
    "W0NOO0008": ("noor", "Noor Haddad", "Noor", "ML Engineering Manager"),
}

CHANNELS = {
    "C0GENERAL1": {"name": "general"},
    "C0ANNOUNC1": {"name": "announcements"},
    "C0ENGINEE1": {"name": "engineering"},
    "C0RANDOM01": {"name": "random"},
    "C0PEOPLE01": {"name": "people-ops"},
    "C0DEVREL01": {"name": "devrel"},
    "C0PARTNER1": {"name": "sales-partnerships"},
    "C0MENTOR01": {"name": "mentoring"},
    "C0ALERTS01": {"name": "alerts"},
    "C0DESIGN01": {"name": "design"},
    "G0LEADERS1": {"name": "leadership", "private": True},
    "D0PRIYA001": {"name": "U0PRI0002", "im": True},
    "G0MPDM0001": {"name": "mpdm-ada--lin--tom-1", "mpim": True},
}

# Messages as Ada can see them. `days` is how long ago; `thread_of` makes a thread reply.
MESSAGES = [
    # Genuine opportunities
    dict(id="hackathon", ch="C0ANNOUNC1", user="U0JOR0007", days=3,
         text=":rocket: *Build Week internal hackathon* is on 4–5 December! Form a team of 2–4, pick a real "
              "customer problem and ship a prototype. It's a friendly competition with prizes for the top "
              "three teams, and the winners demo to the exec team. Sign up by 1 December: "
              "<https://fernhill.example/build-week|sign-up sheet>"),
    dict(id="speakers", ch="C0DEVREL01", user="U0SAM0004", days=5,
         text="Call for speakers :microphone: We have three speaking slots at DataConf Lisbon in March. Submit "
              "a 200-word abstract by 12 December. <@U0ADA0001> your streaming-pipeline work would make a great talk!"),
    dict(id="moderator", ch="C0DEVREL01", user="U0SAM0004", days=4, thread_of="speakers",
         text="Also: the DataConf organisers are looking for a panel moderator. <@U0ADA0001>, interested?"),
    dict(id="role", ch="C0PEOPLE01", user="U0PRI0002", days=8,
         text="We're hiring internally: *Senior Data Engineer, Platform team*. Internal applications are open "
              "until 15 December. Apply through the careers portal, or DM me for an intro to the hiring manager."),
    dict(id="partnership", ch="C0PARTNER1", user="U0LIN0005", days=2,
         text="Partnership ask: Helix Health wants a co-development partner for a demand-forecasting pilot. "
              "Looking for someone with time-series experience to scope it with their team next week. Reply in "
              "thread if you're interested."),
    dict(id="intro", ch="D0PRIYA001", user="U0PRI0002", days=1,
         text="Hi Ada! Could you give me an intro to someone on the Orbit team? I'm looking for a mentor for the "
              "analytics guild and you'd know who fits."),
    dict(id="mentoring", ch="C0MENTOR01", user="U0PRI0002", days=10,
         text="The mentoring programme is open for applications: pair with a senior leader for six months. "
              "Nominate yourself or a colleague by 20 December."),
    dict(id="secondment", ch="C0GENERAL1", user="U0MAR0003", days=6,
         text="Secondment opportunity: three months in our Singapore office to help with the APAC launch. Talk "
              "to me by Friday if you're interested. <!here>"),
    dict(id="grant", ch="C0ENGINEE1", user="U0JOR0007", days=12,
         text="Reminder: the innovation fund has £20k in grant funding for internal R&amp;D proposals. Deadline "
              "is 9 January, and one page is enough."),
    dict(id="award", ch="G0LEADERS1", user="U0MAR0003", days=4,
         text="Nominations for the Fernhill Impact Award close on 5 December. Nominate someone from your team: "
              "<https://fernhill.example/impact-award>"),
    dict(id="collab", ch="G0MPDM0001", user="U0LIN0005", days=2,
         text="<@U0ADA0001> <@U0TOM0006> want to collaborate on the Helix pilot proposal? Kick-off call on Thursday."),
    dict(id="opening", ch="C0ANNOUNC1", user="", username="careers-bot", days=7, text="",
         attachments=[{"fallback": "New internal opening: Product Analyst (Growth). Applications close 18 December.",
                       "title": "New internal opening: Product Analyst (Growth)",
                       "text": "Applications close 18 December. Hiring manager: <@W0NOO0008>."}]),
    dict(id="mentors", ch="C0ENGINEE1", user="W0NOO0008", days=3,
         text="Hackathon mentors wanted: if you've shipped ML to production, volunteer as a mentor for the Build "
              "Week teams."),
    # Everyday noise (some of it shares words with opportunities)
    dict(id="ramen", ch="C0RANDOM01", user="U0TOM0006", days=2,
         text="Anyone up for ramen on Friday? Great opportunity to escape the rain :ramen:"),
    dict(id="rbac", ch="C0ENGINEE1", user="U0TOM0006", days=1,
         text="Deploying the role-based access changes to staging this afternoon."),
    dict(id="plumber", ch="C0RANDOM01", user="U0MAR0003", days=9,
         text="Is anyone hiring a plumber at the moment? Mine just flooded the kitchen."),
    dict(id="etl", ch="C0ALERTS01", user="", username="buildbot", days=1,
         text="Deadline exceeded: job etl-nightly failed after 3 retries"),
    dict(id="closed", ch="C0GENERAL1", user="U0PRI0002", days=3,
         text="Reminder: the office is closed on Monday for the bank holiday."),
    dict(id="flaky", ch="C0ENGINEE1", user="U0TOM0006", days=5,
         text="PR review please: fix the flaky test in the applications service."),
    dict(id="icons", ch="C0DESIGN01", user="U0LIN0005", days=6, text="The new icon set is in Figma."),
    dict(id="mine", ch="C0ENGINEE1", user="U0ADA0001", days=2,
         text="I'm looking for a reviewer for my pipeline PR, any takers?"),
    # Outside a 30-day window
    dict(id="old-hackathon", ch="C0ANNOUNC1", user="U0JOR0007", days=45,
         text="Last year's hackathon recap and photos are up."),
    dict(id="old-speakers", ch="C0DEVREL01", user="U0SAM0004", days=60,
         text="Call for speakers for the spring meetup."),
]

# Fixed when the server starts, so permalinks stay the same across calls.
_BASE = time.time()


def _ts_of(index, m):
    seconds = _BASE - m["days"] * DAY - 3600 - index * 37
    return "%d.%06d" % (int(seconds), 100 + index)


TS = {m["id"]: _ts_of(i, m) for i, m in enumerate(MESSAGES)}

# --- state ---------------------------------------------------------------------------------------

LOCK = threading.RLock()
STATE = {}

STATIC_TOKENS = {
    "xoxp-mock-writer": {"user": ADA, "scopes": READ_SCOPES + ",chat:write"},
    "xoxp-mock-legacy": {"user": ADA, "scopes": "identify,read,post,client"},
    "xoxp-mock-revoked": {"user": ADA, "scopes": READ_SCOPES, "revoked": True},
    "xoxp-mock-noheader": {"user": ADA, "scopes": READ_SCOPES, "noheader": True},
    "xoxp-mock-ratelimited": {"user": ADA, "scopes": READ_SCOPES,
                              "ratelimit": {"after": 0, "retry_after": 30, "style": "429"}},
    "xoxp-mock-nosearch": {"user": ADA, "scopes": "users:read"},
    "xoxb-mock-bot": {"user": "U0BOT0009", "scopes": "chat:write,channels:read", "bot": True},
}


def reset():
    with LOCK:
        STATE.clear()
        STATE.update(codes={}, tokens={}, refresh={}, searches=[])


reset()


def _lookup(token):
    """The token's record, created on first use for the pasteable tokens; None if unknown."""
    with LOCK:
        rec = STATE["tokens"].get(token)
        if rec is None:
            base = STATIC_TOKENS.get(token)
            if base is None and (token == "xoxp-mock-ada" or token.startswith("xoxp-mock-ada-")):
                base = {"user": ADA, "scopes": READ_SCOPES}
            if base is None:
                return None
            rec = copy.deepcopy(base)
            STATE["tokens"][token] = rec
        return rec


def _split(scopes):
    return [s for s in re.split(r"[,\s]+", scopes or "") if s]


# --- responses -----------------------------------------------------------------------------------


def _api(rec, body, method="", status=200, extra=None):
    """A Web API reply with Slack's scope headers."""
    headers = dict(extra or {})
    if not (rec and rec.get("noheader")):
        known = rec is not None and not rec.get("revoked")
        headers["x-oauth-scopes"] = ", ".join(_split(rec["scopes"])) if known else ""
        headers["x-accepted-oauth-scopes"] = READ_METHODS.get(method, "")
    return json_response(body, status=status, headers=headers)


def _error(code, **more):
    return json_response(dict({"ok": False, "error": code}, **more))


def _page(status, text):
    return Response(status=status, body=text.encode("utf-8"), content_type="text/plain; charset=utf-8")


# --- routing -------------------------------------------------------------------------------------


def handle(req):
    path = req.path
    if path == "/slack/oauth/v2/authorize":
        return _authorize(req)
    if path.startswith("/slack/api/"):
        return _web_api(req, path[len("/slack/api/"):])
    if path.startswith("/slack/_mock/"):
        return _control(req, path[len("/slack/_mock/"):])
    return not_found(path)


def _web_api(req, method):
    if method == "oauth.v2.access":
        if req.method != "POST":
            return _page(405, "oauth.v2.access takes POST")
        return _oauth_access(req)
    if method not in READ_METHODS or req.method != "GET":
        record_violation("slack", req)
        return json_response({"ok": False, "error": "mock_read_only",
                              "detail": "%s %s is not a read the connector may make" % (req.method, method)},
                             status=405)
    if "token" in req.query:
        return _api(None, {"ok": False, "error": "invalid_auth"}, method)
    auth = req.headers.get("authorization", "")
    if not auth.startswith("Bearer "):
        return _api(None, {"ok": False, "error": "not_authed"}, method)
    rec = _lookup(auth[len("Bearer "):].strip())
    if rec is None:
        return _api(None, {"ok": False, "error": "invalid_auth"}, method)
    with LOCK:
        pending = rec.get("pending")
        if pending:
            if pending["after"] <= 0:
                rec["scopes"] = pending["scopes"]
                del rec["pending"]
            else:
                pending["after"] -= 1
    if rec.get("revoked"):
        return _api(rec, {"ok": False, "error": "token_revoked"}, method)
    if rec.get("expires_at") and time.time() >= rec["expires_at"]:
        return _api(None, {"ok": False, "error": "token_expired"}, method)
    needed = READ_METHODS[method]
    if needed and needed not in _split(rec["scopes"]):
        return _api(rec, {"ok": False, "error": "missing_scope", "needed": needed, "provided": rec["scopes"]}, method)
    if method == "auth.test":
        return _api(rec, _auth_test(rec), method)
    if method == "users.info":
        return _api(rec, _users_info(req.query), method)
    with LOCK:
        STATE["searches"].append(req.query.get("query", ""))
        limit = rec.get("ratelimit")
        limited = limit is not None and limit["after"] <= 0
        if limit is not None and not limited:
            limit["after"] -= 1
    if limited:
        if limit.get("style") == "body":
            return _api(rec, {"ok": False, "error": "ratelimited"}, method)
        return _api(rec, {"ok": False, "error": "ratelimited"}, method, status=429,
                    extra={"Retry-After": str(limit.get("retry_after", 30))})
    return _api(rec, _search(req.query), method)


# --- OAuth ---------------------------------------------------------------------------------------


def _authorize(req):
    if req.method != "GET":
        return _page(405, "GET only")
    q = req.query
    if q.get("client_id") != CLIENT_ID:
        return _page(400, "invalid_client_id")
    if q.get("scope"):
        return _page(400, "mock: bot scopes were requested; the connector must ask for user scopes only")
    redirect = q.get("redirect_uri", "")
    if not redirect.startswith(("http://", "https://")):
        return _page(400, "bad_redirect_uri")
    state = q.get("state", "")
    if q.get("mock_deny"):
        params = {"error": "access_denied", "state": state}
    else:
        code = "mock-code-" + secrets.token_hex(8)
        with LOCK:
            STATE["codes"][code] = {"redirect_uri": redirect, "scopes": q.get("user_scope", ""),
                                    "created": time.time(), "used": False}
        params = {"code": code, "state": state}
    sep = "&" if "?" in redirect else "?"
    return Response(status=302, body=b"", content_type="text/plain",
                    headers={"Location": redirect + sep + urlencode(params)})


def _client(req, form):
    auth = req.headers.get("authorization", "")
    if auth.startswith("Basic "):
        try:
            user, _, secret = base64.b64decode(auth[len("Basic "):]).decode("utf-8").partition(":")
            return user, secret
        except (ValueError, UnicodeDecodeError):
            return "", ""
    return form.get("client_id", ""), form.get("client_secret", "")


def _oauth_access(req):
    form = {k: v[0] for k, v in req.form().items()}
    client_id, secret = _client(req, form)
    if client_id != CLIENT_ID:
        return _error("invalid_client_id")
    if secret != CLIENT_SECRET:
        return _error("bad_client_secret")
    if form.get("grant_type") == "refresh_token":
        return _refresh(form.get("refresh_token", ""))
    if form.get("grant_type") not in (None, "authorization_code"):
        return _error("invalid_grant_type")
    if form.get("code_verifier"):
        return _error("pkce_not_allowed")  # this app hasn't opted into PKCE
    code = form.get("code", "")
    if code == "mock-code-writer":
        return _issue(READ_SCOPES + ",chat:write")
    if code == "mock-code-bot":
        return _issue(READ_SCOPES, bot_scopes="chat:write,commands")
    if code == "mock-code-rotating":
        return _issue(READ_SCOPES, rotating=True)
    if code == "mock-code-expiring":
        return _issue(READ_SCOPES, rotating=True, expires_in=60)
    with LOCK:
        rec = STATE["codes"].get(code)
        if not rec or rec["used"] or time.time() - rec["created"] > 600:
            return _error("invalid_code")
        if form.get("redirect_uri", "") != rec["redirect_uri"]:
            return _error("bad_redirect_uri")
        rec["used"] = True
    return _issue(rec["scopes"])


def _issue(scopes, rotating=False, expires_in=43200, bot_scopes=None):
    authed = {"id": ADA, "scope": scopes, "token_type": "user"}
    with LOCK:
        if rotating:
            token = "xoxe.xoxp-1-mock-" + secrets.token_hex(12)
            refresh = "xoxe-1-mock-" + secrets.token_hex(12)
            STATE["tokens"][token] = {"user": ADA, "scopes": scopes, "expires_at": time.time() + expires_in}
            STATE["refresh"][refresh] = {"scopes": scopes, "used": False}
            authed.update(access_token=token, expires_in=expires_in, refresh_token=refresh)
        else:
            token = "xoxp-mock-oauth-" + secrets.token_hex(12)
            STATE["tokens"][token] = {"user": ADA, "scopes": scopes}
            authed["access_token"] = token
    body = {"ok": True, "app_id": APP_ID, "authed_user": authed, "team": {"id": TEAM_ID, "name": TEAM_NAME},
            "enterprise": None, "is_enterprise_install": False}
    if bot_scopes:
        body.update(access_token="xoxb-mock-" + secrets.token_hex(12), token_type="bot", scope=bot_scopes,
                    bot_user_id="U0BOT0009")
    return json_response(body)


def _refresh(refresh_token):
    with LOCK:
        rec = STATE["refresh"].get(refresh_token)
        if not rec or rec["used"]:
            return _error("invalid_refresh_token")
        rec["used"] = True  # single use
        token = "xoxe.xoxp-1-mock-" + secrets.token_hex(12)
        new_refresh = "xoxe-1-mock-" + secrets.token_hex(12)
        STATE["tokens"][token] = {"user": ADA, "scopes": rec["scopes"], "expires_at": time.time() + 43200}
        STATE["refresh"][new_refresh] = {"scopes": rec["scopes"], "used": False}
    return json_response({"ok": True, "app_id": APP_ID, "access_token": token, "token_type": "user",
                          "scope": rec["scopes"], "expires_in": 43200, "refresh_token": new_refresh,
                          "team": {"id": TEAM_ID, "name": TEAM_NAME}, "enterprise": None,
                          "is_enterprise_install": False})


# --- read methods --------------------------------------------------------------------------------


def _auth_test(rec):
    body = {"ok": True, "url": TEAM_URL, "team": TEAM_NAME, "team_id": TEAM_ID}
    if rec.get("bot"):
        body.update(user="bot", user_id=rec["user"], bot_id="B0BOT0009")
    else:
        body.update(user=USERS[rec["user"]][0], user_id=rec["user"])
    return body


def _users_info(q):
    uid = q.get("user", "")
    if uid not in USERS:
        return {"ok": False, "error": "user_not_found"}
    handle, real, display, title = USERS[uid]
    return {"ok": True, "user": {
        "id": uid, "team_id": TEAM_ID, "name": handle, "deleted": False, "real_name": real,
        "tz": "Europe/London", "is_bot": False, "is_admin": False,
        "profile": {"real_name": real, "display_name": display, "real_name_normalized": real,
                    "display_name_normalized": display, "title": title}}}


def _day_start(ymd):
    """Midnight UTC of a YYYY-MM-DD, in Unix seconds, or None."""
    try:
        return calendar.timegm(time.strptime(ymd, "%Y-%m-%d"))
    except ValueError:
        return None


TOKEN_RE = re.compile(r'-?"[^"]*"|\S+')
MENTION_RE = re.compile(r"^<@([UW][A-Z0-9]+)(?:\|[^>]*)?>$")


def _parse(query):
    """Slack query syntax → (groups of alternatives, exclusions, filters)."""
    groups, excludes, filters = [], [], []
    join_next = False
    for tok in TOKEN_RE.findall(query):
        if tok == "OR":
            join_next = True
            continue
        neg = tok.startswith("-") and len(tok) > 1
        body = tok[1:] if neg else tok
        key, sep, value = body.partition(":")
        if sep and not body.startswith(('"', "<")) and key.lower() in (
                "after", "before", "on", "during", "in", "from", "has", "is", "with", "to", "creator"):
            filters.append((key.lower(), value, neg))
            join_next = False
            continue
        mention = MENTION_RE.match(body)
        if mention:
            atom = ("mention", mention.group(1))
        elif body.startswith('"') and body.endswith('"') and len(body) >= 2:
            atom = ("phrase", body[1:-1].lower())
        else:
            atom = ("word", body.lower())
        if neg:
            excludes.append(atom)
        elif join_next and groups:
            groups[-1].append(atom)
        else:
            groups.append([atom])
        join_next = False
    return groups, excludes, filters


def _searchable(m):
    parts = [m.get("text", "")]
    for a in m.get("attachments", []):
        parts += [a.get("title", ""), a.get("text", ""), a.get("fallback", "")]
    return " ".join(parts)


def _hits(atom, raw, low):
    kind, value = atom
    if kind == "mention":
        return raw.count("<@%s>" % value) + raw.count("<@%s|" % value)
    stem = value[:-1] if kind == "word" and value.endswith("y") and len(value) > 4 else value
    if not stem:
        return 0
    return len(re.findall(r"(?<![a-z0-9])" + re.escape(stem), low))


def _passes(filters, m, ts):
    for key, value, neg in filters:
        ok = True
        if key in ("after", "before", "on"):
            start = _day_start(value)
            if start is None:
                continue
            ok = {"after": ts >= start + DAY, "before": ts < start, "on": start <= ts < start + DAY}[key]
        elif key == "in":
            ch = CHANNELS[m["ch"]]
            v = value.lstrip("#")
            mention = MENTION_RE.match(value)
            ok = v in (ch["name"], m["ch"]) or bool(mention and ch.get("im") and ch["name"] == mention.group(1))
        elif key == "from":
            mention = MENTION_RE.match(value)
            who = mention.group(1) if mention else value.lstrip("@")
            ok = who in (m.get("user"), USERS.get(m.get("user"), ("",))[0], m.get("username"))
        else:
            continue  # has:, is:, with:... aren't modelled
        if ok == neg:
            return False
    return True


def _score(parsed, m, ts):
    groups, excludes, filters = parsed
    if not _passes(filters, m, ts):
        return None
    raw = _searchable(m)
    low = raw.lower()
    if any(_hits(a, raw, low) for a in excludes):
        return None
    total = 0
    for alternatives in groups:
        best = max(_hits(a, raw, low) for a in alternatives)
        if best == 0:
            return None
        total += best
    return total


def _match_json(m):
    ch_id = m["ch"]
    ch = CHANNELS[ch_id]
    ts = TS[m["id"]]
    kind = "im" if ch.get("im") else "mpim" if ch.get("mpim") else "group" if ch.get("private") else "message"
    permalink = "%sarchives/%s/p%s" % (TEAM_URL, ch_id, ts.replace(".", ""))
    if m.get("thread_of"):
        permalink += "?thread_ts=%s&cid=%s" % (TS[m["thread_of"]], ch_id)
    user = m.get("user", "")
    out = {
        "iid": "iid-" + m["id"],
        "team": TEAM_ID,
        "channel": {"id": ch_id, "name": ch["name"], "is_channel": kind == "message", "is_group": kind == "group",
                    "is_im": kind == "im", "is_mpim": kind == "mpim",
                    "is_private": bool(ch.get("private") or ch.get("im") or ch.get("mpim")),
                    "is_shared": False, "is_org_shared": False, "is_ext_shared": False,
                    "is_pending_ext_shared": False, "pending_shared": []},
        "type": kind,
        "user": user,
        "username": USERS[user][0] if user in USERS else m.get("username", ""),
        "ts": ts,
        "text": m.get("text", ""),
        "permalink": permalink,
    }
    if m.get("attachments"):
        out["attachments"] = m["attachments"]
    return out


def _search(q):
    query = (q.get("query") or "").strip()
    if not query:
        return {"ok": False, "error": "no_query"}
    try:
        count = min(max(int(q.get("count", 20)), 1), 100)
        page = min(max(int(q.get("page", 1)), 1), 100)
    except ValueError:
        return {"ok": False, "error": "invalid_arguments"}
    parsed = _parse(query)
    hits = []
    for m in MESSAGES:
        ts = float(TS[m["id"]])
        score = _score(parsed, m, ts)
        if score is not None:
            hits.append((score, ts, m))
    if q.get("sort", "score") == "timestamp":
        hits.sort(key=lambda h: h[1], reverse=q.get("sort_dir", "desc") != "asc")
    else:
        hits.sort(key=lambda h: (h[0], h[1]), reverse=True)
    total = len(hits)
    pages = max(1, math.ceil(total / count))
    chosen = hits[(page - 1) * count: page * count]
    first = (page - 1) * count + 1 if chosen else 0
    return {"ok": True, "query": query, "messages": {
        "total": total,
        "pagination": {"total_count": total, "page": page, "per_page": count, "page_count": pages,
                       "first": first, "last": first + len(chosen) - 1 if chosen else 0},
        "paging": {"count": count, "total": total, "page": page, "pages": pages},
        "matches": [_match_json(m) for _, _, m in chosen],
    }}


# --- test controls -------------------------------------------------------------------------------


def _control(req, what):
    if req.method == "GET" and what == "info":
        return json_response({"ok": True, "mock": "slack", "instance": os.environ.get("SLACK_MOCK_INSTANCE", "")})
    if req.method == "GET" and what == "searches":
        with LOCK:
            return json_response(list(STATE["searches"]))
    if req.method != "POST":
        return _page(405, "POST only")
    spec = req.json() or {}
    rec = _lookup(spec.get("token", ""))
    if rec is None:
        return json_response({"ok": False, "error": "unknown token"}, status=404)
    with LOCK:
        if what == "scopes":
            rec["pending"] = {"scopes": spec.get("scopes", ""), "after": int(spec.get("after", 0))}
        elif what == "revoke":
            rec["revoked"] = True
        elif what == "ratelimit":
            rec["ratelimit"] = {"after": int(spec.get("after", 0)), "retry_after": int(spec.get("retry_after", 30)),
                                "style": spec.get("style", "429")}
        else:
            return not_found(req.path)
    return json_response({"ok": True})
