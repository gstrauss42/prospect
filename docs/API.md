# Prospect — HTTP API

The contract between `crates/server` and `web/`. JSON in, JSON out, UTF-8. Times are Unix
seconds (integers). Dates are `YYYY-MM-DD` strings. Field names are snake_case. Types named in
`code` are the serde forms of `crates/model` types — the Rust source is the reference for their
exact fields.

## BUSINESS EDITION — authoritative; overrides anything below that conflicts

Prospect is **business intelligence for a business**. One account = one business. There are no
individuals, no members, no roles: you sign in to the business account and everything is about the
business. Onboarding is the business name + website; research builds the profile and the sourced
intelligence dossier (`model::Intel`, the tabs); opportunities are found from that truth.

**Account** (replaces User; same JSON key `user` in responses for continuity):
```json
{ "id": 1, "email": "ops@acme.example", "business_name": "Acme Cleaning", "website": "https://acme.example",
  "created_at": 1795682592, "onboarding": "researching" }
```
`onboarding`: `researching` (research run active) → `review` (profile + intel drafted; confirm) →
`ready`. If research fails: `review` with whatever exists plus the run's `error` (the form lets them
fill in the profile by hand). There is no `choose` step any more.

`POST /api/auth/signup` `{ "business_name", "website", "email", "password", "hints"? }` → 201
`{ "user": Account }` + cookie, and a `research` run starts immediately (trigger `signup`).
`business_name` 1–120 chars; `website` must be http(s) (add `https://` if the scheme is missing);
`hints` optional (≤ 300 chars: country, what they do). No first/last name anywhere.

🔒 `GET /api/me` → `{ "user": Account, "profile": Profile, "intel": Intel, "settings": Settings,
"theme": Theme, "ai": AiSettings, "run": Run|null, "last_run": Run|null }`. `profile` is the business
profile (`model::Profile`: `organization` = business name, `website`, `industry`, `headline` = what it
does, `seeking` = focus areas…). `intel` is `model::Intel` — the web client shows a tab per non-empty
section (Overview, Products & services, Customers (segments), Competitors, Market, Regulation, News)
and never shows placeholders for empty ones.

🔒 `PUT /api/profile` as before (confirming in `review` → `ready` + a discover run).
🔒 `POST /api/intel/refresh` `{}` → 202 `{ "run": Run }` (kind `research`; re-researches the business
and replaces the intel; fills profile fields only where empty). 3 per day.

Prospect categories are the business focus areas in `/api/meta.opportunity_types` (sales, partners,
funding, regulation, market, visibility, talent, operations, other). Items may have `source` set to a
connector ("slack", "microsoft", "confluence") — internal opportunities, linked to the item in that tool.

### AI model (the model selector)
**AiSettings** (never contains a secret):
```json
{ "provider": "deepseek", "model": "deepseek-flash",
  "bedrock": { "region": "us-east-1", "model": "arn:aws:bedrock:…", "auth": "api_key", "key_hint": "••••7Q2c" } | null }
```
🔒 `GET /api/settings/ai` → AiSettings. 🔒 `PUT /api/settings/ai`
`{ "provider": "deepseek", "model": "deepseek-v4-pro" }` or
`{ "provider": "bedrock", "bedrock": { "region": "us-east-1", "model": "<model id or ARN>", "api_key": "…" } }`
(or `access_key_id` + `secret_access_key` (+ `session_token`) instead of `api_key`; omit secrets to keep
the stored ones) → 200 AiSettings. Secrets are sealed with the vault. `422` with `field` on bad input.
🔒 `POST /api/settings/ai/test` `{}` → 200 `{ "ok": true, "message": "Bedrock answered in 1.2 s" }` or
`{ "ok": false, "message": "…" }`. Runs use the account's model for every chat step; web search always
uses DeepSeek's Search API.

### Connectors (hard read-only)
🔒 `GET /api/connectors` →
```json
{ "available": [ { "provider": "slack", "label": "Slack", "oauth": false, "token": true, "reads": "…", "scopes": ["search:read"] } ],
  "connections": [ { "provider": "slack", "account": { "name": "Acme", "user": "Gabriel", "url": "https://acme.slack.com" },
      "mode": "token", "enforcement": "provider", "status": "ok", "connected_at": 1795682592, "last_used_at": null, "error": null } ] }
```
`available` comes from `connectors::Connectors::availability()`. `enforcement`: `provider` (token scopes
verified read-only) or `app` (only Prospect's own GET-only enforcement, e.g. a Confluence API token).
`status`: `ok`, `needs_reconnect`, `error`.
🔒 `POST /api/connectors/{provider}/start` `{}` → 200 `{ "url": "https://…" }` — the browser goes there
(OAuth). State + PKCE verifier are stored server-side for 10 minutes.
`GET /api/connectors/{provider}/callback?code=…&state=…` (no JSON; cookie session) → completes and
redirects to `/?connected={provider}` or `/?connector_error={message}`.
🔒 `POST /api/connectors/{provider}/token` `{ "token", "email"?, "site"? }` → 201 `{ "connection": … }`.
🔒 `DELETE /api/connectors/{provider}` → 204 (credentials deleted).
🔒 `GET /api/connectors/audit?limit=100` → `{ "events": [connectors::AuditEvent…] }` newest first.
Credentials are sealed with the vault, never returned. Discovery fetches recent items from every `ok`
connection (`connectors::FetchQuery::default()`) and passes them to the engine as `context`.

## Conventions

* **Auth**: a session cookie `opp_session` (HttpOnly). Endpoints marked 🔒 answer `401` without
  a valid session.
* **CSRF**: every `POST`/`PUT`/`DELETE` under `/api` must send `Content-Type: application/json`
  (send `{}` when there is nothing to say). If an `Origin` header is present it must match the
  host. Otherwise `403 forbidden`.
* **Body limit**: 64 KiB → `413 payload_too_large`.
* **Errors** always look like:

  ```json
  { "error": { "code": "validation", "message": "Enter a valid email address.", "field": "email" } }
  ```

  `message` is written for people and safe to show as-is. `field` is present when one input is to
  blame. Codes: `bad_request` 400, `unauthorized` 401, `forbidden` 403, `not_found` 404,
  `method_not_allowed` 405, `conflict` 409, `payload_too_large` 413, `unsupported_media_type` 415,
  `validation` 422, `rate_limited` 429 (with `Retry-After`), `server_error` 500,
  `ai_unavailable` 503.
* Unknown JSON fields in requests are ignored. Missing optional fields keep their current value.

## Objects

**User**
```json
{ "id": 1, "first_name": "Gabriel", "last_name": "Strauss", "email": "gabriel@example.com",
  "created_at": 1795682592, "onboarding": "choose" }
```
`onboarding`: `choose` (pick manual or auto) → `researching` (auto-onboarding running) →
`review` (draft ready to confirm) → `ready` (dashboard).

**Profile** — `model::Profile`:
```json
{ "headline": "Chef and caterer starting a food business", "stage": "founder",
  "organization": "", "location": "Cape Town, South Africa", "reach": "global",
  "seeking": ["funding", "competitions", "clients"], "interests": ["cooking", "street food"],
  "skills": ["menu design"], "goals": "Open a small restaurant within a year", "avoid": "",
  "links": [], "languages": [],
  "research": { "confidence": 0.8, "summary": "We found you at…", "sources": ["https://…"], "researched_at": 1795682592 } }
```
`research` is `null` for a profile filled in by hand.

**Settings**
```json
{ "frequency": "weekly", "email_digest": true, "next_run_at": 1796287392, "last_run_at": 1795682592 }
```

**Theme**
```json
{ "label": "Cooking", "accent": "#c2410c", "image_url": "/api/theme/image?v=3",
  "credit": "Photo: Jane Doe · CC0 · via Openverse", "source_url": "https://…", "status": "ready" }
```
`status`: `none` (not built yet), `building`, `ready`. `image_url`/`credit`/`source_url` are
`null` without a photo — use a gradient of `accent`.

**Run**
```json
{ "id": 7, "kind": "discover", "status": "running", "trigger": "signup",
  "created_at": 1795682592, "started_at": 1795682593, "finished_at": null,
  "progress": { "stage": "searching", "message": "Searching for culinary grants in Cape Town", "percent": 35 },
  "log": [ { "stage": "planning", "message": "Planning 14 searches", "percent": 10 } ],
  "error": null,
  "summary": null }
```
`kind`: `discover` | `research` | `theme`. `status`: `queued` | `running` | `done` | `failed`.
`log` is the last ≤16 progress steps, oldest first. `summary` (when done, discover only):
`{ "new": 5, "refreshed": 9, "expired": 2, "total": 18 }`. `error` is a human message when failed.

**Item** — an opportunity on the page: every `model::Prospect` field plus bookkeeping:
```json
{ "id": 42, "url": "https://…", "title": "…", "organization": "…", "category": "funding",
  "summary": "…", "why": "…", "action": "…", "location": "Cape Town", "deadline": "2027-01-15",
  "value": "R35 000", "score": 92, "published": "2026-11-02", "site_name": "SA Chefs",
  "site_icon": "https://…", "verified": true,
  "status": "active", "is_new": true, "vote": 0, "first_seen_at": 1795682592 }
```
`status`: `active` | `saved` (dismissed/expired items are never listed).

## Meta

`GET /api/meta` → 200
```json
{ "opportunity_types": [{ "id": "jobs", "label": "Jobs & roles", "hint": "…" }],
  "stages": [{ "id": "student", "label": "Student" }],
  "reach": [{ "id": "local", "label": "Near me" }],
  "frequencies": [{ "id": "weekly", "label": "Every week" }],
  "ai_ready": true, "email_live": false, "version": "0.1.0" }
```

`GET /healthz` → 200 `{ "ok": true, "version": "0.1.0" }` (503 if the database is unusable).

## Auth

`POST /api/auth/signup` `{ "first_name", "last_name", "email", "password" }`
→ 201 `{ "user": User }` + cookie. Names 1–80 chars after trimming; valid email ≤ 254; password
8–200 chars. `409 conflict` (field `email`) if taken. Sends the welcome email.

`POST /api/auth/login` `{ "email", "password" }` → 200 `{ "user": User }` + cookie;
`401 unauthorized` "Email or password is incorrect."; `429` when rate-limited.

`POST /api/auth/logout` → 204, cookie cleared (works without a session too).

`POST /api/auth/forgot` `{ "email" }` → 202 `{}` always (no enumeration). Emails a link
`PUBLIC_URL/reset?token=…` valid for 1 hour if the account exists.

`POST /api/auth/reset` `{ "token", "password" }` → 200 `{ "user": User }` + cookie (all other
sessions revoked); `400 bad_request` "This link has expired or was already used."

## Me

🔒 `GET /api/me` → 200
```json
{ "user": User, "profile": Profile, "settings": Settings, "theme": Theme,
  "run": Run | null, "last_run": Run | null }
```
`run` is the active (queued/running) run; `last_run` the most recent finished one.

## Onboarding and profile

🔒 `POST /api/onboarding/research` `{ "hints": "Cape Town · chef at … · https://…" }`
(hints optional, ≤ 500 chars) → 202 `{ "run": Run }`, `onboarding` → `researching`.
Allowed while `onboarding` is `choose` or `review` (else `409`). When the run finishes:
success → `onboarding = review` and the drafted profile is in `GET /api/me`; failure →
`onboarding = choose`, the run's `error` says why.

🔒 `PUT /api/profile` `{ "profile": Profile }` → 200
`{ "profile": Profile, "user": User, "run": Run | null }`.
The profile is sanitized (`Profile::sanitized`) and stored. `research` from the client is kept
only if it matches what the server stored (the client can't forge research). If `onboarding` was
`choose`/`review` it becomes `ready` and a discovery run starts (trigger `signup`). If already
`ready` and the profile changed, a discovery run starts (trigger `profile`) and the theme is
rebuilt within it when the subject changed. `422` if the profile is empty (no headline, interests,
skills, goals or seeking).

## Opportunities

🔒 `GET /api/opportunities` → 200
```json
{ "items": [Item], "counts": { "active": 18, "saved": 3, "new": 5 },
  "last_refresh": 1795682592, "next_refresh": 1796287392 }
```
Order: new first, then score, then most recently seen. Optional `?view=saved` lists only saved.

🔒 `POST /api/opportunities/{id}/save` `{ "saved": true }` → 200 `{ "item": Item }`
🔒 `POST /api/opportunities/{id}/vote` `{ "vote": 1 }` (1, 0 or -1) → 200 `{ "item": Item }`
🔒 `POST /api/opportunities/{id}/dismiss` `{}` → 200 `{ "ok": true }` — hidden for good; feeds
steering as "disliked".
🔒 `POST /api/opportunities/{id}/restore` `{}` → 200 `{ "item": Item }` — undo a dismiss.
Another user's id → `404`.

## Runs

🔒 `POST /api/runs` `{ "kind": "discover" }` → 202 `{ "run": Run }` (trigger `manual`). If a run
is already active → 200 `{ "run": <that run> }`. `429` past the daily manual limit; `503
ai_unavailable` without an API key. Only `discover` is accepted here.

🔒 `GET /api/runs/current` → 200 `{ "run": Run | null, "last_run": Run | null }` — poll every
~1.5 s while a run is active; stop when `run` is null.

## Steering

🔒 `GET /api/steering` → 200 `{ "notes": [ { "id": 3, "text": "More remote roles", "created_at": 1795682592 } ] }`
(newest first, at most 20 kept).

🔒 `POST /api/steering` `{ "text": "…", "refresh": true }` (1–300 chars) → 201
`{ "note": Note, "run": Run | null }` — with `refresh`, a discovery run starts (trigger `steer`).

🔒 `DELETE /api/steering/{id}` → 204.

## Settings

🔒 `PUT /api/settings` `{ "frequency": "daily", "email_digest": false }` (either optional) →
200 `{ "settings": Settings }`. `422` for an unknown frequency.

## Theme

🔒 `GET /api/theme/image` → the photo bytes with its `Content-Type`,
`Cache-Control: private, max-age=31536000, immutable` (the `v` query changes with each new photo).
`404` without one.

🔒 `POST /api/theme/refresh` `{}` → 202 `{ "run": Run }` (kind `theme`; 5 per day).

## Account

🔒 `PUT /api/account` `{ "first_name", "last_name" }` → 200 `{ "user": User }`.
🔒 `POST /api/account/password` `{ "current_password", "new_password" }` → 204 (other sessions
revoked); `401` if `current_password` is wrong.
🔒 `DELETE /api/account` `{ "password" }` → 204, everything deleted, cookie cleared.

## Static

`GET /`, `/reset`, and any other non-`/api` path that isn't a file → `index.html` (the client
routes). `/app.js`, `/app.css`, `/favicon.svg` → the files, with an `ETag` and
`Cache-Control: no-cache` (304 on `If-None-Match`).
