# Working on Prospect

## Setup

```bash
source ~/.cargo/env                  # Rust stable (rustup, installed under ~/.cargo)
cargo test --workspace               # every unit test, no network
cargo build --release                # target/release/prospect
python3 tests/mock/server.py --port 9911   # mock upstreams for local runs and tests
```

The real DeepSeek key lives in `/home/gabriel/Documents/opportunity/.env` (git-ignored; also
readable from any worktree by that absolute path). Load it into a shell with
`set -a; source /home/gabriel/Documents/opportunity/.env; set +a`.

Run the app against the mock (no cost):

```bash
DEEPSEEK_API_KEY=mock DEEPSEEK_BASE_URL=http://127.0.0.1:9911 \
OPENVERSE_URL=http://127.0.0.1:9911/openverse/v1 COMMONS_API_URL=http://127.0.0.1:9911/commons/w/api.php \
DATA_DIR=$(mktemp -d) WEB_DIR=web cargo run -p server
```

## The DeepSeek budget — read this

The account has about **US$2.28** in total. It must last through development *and* real use.

* Automated tests always use the mock (`DEEPSEEK_BASE_URL` → `tests/mock/server.py`). Never let
  a test, loop or script hit the real API by default.
* Live calls are deliberate, small and counted. Check the balance (free) before and after:
  `curl -s https://api.deepseek.com/user/balance -H "Authorization: Bearer $DEEPSEEK_API_KEY"`.
* Each agent has a live budget stated in its brief. Stop at it and report.
* Prices: Search/Fetch $0.75 per 1,000 successful calls; `deepseek-flash` $0.15/M input (cache
  miss), $0.003/M (hit), $0.60/M output — double during peak hours (01–04 and 06–10 UTC,
  weekdays). A whole discovery run is about $0.02–0.05.
* Keep everything on `deepseek-flash`. Never switch to `deepseek-v4-pro`.

## Conventions

* Rust 2024, `cargo fmt`, `cargo clippy --workspace --all-targets` clean.
* No new third-party crates without a strong reason; prefer the standard library. If one is
  truly needed, add it to the workspace `Cargo.toml` and say why in your report.
* Errors are values: no `unwrap()`/`expect()` on anything that can fail at runtime (tests are
  fine). Panics are caught per request and per run, but must not be relied upon.
* User-facing text (errors, progress messages, emails, UI) is plain, warm, specific English. No
  exclamation-mark hype, no "AI-powered", no emoji in product copy.
* Keep `docs/API.md` and `docs/ARCHITECTURE.md` true: if you must change a contract, update the
  doc in the same commit and say so in your report.
* Commit your work on your own branch with clear messages. Don't touch files another component
  owns (see below) — if you need something from another component, note it in your report.

## Ownership

| Path | Owner |
|---|---|
| `crates/model`, `crates/deepseek`, `docs/*`, `tests/mock/server.py`, `tests/mock/mocklib.py`, workspace `Cargo.toml` | lead |
| `crates/engine`, `tests/mock/deepseek_mock.py` | engine builder |
| `crates/server`, `tests/server/` | platform builder |
| `web/`, `tests/e2e/`, `web/dev/` | experience builder |
| `crates/imagery`, `crates/mailer`, `tests/mock/media_mock.py`, `tests/mock/mailgun_mock.py` | imagery & mail builder |
