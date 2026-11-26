//! SQLite: one connection per thread (opened lazily), WAL, foreign keys, a 5 s busy timeout, and
//! idempotent migrations tracked in `meta.schema_version`. The schema is the data model in
//! docs/ARCHITECTURE.md for the business edition (one account = one business: `business_name`,
//! `website`, `intel_json`, `ai_json`; connector tables) plus indexes; one partial unique index
//! enforces "one active run per account" in the database itself. Secrets (Bedrock keys in
//! `ai_json`, connector credentials, OAuth verifiers) are stored sealed by the vault only.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior};

pub use rusqlite::Error;
pub type Result<T> = rusqlite::Result<T>;

static PATH: OnceLock<PathBuf> = OnceLock::new();

thread_local! {
    static CONN: RefCell<Option<Rc<Connection>>> = const { RefCell::new(None) };
}

/// Remember where the database lives, open it and bring the schema up to date.
pub fn init(path: PathBuf) -> Result<()> {
    let _ = PATH.set(path);
    migrate(&*conn()?)
}

/// This thread's connection, opened on first use.
pub fn conn() -> Result<Rc<Connection>> {
    CONN.with(|cell| {
        if let Some(c) = cell.borrow().as_ref() {
            return Ok(Rc::clone(c));
        }
        let c = Rc::new(open()?);
        *cell.borrow_mut() = Some(Rc::clone(&c));
        Ok(c)
    })
}

fn open() -> Result<Connection> {
    let path = PATH
        .get()
        .ok_or_else(|| Error::InvalidPath("the database path isn't set".into()))?;
    let c = Connection::open(path)?;
    c.busy_timeout(Duration::from_secs(5))?;
    let _mode: String = c.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    // NORMAL is durable enough with WAL (a power cut can lose the last transactions, never
    // corrupt), a small page cache per connection keeps memory low with many threads.
    c.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA cache_size=-1024; PRAGMA temp_store=MEMORY;")?;
    Ok(c)
}

/// A write transaction that takes the write lock up front (`BEGIN IMMEDIATE`), so concurrent
/// writers wait for each other (busy timeout) instead of failing with SQLITE_BUSY mid-way.
/// Rolls back when dropped without `commit()`, including on panic.
pub fn write_tx(c: &Connection) -> Result<Transaction<'_>> {
    Transaction::new_unchecked(c, TransactionBehavior::Immediate)
}

/// True for a UNIQUE/CHECK/foreign-key violation.
pub fn is_constraint(e: &Error) -> bool {
    e.sqlite_error_code() == Some(ErrorCode::ConstraintViolation)
}

/// True when the database answers (for /healthz).
pub fn healthy() -> bool {
    conn()
        .and_then(|c| c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)))
        .is_ok()
}

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS users(
  id INTEGER PRIMARY KEY,
  email TEXT NOT NULL UNIQUE COLLATE NOCASE,
  password_hash TEXT NOT NULL,
  business_name TEXT NOT NULL,
  website TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  onboarding TEXT NOT NULL DEFAULT 'researching',
  profile_json TEXT NOT NULL DEFAULT '{}',
  intel_json TEXT,
  theme_json TEXT,
  ai_json TEXT,
  frequency TEXT NOT NULL DEFAULT 'weekly',
  email_digest INTEGER NOT NULL DEFAULT 1,
  next_run_at INTEGER,
  last_run_at INTEGER,
  last_ip TEXT
);
CREATE INDEX IF NOT EXISTS users_due ON users(next_run_at) WHERE next_run_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS sessions(
  token_hash TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_user ON sessions(user_id);
CREATE INDEX IF NOT EXISTS sessions_expiry ON sessions(expires_at);

CREATE TABLE IF NOT EXISTS password_resets(
  token_hash TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  expires_at INTEGER NOT NULL,
  used INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS password_resets_user ON password_resets(user_id);

CREATE TABLE IF NOT EXISTS runs(
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  status TEXT NOT NULL,
  trigger TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  started_at INTEGER,
  finished_at INTEGER,
  progress_json TEXT,
  log_json TEXT,
  stats_json TEXT,
  input_json TEXT,
  error TEXT
);
CREATE INDEX IF NOT EXISTS runs_user ON runs(user_id, kind, status);
CREATE INDEX IF NOT EXISTS runs_status ON runs(status);
CREATE INDEX IF NOT EXISTS runs_finished ON runs(finished_at);
CREATE UNIQUE INDEX IF NOT EXISTS runs_one_active ON runs(user_id) WHERE status IN ('queued', 'running');

CREATE TABLE IF NOT EXISTS opportunities(
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  url_key TEXT NOT NULL,
  data_json TEXT NOT NULL,
  score INTEGER NOT NULL,
  category TEXT NOT NULL,
  deadline TEXT,
  status TEXT NOT NULL DEFAULT 'active',
  vote INTEGER NOT NULL DEFAULT 0,
  first_seen_run INTEGER,
  last_seen_run INTEGER,
  first_seen_at INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(user_id, url_key)
);
CREATE INDEX IF NOT EXISTS opportunities_user_status ON opportunities(user_id, status);

CREATE TABLE IF NOT EXISTS steering(
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  text TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS steering_user ON steering(user_id);

CREATE TABLE IF NOT EXISTS emails(
  id INTEGER PRIMARY KEY,
  user_id INTEGER REFERENCES users(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  to_addr TEXT NOT NULL,
  subject TEXT NOT NULL,
  delivery TEXT,
  error TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS emails_user ON emails(user_id);
CREATE INDEX IF NOT EXISTS emails_created ON emails(created_at);

CREATE TABLE IF NOT EXISTS oauth_states(
  state TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  provider TEXT NOT NULL,
  verifier TEXT NOT NULL,
  expires_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS connections(
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  provider TEXT NOT NULL,
  credential TEXT NOT NULL,
  mode TEXT NOT NULL,
  enforcement TEXT NOT NULL,
  account_json TEXT NOT NULL DEFAULT '{}',
  status TEXT NOT NULL DEFAULT 'ok',
  error TEXT,
  connected_at INTEGER NOT NULL,
  last_used_at INTEGER,
  UNIQUE(user_id, provider)
);

CREATE TABLE IF NOT EXISTS audit_events(
  id INTEGER PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  provider TEXT NOT NULL,
  action TEXT NOT NULL,
  method TEXT NOT NULL,
  target TEXT NOT NULL,
  status INTEGER NOT NULL,
  bytes INTEGER NOT NULL,
  ms INTEGER NOT NULL,
  at INTEGER NOT NULL,
  note TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS audit_events_user ON audit_events(user_id, id);
"#;

/// Each step runs once, in order; a database at version N skips steps 1..=N.
const MIGRATIONS: &[&str] = &[SCHEMA_V1];

pub fn schema_version(c: &Connection) -> Result<i64> {
    let v: Option<String> = c
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.and_then(|v| v.parse().ok()).unwrap_or(0))
}

fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT)")?;
    let current = schema_version(c)?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = i as i64 + 1;
        if version <= current {
            continue;
        }
        let tx = write_tx(c)?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [version.to_string()],
        )?;
        tx.commit()?;
    }
    Ok(())
}
