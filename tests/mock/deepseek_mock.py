"""Mock of DeepSeek's v1 API: chat completions, Search API, Fetch API, balance.

Chat replies are chosen by the `X-Opp-Stage` header the client sends with every request, so each
pipeline step gets a reply of the right shape. Owner: the engine builder, who extends STAGES as
the engine's prompts and JSON shapes settle. Keep replies deterministic (derived from the request)
so tests are repeatable.

The engine's stages and what the mock answers (each parses the real prompt the engine sends):

    plan      {"queries": [...]}: one query per wanted kind, from the profile's interests and
              place; the first steering note becomes a query; a query naming the person's own
              organisation is included on purpose (the engine must filter it out).
    evaluate  {"picks": [...], "stale": [...]}: picks candidates by their #number (every number
              divisible by 3 is passed over), scores spread downwards from 92; plus entries the
              engine must ignore (out-of-range and repeated numbers, an expired deadline). Known
              [active] items whose title contains "stale" are reported stale.
    verify    {"verdict": ...}: "closed" when the page text says closed, else "open" with the
              page's "until YYYY-MM-DD" date.
    research  {"matched": [...], "confidence": ..., "profile": {...}}: results whose title
              contains the person's name are theirs; with none, a hints-only draft.
    theme     {"label", "accent", "image_queries", "mood"} from the first interest.

Search: 10 results per query (fewer if `count` is lower) at example-xxxx.org URLs; none for a
query containing "nobody". Fetch: a page open until 2027-01-31, except URLs ending in 0-7, whose
round has closed (see `is_closed_url`), and URLs containing "unavailable" (404).
"""

import hashlib
import json
import re
import threading

from mocklib import error_response, json_response, not_found

LOCK = threading.Lock()
STATE = {"chat_calls": 0, "searches": 0, "fetches": 0}


def reset():
    with LOCK:
        for k in STATE:
            STATE[k] = 0


def _h(text, n=8):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:n]


def _chat_reply(content, prompt_tokens=500, completion_tokens=200):
    return json_response({
        "id": "mock-" + _h(content),
        "object": "chat.completion",
        "model": "deepseek-flash",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens, "prompt_cache_hit_tokens": 0, "prompt_cache_miss_tokens": prompt_tokens},
    })


# ---------------------------------------------------------------------------------------------
# Reading the engine's prompts
# ---------------------------------------------------------------------------------------------

KINDS = {
    "sales": "tender",
    "partners": "partner programme",
    "funding": "grant incentive",
    "regulation": "new regulation",
    "market": "market growth",
    "visibility": "trade show",
    "talent": "learnership",
    "operations": "supplier programme",
    "other": "opportunities",
}


def user_text(body):
    """The user message of a chat request (text parts joined)."""
    for m in body.get("messages") or []:
        if m.get("role") != "user":
            continue
        c = m.get("content")
        if isinstance(c, str):
            return c
        if isinstance(c, list):
            return "\n".join(p.get("text", "") for p in c if isinstance(p, dict))
    return ""


def field(text, label):
    """The value of a "Label: value" line, or ""."""
    m = re.search(r"^" + re.escape(label) + r": (.+)$", text, re.M)
    return m.group(1).strip() if m else ""


def is_closed_url(url):
    """The mock's rule for pages whose round has closed: the URL ends in a digit from 0 to 7
    (about half of the hex-keyed result URLs)."""
    return url.rstrip("/")[-1:] in "01234567"


# ---------------------------------------------------------------------------------------------
# Stages
# ---------------------------------------------------------------------------------------------


def stage_plan(body):
    text = user_text(body)
    m = re.search(r"Write exactly (\d+) queries", text)
    n = int(m.group(1)) if m else 8
    after = re.search(r"after:(\d{4}-\d{2}-\d{2})", text)
    kinds = [k for k in re.findall(r"\b([a-z]+) \(", field(text, "Focus areas")) if k in KINDS] or ["sales", "funding", "visibility"]
    interests = [s.strip() for s in (field(text, "Products and services") or field(text, "Industry")).split(",") if s.strip()] or ["opportunities"]
    location = field(text, "Headquarters")
    city = location.split(",")[0].strip() if location and location != "not given" else ""
    notes = re.findall(r"^- (.+)$", text.split("steering notes", 1)[1], re.M) if "steering notes" in text else []
    queries = []
    if notes:
        queries.append({"q": f"{notes[0]} 2027", "category": kinds[0], "about": notes[0]})
    org = field(text, "Business")
    if org:
        queries.append({"q": f'"{org}" reviews', "category": "other", "about": "their own organisation"})
    i = 0
    while len(queries) < n and i < n * 4:
        kind = kinds[i % len(kinds)]
        topic = interests[(i // len(kinds)) % len(interests)]
        place = city if i % 2 == 0 and city else "international"
        q = f"{topic} {KINDS[kind]} {place} 2027"
        if after and i % 3 == 0:
            q += f" after:{after.group(1)}"
        if all(x["q"] != q for x in queries):
            queries.append({"q": q, "category": kind, "about": f"{topic} {KINDS[kind]} in {place}"})
        i += 1
    return {"queries": queries[:n]}


CANDIDATE = re.compile(r"^#(\d+) \[([a-z]+)\] (.+?) \| ", re.M)
KNOWN = re.compile(r"^K(\d+) \[(active|saved)\] (.+)$", re.M)


def stage_plan_more(body):
    """The second wave: the plan's queries made broader (prefixed "wider", no operators)."""
    out = stage_plan(body)
    return {"queries": [{"q": "wider " + q["q"].split(" after:")[0], "category": q["category"], "about": "wider " + q["about"]} for q in out["queries"]]}


def stage_evaluate(body):
    text = user_text(body)
    m = re.search(r"Pick at most (\d+) opportunities", text)
    max_picks = int(m.group(1)) if m else 10
    candidates = [(int(n), kind, title) for n, kind, title in CANDIDATE.findall(text)]
    # A business named "Thin ..." gets a thin first pass: two picks, unless the leads come from
    # the broader second wave (their titles carry "Wider").
    thin = "thin" in field(text, "Business").lower()
    picks = []
    for n, kind, title in candidates:
        if n % 3 == 0:
            continue
        if thin and "wider" not in title.lower() and len(picks) >= 2:
            break
        picks.append({
            "n": n,
            "title": f"{title} (mock pick)",
            "organization": "Mock Foundation",
            "category": kind if kind in KINDS else "other",
            "summary": f"A mock opportunity built from candidate {n}: applications are open to people like you.",
            "why": "Your profile matches exactly what this mock programme is looking for.",
            "action": "Apply online by 31 January 2027",
            "location": "Remote",
            "deadline": "2027-01-31",
            "value": "$5,000",
            "open": "yes" if n % 2 else "likely",
            "score": max(45, 92 - 3 * len(picks)),
        })
        if len(picks) >= max_picks:
            break
    # Entries a careful engine must ignore.
    passed_over = [n for n, _, _ in candidates if n % 3 == 0]
    if picks:
        picks.append({"n": picks[0]["n"], "title": "Duplicate number (must be ignored)", "score": 99})
    picks.append({"n": 9999, "title": "Out of range (must be ignored)", "score": 99})
    if passed_over:
        picks.append({"n": passed_over[0], "title": "Expired (must be dropped)", "deadline": "2025-01-31", "category": "funding", "score": 97})
    stale = [f"K{k}" for k, status, title in KNOWN.findall(text) if status == "active" and "stale" in title.lower()]
    return {"picks": picks, "stale": stale}


def stage_verify(body):
    text = user_text(body)
    parts = text.split('"""')
    page = parts[1] if len(parts) >= 3 else ""
    if "closed" in page.lower():
        return {"evidence": "The page says this round has closed.", "verdict": "closed", "deadline": "", "value": "", "location": "", "title": "", "summary": "", "action": ""}
    m = re.search(r"until (\d{4}-\d{2}-\d{2})", page)
    return {
        "evidence": "Applications are open until " + (m.group(1) if m else "a date to be announced"),
        "verdict": "open" if m else "unclear",
        "deadline": m.group(1) if m else "",
        "value": "$5,000 prize" if "prize" in page.lower() else "",
        "location": "",
        "title": "",
        "summary": "",
        "action": "Apply on the official page by 31 January 2027" if m else "",
    }


RESULT = re.compile(r"^#(\d+) (.+?) \| .*\n(https?://\S+)", re.M)


def stage_research(body):
    text = user_text(body)
    name = field(text, "Name")
    hints = field(text, "Their hints").strip('"')
    results = RESULT.findall(text)
    linked = re.findall(r"^\[L\d+\] (\S+)", text, re.M)
    matched = [(int(n), url) for n, title, url in results if name and name.lower() in title.lower()][:5]
    if matched or linked:
        links = [url for _, url in matched[:1]] + linked[:1]
        return {
            "matched": [n for n, _ in matched],
            "confidence": 0.85 if len(matched) >= 2 else 0.6,
            "summary": f"We found you on {len(matched) + len(linked)} pages, including your work at Mock Org in Cape Town.",
            "profile": {
                "headline": "Product lead at Mock Org, building tools for restaurants",
                "stage": "growth",
                "organization": "Mock Org",
                "location": "Cape Town, South Africa",
                "reach": "global",
                "seeking": ["sales", "partners", "visibility", "bogus-kind"],
                "interests": ["restaurant technology", "voice interfaces", "religious studies"],
                "skills": ["product management", "go-to-market"],
                "goals": "Grow Mock Org's customer base across Africa",
                "languages": ["English"],
                "links": links + ["https://invented.example/not-a-result"],
            },
        }
    return {
        "matched": [],
        "confidence": 0.4,
        "summary": "We found lots about you!",
        "profile": {"headline": hints if hints and hints != "none" else "", "stage": "", "location": "", "seeking": [], "interests": [], "skills": []},
    }


def stage_theme(body):
    text = user_text(body)
    first = field(text, "Industry") or "business"
    return {
        "label": first.title(),
        "accent": "#c2410c",
        "image_queries": [f"{first} landscape at golden hour", f"{first} atmospheric morning light"],
        "mood": "Warm, hopeful light; calm and spacious",
    }


def _sources(text):
    """[(number, own, title, url)] from the dossier prompt's numbered sources."""
    out = []
    for m in re.finditer(r"^\[S(\d+)\]( \(its own website\))? (.+)\n(https?://\S+)", text, re.M):
        out.append((int(m.group(1)), bool(m.group(2)), m.group(3), m.group(4)))
    return out


def stage_business_profile(body):
    text = user_text(body)
    name = field(text, "Business") or "The business"
    return {
        "profile": {
            "headline": f"{name} provides commercial cleaning for offices and schools",
            "industry": "Commercial cleaning",
            "stage": "established",
            "location": "Cape Town, South Africa",
            "reach": "national",
            "seeking": ["sales", "funding", "regulation", "bogus"],
            "interests": ["office parks", "schools"],
            "skills": ["office cleaning", "deep cleaning", "hygiene services"],
            "goals": "",
            "languages": ["English"],
            "links": ["https://linkedin.com/company/mock-business", "https://invented.example/not-on-the-site"],
        },
        "country": "South Africa",
        "searches": {
            "competitors": ["commercial cleaning companies Cape Town"],
            "market": ["South Africa facilities management market trends 2026"],
            "regulation": ["South Africa cleaning sector minimum wage 2027"],
        },
    }


def stage_business_intel(body):
    text = user_text(body)
    name = field(text, "Business") or "The business"
    sources = _sources(text)
    own = [n for n, is_own, _, _ in sources if is_own] or [1]
    searched = [(n, title, url) for n, is_own, title, url in sources if not is_own]
    competitors = []
    for n, title, url in searched:
        t = title.lower()
        if "competitors" in t or "cleaning companies" in t:
            host = url.split("//")[1].split(".")[0].split("-")[-1]
            competitors.append({"name": f"{host.upper()} Services", "website": "", "summary": "Commercial cleaning provider.", "difference": "Larger, national.", "overlap": "high", "source": f"S{n}"})
        if len(competitors) >= 5:
            break
    competitors.append({"name": "Imaginary Rival", "summary": "Not in any source.", "source": f"S{own[0]}"})
    trend = next((n for n, t, _ in searched if "market" in t.lower()), None)
    rule = next((n for n, t, _ in searched if "wage" in t.lower()), None)
    news = next((n for n, t, _ in searched if name.lower() in t.lower()), None)
    return {
        "overview": {"summary": f"{name} cleans offices and schools in Cape Town.", "industry": "Commercial cleaning", "headquarters": "Cape Town, South Africa", "founded": "2009", "size": "51-200 employees", "business_model": "Service contracts", "markets": ["South Africa"], "sources": [f"S{own[0]}", "S999"]},
        "offerings": [
            {"name": "Office cleaning", "description": "Daily and weekly office cleaning.", "audience": "Office parks", "pricing": "", "source": f"S{own[0]}"},
            {"name": "Invented offering", "description": "No such source.", "source": "S999"},
        ],
        "segments": [{"name": "Office parks", "description": "Corporate campuses and office parks.", "examples": ["Made Up Holdings"], "source": f"S{own[0]}"}],
        "competitors": competitors,
        "trends": [{"title": "Hybrid work changes cleaning schedules", "summary": "Fewer daily cleans, more deep cleans.", "impact": "mixed", "source": f"S{trend}"}] if trend else [],
        "regulation": [{"title": "Cleaning sector minimum wage increase", "summary": "Wages rise in March.", "jurisdiction": "South Africa", "status": "passed", "effective": "2027-03-01", "impact": "risk", "source": f"S{rule}"}] if rule else [],
        "news": [{"title": f"{name} in the news", "summary": "A recent mention.", "source": f"S{news}"}] if news else [],
        "confidence": 0.8,
        "summary": f"We read {name}'s website and the web to build its profile.",
    }


def stage_context(body):
    text = user_text(body)
    picks = []
    blocks = re.split(r"^(?=\[C\d+\] )", text.split("Items from its tools:", 1)[-1], flags=re.M)
    for block in blocks:
        m = re.match(r"\[C(\d+)\] ", block)
        if not m:
            continue
        n = int(m.group(1))
        t = re.search(r"^Title: (.+)$", block, re.M)
        title = t.group(1) if t else "request"
        if "quote" in block.lower() or "rfp" in block.lower():
            picks.append({"item": f"C{n}", "title": f"Internal: {title}", "category": "sales", "summary": "A request came in through the team's tools.", "why": "It is a direct buying signal.", "action": "Reply with a quote this week", "deadline": "2027-01-15", "value": "", "score": 88})
    picks.append({"item": "C999", "title": "Out of range (must be ignored)", "score": 99})
    return {"opportunities": picks}


# stage name -> function(request_json) -> dict (the JSON object the model would return)
STAGES = {
    "plan": stage_plan,
    "plan_more": stage_plan_more,
    "evaluate": stage_evaluate,
    "verify": stage_verify,
    "research": stage_research,
    "theme": stage_theme,
    "business_profile": stage_business_profile,
    "business_intel": stage_business_intel,
    "context": stage_context,
}


def _stage_fn(stage):
    """The handler for a stage: ours first, then ones other mock modules contribute (the media
    mock's CHAT_STAGES, e.g. "curate" for the vision pick), looked up at call time."""
    if stage in STAGES:
        return STAGES[stage]
    try:
        import media_mock

        return getattr(media_mock, "CHAT_STAGES", {}).get(stage)
    except ImportError:
        return None


def chat(req):
    body = req.json() or {}
    if not body.get("messages"):
        return error_response(400, "messages is required")
    stage = req.headers.get("x-opp-stage", "")
    with LOCK:
        STATE["chat_calls"] += 1
    fn = _stage_fn(stage)
    obj = fn(body) if fn else {}
    return _chat_reply(json.dumps(obj))


def search(req):
    body = req.json() or {}
    query = (body.get("query") or "").strip()
    if not query or len(query) > 128:
        return error_response(400, "'query' must contain 1 to 128 characters")
    count = int(body.get("count") or 10)
    if not 1 <= count <= 50:
        return error_response(400, "'count' must be between 1 and 50")
    with LOCK:
        STATE["searches"] += 1
    if "nobody" in query.lower():
        return json_response({"results": []})
    results = []
    for i in range(min(count, 10)):
        key = _h(f"{query}|{i}")
        results.append({
            "rerank_score": round(2.6 - i * 0.2, 2),
            "title": f"{query.title()} — result {i + 1}",
            "url": f"https://example-{key[:4]}.org/opportunities/{key}",
            "snippet": f"Applications are open for {query}. Closing date in a few weeks.",
            "summary": f"This page describes an opportunity matching '{query}'. Applications are open now and close on 2027-01-31. Eligible applicants anywhere may apply online.",
            "content": "",
            "site_name": f"Example {key[:4].upper()}",
            "site_icon": None,
            "date": "2026-11-01T00:00:00+00:00",
            "authority": 2,
        })
    return json_response({"results": results})


def fetch(req):
    body = req.json() or {}
    url = body.get("url") or ""
    if not url.startswith(("http://", "https://")):
        return error_response(400, "'url' must be an HTTP or HTTPS URL")
    if "unavailable" in url:
        return error_response(404, "could not retrieve usable page content")
    with LOCK:
        STATE["fetches"] += 1
    filler = "This programme supports people and organisations with practical help, mentoring and a cash prize. " * 4
    rest = url.split("//", 1)[-1]
    path = rest.split("/", 1)[1] if "/" in rest else ""
    if url.startswith("https://www.") and path.strip("/") in ("", "about", "services", "case-studies"):
        site = rest.split("/")[0]
        content = f"# {site}\n\nWe provide commercial cleaning for offices and schools.\n\n[About us](/about) [Services](https://{site}/services) [Case studies](/case-studies) [LinkedIn](https://linkedin.com/company/mock-business) [Contact](mailto:hi@example.com)\n\n{filler}"
        with LOCK:
            STATE["fetches"] += 1
        return json_response({"title": f"{site} - home", "content": content})
    if is_closed_url(url):
        content = f"# Opportunity\n\nApplications for this round are now closed. Winners will be announced soon.\n\n{filler}\n\nSource: {url}\n"
    else:
        content = f"# Opportunity\n\nApplications are open until 2027-01-31.\n\n{filler}\n\nSource: {url}\n"
    return json_response({"title": "Opportunity page", "content": content})


def balance(req):
    return json_response({"is_available": True, "balance_infos": [{"currency": "USD", "total_balance": "9.99", "granted_balance": "0.00", "topped_up_balance": "9.99"}]})


def handle(req):
    if req.headers.get("authorization", "") in ("", "Bearer ", "Bearer bad-key"):
        return error_response(401, "Authentication Fails, Your api key is invalid", "authentication_error")
    if req.path in ("/v1/chat/completions", "/chat/completions"):
        return chat(req)
    if req.path == "/v1/tools/search":
        return search(req)
    if req.path == "/v1/tools/fetch":
        return fetch(req)
    if req.path == "/user/balance":
        return balance(req)
    return not_found(req.path)
