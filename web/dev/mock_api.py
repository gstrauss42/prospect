#!/usr/bin/env python3
"""Prospect: a development mock of the HTTP API in docs/API.md, with in-memory state.

It serves web/ the way the real server does (SPA fallback, ETag + no-cache, the strict CSP from
docs/ARCHITECTURE.md) and fakes background runs on a clock:

* research (auto-onboarding) finishes in ~8 s with a drafted profile;
* discovery advances over ~15 s with realistic progress messages, then yields ~14 opportunities;
* the theme photo is ready ~5 s into the first discovery.

Stdlib only. Development and tests only; never deployed.

    python3 web/dev/mock_api.py --port 9940            # realistic timings
    python3 web/dev/mock_api.py --port 9941 --speed 5  # 5x faster, for the e2e suite

Dev-only controls live under /__mock/ (they are not part of the real API):

    GET  /__mock/ping                        -> {"mock": true}
    POST /__mock/next   {"discover": "fail"|"empty"|"ok", "research": "fail"|"ok"}
                                             -> the next run of that kind ends that way
    POST /__mock/seed   {"state": ..., "email", "password", "business_name", "website", "connections"}
                                             -> creates a business account in a given state and signs in
         states: researching | review | review_failed | loading | dashboard | refreshing | empty | failed
    POST /__mock/next   {"microsoft_oauth": false}  -> Microsoft 365 shows as not enabled by the admin
    GET  /__mock/reset-link?email=...        -> {"url": "/reset?token=..."} (the last emailed link)
"""

import argparse
import hashlib
import json
import os
import re
import secrets
import sys
import threading
import time
import traceback
from datetime import date, timedelta
from http.cookies import SimpleCookie
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

WEB_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PHOTO = os.path.join(WEB_DIR, "dev", "theme-cooking.jpg")
PHOTO_CREDIT = "Photo: Todd Quackenbush · CC0 · via Wikimedia Commons"
PHOTO_SOURCE = "https://commons.wikimedia.org/wiki/File:Chef%27s_Station_(Unsplash).jpg"
VERSION = "0.1.0-mock"
SPEED = 1.0
BODY_LIMIT = 64 * 1024
SESSION_COOKIE = "opp_session"
CSP = (
    "default-src 'self'; img-src 'self' data: https:; style-src 'self'; script-src 'self'; "
    "connect-src 'self'; font-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"
)
STATIC = {
    "/app.js": "text/javascript; charset=utf-8",
    "/app.css": "text/css; charset=utf-8",
    "/favicon.svg": "image/svg+xml",
}

# --- Taxonomy (mirrors crates/model) ---------------------------------------------------------

OPPORTUNITY_TYPES = [
    ("sales", "Sales & customers", "Leads, tenders, RFPs and buying signals"),
    ("partners", "Partnerships & channels", "Partners, resellers, integrations and distribution"),
    ("funding", "Funding & incentives", "Grants, tax incentives, public programmes and investors"),
    ("regulation", "Regulation & compliance", "New rules, deadlines and the demand they create"),
    ("market", "Market & expansion", "New segments, regions, products and competitor gaps"),
    ("visibility", "Events, awards & media", "Trade shows, awards, speaking slots and press"),
    ("talent", "Talent & skills", "Hiring pools, training subsidies and skills programmes"),
    ("operations", "Operations & suppliers", "Suppliers, cost savings and technology"),
    ("other", "Other", "Anything else worth acting on"),
]
STAGES = [("startup", "Startup"), ("growth", "Scale-up"), ("established", "Established business"),
          ("enterprise", "Enterprise"), ("nonprofit", "Non-profit"), ("public", "Public sector")]
REACH = [("local", "Local"), ("national", "National"), ("international", "International")]
AI_MODELS = [
    {"provider": "deepseek", "id": "deepseek-flash", "label": "DeepSeek Flash", "note": "Fast and inexpensive. The default."},
    {"provider": "deepseek", "id": "deepseek-v4-pro", "label": "DeepSeek V4 Pro", "note": "Deeper reasoning, at about four times the cost."},
    {"provider": "bedrock", "id": "", "label": "Claude on Amazon Bedrock", "note": "Runs in your own AWS account, with your model id or inference profile ARN."},
]
FREQUENCIES = [
    ("daily", "Every day", 86400), ("weekly", "Every week", 7 * 86400),
    ("fortnightly", "Every two weeks", 14 * 86400), ("monthly", "Every month", 30 * 86400),
    ("off", "Only when I ask", None),
]
TYPE_IDS = {t[0] for t in OPPORTUNITY_TYPES}
SHORT_TYPE = {
    "sales": "sales leads", "partners": "partners", "funding": "grants and incentives", "regulation": "regulatory changes",
    "market": "market signals", "visibility": "events and awards", "talent": "skills programmes",
    "operations": "suppliers", "other": "openings",
}


def meta_json():
    return {
        "opportunity_types": [{"id": i, "label": l, "hint": h} for i, l, h in OPPORTUNITY_TYPES],
        "stages": [{"id": i, "label": l} for i, l in STAGES],
        "reach": [{"id": i, "label": l} for i, l in REACH],
        "frequencies": [{"id": i, "label": l} for i, l, _ in FREQUENCIES],
        "ai_models": AI_MODELS,
        "ai_ready": True,
        "email_live": False,
        "version": VERSION,
    }


def interval(freq):
    return next((s for i, _, s in FREQUENCIES if i == freq), None)


# --- Text helpers (mirror crates/model/src/text.rs and links.rs) -----------------------------


def squash(s):
    return " ".join("".join(c for c in str(s or "") if c.isprintable() or c.isspace()).split())


def truncate(s, n):
    s = str(s or "")
    if len(s) <= n:
        return s
    return s[: n - 1].rstrip() + "…" if n else ""


def clean_list(items, max_items, max_chars):
    out = []
    for item in items if isinstance(items, list) else []:
        item = truncate(squash(item if isinstance(item, str) else ""), max_chars)
        if item and not any(o.lower() == item.lower() for o in out):
            out.append(item)
        if len(out) >= max_items:
            break
    return out


HTTP_RE = re.compile(r"^https?://([^/?#@\s]+)([/?#][^\s]*)?$", re.I)


def is_http_url(s):
    m = HTTP_RE.match(str(s or "").strip())
    if not m:
        return False
    host = m.group(1).rsplit(":", 1)[0] if re.search(r":\d+$", m.group(1)) else m.group(1)
    return ("." in host and not host.startswith(".") and not host.endswith(".")) or host.lower() == "localhost"


def text_field(d, key, n):
    v = d.get(key)
    return truncate(squash(v), n) if isinstance(v, str) else ""


def sanitize_profile(p):
    p = p if isinstance(p, dict) else {}
    stage = str(p.get("stage") or "").strip()
    reach = str(p.get("reach") or "").strip()
    seeking = []
    for s in p.get("seeking") or []:
        s = str(s).strip().lower()
        if s in TYPE_IDS and s not in seeking:
            seeking.append(s)
    goals = p.get("goals") if isinstance(p.get("goals"), str) else ""
    avoid = p.get("avoid") if isinstance(p.get("avoid"), str) else ""
    research = p.get("research") if isinstance(p.get("research"), dict) else None
    if research is not None:
        try:
            conf = max(0.0, min(1.0, float(research.get("confidence") or 0)))
        except (TypeError, ValueError):
            conf = 0.0
        research = {
            "confidence": conf,
            "summary": truncate(str(research.get("summary") or "").strip(), 600),
            "sources": [u for u in clean_list(research.get("sources") or [], 12, 500) if is_http_url(u)],
            "researched_at": int(research.get("researched_at") or 0),
        }
    return {
        "headline": text_field(p, "headline", 200),
        "stage": stage if stage in dict(STAGES) else "",
        "organization": text_field(p, "organization", 120),
        "website": str(p.get("website") or "").strip()[:300] if is_http_url(p.get("website")) else "",
        "industry": text_field(p, "industry", 80),
        "location": text_field(p, "location", 120),
        "reach": reach if reach in dict(REACH) else "",
        "seeking": seeking,
        "interests": clean_list(p.get("interests") or [], 20, 60),
        "skills": clean_list(p.get("skills") or [], 20, 60),
        "goals": truncate(goals.strip(), 1000),
        "avoid": truncate(avoid.strip(), 600),
        "links": [u for u in clean_list(p.get("links") or [], 10, 300) if is_http_url(u)],
        "languages": clean_list(p.get("languages") or [], 8, 40),
        "research": research,
    }


def profile_is_empty(p):
    return not (p["headline"].strip() or p["industry"].strip() or p["interests"] or p["skills"] or p["goals"].strip() or p["seeking"])


def empty_profile():
    return sanitize_profile({})


EMAIL_RE = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")
FREE_MAIL = {"gmail.com", "googlemail.com", "outlook.com", "hotmail.com", "yahoo.com", "icloud.com",
             "proton.me", "protonmail.com", "live.com", "aol.com", "me.com", "example.com"}

# --- Sample data --------------------------------------------------------------------------------

FOOD_WORDS = ("cook", "chef", "food", "restaurant", "cater", "kitchen", "bak", "culinary", "cuisine")

OPPORTUNITIES = [
    # (category, title, organization, site, location, deadline_days, value, score, verified, summary, why, action[, source, url])
    ("sales", "Catering partner wanted for a 12-week conference series",
     "Harbour Convention Centre", "harbourcc.example", "Cape Town, South Africa", 9, "Contract, about R400 000", 93, True,
     "The centre is appointing one local caterer for lunches and evening receptions at twelve industry conferences from March.",
     "Corporate conference work is your fastest-growing line, and this is a quarter of steady volume.",
     "Request the tender pack and attend the briefing session"),
    ("sales", "RFP: three-year staff lunch contract for 200 employees",
     "City of Cape Town procurement", "outlook.office.com", "Cape Town, South Africa", 16, "Public tender", 90, True,
     "An invitation to bid arrived in the ops inbox: daily lunches for the Foreshore offices, scored in favour of small local suppliers.",
     "Your office lunch subscriptions already run at this scale for two clients.",
     "Register on the supplier database and submit the bid", "microsoft", "https://outlook.office.com/mail/inbox/id/AAQkAGI2"),
    ("funding", "Small Food Business Grant: up to R150 000 for kitchen equipment",
     "Western Cape Enterprise Fund", "wcef.example", "Western Cape, South Africa", 5, "Up to R150 000", 88, True,
     "A once-off grant for owner-run food businesses under ten years old, covering commercial equipment and refrigeration.",
     "The second kitchen you're fitting out in Woodstock is exactly what this covers.",
     "Apply online with a one-page plan and two quotes"),
    ("regulation", "Allergen labelling becomes mandatory: get packed lunches compliant first",
     "Department of Health", "health.example", "South Africa", 41, "Head start on competitors", 86, True,
     "New labelling rules require allergen information on pre-packed food, including delivered office lunches.",
     "Offices will switch to caterers who can show compliant labels; you can be ready before the deadline.",
     "Audit your lunch menus and print compliant labels"),
    ("sales", "Atlantic Re asked about a year-end function for 250 guests",
     "Slack · #sales", "saltandember.slack.com", "Cape Town, South Africa", 7, "About R180 000", 85, False,
     "An enquiry from Atlantic Re was shared in #sales: a seated dinner for 250 in early December, venue still open.",
     "They're an existing lunch client, and December dates are filling fast.",
     "Reply in the thread with two menu options", "slack", "https://saltandember.slack.com/archives/C04SALES/p1795600000"),
    ("partners", "Preferred caterer listing at Kloof House",
     "Kloof House", "kloofhouse.example", "Constantia, Cape Town", 21, "About 30 weddings a year", 84, False,
     "The venue is choosing three preferred caterers for 2027 and will recommend them to every couple who books.",
     "Weddings are a core segment, and preferred listings bring bookings without marketing spend.",
     "Send your wedding menus and references"),
    ("visibility", "Eat Out Best Caterer awards: entries close soon",
     "Eat Out", "eatout.example", "South Africa", 2, "National press coverage", 82, True,
     "The national awards recognise caterers for quality, service and sustainability. Finalists are featured nationally.",
     "You were a finalist last year; a second entry builds on that recognition.",
     "Submit the entry form with five photos"),
    ("market", "Plant-based menus are now expected at corporate events",
     "Cape Town Events Report", "capetownevents.example", "Cape Town, South Africa", None, "Growing demand", 79, False,
     "Event organisers report that most corporate briefs now ask for a full plant-based option, not a single dish.",
     "A strong plant-based menu would set you apart from Tableau Events in corporate pitches.",
     "Design a plant-based corporate menu and add it to proposals"),
    ("talent", "Learnership subsidy: up to R40 000 per trainee chef",
     "Food and Beverages SETA", "foodbev.example", "South Africa", 33, "Up to R40 000 per trainee", 77, True,
     "A funded twelve-month learnership that covers most of a trainee chef's stipend and training.",
     "You're hiring for the second kitchen, and this halves the cost of growing your own team.",
     "Register as a host employer"),
    ("operations", "Direct seasonal produce at wholesale prices",
     "Overberg Growers Collective", "overberggrowers.example", "Overberg, South Africa", None, "Up to 18% off produce", 75, False,
     "Forty small farms supplying seasonal produce directly to caterers, with weekly delivery into Cape Town.",
     "Food costs are your biggest headwind, and this supports the local sourcing you promote.",
     "Book an introductory call"),
    ("visibility", "Good Food & Wine Show is on the Q1 calendar, but no stand is booked",
     "Confluence · Events calendar", "saltandember.atlassian.net", "Cape Town, South Africa", 26, "Subsidised stand", 74, False,
     "The Q1 events page lists the show as a priority, but the stand booking row is still empty. Subsidised stands close soon.",
     "Thirty thousand food lovers in three days is your best source of wedding leads.",
     "Book a subsidised stand", "confluence", "https://saltandember.atlassian.net/wiki/spaces/OPS/pages/1234/Q1+events"),
    ("funding", "Food tourism grants for new culinary experiences",
     "Cape Tourism Fund", "capetourismfund.example", "Western Cape, South Africa", 40, "Up to R500 000", 72, True,
     "Grants for tours, classes and dining experiences that bring visitors to local food businesses.",
     "Your Cape Malay tasting menu could become a bookable experience for visitors.",
     "Book a pre-application consultation"),
]

RESERVE = [
    ("partners", "Food tourism operator looking for a Cape Malay cooking class partner",
     "Bo-Kaap Walking Tours", "bokaaptours.example", "Cape Town, South Africa", None, "Revenue share", 83, False,
     "A tour operator wants a partner kitchen for a weekly cooking class added to its most popular walking tour.",
     "It's a new revenue line that uses your signature cuisine and quiet weekday kitchen time.",
     "Reply to the partnership call"),
    ("market", "Winelands wedding enquiries are at a five-year high",
     "Winelands Venues Association", "winelandsvenues.example", "Winelands, South Africa", None, "Peak season demand", 76, False,
     "Venues report record enquiries for 2027, with couples booking caterers earlier than usual.",
     "Opening 2027 bookings now would fill your calendar before competitors do.",
     "Open 2027 wedding bookings"),
    ("operations", "Commercial kitchen solar rebate",
     "City of Cape Town energy office", "capetownenergy.example", "Cape Town, South Africa", 30, "Up to 30% of system cost", 70, True,
     "A rebate for small businesses installing rooftop solar and battery backup.",
     "Backup power is a fixed cost for your kitchens; this cuts it substantially.",
     "Apply with an installer's quote"),
]


def iso_in(days):
    return None if days is None else (date.today() + timedelta(days=days)).isoformat()


def make_opportunity(t, published_days_ago=9):
    cat, title, org, site, loc, dl, value, score, verified, summary, why, action = t[:12]
    source, url = (t[12], t[13]) if len(t) > 12 else ("", "")
    deadline = iso_in(dl)
    if deadline:
        action = f"{action} by {date.fromisoformat(deadline).strftime('%-d %B')}"
    slug = re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")[:60]
    return {
        "url": url or f"https://{site}/{slug}", "title": title, "organization": org, "category": cat, "source": source,
        "summary": summary, "why": why, "action": action, "location": loc, "deadline": deadline,
        "value": value, "score": score, "published": (date.today() - timedelta(days=published_days_ago)).isoformat(),
        "site_name": {"slack": "Slack", "microsoft": "Outlook", "confluence": "Confluence"}.get(source, org), "site_icon": "", "verified": verified,
    }


# --- State ----------------------------------------------------------------------------------------

LOCK = threading.RLock()
STATE = {"users": {}, "sessions": {}, "runs": {}, "opps": {}, "notes": {}, "resets": {}, "failures": {},
         "next_id": 1, "next_outcome": {}, "reset_links": {}}


def new_id():
    STATE["next_id"] += 1
    return STATE["next_id"]


def now():
    return int(time.time())


def pw_hash(pw):
    return hashlib.sha256(("mock-salt:" + pw).encode()).hexdigest()


def user_json(u):
    return {k: u[k] for k in ("id", "email", "business_name", "website", "created_at", "onboarding")}


def settings_json(u):
    return {"frequency": u["frequency"], "email_digest": u["email_digest"],
            "next_run_at": u["next_run_at"], "last_run_at": u["last_run_at"]}


def theme_json(u):
    t = u["theme"]
    has_photo = t["photo"] and t["version"] > 0
    return {
        "label": t["label"], "accent": t["accent"],
        "image_url": f"/api/theme/image?v={t['version']}" if has_photo else None,
        "credit": PHOTO_CREDIT if has_photo else None,
        "source_url": PHOTO_SOURCE if has_photo else None,
        "status": t["status"],
    }


def run_json(r):
    return {k: r[k] for k in ("id", "kind", "status", "trigger", "created_at", "started_at", "finished_at",
                              "progress", "log", "error", "summary")}


def make_user(business_name, website, email, password):
    uid = new_id()
    u = {
        "id": uid, "business_name": business_name, "website": website, "email": email, "password": pw_hash(password),
        "created_at": now(), "onboarding": "researching",
        "profile": sanitize_profile({"organization": business_name, "website": website}), "intel": empty_intel(),
        "theme": {"label": "", "accent": "#1d6b57", "photo": False, "version": 0, "status": "none",
                  "ready_at": None, "subject": ""},
        "frequency": "weekly", "email_digest": True, "next_run_at": None, "last_run_at": None,
        "ai": {"provider": "deepseek", "model": "deepseek-flash", "bedrock": None},
        "connections": {}, "audit": [],
    }
    STATE["users"][uid] = u
    return u


def empty_intel():
    return {"overview": {"summary": "", "industry": "", "headquarters": "", "founded": "", "size": "",
                         "business_model": "", "markets": [], "sources": []},
            "offerings": [], "segments": [], "competitors": [], "trends": [], "regulation": [], "news": [],
            "confidence": 0.0, "updated_at": 0}


def find_user_by_email(email):
    email = email.strip().lower()
    return next((u for u in STATE["users"].values() if u["email"].lower() == email), None)


def new_session(uid):
    token = secrets.token_urlsafe(32)
    STATE["sessions"][token] = uid
    return token


def session_cookie(token, clear=False):
    if clear:
        return f"{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"
    return f"{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age=2592000"


def user_runs(uid, kind=None):
    return [r for r in STATE["runs"].values() if r["user_id"] == uid and (kind is None or r["kind"] == kind)]


def active_run(uid):
    runs = [r for r in user_runs(uid) if r["status"] in ("queued", "running")]
    return runs[0] if runs else None


def last_finished_run(uid):
    runs = [r for r in user_runs(uid) if r["status"] in ("done", "failed")]
    return max(runs, key=lambda r: (r["finished_at"] or 0, r["id"])) if runs else None


def latest_done_discover(uid):
    runs = [r for r in user_runs(uid, "discover") if r["status"] == "done"]
    return max(runs, key=lambda r: r["id"]) if runs else None


def user_opps(uid):
    return [o for o in STATE["opps"].values() if o["user_id"] == uid]


def item_json(o, latest_id):
    d = dict(o["data"])
    d.update({"id": o["id"], "status": o["status"], "is_new": o["first_seen_run"] == latest_id,
              "vote": o["vote"], "first_seen_at": o["first_seen_at"]})
    return d


# --- Theme --------------------------------------------------------------------------------------

PALETTE = ["#1d6b57", "#1e4fa3", "#9a3412", "#0f766e", "#a16207", "#be123c", "#4d7c0f", "#6d28d9"]


def theme_for(profile):
    words = " ".join([profile["headline"], " ".join(profile["interests"]), " ".join(profile["skills"])]).lower()
    if any(w in words for w in FOOD_WORDS) or any(w in profile.get("industry", "").lower() for w in FOOD_WORDS):
        return (profile.get("industry") or "Food & catering"), "#c2410c", True
    subject = (profile["interests"] or profile["skills"] or [profile["headline"] or "Opportunities"])[0]
    label = truncate(subject[:1].upper() + subject[1:], 30)
    accent = PALETTE[int(hashlib.md5(label.lower().encode()).hexdigest(), 16) % len(PALETTE)]
    return label, accent, False


def start_theme_build(u, ready_at, pending=None):
    label, accent, photo = pending or theme_for(u["profile"])
    t = u["theme"]
    if pending is None and t["status"] == "ready" and t["subject"] == label:
        return
    t.update({"status": "building", "ready_at": ready_at, "pending": (label, accent, photo), "subject": label})


def complete_theme(u):
    t = u["theme"]
    label, accent, photo = t.pop("pending", None) or (t["label"], t["accent"], t["photo"])
    t.update({"status": "ready", "label": label, "accent": accent, "photo": photo,
              "version": t["version"] + 1, "ready_at": None, "subject": label})


# --- Runs ---------------------------------------------------------------------------------------

def discover_steps(u, first):
    p = u["profile"]
    city = (p["location"].split(",")[0].strip() or "your area")
    country = p["location"].split(",")[-1].strip() if "," in p["location"] else "your country"
    topic = (p["interests"][0] if p["interests"] else (p["skills"][0] if p["skills"] else "your field")).lower()
    seeking = p["seeking"] or ["sales", "partners", "funding", "visibility"]
    kinds = [SHORT_TYPE.get(s, "openings") for s in seeking] + ["partners", "market signals", "events and awards"]
    plan = ", ".join(kinds[:2]) + " and " + kinds[2]
    steps = [
        (0.0, "planning", "Reading your profile" if first else "Looking for anything new since your last refresh", 4),
        (1.2, "planning", f"Planning 14 searches across {plan}", 9),
        (2.4, "searching", f"Searching for {topic} {kinds[0]} in {city}", 18),
        (3.6, "searching", f"Searching for {kinds[1]} for {topic} in {country}", 26),
        (5.0, "theming" if first else "searching", "Choosing a photo for your page" if first else f"Searching for {kinds[2]} near {city}", 33),
        (6.0, "searching", f"Searching for {kinds[3 % len(kinds)]} near {city}", 40),
        (7.2, "searching", "Looking further afield: remote and international openings", 48),
        (8.4, "reviewing", "Reviewing 86 results", 58),
        (9.9, "reviewing", "Keeping the 21 that are open and fit you", 68),
        (11.4, "verifying", "Reading the top pages to confirm they're still open", 78),
        (12.9, "verifying", "Checking deadlines and amounts", 88),
        (14.2, "saving", "Saving your opportunities", 96),
    ]
    return steps, 15.0


def research_steps(u, hints):
    name, host = u["business_name"], re.sub(r"^https?://(www\.)?", "", u["website"]).rstrip("/")
    return [
        (0.0, "planning", f"Reading {host}", 6),
        (1.2, "searching", f"Finding what {name} sells and who it serves", 20),
        (2.4, "searching", f"Searching the news for {name}", 34),
        (3.6, "searching", "Mapping competitors in your market", 48),
        (4.8, "reviewing", "Checking regulation that affects your industry", 62),
        (6.0, "reviewing", "Reading market trends and drivers", 76),
        (7.0, "saving", "Writing your business profile", 92),
    ], 8.0


def theme_steps():
    return [(0.0, "theming", "Looking for a new photo for your page", 20),
            (1.5, "theming", "Comparing a few candidates", 60),
            (3.0, "saving", "Saving your new background", 92)], 4.0


def start_run(u, kind, trigger, hints=""):
    rid = new_id()
    first = kind == "discover" and not user_runs(u["id"], "discover")
    if kind == "discover":
        steps, duration = discover_steps(u, first)
    elif kind == "research":
        steps, duration = research_steps(u, hints)
    else:
        steps, duration = theme_steps()
    outcome = STATE["next_outcome"].pop(kind, "ok")
    r = {
        "id": rid, "user_id": u["id"], "kind": kind, "status": "queued", "trigger": trigger,
        "created_at": now(), "started_at": None, "finished_at": None,
        "progress": {"stage": "queued", "message": "Getting ready", "percent": 0}, "log": [],
        "error": None, "summary": None,
        "_t0": time.time(), "_steps": steps, "_duration": duration, "_outcome": outcome,
        "_first": first, "_hints": hints, "_applied": False,
    }
    STATE["runs"][rid] = r
    if kind == "discover" and (first or trigger == "profile"):
        start_theme_build(u, time.time() + 5.6 / SPEED)
    if kind == "theme":
        label, accent, photo = theme_for(u["profile"])
        start_theme_build(u, time.time() + (duration + 0.6) / SPEED, (label, accent, photo))
    return r


def tick_run(r):
    if r["status"] not in ("queued", "running"):
        return
    elapsed = (time.time() - r["_t0"]) * SPEED
    if elapsed < 0.6:
        return
    if r["status"] == "queued":
        r["status"] = "running"
        r["started_at"] = now()
    t = elapsed - 0.6
    passed = [s for s in r["_steps"] if s[0] <= t]
    if r["_outcome"] == "fail" and t >= r["_duration"] * 0.62:
        passed = [s for s in r["_steps"] if s[0] <= r["_duration"] * 0.62]
    r["log"] = [{"stage": s[1], "message": s[2], "percent": s[3]} for s in passed][-16:]
    if r["log"]:
        r["progress"] = dict(r["log"][-1])
    u = STATE["users"].get(r["user_id"])
    if u is None:
        return
    if r["_outcome"] == "fail" and t >= r["_duration"] * 0.62:
        finish_failed(u, r)
    elif t >= r["_duration"]:
        finish_done(u, r)


def finish_failed(u, r):
    r["status"] = "failed"
    r["finished_at"] = now()
    r["progress"] = {"stage": "failed", "message": "Stopped", "percent": r["progress"].get("percent", 0)}
    if r["kind"] == "research":
        r["error"] = ("We couldn't read enough about your business on the public web. "
                      "Fill in the profile below and we'll take it from there.")
        if u["onboarding"] == "researching":
            u["onboarding"] = "review"
    elif r["kind"] == "discover":
        r["error"] = ("The search service didn't respond in time, so this refresh stopped early. "
                      "Your saved opportunities are safe. Try again in a few minutes.")
    else:
        r["error"] = "We couldn't find a better photo this time."
        u["theme"].pop("pending", None)
        u["theme"]["status"] = "ready" if u["theme"]["version"] else "none"


def finish_done(u, r):
    r["status"] = "done"
    r["finished_at"] = now()
    if r["kind"] == "research":
        drafted = drafted_profile(u, r["_hints"])
        if u["onboarding"] == "ready":
            for k, v in drafted.items():
                if k != "research" and not u["profile"].get(k):
                    u["profile"][k] = v
        else:
            u["profile"] = drafted
            u["onboarding"] = "review"
        u["intel"] = drafted_intel(u)
    elif r["kind"] == "discover":
        r["summary"] = apply_discovery(u, r)
        for prov, conn in u["connections"].items():
            target = {"slack": "https://slack.com/api/search.messages", "microsoft": "https://graph.microsoft.com/v1.0/me/messages",
                      "confluence": conn["account"]["url"] + "/wiki/rest/api/search"}[prov]
            audit(u, prov, "read", "GET", target, 200, "Recent items for discovery", 18400, 420)
            conn["last_used_at"] = now()
        u["last_run_at"] = now()
        iv = interval(u["frequency"])
        u["next_run_at"] = now() + iv if iv else None
    elif u["theme"]["status"] == "building":
        complete_theme(u)
    r["progress"] = {"stage": "done", "message": "Done", "percent": 100}


def apply_discovery(u, r):
    existing = user_opps(u["id"])
    urls = {o["data"]["url"] for o in existing}
    if r["_outcome"] == "empty":
        live = [o for o in existing if o["status"] in ("active", "saved")]
        return {"new": 0, "refreshed": len(live), "expired": 0, "total": len(live)}
    pool = OPPORTUNITIES if not existing else RESERVE
    fresh = [make_opportunity(t, 3 + i) for i, t in enumerate(pool)]
    fresh = [d for d in fresh if d["url"] not in urls][: (len(OPPORTUNITIES) if not existing else 3)]
    refreshed = 0
    for o in existing:
        if o["status"] in ("active", "saved"):
            o["last_seen_run"] = r["id"]
            refreshed += 1
    for d in fresh:
        oid = new_id()
        STATE["opps"][oid] = {"id": oid, "user_id": u["id"], "data": d, "status": "active", "vote": 0,
                              "first_seen_run": r["id"], "last_seen_run": r["id"], "first_seen_at": now()}
    expired = 0
    if existing:
        candidates = sorted([o for o in existing if o["status"] == "active"], key=lambda o: o["data"]["score"])
        for o in candidates[:1]:
            o["status"] = "expired"
            expired += 1
            refreshed -= 1
    live = [o for o in user_opps(u["id"]) if o["status"] in ("active", "saved")]
    return {"new": len(fresh), "refreshed": refreshed, "expired": expired, "total": len(live)}


def drafted_profile(u, hints):
    intel = drafted_intel(u)
    return sanitize_profile({
        "headline": "Catering for weddings, corporate events and offices in Cape Town",
        "stage": "growth", "organization": u["business_name"], "website": u["website"],
        "industry": "Catering and events", "location": "Cape Town, South Africa", "reach": "national",
        "seeking": ["sales", "partners", "funding", "visibility"],
        "interests": ["corporate events", "weddings", "Cape Malay cuisine", "sustainable sourcing"],
        "skills": ["event catering", "office lunch subscriptions", "menu design"],
        "goals": "Win more corporate contracts and open a second kitchen in 2027.",
        "avoid": "", "links": [], "languages": ["English", "Afrikaans"],
        "research": {"confidence": intel["confidence"], "summary": intel["overview"]["summary"],
                     "sources": [x["url"] for x in intel["overview"]["sources"]], "researched_at": now()},
    })


def drafted_intel(u):
    name, site = u["business_name"], u["website"].rstrip("/")
    ago = lambda days: (date.today() - timedelta(days=days)).isoformat()
    own = lambda path, title, days: {"url": site + path, "title": title, "date": ago(days)}
    ext = lambda url, title, days: {"url": url, "title": title, "date": ago(days)}
    return {
        "overview": {
            "summary": (f"{name} is a Cape Town catering company that cooks for weddings, corporate events and conferences, "
                        "and runs office lunch subscriptions. It is known for Cape Malay tasting menus and seasonal, locally sourced food."),
            "industry": "Catering and events", "headquarters": "Cape Town, South Africa", "founded": "2019",
            "size": "11-50 employees", "business_model": "Event contracts and recurring office lunch subscriptions",
            "markets": ["Western Cape", "Gauteng"],
            "sources": [own("/about", f"About {name}", 40), ext("https://www.eatout.example/features/cape-malay-revival", "The Cape Malay revival", 120)],
        },
        "offerings": [
            {"name": "Wedding catering", "description": "Plated and family-style menus for 60 to 300 guests, with tastings and full service staff.",
             "audience": "Couples and wedding planners", "pricing": "From R650 per guest", "source": own("/weddings", "Weddings", 30)},
            {"name": "Corporate events", "description": "Conference lunches, product launches and evening receptions across the Western Cape.",
             "audience": "Event managers and corporate teams", "pricing": "", "source": own("/corporate", "Corporate catering", 30)},
            {"name": "Office lunch subscriptions", "description": "Weekly rotating lunch menus delivered to offices, with vegetarian, halaal and allergy-aware options.",
             "audience": "Offices with 20 to 200 staff", "pricing": "From R95 per lunch", "source": own("/office-lunches", "Office lunches", 30)},
            {"name": "Cape Malay tasting menu", "description": "A seven-course signature menu for private dinners and supper clubs.",
             "audience": "Private hosts", "pricing": "R1 200 per guest", "source": own("/tasting-menu", "Tasting menu", 30)},
        ],
        "segments": [
            {"name": "Corporate clients", "description": "Companies booking conference catering, launches and recurring staff lunches.",
             "examples": ["Harbour Convention Centre", "Atlantic Re", "Southlight Media"], "source": own("/clients", "Clients", 60)},
            {"name": "Weddings and private events", "description": "Couples and planners in Cape Town and the Winelands, mostly 80 to 200 guests.",
             "examples": [], "source": own("/weddings", "Weddings", 30)},
            {"name": "Venues and hospitality partners", "description": "Venues that recommend a preferred caterer to the people who book them.",
             "examples": ["Kloof House", "The Old Biscuit Mill"], "source": own("/partners", "Partners", 45)},
        ],
        "competitors": [
            {"name": "Tableau Events", "website": "https://tableauevents.example", "summary": "The largest event caterer in Cape Town, focused on corporate functions.",
             "difference": "Bigger team and higher prices; no heritage or plant-based specialism.", "overlap": "high",
             "source": ext("https://tableauevents.example/about", "Tableau Events: about", 20)},
            {"name": "Harvest & Co.", "website": "https://harvestco.example", "summary": "Farm-to-table caterer for weddings in the Winelands.",
             "difference": "Strong in weddings; no corporate or office lunch offer.", "overlap": "medium",
             "source": ext("https://harvestco.example", "Harvest & Co.", 35)},
            {"name": "Mzansi Feasts", "website": "https://mzansifeasts.example", "summary": "Corporate and conference caterer in Cape Town and Johannesburg.",
             "difference": "Competes on price for large conferences; smaller menu range.", "overlap": "medium",
             "source": ext("https://mzansifeasts.example/services", "Mzansi Feasts: services", 50)},
            {"name": "LunchBox Office", "website": "https://lunchbox.example", "summary": "An app-based office lunch delivery service.",
             "difference": "Cheaper and tech-led, but no events and no chef-led menus.", "overlap": "low",
             "source": ext("https://lunchbox.example", "LunchBox Office", 15)},
        ],
        "trends": [
            {"title": "Conference tourism is back above 2019 levels", "summary": "Cape Town hosted more business events this year than before the pandemic, and bookings for next year are ahead again.",
             "impact": "tailwind", "source": ext("https://capetownevents.example/report-2026", "Business events report 2026", 25)},
            {"title": "Plant-based menus are now expected at corporate events", "summary": "Most corporate briefs ask for a full plant-based menu rather than a single dish.",
             "impact": "tailwind", "source": ext("https://capetownevents.example/menus", "What event organisers want", 40)},
            {"title": "Food costs are rising faster than inflation", "summary": "Fresh produce and protein prices rose roughly 9% this year, squeezing fixed-price contracts.",
             "impact": "headwind", "source": ext("https://statssa.example/food-prices", "Food price index", 18)},
            {"title": "Power cuts are easing, but backup costs remain", "summary": "Fewer outages this year, though kitchens still carry generator and battery costs.",
             "impact": "mixed", "source": ext("https://energy.example/outlook", "Energy outlook", 30)},
        ],
        "regulation": [
            {"title": "Allergen labelling on pre-packed food (R3337)", "summary": "Allergen information becomes mandatory on pre-packed food, including delivered office lunches.",
             "jurisdiction": "South Africa", "status": "passed", "effective": (date.today() + timedelta(days=41)).isoformat(), "impact": "both",
             "source": ext("https://health.example/r3337", "Regulations relating to labelling", 60)},
            {"title": "Certificate of acceptability renewals move online", "summary": "Food premises certificates are now renewed online, with inspections booked within 14 days.",
             "jurisdiction": "City of Cape Town", "status": "in force", "effective": (date.today() - timedelta(days=75)).isoformat(), "impact": "opportunity",
             "source": ext("https://capetown.example/coa", "Certificate of acceptability", 80)},
            {"title": "Proposed levy on single-use packaging", "summary": "A draft levy on single-use food packaging is open for public comment.",
             "jurisdiction": "South Africa", "status": "consultation", "effective": None, "impact": "risk",
             "source": ext("https://environment.example/packaging-levy", "Packaging levy consultation", 22)},
        ],
        "news": [
            {"title": f"{name} named a finalist for Best Caterer", "summary": "The Eat Out awards shortlisted the business for its Cape Malay tasting menus.",
             "source": ext("https://www.eatout.example/awards-2026-finalists", "Eat Out awards finalists", 12)},
            {"title": f"{name} opens a second kitchen in Woodstock", "summary": "The new kitchen doubles production capacity for office lunches.",
             "source": own("/news/woodstock-kitchen", "Our new Woodstock kitchen", 48)},
            {"title": f"{name} caters the Cape Town Food Summit", "summary": "Lunch for 600 delegates over two days, with a fully plant-based menu.",
             "source": ext("https://foodsummit.example/thanks", "Food Summit partners", 95)},
        ],
        "confidence": 0.82, "updated_at": now(),
    }


def tick_user(u):
    for r in sorted(user_runs(u["id"]), key=lambda r: r["id"]):
        tick_run(r)
    t = u["theme"]
    if t["status"] == "building" and t.get("ready_at") is not None and time.time() >= t["ready_at"]:
        complete_theme(u)


# --- Seeding (dev-only, for screenshots and fast tests) -------------------------------------

def seed(body):
    name = squash(body.get("business_name") or "Salt & Ember Catering")
    site = body.get("website") or "https://saltandember.example"
    email = (body.get("email") or f"seed-{secrets.token_hex(4)}@example.com").strip()
    password = body.get("password") or "correct horse"
    state = body.get("state") or "dashboard"
    old = find_user_by_email(email)
    if old:
        delete_user(old["id"])
    u = make_user(name, site, email, password)
    if state == "researching":
        start_run(u, "research", "signup", body.get("hints") or "")
    elif state in ("review", "review_failed"):
        if state == "review":
            u["profile"] = drafted_profile(u, "")
            u["intel"] = drafted_intel(u)
        r = start_run(u, "research", "signup")
        r["_outcome"] = "ok" if state == "review" else "fail"
        r["_t0"] -= 100
        tick_run(r)
    else:
        u["profile"] = drafted_profile(u, "")
        u["intel"] = drafted_intel(u)
        u["onboarding"] = "ready"
        if body.get("connections"):
            connect(u, "slack", "token", "provider", {"name": name, "user": "ops", "url": "https://saltandember.slack.com"})
            audit(u, "slack", "token", "POST", "https://slack.com/api/auth.test", 200, "Token verified: scopes search:read, users:read, team:read", 412, 180)
            audit(u, "slack", "read", "GET", "https://slack.com/api/search.messages", 200, "Recent items for discovery", 18400, 420)
            audit(u, "slack", "blocked", "POST", "https://slack.com/api/chat.postMessage", 0, "Blocked: Opportunity never sends write requests")
        if state == "loading":
            start_run(u, "discover", "signup")
            if body.get("theme_ready"):
                tick_ready_theme(u)
        else:
            tick_ready_theme(u)
            if state in ("dashboard", "refreshing"):
                seed_history(u)
                if state == "refreshing":
                    start_run(u, "discover", "manual")
            elif state in ("empty", "failed"):
                r = start_run(u, "discover", "signup")
                r["_outcome"] = "empty" if state == "empty" else "fail"
                r["_t0"] -= 100
                tick_run(r)
                u["last_run_at"] = now() - 3600
                u["next_run_at"] = now() + 7 * 86400 - 3600
    token = new_session(u["id"])
    return u, token


def tick_ready_theme(u):
    u["theme"]["pending"] = theme_for(u["profile"])
    complete_theme(u)


def seed_history(u):
    t = now()
    r1 = {"id": new_id(), "user_id": u["id"], "kind": "discover", "status": "done", "trigger": "signup",
          "created_at": t - 9 * 86400, "started_at": t - 9 * 86400, "finished_at": t - 9 * 86400 + 60,
          "progress": {"stage": "done", "message": "Done", "percent": 100}, "log": [], "error": None,
          "summary": {"new": 12, "refreshed": 0, "expired": 0, "total": 12},
          "_t0": 0, "_steps": [], "_duration": 0, "_outcome": "ok", "_first": True, "_hints": "", "_applied": True}
    STATE["runs"][r1["id"]] = r1
    for i, tpl in enumerate(OPPORTUNITIES):
        oid = new_id()
        STATE["opps"][oid] = {"id": oid, "user_id": u["id"], "data": make_opportunity(tpl, 12 + i), "status": "active",
                              "vote": 0, "first_seen_run": r1["id"], "last_seen_run": r1["id"],
                              "first_seen_at": t - 9 * 86400}
    r2 = dict(r1, id=new_id(), trigger="schedule", created_at=t - 2 * 86400, started_at=t - 2 * 86400,
              finished_at=t - 2 * 86400 + 70, summary={"new": 3, "refreshed": 11, "expired": 1, "total": 14})
    STATE["runs"][r2["id"]] = r2
    for i, tpl in enumerate(RESERVE):
        oid = new_id()
        STATE["opps"][oid] = {"id": oid, "user_id": u["id"], "data": make_opportunity(tpl, 2 + i), "status": "active",
                              "vote": 0, "first_seen_run": r2["id"], "last_seen_run": r2["id"],
                              "first_seen_at": t - 2 * 86400}
    mine = sorted(user_opps(u["id"]), key=lambda o: o["id"])
    for o in mine:
        o["last_seen_run"] = r2["id"]
    mine[11]["status"] = "expired"   # pruned by the second run
    mine[1]["status"] = "saved"
    mine[5]["status"] = "saved"
    mine[0]["vote"] = 1
    u["last_run_at"] = t - 2 * 86400
    u["next_run_at"] = t + 5 * 86400
    for text, ago in (("More corporate contracts, fewer one-off parties", 3), ("Nothing that needs a liquor licence", 6)):
        nid = new_id()
        STATE["notes"][nid] = {"id": nid, "user_id": u["id"], "text": text, "created_at": t - ago * 86400}


def delete_user(uid):
    STATE["users"].pop(uid, None)
    for k in [k for k, v in STATE["sessions"].items() if v == uid]:
        STATE["sessions"].pop(k)
    for table in ("runs", "opps", "notes"):
        for k in [k for k, v in STATE[table].items() if v["user_id"] == uid]:
            STATE[table].pop(k)


# --- HTTP plumbing ------------------------------------------------------------------------------

class ApiError(Exception):
    STATUS = {"bad_request": 400, "unauthorized": 401, "forbidden": 403, "not_found": 404,
              "method_not_allowed": 405, "conflict": 409, "payload_too_large": 413,
              "unsupported_media_type": 415, "validation": 422, "rate_limited": 429,
              "server_error": 500, "ai_unavailable": 503}

    def __init__(self, code, message, field=None, retry_after=None):
        super().__init__(message)
        self.code, self.message, self.field, self.retry_after = code, message, field, retry_after

    def response(self):
        err = {"code": self.code, "message": self.message}
        if self.field:
            err["field"] = self.field
        headers = [("Retry-After", str(self.retry_after))] if self.retry_after else []
        return json_resp({"error": err}, self.STATUS[self.code], headers)


def json_resp(obj, status=200, headers=None):
    return status, [("Content-Type", "application/json; charset=utf-8")] + (headers or []), json.dumps(obj).encode()


def empty_resp(status=204, headers=None):
    return status, headers or [], b""


def rate_check(key, limit, window):
    t = time.time()
    hits = [h for h in STATE["failures"].get(key, []) if t - h < window]
    STATE["failures"][key] = hits
    return len(hits) >= limit


def rate_hit(key):
    STATE["failures"].setdefault(key, []).append(time.time())


class Ctx:
    def __init__(self, method, path, query, headers, body):
        self.method, self.path, self.query, self.headers, self.body = method, path, query, headers, body
        self.set_cookie = None

    def json(self):
        if not self.body:
            return {}
        try:
            data = json.loads(self.body.decode("utf-8"))
        except (ValueError, UnicodeDecodeError):
            raise ApiError("bad_request", "That request wasn't valid JSON.")
        if not isinstance(data, dict):
            raise ApiError("bad_request", "That request wasn't valid JSON.")
        return data

    def session_token(self):
        raw = self.headers.get("Cookie")
        if not raw:
            return None
        c = SimpleCookie()
        try:
            c.load(raw)
        except Exception:
            return None
        return c[SESSION_COOKIE].value if SESSION_COOKIE in c else None

    def user(self):
        token = self.session_token()
        uid = STATE["sessions"].get(token) if token else None
        u = STATE["users"].get(uid) if uid else None
        if not u:
            raise ApiError("unauthorized", "Please sign in to continue.")
        tick_user(u)
        return u


def validate_name(v, field, what):
    v = squash(v if isinstance(v, str) else "")
    if not v:
        raise ApiError("validation", f"Enter your {what}.", field)
    if len(v) > 80:
        raise ApiError("validation", f"Keep your {what} under 80 characters.", field)
    return v


def validate_email(v):
    v = (v if isinstance(v, str) else "").strip()
    if not EMAIL_RE.match(v) or len(v) > 254:
        raise ApiError("validation", "Enter a valid email address.", "email")
    return v


def validate_password(v, field="password"):
    v = v if isinstance(v, str) else ""
    if len(v) < 8:
        raise ApiError("validation", "Use at least 8 characters for your password.", field)
    if len(v) > 200:
        raise ApiError("validation", "Use at most 200 characters for your password.", field)
    return v


def ai_json(u):
    a = u["ai"]
    b = a.get("bedrock")
    return {"provider": a["provider"], "model": a["model"],
            "bedrock": {k: b[k] for k in ("region", "model", "auth", "key_hint")} if b else None}


def me_json(u):
    return {"user": user_json(u), "profile": u["profile"], "intel": u["intel"], "ai": ai_json(u),
            "settings": settings_json(u), "theme": theme_json(u),
            "run": run_json(active_run(u["id"])) if active_run(u["id"]) else None,
            "last_run": run_json(last_finished_run(u["id"])) if last_finished_run(u["id"]) else None}


def opp_for(u, oid):
    o = STATE["opps"].get(int(oid))
    if not o or o["user_id"] != u["id"] or o["status"] in ("expired",):
        raise ApiError("not_found", "That opportunity isn't available any more.")
    return o


ROUTES = []


def route(method, pattern):
    def deco(fn):
        ROUTES.append((method, re.compile("^" + pattern + "$"), fn))
        return fn
    return deco


@route("GET", "/api/meta")
def r_meta(ctx):
    return json_resp(meta_json())


@route("POST", "/api/auth/signup")
def r_signup(ctx):
    b = ctx.json()
    name = squash(b.get("business_name") if isinstance(b.get("business_name"), str) else "")
    if not name:
        raise ApiError("validation", "Enter your business name.", "business_name")
    if len(name) > 120:
        raise ApiError("validation", "Keep the business name under 120 characters.", "business_name")
    site = (b.get("website") if isinstance(b.get("website"), str) else "").strip()
    if site and not re.match(r"^https?://", site, re.I):
        site = "https://" + site
    if not is_http_url(site):
        raise ApiError("validation", "Enter your website address, like acme.co.za.", "website")
    email = validate_email(b.get("email"))
    password = validate_password(b.get("password"))
    hints = squash(b.get("hints") if isinstance(b.get("hints"), str) else "")
    if len(hints) > 300:
        raise ApiError("validation", "Keep this under 300 characters.", "hints")
    if find_user_by_email(email):
        raise ApiError("conflict", "There's already an account with that email. Try signing in instead.", "email")
    u = make_user(name, site, email, password)
    start_run(u, "research", "signup", hints)
    ctx.set_cookie = session_cookie(new_session(u["id"]))
    return json_resp({"user": user_json(u)}, 201)


@route("POST", "/api/auth/login")
def r_login(ctx):
    b = ctx.json()
    email = (b.get("email") or "").strip().lower() if isinstance(b.get("email"), str) else ""
    key = "login:" + email
    if rate_check(key, 5, 600):
        raise ApiError("rate_limited", "Too many attempts. Wait a few minutes, then try again.", retry_after=300)
    u = find_user_by_email(email) if email else None
    if not u or u["password"] != pw_hash(b.get("password") if isinstance(b.get("password"), str) else ""):
        rate_hit(key)
        raise ApiError("unauthorized", "Email or password is incorrect.")
    ctx.set_cookie = session_cookie(new_session(u["id"]))
    return json_resp({"user": user_json(u)})


@route("POST", "/api/auth/logout")
def r_logout(ctx):
    token = ctx.session_token()
    if token:
        STATE["sessions"].pop(token, None)
    ctx.set_cookie = session_cookie("", clear=True)
    return empty_resp(204)


@route("POST", "/api/auth/forgot")
def r_forgot(ctx):
    b = ctx.json()
    email = b.get("email") if isinstance(b.get("email"), str) else ""
    u = find_user_by_email(email) if email else None
    if u:
        token = secrets.token_urlsafe(24)
        STATE["resets"][token] = {"user_id": u["id"], "expires": time.time() + 3600, "used": False}
        link = f"/reset?token={token}"
        STATE["reset_links"][u["email"].lower()] = link
        print(f"[mock] password reset link for {u['email']}: {link}", file=sys.stderr)
    return json_resp({}, 202)


@route("POST", "/api/auth/reset")
def r_reset(ctx):
    b = ctx.json()
    token = b.get("token") if isinstance(b.get("token"), str) else ""
    rec = STATE["resets"].get(token)
    if not rec or rec["used"] or rec["expires"] < time.time() or rec["user_id"] not in STATE["users"]:
        raise ApiError("bad_request", "This link has expired or was already used.")
    password = validate_password(b.get("password"))
    rec["used"] = True
    u = STATE["users"][rec["user_id"]]
    u["password"] = pw_hash(password)
    for k in [k for k, v in STATE["sessions"].items() if v == u["id"]]:
        STATE["sessions"].pop(k)
    ctx.set_cookie = session_cookie(new_session(u["id"]))
    return json_resp({"user": user_json(u)})


@route("GET", "/api/me")
def r_me(ctx):
    return json_resp(me_json(ctx.user()))


@route("POST", "/api/intel/refresh")
def r_intel_refresh(ctx):
    u = ctx.user()
    ctx.json()
    run = active_run(u["id"])
    if run:
        return json_resp({"run": run_json(run)}, 202)
    if rate_check("research:%d" % u["id"], 3, 86400):
        raise ApiError("rate_limited", "That's the most research we can do today. Try again tomorrow.", retry_after=3600)
    rate_hit("research:%d" % u["id"])
    return json_resp({"run": run_json(start_run(u, "research", "manual"))}, 202)


@route("GET", "/api/settings/ai")
def r_ai_get(ctx):
    return json_resp(ai_json(ctx.user()))


@route("PUT", "/api/settings/ai")
def r_ai_put(ctx):
    u = ctx.user()
    b = ctx.json()
    provider = b.get("provider")
    if provider == "deepseek":
        model = b.get("model") or "deepseek-flash"
        if model not in ("deepseek-flash", "deepseek-v4-pro"):
            raise ApiError("validation", "Choose DeepSeek Flash or DeepSeek V4 Pro.", "model")
        u["ai"]["provider"], u["ai"]["model"] = "deepseek", model
    elif provider == "bedrock":
        bb = b.get("bedrock") if isinstance(b.get("bedrock"), dict) else {}
        old = u["ai"].get("bedrock") or {}
        region = squash(bb.get("region") or "")
        model = squash(bb.get("model") or "")
        if not re.match(r"^[a-z]{2}(-gov)?-[a-z]+-\d$", region):
            raise ApiError("validation", "Enter an AWS region, like us-east-1.", "region")
        if not model:
            raise ApiError("validation", "Enter a Bedrock model ID or inference profile ARN.", "model")
        key = (bb.get("api_key") or "").strip()
        akid, secret = (bb.get("access_key_id") or "").strip(), (bb.get("secret_access_key") or "").strip()
        if key:
            auth, hint = "api_key", "••••" + key[-4:]
        elif akid or secret:
            if not akid:
                raise ApiError("validation", "Enter the access key ID.", "access_key_id")
            if not secret:
                raise ApiError("validation", "Enter the secret access key.", "secret_access_key")
            auth, hint = "access_keys", "••••" + akid[-4:]
        elif old.get("key_hint"):
            auth, hint = old["auth"], old["key_hint"]
        else:
            raise ApiError("validation", "Add a Bedrock API key, or an access key pair.", "api_key")
        u["ai"]["provider"], u["ai"]["model"] = "bedrock", ""
        u["ai"]["bedrock"] = {"region": region, "model": model, "auth": auth, "key_hint": hint}
    else:
        raise ApiError("validation", "Choose a model.", "provider")
    return json_resp(ai_json(u))


@route("POST", "/api/settings/ai/test")
def r_ai_test(ctx):
    u = ctx.user()
    ctx.json()
    a = u["ai"]
    if a["provider"] == "bedrock":
        b = a.get("bedrock") or {}
        if "fail" in b.get("model", ""):
            return json_resp({"ok": False, "message": f"Bedrock said this model isn't available in {b.get('region')}. Check the model ID and that access is granted in the Bedrock console."})
        return json_resp({"ok": True, "message": "Bedrock answered in 1.2 s"})
    return json_resp({"ok": True, "message": f"{'DeepSeek V4 Pro' if a['model'] == 'deepseek-v4-pro' else 'DeepSeek Flash'} answered in 0.8 s"})


CONNECTORS = [
    {"provider": "slack", "label": "Slack", "oauth": False, "token": True,
     "reads": "Messages that mention opportunities in channels and conversations you can already see, found with Slack search.",
     "scopes": ["search:read", "users:read", "team:read"]},
    {"provider": "microsoft", "label": "Microsoft 365", "oauth": True, "token": False,
     "reads": "Your recent Outlook emails and calendar invitations (subject, sender and a short preview).",
     "scopes": ["Mail.Read", "Calendars.Read", "User.Read", "offline_access"]},
    {"provider": "confluence", "label": "Confluence", "oauth": False, "token": True,
     "reads": "Confluence pages you can already see that mention opportunities, found with Confluence search.",
     "scopes": ["search:confluence", "read:page:confluence", "read:me"]},
]


def audit(u, provider, action, method, target, status, note, nbytes=0, ms=0):
    u["audit"].insert(0, {"provider": provider, "action": action, "method": method, "target": target, "status": status,
                          "bytes": nbytes, "ms": ms, "at": now(), "note": note})


def connect(u, provider, mode, enforcement, account):
    u["connections"][provider] = {"provider": provider, "account": account, "mode": mode, "enforcement": enforcement,
                                  "status": "ok", "connected_at": now(), "last_used_at": None, "error": None}
    return u["connections"][provider]


@route("GET", "/api/connectors")
def r_connectors(ctx):
    u = ctx.user()
    avail = [dict(c) for c in CONNECTORS]
    if STATE.get("ms_oauth") is False:
        avail[1]["oauth"] = False
    return json_resp({"available": avail, "connections": list(u["connections"].values())})


@route("POST", r"/api/connectors/(slack|microsoft|confluence)/start")
def r_conn_start(ctx, provider):
    u = ctx.user()
    ctx.json()
    if provider != "microsoft" or STATE.get("ms_oauth") is False:
        raise ApiError("conflict", "This connector isn't set up for sign-in on this server.")
    state = secrets.token_urlsafe(12)
    STATE.setdefault("oauth_states", {})[state] = (u["id"], provider)
    return json_resp({"url": f"/api/connectors/{provider}/callback?code=mock-code&state={state}"})


@route("GET", r"/api/connectors/(slack|microsoft|confluence)/callback")
def r_conn_callback(ctx, provider):
    u = ctx.user()
    rec = STATE.get("oauth_states", {}).pop(ctx.query.get("state", ""), None)
    if not rec or rec[0] != u["id"]:
        return 302, [("Location", "/?connector_error=" + "That sign-in link expired. Please try connecting again.".replace(" ", "%20"))], b""
    connect(u, provider, "oauth", "provider", {"name": u["business_name"], "user": u["email"], "url": "https://outlook.office.com"})
    audit(u, provider, "token", "POST", "https://login.microsoftonline.com/common/oauth2/v2.0/token", 200,
          "Signed in; granted scopes Mail.Read, Calendars.Read, User.Read, offline_access", 1830, 240)
    return 302, [("Location", f"/?connected={provider}")], b""


@route("POST", r"/api/connectors/(slack|microsoft|confluence)/token")
def r_conn_token(ctx, provider):
    u = ctx.user()
    b = ctx.json()
    token = (b.get("token") or "").strip()
    if provider == "slack":
        if not token:
            raise ApiError("validation", "Paste a Slack user token.", "token")
        if not token.startswith(("xoxp-", "xoxb-")):
            raise ApiError("validation", "That doesn't look like a Slack token. It starts with xoxp-.", "token")
        if "write" in token:
            audit(u, provider, "scope_rejected", "POST", "https://slack.com/api/auth.test", 200, "Rejected: token has chat:write")
            raise ApiError("validation", "This token can post messages. Prospect only accepts read-only tokens (search:read).", "token")
        conn = connect(u, provider, "token", "provider", {"name": u["business_name"], "user": "ops", "url": "https://saltandember.slack.com"})
        audit(u, provider, "token", "POST", "https://slack.com/api/auth.test", 200, "Token verified: scopes search:read, users:read, team:read", 412, 180)
    elif provider == "confluence":
        site = (b.get("site") or "").strip()
        email = (b.get("email") or "").strip()
        if not site:
            raise ApiError("validation", "Enter your Confluence site, like acme.atlassian.net.", "site")
        if not EMAIL_RE.match(email):
            raise ApiError("validation", "Enter the email you sign in to Confluence with.", "email")
        if not token:
            raise ApiError("validation", "Paste an Atlassian API token.", "token")
        host = re.sub(r"^https?://", "", site).rstrip("/")
        conn = connect(u, provider, "token", "app", {"name": host.split(".")[0], "user": email, "url": "https://" + host})
        audit(u, provider, "read", "GET", f"https://{host}/wiki/rest/api/user/current", 200, "Token verified", 655, 210)
    else:
        raise ApiError("conflict", "Microsoft 365 connects by signing in, not with a token.")
    return json_resp({"connection": conn}, 201)


@route("DELETE", r"/api/connectors/(slack|microsoft|confluence)")
def r_conn_delete(ctx, provider):
    u = ctx.user()
    u["connections"].pop(provider, None)
    return empty_resp(204)


@route("GET", "/api/connectors/audit")
def r_conn_audit(ctx):
    u = ctx.user()
    limit = max(1, min(500, int(ctx.query.get("limit") or 100)))
    return json_resp({"events": u["audit"][:limit]})


@route("PUT", "/api/profile")
def r_profile(ctx):
    u = ctx.user()
    b = ctx.json()
    if not isinstance(b.get("profile"), dict):
        raise ApiError("validation", "Send a profile to save.", "profile")
    p = sanitize_profile(b["profile"])
    stored = u["profile"].get("research")
    if p["research"] is not None and (stored is None or p["research"] != stored):
        p["research"] = stored if stored and p["research"].get("researched_at") == stored.get("researched_at") else None
    if profile_is_empty(p):
        raise ApiError("validation", "Tell us at least what you do or what you're looking for.", "headline")
    changed = {k: v for k, v in p.items() if k != "research"} != {k: v for k, v in u["profile"].items() if k != "research"}
    u["profile"] = p
    run = active_run(u["id"])
    if u["onboarding"] in ("review", "researching"):
        u["onboarding"] = "ready"
        run = run or start_run(u, "discover", "signup")
    elif u["onboarding"] == "ready" and changed and not run:
        run = start_run(u, "discover", "profile")
    return json_resp({"profile": p, "user": user_json(u), "run": run_json(run) if run else None})


@route("GET", "/api/opportunities")
def r_opps(ctx):
    u = ctx.user()
    latest = latest_done_discover(u["id"])
    latest_id = latest["id"] if latest else None
    listed = [o for o in user_opps(u["id"]) if o["status"] in ("active", "saved")]
    counts = {"active": sum(o["status"] == "active" for o in listed),
              "saved": sum(o["status"] == "saved" for o in listed),
              "new": sum(o["first_seen_run"] == latest_id for o in listed)}
    if ctx.query.get("view") == "saved":
        listed = [o for o in listed if o["status"] == "saved"]
    listed.sort(key=lambda o: (o["first_seen_run"] != latest_id, -o["data"]["score"], -o["first_seen_at"]))
    return json_resp({"items": [item_json(o, latest_id) for o in listed], "counts": counts,
                      "last_refresh": u["last_run_at"], "next_refresh": u["next_run_at"]})


def latest_id_for(u):
    latest = latest_done_discover(u["id"])
    return latest["id"] if latest else None


@route("POST", r"/api/opportunities/(\d+)/save")
def r_save(ctx, oid):
    u = ctx.user()
    o = opp_for(u, oid)
    if o["status"] == "dismissed":
        raise ApiError("not_found", "That opportunity isn't available any more.")
    saved = ctx.json().get("saved", True)
    o["status"] = "saved" if saved is True else "active"
    return json_resp({"item": item_json(o, latest_id_for(u))})


@route("POST", r"/api/opportunities/(\d+)/vote")
def r_vote(ctx, oid):
    u = ctx.user()
    o = opp_for(u, oid)
    vote = ctx.json().get("vote")
    if vote not in (1, 0, -1) or isinstance(vote, bool):
        raise ApiError("validation", "A vote is 1, 0 or -1.", "vote")
    o["vote"] = vote
    return json_resp({"item": item_json(o, latest_id_for(u))})


@route("POST", r"/api/opportunities/(\d+)/dismiss")
def r_dismiss(ctx, oid):
    u = ctx.user()
    o = opp_for(u, oid)
    o["status"] = "dismissed"
    return json_resp({"ok": True})


@route("POST", r"/api/opportunities/(\d+)/restore")
def r_restore(ctx, oid):
    u = ctx.user()
    o = opp_for(u, oid)
    if o["status"] == "dismissed":
        o["status"] = "active"
    return json_resp({"item": item_json(o, latest_id_for(u))})


@route("POST", "/api/runs")
def r_runs(ctx):
    u = ctx.user()
    b = ctx.json()
    if b.get("kind", "discover") != "discover":
        raise ApiError("validation", "Only discovery runs can be started here.", "kind")
    run = active_run(u["id"])
    if run:
        return json_resp({"run": run_json(run)}, 200)
    if u["onboarding"] != "ready":
        raise ApiError("conflict", "Finish your profile first.")
    key = "manual:%d" % u["id"]
    if rate_check(key, 6, 86400):
        raise ApiError("rate_limited", "You've refreshed a lot today. Your next scheduled refresh will pick up anything new.", retry_after=3600)
    rate_hit(key)
    return json_resp({"run": run_json(start_run(u, "discover", "manual"))}, 202)


@route("GET", "/api/runs/current")
def r_current(ctx):
    u = ctx.user()
    run, last = active_run(u["id"]), last_finished_run(u["id"])
    return json_resp({"run": run_json(run) if run else None, "last_run": run_json(last) if last else None})


@route("GET", "/api/steering")
def r_notes(ctx):
    u = ctx.user()
    notes = sorted([n for n in STATE["notes"].values() if n["user_id"] == u["id"]], key=lambda n: (-n["created_at"], -n["id"]))
    return json_resp({"notes": [{k: n[k] for k in ("id", "text", "created_at")} for n in notes[:20]]})


@route("POST", "/api/steering")
def r_add_note(ctx):
    u = ctx.user()
    b = ctx.json()
    text = squash(b.get("text") if isinstance(b.get("text"), str) else "")
    if not text:
        raise ApiError("validation", "Write a short note first.", "text")
    if len(text) > 300:
        raise ApiError("validation", "Keep notes under 300 characters.", "text")
    nid = new_id()
    STATE["notes"][nid] = {"id": nid, "user_id": u["id"], "text": text, "created_at": now()}
    mine = sorted([n for n in STATE["notes"].values() if n["user_id"] == u["id"]], key=lambda n: -n["id"])
    for n in mine[20:]:
        STATE["notes"].pop(n["id"], None)
    run = None
    if b.get("refresh") is True and u["onboarding"] == "ready":
        run = active_run(u["id"]) or start_run(u, "discover", "steer")
    note = {k: STATE["notes"][nid][k] for k in ("id", "text", "created_at")}
    return json_resp({"note": note, "run": run_json(run) if run else None}, 201)


@route("DELETE", r"/api/steering/(\d+)")
def r_del_note(ctx, nid):
    u = ctx.user()
    n = STATE["notes"].get(int(nid))
    if not n or n["user_id"] != u["id"]:
        raise ApiError("not_found", "That note was already removed.")
    STATE["notes"].pop(int(nid))
    return empty_resp(204)


@route("PUT", "/api/settings")
def r_settings(ctx):
    u = ctx.user()
    b = ctx.json()
    if "frequency" in b:
        if b["frequency"] not in {i for i, _, _ in FREQUENCIES}:
            raise ApiError("validation", "Choose how often to refresh.", "frequency")
        u["frequency"] = b["frequency"]
        iv = interval(u["frequency"])
        u["next_run_at"] = ((u["last_run_at"] or now()) + iv) if iv else None
    if "email_digest" in b:
        if not isinstance(b["email_digest"], bool):
            raise ApiError("validation", "Email digest is on or off.", "email_digest")
        u["email_digest"] = b["email_digest"]
    return json_resp({"settings": settings_json(u)})


@route("GET", "/api/theme/image")
def r_theme_image(ctx):
    u = ctx.user()
    t = u["theme"]
    if not (t["photo"] and t["version"] > 0):
        raise ApiError("not_found", "There's no photo yet.")
    with open(PHOTO, "rb") as f:
        data = f.read()
    return 200, [("Content-Type", "image/jpeg"), ("Cache-Control", "private, max-age=31536000, immutable")], data


@route("POST", "/api/theme/refresh")
def r_theme_refresh(ctx):
    u = ctx.user()
    ctx.json()
    if u["onboarding"] != "ready":
        raise ApiError("conflict", "Finish your profile first.")
    run = active_run(u["id"])
    if run:
        return json_resp({"run": run_json(run)}, 202)
    key = "theme:%d" % u["id"]
    if rate_check(key, 5, 86400):
        raise ApiError("rate_limited", "That's enough new photos for today. Try again tomorrow.", retry_after=3600)
    rate_hit(key)
    return json_resp({"run": run_json(start_run(u, "theme", "manual"))}, 202)


@route("PUT", "/api/account")
def r_account(ctx):
    u = ctx.user()
    b = ctx.json()
    if "business_name" in b:
        name = squash(b.get("business_name") or "")
        if not name:
            raise ApiError("validation", "Enter your business name.", "business_name")
        u["business_name"] = name[:120]
    return json_resp({"user": user_json(u)})


@route("POST", "/api/account/password")
def r_password(ctx):
    u = ctx.user()
    b = ctx.json()
    if u["password"] != pw_hash(b.get("current_password") if isinstance(b.get("current_password"), str) else ""):
        raise ApiError("unauthorized", "Your current password isn't right.", "current_password")
    u["password"] = pw_hash(validate_password(b.get("new_password"), "new_password"))
    keep = ctx.session_token()
    for k in [k for k, v in STATE["sessions"].items() if v == u["id"] and k != keep]:
        STATE["sessions"].pop(k)
    return empty_resp(204)


@route("DELETE", "/api/account")
def r_delete_account(ctx):
    u = ctx.user()
    b = ctx.json()
    if u["password"] != pw_hash(b.get("password") if isinstance(b.get("password"), str) else ""):
        raise ApiError("unauthorized", "That password isn't right.", "password")
    delete_user(u["id"])
    ctx.set_cookie = session_cookie("", clear=True)
    return empty_resp(204)


def handle_api(ctx):
    if ctx.method in ("POST", "PUT", "DELETE"):
        ctype = (ctx.headers.get("Content-Type") or "").split(";")[0].strip().lower()
        if ctype != "application/json":
            raise ApiError("forbidden", "Requests must send JSON.")
        origin = ctx.headers.get("Origin")
        if origin and urlsplit(origin).netloc != (ctx.headers.get("Host") or ""):
            raise ApiError("forbidden", "That request came from another site.")
    allowed = False
    for method, pattern, fn in ROUTES:
        m = pattern.match(ctx.path)
        if m:
            allowed = True
            if method == ctx.method:
                return fn(ctx, *m.groups())
    if allowed:
        raise ApiError("method_not_allowed", "That method isn't allowed here.")
    raise ApiError("not_found", "There's nothing here.")


def handle_mock(ctx):
    if ctx.path == "/__mock/ping":
        return json_resp({"mock": True, "speed": SPEED})
    if ctx.path == "/__mock/next" and ctx.method == "POST":
        b = ctx.json()
        if "microsoft_oauth" in b:
            STATE["ms_oauth"] = bool(b["microsoft_oauth"])
        for kind in ("discover", "research", "theme"):
            if b.get(kind) in ("ok", "fail", "empty"):
                STATE["next_outcome"][kind] = b[kind]
        return json_resp({"next": STATE["next_outcome"]})
    if ctx.path == "/__mock/seed" and ctx.method == "POST":
        u, token = seed(ctx.json())
        ctx.set_cookie = session_cookie(token)
        return json_resp({"user": user_json(u)}, 201)
    if ctx.path == "/__mock/reset-link":
        link = STATE["reset_links"].get((ctx.query.get("email") or "").lower())
        return json_resp({"url": link}, 200 if link else 404)
    raise ApiError("not_found", "No such mock control.")


INDEX_CACHE = {}


def static(ctx):
    if ctx.method not in ("GET", "HEAD"):
        raise ApiError("method_not_allowed", "That method isn't allowed here.")
    path = ctx.path if ctx.path in STATIC else "/index.html"
    ctype = STATIC.get(path, "text/html; charset=utf-8")
    with open(os.path.join(WEB_DIR, path.lstrip("/")), "rb") as f:
        data = f.read()
    etag = '"' + hashlib.sha1(data).hexdigest()[:16] + '"'
    headers = [("Content-Type", ctype), ("ETag", etag), ("Cache-Control", "no-cache")]
    if path == "/index.html":
        headers.append(("Content-Security-Policy", CSP))
    if ctx.headers.get("If-None-Match") == etag:
        return 304, headers, b""
    return 200, headers, data


class Handler(BaseHTTPRequestHandler):
    server_version = "ProspectMock/0.1"
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        if self.server.verbose:
            sys.stderr.write("[mock] " + (fmt % args) + "\n")

    def do_GET(self):
        self.dispatch("GET")

    def do_HEAD(self):
        self.dispatch("HEAD")

    def do_POST(self):
        self.dispatch("POST")

    def do_PUT(self):
        self.dispatch("PUT")

    def do_DELETE(self):
        self.dispatch("DELETE")

    def dispatch(self, method):
        url = urlsplit(self.path)
        query = {k: v[0] for k, v in parse_qs(url.query).items()}
        length = int(self.headers.get("Content-Length") or 0)
        ctx = Ctx(method, url.path, query, self.headers, b"")
        try:
            if length > BODY_LIMIT:
                self.close_connection = True
                raise ApiError("payload_too_large", "That request is too large.")
            ctx.body = self.rfile.read(length) if length else b""
            with LOCK:
                if url.path.startswith("/api/"):
                    status, headers, body = handle_api(ctx)
                elif url.path.startswith("/__mock/"):
                    status, headers, body = handle_mock(ctx)
                elif url.path == "/healthz":
                    status, headers, body = json_resp({"ok": True, "version": VERSION})
                else:
                    status, headers, body = static(ctx)
        except ApiError as e:
            status, headers, body = e.response()
        except Exception:
            traceback.print_exc()
            status, headers, body = ApiError("server_error", "Something went wrong on our side.").response()
        delay = self.server.latency
        if delay and url.path.startswith("/api/"):
            time.sleep(delay)
        self.send_response(status)
        for k, v in headers:
            self.send_header(k, v)
        if ctx.set_cookie:
            self.send_header("Set-Cookie", ctx.set_cookie)
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "strict-origin-when-cross-origin")
        self.send_header("X-Frame-Options", "DENY")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if method != "HEAD" and status not in (204, 304):
            self.wfile.write(body)


def main():
    global SPEED
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=int(os.environ.get("MOCK_PORT", "9940")))
    ap.add_argument("--speed", type=float, default=float(os.environ.get("MOCK_SPEED", "1")),
                    help="run clock multiplier (5 = runs finish five times faster)")
    ap.add_argument("--latency", type=float, default=0.0, help="seconds of delay added to every API call")
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()
    SPEED = max(0.1, args.speed)
    srv = ThreadingHTTPServer((args.host, args.port), Handler)
    srv.daemon_threads = True
    srv.verbose = args.verbose
    srv.latency = args.latency
    print(f"[mock] Prospect mock API on http://{args.host}:{args.port} (speed x{SPEED:g})", file=sys.stderr, flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        srv.server_close()


if __name__ == "__main__":
    main()
