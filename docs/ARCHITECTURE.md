# Prospect — architecture

Prospect is business intelligence for a business. One account is one business: it signs up with
its name and website, research builds a sourced profile and intelligence dossier
(`model::Profile`, `model::Intel`), and discovery finds opportunities across eight focus areas
from that truth, on a schedule. There are no individual users or roles.

**Business edition at a glance** (where this document predates it, the business edition wins;
the HTTP contract is the "BUSINESS EDITION" section of [API.md](API.md)):

* **Onboarding:** sign-up starts a `research` run (`engine::research_business`: the site via the
  Fetch API, news, competitors, market and regulation via the Search API) → review → confirm →
  the first `discover` run.
* **Intel:** every dossier item carries a `Source`; `Intel::sanitized()` drops anything unsourced;
  the web client shows a tab only for non-empty sections.
* **Discovery:** plans searches by business model and focus area, reviews candidates, verifies
  top picks by reading their pages, and runs one bounded second wave when results are thin.
  Connector items become internal opportunities and suppress what the team already knows.
* **AI model:** per account, DeepSeek Flash (default), DeepSeek V4 Pro, or Claude on Amazon
  Bedrock (`crates/bedrock`, Converse API, API key or SigV4) through `deepseek::ChatBackend`; web
  search always uses DeepSeek's Search API. Secrets are sealed with `crates/vault`
  (ChaCha20-Poly1305).
* **Connectors:** `crates/connectors` — Slack, Microsoft 365 and Confluence, hard read-only in four
  layers (read scopes; verified grants; a GET-only allow-listed gateway; no AI tools), audited.
* **Tables added:** `intel_json` and `ai_json` on accounts; `oauth_states`, `connections`,
  `audit_events`.

## Principles

1. **Ultra-lightweight.** One Rust binary, no async runtime, no framework, SQLite in-process,
   the web client is three hand-written files embedded in the binary. Few dependencies (see the
   workspace `Cargo.toml`); add one only when the standard library genuinely can't do the job.
2. **Autonomous.** After onboarding, nothing needs a human: the scheduler refreshes, prunes and
   emails on its own; failures retry or degrade gracefully and never take the server down.
3. **Works for anyone.** Nothing is specific to a country, industry or kind of person. The
   profile is high-level; the engine adapts search to the person.
4. **Real and current.** Every opportunity links to a real page found by search. Nothing past its
   deadline is shown. Top picks are verified by reading their page.
5. **Pure functionality, beautiful design.** No chatbots, no "AI magic" decoration, no clutter.
6. **Frugal with the API.** A discovery run costs roughly US$0.02–0.05. Tests use the mock.

## Layout

```
Cargo.toml              workspace; one place for every dependency
crates/model            shared types (Profile, Opportunity, ThemeHint, Progress, Frequency…),
                        taxonomy, date/URL/text helpers. serde only.
crates/deepseek         DeepSeek v1 client: chat (JSON, thinking, vision), Search API, Fetch API,
                        balance; retries; usage + cost accounting. ureq.
crates/engine           the intelligence: discover(), research(), theme_hint(). Prompts live here.
crates/imagery          background photo: Openverse + Wikimedia Commons candidates, vision pick,
                        download, sniffing, credit line.
crates/mailer           Mailgun (US→EU fallback) or local outbox; email templates.
crates/server           the binary `opportunity`: config, SQLite, auth, HTTP API, static web
                        client, run queue, workers, scheduler.
web/                    index.html, app.css, app.js (+ favicon.svg): the whole web client.
tests/mock              one mock server for every upstream (DeepSeek, Openverse, Commons, Mailgun).
tests/e2e               Playwright browser tests.
docs/                   this file, API.md (the HTTP contract), DEVELOPMENT.md (how to work here).
```

Dependency direction: `model` ← `deepseek` ← `engine`, `imagery`; `model` ← `mailer`;
`server` → everything. Nothing depends on `server`.

## Runtime

One process:

* **HTTP**: `tiny_http` server; `HTTP_THREADS` worker threads (default 8) each loop on
  `recv()`. Every request is wrapped in `catch_unwind` so a panic answers 500 and the thread lives
  on. Request bodies are capped (64 KiB). Static files are embedded with `include_str!` (or read
  from `WEB_DIR` on every request in development, for live editing).
* **Run workers**: `MAX_CONCURRENT_RUNS` threads (default 2) claim queued runs from SQLite and
  execute them (discovery, research, theme). A run never blocks HTTP.
* **Scheduler**: one thread, wakes every 30 s, enqueues discovery for users whose `next_run_at`
  has passed, sweeps expired sessions and reset tokens.
* **SQLite** (rusqlite, bundled): `DATA_DIR/opportunity.db`, WAL mode, `busy_timeout` 5 s,
  `foreign_keys=ON`. One connection per thread (thread-local), opened lazily.
* **Shutdown**: on SIGINT/SIGTERM (or stdin EOF is not used), stop accepting, set the engine's
  `cancel` flag, give runs a few seconds, mark unfinished runs `queued` again so they resume on
  the next start.

## Data model (SQLite)

```sql
users(
  id INTEGER PRIMARY KEY,
  email TEXT NOT NULL UNIQUE COLLATE NOCASE,
  password_hash TEXT NOT NULL,              -- argon2id PHC string
  first_name TEXT NOT NULL, last_name TEXT NOT NULL,
  created_at INTEGER NOT NULL,              -- unix seconds (all times are)
  onboarding TEXT NOT NULL DEFAULT 'choose',-- choose | researching | review | ready
  profile_json TEXT NOT NULL DEFAULT '{}',  -- model::Profile
  theme_json TEXT,                          -- {label, accent, image: model::ThemeImage|null, file, version, built_for}
  frequency TEXT NOT NULL DEFAULT 'weekly', -- model::Frequency
  email_digest INTEGER NOT NULL DEFAULT 1,
  next_run_at INTEGER, last_run_at INTEGER,
  last_ip TEXT                              -- last public client IP (search localisation)
)
sessions(token_hash TEXT PRIMARY KEY, user_id → users ON DELETE CASCADE,
         created_at, expires_at, last_seen_at)          -- sha256 of a 32-byte random token
password_resets(token_hash TEXT PRIMARY KEY, user_id → users ON DELETE CASCADE,
         expires_at, used INTEGER NOT NULL DEFAULT 0)
runs(
  id INTEGER PRIMARY KEY, user_id → users ON DELETE CASCADE,
  kind TEXT NOT NULL,        -- discover | research | theme
  status TEXT NOT NULL,      -- queued | running | done | failed
  trigger TEXT NOT NULL,     -- signup | manual | schedule | profile | steer | retry
  attempts INTEGER NOT NULL DEFAULT 0,
  created_at, started_at, finished_at,
  progress_json TEXT,        -- latest model::Progress
  log_json TEXT,             -- last ~16 Progress entries (the live feed)
  stats_json TEXT,           -- engine stats + deepseek::Usage summary + merge counts + cost_usd
  input_json TEXT,           -- research hints etc.
  error TEXT                 -- a human message when failed
)
opportunities(
  id INTEGER PRIMARY KEY, user_id → users ON DELETE CASCADE,
  url_key TEXT NOT NULL,     -- model::links::normalize_url(url)
  data_json TEXT NOT NULL,   -- model::Opportunity as last returned by the engine
  score INTEGER NOT NULL, category TEXT NOT NULL, deadline TEXT,
  status TEXT NOT NULL DEFAULT 'active', -- active | saved | dismissed | expired
  vote INTEGER NOT NULL DEFAULT 0,       -- -1 | 0 | 1
  first_seen_run INTEGER, last_seen_run INTEGER,
  first_seen_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  UNIQUE(user_id, url_key)
)
steering(id INTEGER PRIMARY KEY, user_id → users ON DELETE CASCADE, text TEXT NOT NULL, created_at)
emails(id INTEGER PRIMARY KEY, user_id, kind, to_addr, subject, delivery, error, created_at)
meta(key TEXT PRIMARY KEY, value TEXT)  -- schema version etc.
```

Migrations are idempotent `CREATE TABLE IF NOT EXISTS` plus a `meta.schema_version` step list.

## Lifecycle

1. **Sign up** (first name, last name, email, password) → session cookie, `onboarding = choose`,
   a welcome email. Or **sign in**.
2. **Choose** how to build the opportunity profile:
   * **Fill it in** — a short form (headline, stage, location, reach, what you're seeking,
     interests, skills, goals, things to avoid). Saving it sets `onboarding = ready`.
   * **Auto-onboard** — optional hints (city, company, a link). A `research` run searches the
     public web for the person and drafts the profile; `onboarding = researching` → `review`.
     The person sees the drafted form with a note of what was found and its sources, edits
     anything, and confirms → `ready`. If research fails, back to `choose` with the reason.
3. **Ready** → a `discover` run is queued immediately (trigger `signup`); the theme is built in
   parallel inside that run. The page shows a live loading animation fed by run progress, the
   background photo as soon as it's chosen, then the opportunities.
4. **Ongoing**: the scheduler re-runs discovery at the chosen frequency (default weekly; daily,
   fortnightly, monthly or off). New finds are marked new and float to the top; stale ones are
   pruned. If the digest is on and there are new finds, an email goes out.
5. **Steering**: save/dismiss/thumbs on cards and short free-text notes ("more remote roles")
   feed the next run. Editing the profile queues a refresh (and a new theme if its subject
   changed).

## Pipelines (crates/engine)

All model calls use `deepseek-flash` through `crates/deepseek`, JSON mode, `X-Opp-Stage` set to
the step name. The engine reports progress (`ctx.report`) at every step with short, specific,
human messages — they are the loading screen's live feed.

### Discovery — `discover(ctx, input) -> DiscoverOutput`

1. **Plan** (`stage: plan`, thinking off or low): from the profile, steering and today's date,
   write `limits.max_queries` diverse Search API queries spanning the person's `seeking` types
   (all types sensible for them if empty) and interests, local (their city/country, per `reach`)
   and global/remote, in their languages. Use operators where they help: `after:` (recent — e.g.
   the last ~120 days), `lang:`, `-site:` to avoid noise. Each query has the category it targets.
   Steering notes and liked/disliked titles shape the plan. Never put the person's name, email or
   other identifiers into discovery queries.
2. **Search** (`/v1/tools/search`, ≤ 8 in parallel, `count = results_per_query`, no content —
   the 600–800 char `summary` is enough): failed queries are skipped, not fatal, unless every
   query fails or the error is an account problem (401/402).
3. **Shortlist** (no model): dedupe by `normalize_url`, drop dismissed keys, drop obvious noise
   (social media posts, generic Q&A, shopping), prefer higher `rerank_score`/`authority` and
   recent `date`, keep at most `max_candidates`.
4. **Evaluate** (`stage: evaluate`, thinking low/high): the model reads the numbered candidates
   (title, site, date, summary, url) with the profile, steering and today's date, and returns the
   genuine, currently open, actionable opportunities that fit — referenced **by candidate number**
   so every URL is one search actually returned. For each: title, organization, category, summary,
   why (to the person), action, location, deadline (YYYY-MM-DD if stated), value, score 0–100,
   and whether its status is certain. It also names known active opportunities that no longer fit
   (`stale`). Reject: expired, generic news/listicles with no specific opportunity, irrelevant
   geography (unless remote/global fits), duplicates of known items, anything sketchy (fees to
   apply, MLM, scams). An aggregator page may be kept only when it is a curated, current list
   squarely on target.
5. **Verify** (`/v1/tools/fetch` + `stage: verify`): read the pages of the top `verify_top` picks
   in parallel (truncate each to ~8–10k chars) and confirm each is real, open as of today and as
   described; fix deadline/value/details from the page; drop those that fail. Pages that can't be
   fetched keep their unverified pick (verified = false).
6. **Rank and return**: final score, sanitized via `Opportunity::sanitized(today)`, unique by key,
   at most `max_results`, best first.

Typical cost: ~14 searches ($0.0105) + ~8 fetches ($0.006) + ~40–70k tokens ($0.01–0.03).

### Research (auto-onboarding) — `research(ctx, input) -> Profile`

1. Queries built from the name (quoted), the email domain when it isn't a free mail provider
   (gmail, outlook, yahoo, icloud, proton…), the hints, and links in the hints (which may also be
   read with Fetch).
2. Search (Search API), then one `stage: research` call: decide which results are about *this*
   person (name + email domain + hints must agree; never merge different people), and draft the
   profile — headline, stage, organization, location, interests, skills, likely `seeking` types,
   goals — with `research.confidence`, a short `research.summary` addressed to the person ("We
   found you at HeyHi in Cape Town, working on…"), and the `sources` used.
3. When nothing reliable is found, return a sparse profile from the hints alone with low
   confidence and a summary that says so — never invent facts.

### Theme — `theme_hint(ctx, profile) -> ThemeHint` + `imagery::find_background`

* `theme_hint` (`stage: theme`, thinking off): the subject of the page in 1–3 words, an accent
  colour (#rrggbb, readable on white, suits the subject), 2–4 photo searches that would make a
  stunning, atmospheric, landscape background (no text), and a one-line mood. Never fails: a
  deterministic fallback from the profile.
* `imagery::find_background(cfg, ai, hint)`: query Openverse (`aspect_ratio=wide`, `size=large`,
  `mature=false`, prefer photo sources like stocksnap/rawpixel/wikimedia/flickr) and Commons
  (`filetype:bitmap filew:>1800`, quality images first); keep landscape candidates ≥ 1600 px wide;
  download up to ~8 small versions (Commons: a `thumburl` at a standard width such as 960 — Commons
  only serves standard widths; Openverse: `thumbnail`/`url`); validate by sniffing the bytes;
  `stage: curate` vision call with the images inline (base64, detail low) asks for the best index
  or none (penalise text, watermarks, logos, people's faces as the subject, disasters, gore,
  clutter, off-topic, low quality); download the winner at ~1920 px; return bytes + `ThemeImage`
  with a proper credit line ("Photo: <creator> · <license> · via Openverse/Wikimedia Commons").
* The server stores the bytes as `DATA_DIR/themes/<user>-<version>.<ext>` and serves them at
  `GET /api/theme/image`. Without a photo the page uses a gradient built from the accent.

## Applying a discovery (server)

For each returned opportunity, upsert by `(user_id, url_key)`:

* new → insert `active`, `first_seen_run = last_seen_run = run`; it is **new** (shown with a
  "New" badge, sorted first) while `first_seen_run` is the user's latest completed discover run;
* existing `active`/`saved` → refresh `data_json`, score, deadline, `last_seen_run`;
* existing `dismissed`/`expired` with a dismissed key → never resurrected (expired may come back
  as active if the engine finds it open again with a future deadline).

Then prune (status → `expired`), never touching `saved`:

* deadline before today;
* keys the engine listed in `stale`;
* not seen in the last 3 discover runs and first seen more than 21 days ago;
* beyond the 40 best active (lowest score first).

The run's stats record `new`, `refreshed`, `expired` and the engine/usage numbers.

## Runs, scheduler and budgets

* One active (queued or running) run per user. Asking for another returns the active one.
* Workers claim the oldest queued run atomically (`UPDATE … WHERE id = (SELECT … LIMIT 1)
  AND status = 'queued' RETURNING`). Progress is written to the run row as it happens (throttled
  to at most every ~300 ms) so the web client can poll it.
* On start-up, `running` runs are put back to `queued` (attempts + 1); after 3 attempts → failed.
* Discovery for `ready` users with `frequency != off` and `next_run_at <= now`; after scheduling,
  `next_run_at = now + interval`. Changing the frequency recomputes `next_run_at` from the last run.
* **Daily spend guard**: if the sum of `cost_usd` over the last 24 h reaches
  `DAILY_SPEND_LIMIT_USD` (default 1.00), new runs wait (scheduled ones are pushed back an hour,
  manual ones fail fast with a clear message). Manual refreshes are limited per user per day
  (`MANUAL_RUNS_PER_DAY`, default 6); research runs to 3 per user per day.
* Account-level DeepSeek errors (401, 402) fail the run with a plain message ("The AI service
  needs attention…"); the person's existing opportunities are untouched.
* Without `DEEPSEEK_API_KEY` the app runs, but runs fail with "AI isn't configured", and
  `/api/meta` reports `ai_ready: false`.

## Email

* **Welcome** after sign-up. **Digest** after a discovery run with new finds (scheduled or
  manual, if `email_digest` is on): the new opportunities, best first, styled with the person's
  accent and theme label, with a link back. **Password reset** with a one-hour link.
* Mailgun when `MAILGUN_API_KEY`, `MAILGUN_DOMAIN` and `MAILGUN_SENDER_EMAIL` are set; otherwise
  written to `DATA_DIR/outbox/` (`.eml` + `.html`). Every send is logged in `emails`. A failed
  email never fails a run.

## Security

* Passwords: argon2id (≈19 MiB, t=2, p=1), PHC strings; 8–200 chars.
* Sessions: 32 random bytes (getrandom) in an `opp_session` cookie; only the SHA-256 is stored.
  `HttpOnly; SameSite=Lax; Path=/; Max-Age=30 days`, plus `Secure` when `PUBLIC_URL` is https.
  Sliding expiry. Logout and password change revoke sessions.
* CSRF: every state-changing `/api` request must be `Content-Type: application/json`, and when an
  `Origin` header is present it must match the request's host or `PUBLIC_URL`. Otherwise 403.
* Rate limits (in memory, per IP and per email): sign-in, sign-up, forgot-password.
* Generic auth errors (no account enumeration); constant-time token comparison via hashing.
* Headers on every response: `X-Content-Type-Options: nosniff`, `Referrer-Policy:
  strict-origin-when-cross-origin`, `X-Frame-Options: DENY`, and on HTML a strict CSP:
  `default-src 'self'; img-src 'self' data: https:; style-src 'self'; script-src 'self';
  connect-src 'self'; font-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'`.
* The web client never puts untrusted text into `innerHTML`: text via `textContent`, links only
  `http(s)` with `rel="noopener noreferrer"`.
* Outbound fetches: only DeepSeek, the image sources and Mailgun; image downloads are size-capped
  and content-sniffed; nothing user-supplied is fetched by the server except through DeepSeek's
  Fetch API.

## Configuration (environment, or a `.env` file in the working directory)

| Variable | Default | Meaning |
|---|---|---|
| `ADDR` | `127.0.0.1:8080` | listen address (`0.0.0.0:8080` behind a proxy / in Docker) |
| `PUBLIC_URL` | `http://localhost:8080` | the public origin, for links and cookie `Secure` |
| `DATA_DIR` | `./data` | SQLite database, theme images, outbox |
| `DEEPSEEK_API_KEY` | — | required for the AI features |
| `DEEPSEEK_BASE_URL` | `https://api.deepseek.com` | tests point it at the mock |
| `DEEPSEEK_MODEL` | `deepseek-flash` | keep everything on Flash |
| `MAILGUN_API_KEY`, `MAILGUN_DOMAIN`, `MAILGUN_SENDER_EMAIL` | — | real email; outbox otherwise |
| `MAILGUN_BASE_URL` | US then EU | tests point it at the mock |
| `OPENVERSE_URL`, `COMMONS_API_URL` | public APIs | tests point them at the mock |
| `HTTP_THREADS` | `8` | HTTP worker threads |
| `MAX_CONCURRENT_RUNS` | `2` | run worker threads |
| `DAILY_SPEND_LIMIT_USD` | `1.00` | spend guard across all users |
| `MANUAL_RUNS_PER_DAY` | `6` | per-user manual refresh cap |
| `DEFAULT_FREQUENCY` | `weekly` | for new accounts |
| `TRUST_PROXY` | `false` | use `X-Forwarded-For` / `X-Forwarded-Proto` |
| `WEB_DIR` | — | serve the web client from disk (development) |
| `LOG` | `info` | `debug` adds per-call AI logs |

## Testing

* Unit tests in every crate (`cargo test --workspace`), no network.
* `tests/mock/server.py`: one deterministic mock for DeepSeek (stage-aware chat replies, search,
  fetch, balance), Openverse, Commons, image bytes and Mailgun, plus failure injection.
* Server integration tests start the real binary against the mock with a temp `DATA_DIR`.
* `tests/e2e`: Playwright (pinned `playwright@1.64.1`, Chromium already cached on this machine)
  drives sign-up → onboarding → dashboard on desktop and mobile widths, with screenshots.
* Live checks against the real DeepSeek API are manual, few, and budgeted (see DEVELOPMENT.md).
