# Prospect

**AI business intelligence that prospects your market and brings back the nuggets: the tenders,
partners, funding, rule changes and openings worth acting on — each one sourced, each one explained.**

---

## The problem

### Who it's for

**Owner-managers and business-development leads at small and mid-sized B2B companies (roughly
10–200 people) that have no dedicated bid, partnerships or market-intelligence team.** In South
Africa alone there were about **710,000 small formal businesses** in 2022
([TIPS, *The State of Small Business in South Africa*](https://www.tips.org.za/images/REB_The_State_of_Small_Business_in_South_Africa_2024_.pdf)),
out of an estimated **2.67 million SMMEs**, about a third of them formal
([Absa Small Business Growth Index](https://www.absa.co.za/business/absa-small-business-growth-index-2025/)).

### The pain

In these companies, finding growth opportunities is nobody's full-time job. A tender is spotted
because someone happened to open the right portal that week; a partner programme because a
contact mentioned it; a grant or a new regulation after the deadline that mattered. The searching
is fragmented across tender portals, newsletters, LinkedIn, industry sites and inboxes, so the
opportunities that fit the business best are routinely **missed, found late, or found by a
competitor first** — while the team already knows about, and is working on, a few of the obvious
ones.

The scope is deliberately narrow: **find the external opportunities a specific business is missing,
and explain why each one fits it.** Not a CRM, not a contact database, not a dashboard of internal
metrics.

### Evidence of need

- **The sources are fragmented by design.** Public procurement was about **15% of South Africa's
  GDP** in FY21/22, and about **76%** of it is spent by provinces, municipalities and public
  entities rather than national government
  ([IMF, *Public Procurement in South Africa*, 2023](https://www.imf.org/-/media/files/publications/selected-issues-papers/2023/english/sipea2023041.pdf.md)).
  National Treasury's own eTender portal notes its data is not a complete record because sharing is
  voluntary for some entities ([Open Contracting Data Registry](https://data.open-contracting.org/en/publication/143)).
  A small team cannot watch all of it.
- **Small businesses are under pressure to find growth.** 52.8% of South African SMMEs were in
  contraction, difficulty or at risk of closure in 2025
  ([Absa Small Business Growth Index](https://www.absa.co.za/business/absa-small-business-growth-index-2025/)).
- **What we observed building and testing Prospect on real businesses** (live runs, public
  information only):
  - **Personio** (HR software, B2B): research produced a sourced dossier — 10 offerings, 5 customer
    segments, 7 competitors, 6 market trends, 2 applicable regulations (the EU AI Act for HR uses,
    German employee-data rules), 5 news items, 82% confidence — and discovery returned
    **14 open opportunities**, including a **€660k e-HRM tender** and exhibitor places at
    **Zukunft Personal Europe 2027**, with most confirmed open by reading the page itself.
  - **SweepSouth** (home services, South Africa, consumer-facing): research was strong (five real
    local competitors; the R30.23/hour minimum wage; a draft wage law), but the first discovery
    returned only **1** opportunity.
  - **What we learned:** a consumer-facing business has few formal tenders, so a tool that only
    watches tenders fails it. We changed the engine to plan its searches from the business model
    (partner and supplier programmes, expansion, awards, funds, skills programmes for consumer
    businesses; tenders and procurement for B2B) and to accept genuine informal openings. The same
    business then returned **7 open, relevant opportunities**, including a provincial home-based
    care tender and two industry partnerships — evidence that the need is broad, but only a
    business-specific approach meets it.
- **Next validation step:** pilot with ten owner-managers, measuring the success criteria below on
  their own businesses.

### Opportunity and why now

- **Existing alternatives each cover one slice:** tender-alert subscriptions (tenders only, by
  keyword, no fit judgement), Google Alerts (noisy, no structure), sales-intelligence tools (contact
  data, not opportunities, priced and built for larger sales teams), competitive-intelligence
  platforms (enterprise pricing, competitors only), consultants (expensive, periodic).
  None builds an understanding of *this* business first and then looks across every kind of
  opportunity for it.
- **The cost of the status quo** is time and missed revenue. As an illustrative assumption: if a
  business-development lead spends three hours a week scanning portals, newsletters and news, that
  is about **150 hours a year** — and still misses most of what doesn't reach their inbox.
- **Why now:** AI search and page-reading APIs make continuous, sourced research cheap. Measured in
  our live runs, researching a business costs about **US$0.015** and a full discovery run
  **US$0.02–0.04**, so a weekly refresh costs well under **US$1 per business per month** in AI
  usage — cheap enough to watch everything, all the time, for a business of any size.

### What success looks like

| Outcome | Target |
|---|---|
| Truth | 100% of dossier facts and opportunities link to their source (enforced by the data layer); ≥ 90% of facts confirmed correct by the owner at review |
| Relevance | ≥ 5 open, relevant opportunities on the first run (live: Personio 14, SweepSouth 7); ≥ 50% rated worth pursuing by the owner |
| New perspective | ≥ 30% of opportunities the team had not already seen |
| Speed | from sign-up to first opportunities in under 5 minutes (live: about 2–3 minutes) |
| Cost | under US$1 per business per month in AI usage at a weekly refresh |

**In scope:** researching the business from its name and website, a sourced intelligence dossier,
finding and explaining opportunities across sales, partnerships, funding, regulation, market
expansion, events and awards, talent and operations, scheduled refreshes, digests, read-only
connectors to internal tools.
**Out of scope:** acting on opportunities (outreach, bid writing), CRM and contact lists, legal
advice on regulation, full market-research reports, consumers (it serves businesses).

---

## The product

A business signs up with its name and website. Prospect researches it — its own site first, then
the public web — and builds a sourced intelligence dossier: what it sells, who it serves, its
competitors, its market, the regulation that touches it, and what's happening around it. Then it
keeps finding opportunities across every part of the business, explains why each one matters to
*this* business, and refreshes on a schedule. Connect Slack, Microsoft 365 or Confluence (strictly
read-only) and it also finds the opportunities in your own work, and stops repeating the ones your
team is already on.

One small Rust binary (about 6 MB, about 8 MB of memory at rest) with SQLite inside; no framework,
no async runtime, no build step for the web client.

### How it works

1. **Sign up as a business:** name, website, email, password.
2. **Prospecting** (about a minute, live on screen): Prospect reads the website with DeepSeek's
   Fetch API and searches the web with DeepSeek's Search API for news, competitors, market trends
   and regulation. A model (DeepSeek Flash by default, or your own Claude on Amazon Bedrock) turns
   only what those sources say into the dossier. **Every fact carries its source**; anything
   unsourced is dropped by the data layer itself; a section with nothing reliable is not shown.
   Customers are described as segments, never as customer lists.
3. **Review** what was found, adjust the profile, confirm.
4. **The nuggets:** opportunities arrive grouped by focus area, each with a source link, deadline,
   value, why it fits this business and the next step. The dashboard's tabs show the dossier:
   Overview, Products & services, Customers, Competitors (with a "where you sit" view), Market,
   Regulation, News.
5. **Autonomy:** it refreshes on a schedule (weekly by default; daily to monthly, or manual),
   marks new finds, prunes expired or stale ones, and emails a digest.

### Model selection

The model selector (header chip) switches the model that researches, judges and writes:

| Choice | Notes |
|---|---|
| DeepSeek Flash | default; fast and inexpensive |
| DeepSeek V4 Pro | deeper reasoning, about 4× the cost |
| Claude on Amazon Bedrock | your AWS account: region + model id or inference-profile ARN, with a Bedrock API key or IAM access keys (SigV4). Keys are encrypted at rest and never sent back to the browser. **Test connection** checks it. |

Web search always runs on DeepSeek's Search API.

### Connectors — hard read-only

Slack, Microsoft 365 (Outlook mail and calendar) and Confluence. Read-only is enforced in four
independent layers, so no single bug can turn a read into a write:

1. **The credential can't write:** only read scopes are requested.
2. **Granted scopes are verified** on connect, on every refresh and (Slack) on every call, against a
   read-only allow-list and a hard-coded write deny-list; a token that can write is refused.
3. **There is no code path that writes:** all data access goes through one gateway whose only
   method is HTTP GET, to allow-listed hosts and exact read endpoints, with redirects and
   method-override headers refused. The only other request is the OAuth token exchange.
4. **The AI never gets tools:** connectors are called by deterministic code; the model only sees text.

Every request (and every refusal) is audit-logged and visible in the app; content is fetched at run
time and not stored; tokens are encrypted at rest. Setup guides for administrators:
[Slack](docs/connectors/slack.md) · [Microsoft 365](docs/connectors/microsoft.md) ·
[Confluence](docs/connectors/confluence.md).

## Quick start

```bash
cp .env.example .env        # add DEEPSEEK_API_KEY
./run.sh                    # builds the release binary and starts it
# open http://localhost:8080 and create your business account
```

## Configuration

Environment variables (or `.env` in the working directory):

| Variable | Default | |
|---|---|---|
| `DEEPSEEK_API_KEY` | — | required for research and discovery |
| `DEEPSEEK_MODEL` | `deepseek-flash` | default model |
| `ADDR` | `127.0.0.1:8080` | `0.0.0.0:8080` behind a proxy or in Docker |
| `PUBLIC_URL` | `http://localhost:8080` | links in emails, OAuth callbacks, secure cookies when https |
| `DATA_DIR` | `./data` | database, theme photos, outbox, `secret.key` |
| `SECRETS_KEY` | generated | base64 32-byte key that encrypts stored credentials (else `DATA_DIR/secret.key`) |
| `MAILGUN_API_KEY`, `MAILGUN_DOMAIN`, `MAILGUN_SENDER_EMAIL` | — | real email; otherwise emails go to `DATA_DIR/outbox` |
| `SLACK_CLIENT_ID`, `SLACK_CLIENT_SECRET` | — | Slack OAuth app (a Slack user token can also be pasted) |
| `MS_CLIENT_ID`, `MS_CLIENT_SECRET`, `MS_TENANT` | —, —, `organizations` | Microsoft Entra app |
| `ATLASSIAN_CLIENT_ID`, `ATLASSIAN_CLIENT_SECRET` | — | Atlassian OAuth app (an API token can also be pasted) |
| `DAILY_SPEND_LIMIT_USD` | `1.00` | stops AI runs for the day past this DeepSeek spend |
| `DEFAULT_FREQUENCY` | `weekly` | refresh cadence for new accounts |

## Development

```bash
cargo test --workspace      # unit and integration tests; no network, no spend
scripts/e2e.sh              # release build + mock upstreams + real server + Playwright
python3 tests/mock/server.py --port 9911   # one mock for DeepSeek, Bedrock, Openverse, Commons, Mailgun, Slack, Microsoft, Atlassian
```

Docs: [architecture](docs/ARCHITECTURE.md) · [HTTP API](docs/API.md) · [development](docs/DEVELOPMENT.md).
A `Dockerfile` (distroless, non-root) is included.
