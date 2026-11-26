//! Background runs: a queue in the `runs` table, `MAX_CONCURRENT_RUNS` worker threads claiming
//! from it, and the three kinds of run:
//!
//! * `discover`: engine::discover with what the person already has and their steering, then the
//!   merge/prune rules of docs/ARCHITECTURE.md ("Applying a discovery") in one transaction with
//!   the run's completion; the page theme is built in parallel when missing or outdated; a digest
//!   email goes out when there are new finds.
//! * `research`: researches the business and drafts its profile and intel dossier (onboarding
//!   `researching` → `review`, also on failure, with the reason); later runs refresh the intel.
//! * `theme`: engine::theme_hint + imagery::find_background, bytes stored in `DATA_DIR/themes/`.
//!
//! Progress is kept in memory and written to the run row at most every ~300 ms by a small flusher
//! thread, so the web client can poll it. Panics are caught per run. At start-up, runs a crash left
//! `running` are queued again (attempts + 1) and fail for good after three attempts; a graceful
//! shutdown puts unfinished runs back without counting an attempt.

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use model::{KnownOpportunity, Opportunity, Profile, Progress, Steering};
use rusqlite::{Connection, OptionalExtension, Row};
use serde_json::{Value, json};

use crate::users::{DEFAULT_ACCENT, ThemeRecord, User, subject_key};
use crate::util::{self, lock};
use crate::{app, db, mail};

const MAX_ATTEMPTS: i64 = 3;
const LOG_LEN: usize = 16;
const FLUSH_EVERY: Duration = Duration::from_millis(300);
/// A run still `running` after this long is stopped and failed by the scheduler's watchdog.
pub const RUN_TIMEOUT_SECS: i64 = 45 * 60;
/// Opportunities kept active per person (the rest are pruned, lowest score first).
const MAX_ACTIVE: i64 = 40;
/// A scheduled discovery that failed for a passing reason is retried after this long, once.
const RETRY_AFTER_SECS: i64 = 2 * 3600;
/// Runs waiting for the daily spend guard are pushed back this long.
pub const BUDGET_DELAY_SECS: i64 = 3600;

pub const MSG_NO_AI: &str =
    "AI isn't configured on this server yet, so we can't search for you right now.";
const MSG_ACCOUNT: &str = "The AI service needs attention (its key or credit), so this update couldn't run. Your opportunities are safe, and we'll try again later.";
pub const MSG_BUDGET: &str = "We've reached today's spending limit for searches. We'll pick this up automatically within a day.";
const MSG_EMPTY: &str = "Your profile is too empty to search with. Add a headline, a few interests or what you're looking for.";
const MSG_AI_FAILED: &str = "We couldn't reach the search service just now. We'll try again soon.";
const MSG_RESEARCH_FAILED: &str = "We couldn't finish looking you up just now. You can try again, or fill in your profile yourself.";
const MSG_THEME_FAILED: &str =
    "We couldn't refresh the look of your page just now. Please try again later.";
const MSG_INTERNAL: &str =
    "Something went wrong on our side while working on this. We'll try again later.";
pub const MSG_TIMEOUT: &str =
    "This took far longer than it should, so we stopped it. We'll try again later.";
const MSG_ABANDONED: &str =
    "This was interrupted too many times, so we stopped trying. Please start it again.";

// ---------------------------------------------------------------------------------------------
// Rows and JSON
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct RunRow {
    pub id: i64,
    pub user_id: i64,
    pub kind: String,
    pub status: String,
    pub trigger: String,
    pub attempts: i64,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub progress_json: Option<String>,
    pub log_json: Option<String>,
    pub stats_json: Option<String>,
    pub input_json: Option<String>,
    pub error: Option<String>,
}

const COLS: &str = "id, user_id, kind, status, trigger, attempts, created_at, started_at, finished_at, progress_json, log_json, stats_json, input_json, error";

fn from_row(r: &Row) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        id: r.get(0)?,
        user_id: r.get(1)?,
        kind: r.get(2)?,
        status: r.get(3)?,
        trigger: r.get(4)?,
        attempts: r.get(5)?,
        created_at: r.get(6)?,
        started_at: r.get(7)?,
        finished_at: r.get(8)?,
        progress_json: r.get(9)?,
        log_json: r.get(10)?,
        stats_json: r.get(11)?,
        input_json: r.get(12)?,
        error: r.get(13)?,
    })
}

impl RunRow {
    /// The API's Run object.
    pub fn json(&self) -> Value {
        let parse = |s: &Option<String>| {
            s.as_deref()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
        };
        let summary = (self.kind == "discover" && self.status == "done")
            .then(|| parse(&self.stats_json))
            .flatten()
            .map(|s| json!({ "new": s["new"], "refreshed": s["refreshed"], "expired": s["expired"], "total": s["total"] }));
        json!({
            "id": self.id,
            "kind": self.kind,
            "status": self.status,
            "trigger": self.trigger,
            "created_at": self.created_at,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "progress": parse(&self.progress_json).unwrap_or(Value::Null),
            "log": parse(&self.log_json).filter(Value::is_array).unwrap_or_else(|| json!([])),
            "error": self.error,
            "summary": summary,
        })
    }
}

pub fn get(c: &Connection, id: i64) -> rusqlite::Result<Option<RunRow>> {
    c.prepare_cached(&format!("SELECT {COLS} FROM runs WHERE id = ?1"))?
        .query_row([id], from_row)
        .optional()
}

/// The user's queued or running run.
pub fn active(c: &Connection, user_id: i64) -> rusqlite::Result<Option<RunRow>> {
    c.prepare_cached(&format!("SELECT {COLS} FROM runs WHERE user_id = ?1 AND status IN ('queued', 'running') ORDER BY id DESC LIMIT 1"))?
        .query_row([user_id], from_row)
        .optional()
}

/// The user's most recently finished run, of any kind.
pub fn last_finished(c: &Connection, user_id: i64) -> rusqlite::Result<Option<RunRow>> {
    c.prepare_cached(&format!("SELECT {COLS} FROM runs WHERE user_id = ?1 AND status IN ('done', 'failed') ORDER BY id DESC LIMIT 1"))?
        .query_row([user_id], from_row)
        .optional()
}

fn progress_text(p: &Progress) -> String {
    serde_json::to_string(p).unwrap_or_else(|_| "null".into())
}

// ---------------------------------------------------------------------------------------------
// Queue
// ---------------------------------------------------------------------------------------------

pub enum Enqueued {
    New(RunRow),
    /// The user already had an active run (one at a time); this is it.
    Existing(RunRow),
}

impl Enqueued {
    pub fn into_run(self) -> RunRow {
        match self {
            Enqueued::New(r) | Enqueued::Existing(r) => r,
        }
    }
}

/// Queue a run, or return the user's active one. Wakes the workers unless called inside a
/// transaction (the caller wakes them after committing).
pub fn enqueue(
    c: &Connection,
    user_id: i64,
    kind: &str,
    trigger: &str,
    input: Option<&Value>,
) -> rusqlite::Result<Enqueued> {
    if let Some(r) = active(c, user_id)? {
        return Ok(Enqueued::Existing(r));
    }
    let progress = progress_text(&Progress::new("queued", "Waiting to start", 0));
    let inserted = c.execute(
        "INSERT INTO runs(user_id, kind, status, trigger, created_at, progress_json, log_json, input_json) VALUES(?1, ?2, 'queued', ?3, ?4, ?5, '[]', ?6)",
        (user_id, kind, trigger, util::now(), progress, input.map(Value::to_string)),
    );
    match inserted {
        Ok(_) => {
            let id = c.last_insert_rowid();
            if c.is_autocommit() {
                wake_workers();
            }
            get(c, id)?
                .map(Enqueued::New)
                .ok_or(rusqlite::Error::QueryReturnedNoRows)
        }
        // The partial unique index caught a concurrent enqueue for the same user.
        Err(e) if db::is_constraint(&e) => active(c, user_id)?.map(Enqueued::Existing).ok_or(e),
        Err(e) => Err(e),
    }
}

/// How many runs of `kind` (optionally only these triggers) the user started in the last 24 h,
/// and when the oldest of them leaves the window: Some(seconds) when `limit` is reached.
pub fn daily_limit(
    c: &Connection,
    user_id: i64,
    kind: &str,
    triggers: &[&str],
    limit: i64,
) -> rusqlite::Result<Option<i64>> {
    let now = util::now();
    let mut sql = String::from(
        "SELECT COUNT(*), MIN(created_at) FROM runs WHERE user_id = ?1 AND kind = ?2 AND created_at > ?3",
    );
    if !triggers.is_empty() {
        let list: Vec<String> = triggers
            .iter()
            .map(|t| format!("'{}'", t.replace('\'', "")))
            .collect();
        sql.push_str(&format!(" AND trigger IN ({})", list.join(", ")));
    }
    let (count, oldest): (i64, Option<i64>) =
        c.query_row(&sql, (user_id, kind, now - 86_400), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    Ok((count >= limit).then(|| oldest.map_or(3600, |o| o + 86_400 - now).max(1)))
}

/// Estimated AI spend of all runs in the last 24 hours, in USD.
pub fn spent_last_day(c: &Connection) -> rusqlite::Result<f64> {
    c.query_row(
        "SELECT COALESCE(SUM(json_extract(stats_json, '$.cost_usd')), 0.0) FROM runs WHERE stats_json IS NOT NULL AND COALESCE(finished_at, started_at, created_at) > ?1",
        [util::now() - 86_400],
        |r| r.get(0),
    )
}

/// True when the daily spend guard says new AI work must wait.
pub fn over_budget(c: &Connection) -> rusqlite::Result<bool> {
    Ok(spent_last_day(c)? >= app().cfg.daily_spend_limit_usd)
}

// ---------------------------------------------------------------------------------------------
// Registry: cancel flags, follow-ups, worker wake-ups
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct Registry {
    /// run id → (user id, cancel flag) for every run executing in this process.
    running: Mutex<HashMap<i64, (i64, Arc<AtomicBool>)>>,
    /// user id → trigger of a discovery to start as soon as their current run finishes.
    followups: Mutex<HashMap<i64, &'static str>>,
    generation: Mutex<u64>,
    wake: Condvar,
}

impl Registry {
    fn register(&self, run_id: i64, user_id: i64) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(app().stopping()));
        lock(&self.running).insert(run_id, (user_id, Arc::clone(&flag)));
        flag
    }

    fn unregister(&self, run_id: i64) {
        lock(&self.running).remove(&run_id);
    }

    /// Ask every executing run to stop at its next step (shutdown).
    pub fn cancel_all(&self) {
        for (_, flag) in lock(&self.running).values() {
            flag.store(true, Ordering::SeqCst);
        }
    }

    pub fn cancel_run(&self, run_id: i64) {
        if let Some((_, flag)) = lock(&self.running).get(&run_id) {
            flag.store(true, Ordering::SeqCst);
        }
    }

    pub fn cancel_user(&self, user_id: i64) {
        for (uid, flag) in lock(&self.running).values() {
            if *uid == user_id {
                flag.store(true, Ordering::SeqCst);
            }
        }
        lock(&self.followups).remove(&user_id);
    }

    /// Start a discovery for this user right after their current run (their profile or steering
    /// changed while it was already under way).
    pub fn want_followup(&self, user_id: i64, trigger: &'static str) {
        lock(&self.followups).insert(user_id, trigger);
    }

    fn take_followup(&self, user_id: i64) -> Option<&'static str> {
        lock(&self.followups).remove(&user_id)
    }
}

fn generation() -> u64 {
    *lock(&app().runs.generation)
}

/// Tell idle workers there may be something to claim.
pub fn wake_workers() {
    let r = &app().runs;
    *lock(&r.generation) += 1;
    r.wake.notify_all();
}

fn wait_for_work(seen: u64, timeout: Duration) {
    let r = &app().runs;
    let guard = lock(&r.generation);
    let _ = r
        .wake
        .wait_timeout_while(guard, timeout, |g| *g == seen && !app().stopping());
}

// ---------------------------------------------------------------------------------------------
// Workers, recovery, shutdown
// ---------------------------------------------------------------------------------------------

pub fn start_workers(n: usize) -> Vec<JoinHandle<()>> {
    (0..n.max(1))
        .filter_map(|i| {
            match std::thread::Builder::new()
                .name(format!("run-{i}"))
                .spawn(worker)
            {
                Ok(h) => Some(h),
                Err(e) => {
                    error!("couldn't start a run worker: {e}");
                    None
                }
            }
        })
        .collect()
}

fn worker() {
    while !app().stopping() {
        let seen = generation();
        match claim() {
            Ok(Some(run)) => execute(run),
            Ok(None) => wait_for_work(seen, Duration::from_secs(5)),
            Err(e) => {
                error!("run queue: {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Atomically take the oldest queued run.
fn claim() -> rusqlite::Result<Option<RunRow>> {
    let c = db::conn()?;
    let progress = progress_text(&Progress::new("starting", "Getting started", 1));
    c.prepare_cached(&format!(
        "UPDATE runs SET status = 'running', started_at = ?1, progress_json = ?2 \
         WHERE id = (SELECT id FROM runs WHERE status = 'queued' ORDER BY id LIMIT 1) AND status = 'queued' RETURNING {COLS}"
    ))?
    .query_row((util::now(), progress), from_row)
    .optional()
}

/// At start-up: runs a crash left `running` go back to the queue (attempts + 1); after three
/// attempts they fail. Accounts left `researching` without a research run move on to `review`.
pub fn recover() -> rusqlite::Result<()> {
    let c = db::conn()?;
    let tx = db::write_tx(&c)?;
    let progress = progress_text(&Progress::new("queued", "Picking up where we left off", 0));
    let requeued = tx.execute(
        "UPDATE runs SET status = 'queued', attempts = attempts + 1, started_at = NULL, progress_json = ?1 WHERE status = 'running'",
        [&progress],
    )?;
    let given_up = tx.execute(
        "UPDATE runs SET status = 'failed', finished_at = ?1, error = ?2, progress_json = ?3 WHERE status = 'queued' AND attempts >= ?4",
        (util::now(), MSG_ABANDONED, progress_text(&Progress::new("failed", MSG_ABANDONED, 0)), MAX_ATTEMPTS),
    )?;
    tx.execute(
        "UPDATE users SET onboarding = 'review' WHERE onboarding = 'researching' AND NOT EXISTS \
         (SELECT 1 FROM runs WHERE runs.user_id = users.id AND runs.kind = 'research' AND runs.status IN ('queued', 'running'))",
        [],
    )?;
    tx.commit()?;
    if requeued > 0 {
        info!(
            "recovered {requeued} interrupted run(s) ({given_up} gave up after {MAX_ATTEMPTS} attempts)"
        );
    }
    Ok(())
}

/// On shutdown: whatever is still running goes back to the queue, without counting an attempt.
pub fn requeue_unfinished() -> rusqlite::Result<usize> {
    let c = db::conn()?;
    let progress = progress_text(&Progress::new("queued", "Picking up where we left off", 0));
    c.execute("UPDATE runs SET status = 'queued', started_at = NULL, progress_json = ?1 WHERE status = 'running'", [progress])
}

/// Why a run didn't complete.
enum RunError {
    /// Asked to stop: shutting down (the run is requeued) or stopped by the watchdog.
    Cancelled,
    /// The account was deleted while the run was under way.
    Gone,
    /// Fail with a message for the person.
    Failed { message: String, retry: Retry },
}

/// What a failed discovery means for the schedule.
#[derive(Clone, Copy, PartialEq)]
enum Retry {
    /// Nothing: wait for the next regular refresh.
    No,
    /// A scheduled run failed for a passing reason: try once more in a couple of hours.
    Soon,
    /// The spend guard: try again in an hour.
    Budget,
}

impl RunError {
    fn failed(message: &str, retry: Retry) -> RunError {
        RunError::Failed {
            message: message.to_string(),
            retry,
        }
    }
}

impl From<rusqlite::Error> for RunError {
    fn from(e: rusqlite::Error) -> RunError {
        error!("run: database: {e}");
        RunError::failed(MSG_INTERNAL, Retry::Soon)
    }
}

fn engine_error(e: engine::Error, failed_message: &str) -> RunError {
    match e {
        engine::Error::Cancelled => RunError::Cancelled,
        engine::Error::EmptyProfile => RunError::failed(MSG_EMPTY, Retry::No),
        engine::Error::Ai(e) if e.is_account_problem() => {
            error!("AI account problem: {e}");
            RunError::failed(MSG_ACCOUNT, Retry::No)
        }
        engine::Error::Ai(e) => {
            warn!("AI error: {e}");
            RunError::failed(failed_message, Retry::Soon)
        }
    }
}

fn execute(run: RunRow) {
    let started = Instant::now();
    let cancel = app().runs.register(run.id, run.user_id);
    let sink = Sink::new(run.id);
    debug!(
        "run {} started: {} ({}) for user {}, attempt {}",
        run.id,
        run.kind,
        run.trigger,
        run.user_id,
        run.attempts + 1
    );
    let outcome = catch_unwind(AssertUnwindSafe(|| match run.kind.as_str() {
        "discover" => discover(&run, &sink, &cancel),
        "research" => research(&run, &sink, &cancel),
        "theme" => theme(&run, &sink, &cancel),
        other => Err(RunError::failed(
            &format!("Unknown kind of run: {other}"),
            Retry::No,
        )),
    }));
    app().runs.unregister(run.id);
    let result = outcome.unwrap_or_else(|_| {
        error!("run {} ({}) panicked", run.id, run.kind);
        Err(RunError::failed(MSG_INTERNAL, Retry::Soon))
    });
    let secs = started.elapsed().as_secs_f32();
    match result {
        Ok(()) => info!(
            "run {} ({} {}, user {}) done in {secs:.1}s",
            run.id, run.kind, run.trigger, run.user_id
        ),
        Err(RunError::Cancelled) if app().stopping() => {
            let requeued = db::conn().and_then(|c| {
                let progress = progress_text(&Progress::new("queued", "Picking up where we left off", 0));
                c.execute("UPDATE runs SET status = 'queued', started_at = NULL, progress_json = ?2 WHERE id = ?1 AND status = 'running'", (run.id, progress))
            });
            match requeued {
                Ok(_) => info!(
                    "run {} paused for shutdown; it resumes on the next start",
                    run.id
                ),
                Err(e) => error!("couldn't requeue run {}: {e}", run.id),
            }
        }
        Err(RunError::Cancelled) => fail(&run, &sink, MSG_TIMEOUT, Retry::Soon),
        Err(RunError::Gone) => debug!("run {}: the account is gone", run.id),
        Err(RunError::Failed { message, retry }) => {
            info!(
                "run {} ({} {}, user {}) failed after {secs:.1}s: {message}",
                run.id, run.kind, run.trigger, run.user_id
            );
            fail(&run, &sink, &message, retry);
        }
    }
}

/// Mark a run failed and undo what its kind started: research goes back to `choose`, a failed
/// scheduled discovery is retried later.
fn fail(run: &RunRow, sink: &Sink, message: &str, retry: Retry) {
    sink.finish(Progress::new("failed", message, sink.percent()));
    let result = (|| -> rusqlite::Result<bool> {
        let c = db::conn()?;
        let tx = db::write_tx(&c)?;
        if !finish_row(&tx, run.id, "failed", sink, None, Some(message))? {
            return Ok(false); // already finished by the watchdog or requeued by a shutdown
        }
        let now = util::now();
        if run.kind == "research" {
            tx.execute("UPDATE users SET onboarding = 'review' WHERE id = ?1 AND onboarding = 'researching'", [run.user_id])?;
        }
        if run.kind == "discover" {
            match retry {
                Retry::Budget => {
                    tx.execute(
                        "UPDATE users SET next_run_at = ?2 WHERE id = ?1 AND onboarding = 'ready'",
                        (run.user_id, now + BUDGET_DELAY_SECS),
                    )?;
                }
                Retry::Soon if run.trigger == "schedule" => {
                    tx.execute(
                        "UPDATE users SET next_run_at = ?2 WHERE id = ?1 AND onboarding = 'ready'",
                        (run.user_id, now + RETRY_AFTER_SECS),
                    )?;
                }
                _ => {}
            }
        }
        let follow = start_followup(&tx, run.user_id)?;
        tx.commit()?;
        Ok(follow)
    })();
    match result {
        Ok(true) => wake_workers(),
        Ok(false) => {}
        Err(e) => error!("couldn't record the failure of run {}: {e}", run.id),
    }
}

/// Complete a run row (only if it is still `running`: the watchdog or a shutdown may have taken
/// it back). Returns true when the row was updated.
fn finish_row(
    c: &Connection,
    run_id: i64,
    status: &str,
    sink: &Sink,
    stats: Option<&Value>,
    error: Option<&str>,
) -> rusqlite::Result<bool> {
    let (progress, log) = sink.snapshot();
    let n = c.execute(
        "UPDATE runs SET status = ?2, finished_at = ?3, progress_json = ?4, log_json = ?5, stats_json = COALESCE(?6, stats_json), error = ?7 \
         WHERE id = ?1 AND status = 'running'",
        (run_id, status, util::now(), progress, log, stats.map(Value::to_string), error),
    )?;
    Ok(n == 1)
}

/// Queue the discovery someone asked for while their previous run was under way.
fn start_followup(c: &Connection, user_id: i64) -> rusqlite::Result<bool> {
    let Some(trigger) = app().runs.take_followup(user_id) else {
        return Ok(false);
    };
    let ready = c
        .query_row(
            "SELECT onboarding = 'ready' FROM users WHERE id = ?1",
            [user_id],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false);
    if !ready {
        return Ok(false);
    }
    Ok(matches!(
        enqueue(c, user_id, "discover", trigger, None)?,
        Enqueued::New(_)
    ))
}

// ---------------------------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------------------------

/// The live progress of one run: the latest step and the last few, flushed to the run row.
struct Sink {
    run_id: i64,
    state: Mutex<SinkState>,
    /// The run's AI client, to record the spend so far with each flush (for the spend guard).
    ai: Mutex<Option<deepseek::Client>>,
}

struct SinkState {
    current: Progress,
    log: VecDeque<Progress>,
    dirty: bool,
    last_write: Option<Instant>,
}

impl Sink {
    fn new(run_id: i64) -> Sink {
        Sink {
            run_id,
            state: Mutex::new(SinkState {
                current: Progress::new("starting", "Getting started", 1),
                log: VecDeque::new(),
                dirty: false,
                last_write: None,
            }),
            ai: Mutex::new(None),
        }
    }

    fn set_client(&self, ai: &deepseek::Client) {
        *lock(&self.ai) = Some(ai.clone());
    }

    fn percent(&self) -> u8 {
        lock(&self.state).current.percent
    }

    /// A step of the main pipeline. The percentage only moves forward.
    fn report(&self, p: Progress) {
        let mut s = lock(&self.state);
        let percent = p.percent.max(s.current.percent).min(99);
        let p = Progress {
            percent,
            message: model::text::truncate(&model::text::squash(&p.message), 200),
            stage: p.stage,
        };
        push_log(&mut s.log, &p);
        s.current = p;
        s.dirty = true;
    }

    /// A side note from a parallel step (the theme): shown in the feed, keeps the percentage.
    fn note(&self, stage: &str, message: impl Into<String>) {
        let percent = self.percent();
        self.report(Progress::new(stage, message, percent));
    }

    /// The final step (done/failed), which may say 100%.
    fn finish(&self, p: Progress) {
        let mut s = lock(&self.state);
        push_log(&mut s.log, &p);
        s.current = p;
        s.dirty = true;
    }

    fn snapshot(&self) -> (String, String) {
        let s = lock(&self.state);
        (
            progress_text(&s.current),
            serde_json::to_string(&s.log).unwrap_or_else(|_| "[]".into()),
        )
    }

    /// Write the progress to the run row if it changed and the last write was long enough ago.
    fn flush_if_due(&self) {
        {
            let s = lock(&self.state);
            if !s.dirty || s.last_write.is_some_and(|t| t.elapsed() < FLUSH_EVERY) {
                return;
            }
        }
        let (progress, log) = self.snapshot();
        let cost = lock(&self.ai).as_ref().map(|ai| ai.usage().cost_usd);
        let written = db::conn().and_then(|c| {
            c.execute(
                "UPDATE runs SET progress_json = ?2, log_json = ?3, stats_json = COALESCE(?4, stats_json) WHERE id = ?1 AND status = 'running'",
                (self.run_id, progress, log, cost.map(|c| json!({ "cost_usd": c }).to_string())),
            )
        });
        let mut s = lock(&self.state);
        match written {
            Ok(_) => {
                s.dirty = false;
                s.last_write = Some(Instant::now());
            }
            Err(e) => debug!("run {}: couldn't write progress: {e}", self.run_id),
        }
    }
}

fn push_log(log: &mut VecDeque<Progress>, p: &Progress) {
    if p.message.is_empty() || log.back().is_some_and(|last| last.message == p.message) {
        return;
    }
    if log.len() >= LOG_LEN {
        log.pop_front();
    }
    log.push_back(p.clone());
}

/// Sets a flag when dropped, also while unwinding from a panic.
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Run `f` while a helper thread flushes the sink's progress to the database.
fn with_flusher<T>(sink: &Sink, f: impl FnOnce() -> T) -> T {
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
                sink.flush_if_due();
            }
        });
        let _stop = SetOnDrop(&done);
        f()
    })
}

/// The run's AI client: this server's DeepSeek connection with the account's chosen model, or
/// with chat on the account's Bedrock (fresh usage counters either way).
fn ai_client(sink: &Sink, user: &User) -> Result<deepseek::Client, RunError> {
    let base = app()
        .ai
        .as_ref()
        .ok_or_else(|| RunError::failed(MSG_NO_AI, Retry::No))?;
    let ai = crate::ai::client_for(base, user).map_err(|e| RunError::Failed {
        message: format!("Your AI model settings need attention: {e}."),
        retry: Retry::No,
    })?;
    sink.set_client(&ai);
    Ok(ai)
}

fn usage_json(u: &deepseek::Usage) -> Value {
    json!({
        "chat_calls": u.chat_calls,
        "input_tokens": u.input_tokens,
        "cached_tokens": u.cached_tokens,
        "output_tokens": u.output_tokens,
        "searches": u.searches,
        "fetches": u.fetches,
        "failed_calls": u.failed_calls,
    })
}

fn log_calls(run_id: i64, u: &deepseek::Usage) {
    if util::debug_enabled() {
        for call in &u.calls {
            debug!(
                "run {run_id}: {} {} status {} attempts {} {}ms in {} out {} {}",
                call.kind,
                call.stage,
                call.status,
                call.attempts,
                call.ms,
                call.tokens_in,
                call.tokens_out,
                call.note
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Discover
// ---------------------------------------------------------------------------------------------

fn discover(run: &RunRow, sink: &Sink, cancel: &AtomicBool) -> Result<(), RunError> {
    let c = db::conn()?;
    let user = User::load(&c, run.user_id)?.ok_or(RunError::Gone)?;
    let ai = ai_client(sink, &user)?;
    if over_budget(&c)? {
        return Err(RunError::failed(MSG_BUDGET, Retry::Budget));
    }
    let profile = user.profile().sanitized();
    if profile.is_empty() {
        return Err(RunError::failed(MSG_EMPTY, Retry::No));
    }
    let mut input = engine::DiscoverInput {
        profile: profile.clone(),
        steering: load_steering(&c, user.id)?,
        known: load_known(&c, user.id)?,
        limits: engine::Limits::default(),
        intel: user.intel(),
        context: Vec::new(),
    };
    let has_connections = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM connections WHERE user_id = ?1 AND status = 'ok')",
        [user.id],
        |r| r.get::<_, bool>(0),
    )?;
    if has_connections {
        sink.report(Progress::new(
            "planning",
            "Reading recent activity in your connected tools",
            2,
        ));
        let context = crate::conns::fetch_context(user.id);
        debug!(
            "run {}: {} item(s) from connected tools",
            run.id,
            context.len()
        );
        input.context = context;
    }
    let theme_needed = user
        .theme()
        .is_none_or(|t| t.built_for != subject_key(&profile));
    let today = model::date::today();
    sink.report(Progress::new("planning", "Getting ready to search", 2));

    let report = |p: Progress| sink.report(p);
    let ctx = engine::Ctx {
        ai: &ai,
        progress: &report,
        today: today.clone(),
        localization: user.last_ip.clone(),
        cancel,
    };
    let started = Instant::now();
    let (result, theme) = with_flusher(sink, || {
        std::thread::scope(|s| {
            let theme = theme_needed.then(|| {
                s.spawn(|| build_theme_safely(&ai, sink, &user, &profile, &today, cancel))
            });
            let result = engine::discover(&ctx, &input);
            let theme = theme.map(|h| h.join().unwrap_or("failed"));
            (result, theme)
        })
    });
    let output = result.map_err(|e| engine_error(e, MSG_AI_FAILED))?;

    sink.report(Progress::new("saving", "Saving what we found", 97));
    let tx = db::write_tx(&c)?;
    let still_ours = tx
        .query_row("SELECT status FROM runs WHERE id = ?1", [run.id], |r| {
            r.get::<_, String>(0)
        })
        .optional()?;
    if still_ours.as_deref() != Some("running") {
        return Err(RunError::Cancelled);
    }
    let merge = apply_discovery(&tx, run.id, user.id, &output, &today)?;
    let usage = ai.usage();
    let stats = json!({
        "new": merge.new,
        "refreshed": merge.refreshed,
        "expired": merge.expired,
        "total": merge.total,
        "engine": serde_json::to_value(&output.stats).unwrap_or(Value::Null),
        "usage": usage_json(&usage),
        "cost_usd": usage.cost_usd,
        "theme": theme.unwrap_or("kept"),
        "seconds": started.elapsed().as_secs(),
    });
    let message = match merge.new {
        0 => "Your opportunities are up to date".to_string(),
        1 => "Found 1 new opportunity".to_string(),
        n => format!("Found {n} new opportunities"),
    };
    sink.finish(Progress::new("done", message, 100));
    finish_row(&tx, run.id, "done", sink, Some(&stats), None)?;
    let now = util::now();
    let next = user.frequency().interval_secs().map(|i| now + i);
    tx.execute(
        "UPDATE users SET last_run_at = ?2, next_run_at = ?3 WHERE id = ?1",
        (user.id, now, next),
    )?;
    let follow = start_followup(&tx, user.id)?;
    tx.commit()?;
    if follow {
        wake_workers();
    }
    info!(
        "run {}: {} new, {} refreshed, {} expired, {} listed; {} searches, {} fetches, {} chat calls, ${:.4}",
        run.id,
        merge.new,
        merge.refreshed,
        merge.expired,
        merge.total,
        usage.searches,
        usage.fetches,
        usage.chat_calls,
        usage.cost_usd
    );
    log_calls(run.id, &usage);

    let digest_trigger = matches!(run.trigger.as_str(), "schedule" | "manual" | "retry");
    if merge.new > 0
        && user.email_digest
        && digest_trigger
        && let Err(e) = send_digest(&c, &user, run.id, merge.total)
    {
        warn!("couldn't prepare the digest for user {}: {e}", user.id);
    }
    Ok(())
}

/// What the person already has, for the engine: active, saved and dismissed opportunities.
fn load_known(c: &Connection, user_id: i64) -> rusqlite::Result<Vec<KnownOpportunity>> {
    let mut stmt = c.prepare_cached(
        "SELECT url_key, data_json, status, deadline FROM opportunities WHERE user_id = ?1 AND status IN ('active', 'saved', 'dismissed') \
         ORDER BY status = 'dismissed', updated_at DESC LIMIT 600",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        let data: String = r.get(1)?;
        let opp: Opportunity = serde_json::from_str(&data).unwrap_or_default();
        Ok(KnownOpportunity {
            key: r.get(0)?,
            title: opp.title,
            category: opp.category,
            status: r.get(2)?,
            deadline: r.get(3)?,
        })
    })?;
    rows.collect()
}

/// The person's notes and likes/dislikes, most recent first.
fn load_steering(c: &Connection, user_id: i64) -> rusqlite::Result<Steering> {
    let notes = c
        .prepare_cached("SELECT text FROM steering WHERE user_id = ?1 ORDER BY id DESC LIMIT 20")?
        .query_map([user_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let titles = |condition: &str| -> rusqlite::Result<Vec<String>> {
        let sql = format!(
            "SELECT json_extract(data_json, '$.title') FROM opportunities WHERE user_id = ?1 AND ({condition}) ORDER BY updated_at DESC LIMIT 20"
        );
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map([user_id], |r| r.get::<_, Option<String>>(0))?;
        Ok(rows
            .filter_map(|t| t.ok().flatten())
            .filter(|t| !t.is_empty())
            .collect())
    };
    Ok(Steering {
        notes,
        liked: titles("status = 'saved' OR (vote = 1 AND status != 'dismissed')")?,
        disliked: titles("status = 'dismissed' OR vote = -1")?,
    })
}

struct Merge {
    new: i64,
    refreshed: i64,
    expired: i64,
    total: i64,
}

/// Upsert the engine's finds and prune, per docs/ARCHITECTURE.md ("Applying a discovery").
fn apply_discovery(
    c: &Connection,
    run_id: i64,
    user_id: i64,
    out: &engine::DiscoverOutput,
    today: &str,
) -> rusqlite::Result<Merge> {
    let now = util::now();
    let mut returned: HashSet<String> = HashSet::new();
    for opp in &out.opportunities {
        let Some(opp) = opp.clone().sanitized(today) else {
            continue;
        };
        let Some(key) = opp.key() else { continue };
        if !returned.insert(key.clone()) {
            continue;
        }
        let data = serde_json::to_string(&opp).unwrap_or_else(|_| "{}".into());
        let existing: Option<(i64, String)> = c
            .prepare_cached(
                "SELECT id, status FROM opportunities WHERE user_id = ?1 AND url_key = ?2",
            )?
            .query_row((user_id, &key), |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        match existing {
            None => {
                c.prepare_cached(
                    "INSERT INTO opportunities(user_id, url_key, data_json, score, category, deadline, status, first_seen_run, last_seen_run, first_seen_at, last_seen_at, updated_at) \
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?7, ?8, ?8, ?8)",
                )?
                .execute((user_id, &key, &data, opp.score, &opp.category, &opp.deadline, run_id, now))?;
            }
            Some((id, status)) if status == "active" || status == "saved" => {
                c.prepare_cached(
                    "UPDATE opportunities SET data_json = ?2, score = ?3, category = ?4, deadline = ?5, last_seen_run = ?6, last_seen_at = ?7, updated_at = ?7 WHERE id = ?1",
                )?
                .execute((id, &data, opp.score, &opp.category, &opp.deadline, run_id, now))?;
            }
            // Expired comes back when the engine finds it open again with a future deadline
            // (sanitized() already dropped past deadlines); dismissed never comes back.
            Some((id, status)) if status == "expired" && opp.deadline.is_some() => {
                c.prepare_cached(
                    "UPDATE opportunities SET status = 'active', data_json = ?2, score = ?3, category = ?4, deadline = ?5, last_seen_run = ?6, last_seen_at = ?7, updated_at = ?7 WHERE id = ?1",
                )?
                .execute((id, &data, opp.score, &opp.category, &opp.deadline, run_id, now))?;
            }
            Some(_) => {}
        }
    }

    // Prune active ones (never saved): past deadline, stale, unseen for a while, beyond the best 40.
    let mut expired = c.execute(
        "UPDATE opportunities SET status = 'expired', updated_at = ?3 WHERE user_id = ?1 AND status = 'active' AND deadline IS NOT NULL AND deadline < ?2",
        (user_id, today, now),
    )?;
    for key in out.stale.iter().filter(|k| !returned.contains(*k)) {
        expired += c.execute(
            "UPDATE opportunities SET status = 'expired', updated_at = ?3 WHERE user_id = ?1 AND url_key = ?2 AND status = 'active'",
            (user_id, key, now),
        )?;
    }
    let previous: Vec<i64> = c
        .prepare_cached("SELECT id FROM runs WHERE user_id = ?1 AND kind = 'discover' AND status = 'done' AND id != ?2 ORDER BY id DESC LIMIT 2")?
        .query_map((user_id, run_id), |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if let Some(&third_latest) = previous.get(1) {
        expired += c.execute(
            "UPDATE opportunities SET status = 'expired', updated_at = ?4 WHERE user_id = ?1 AND status = 'active' \
             AND COALESCE(last_seen_run, 0) < ?2 AND first_seen_at < ?3",
            (user_id, third_latest, now - 21 * 86_400, now),
        )?;
    }
    expired += c.execute(
        "UPDATE opportunities SET status = 'expired', updated_at = ?3 WHERE id IN \
         (SELECT id FROM opportunities WHERE user_id = ?1 AND status = 'active' ORDER BY score DESC, last_seen_at DESC, id DESC LIMIT -1 OFFSET ?2)",
        (user_id, MAX_ACTIVE, now),
    )?;

    let (new, refreshed, total) = c.query_row(
        "SELECT COALESCE(SUM(first_seen_run = ?2), 0), COALESCE(SUM(last_seen_run = ?2 AND first_seen_run != ?2), 0), COUNT(*) \
         FROM opportunities WHERE user_id = ?1 AND status IN ('active', 'saved')",
        (user_id, run_id),
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(Merge {
        new,
        refreshed,
        expired: expired as i64,
        total,
    })
}

fn send_digest(c: &Connection, user: &User, run_id: i64, total: i64) -> rusqlite::Result<()> {
    let items: Vec<Opportunity> = c
        .prepare_cached("SELECT data_json FROM opportunities WHERE user_id = ?1 AND first_seen_run = ?2 AND status IN ('active', 'saved') ORDER BY score DESC, id")?
        .query_map((user.id, run_id), |r| r.get::<_, String>(0))?
        .filter_map(|d| d.ok().and_then(|d| serde_json::from_str(&d).ok()))
        .collect();
    if items.is_empty() {
        return Ok(());
    }
    let theme = User::load(c, user.id)?
        .and_then(|u| u.theme())
        .unwrap_or_default();
    let label = if theme.label.is_empty() {
        "Opportunity".to_string()
    } else {
        theme.label
    };
    let accent = model::sanitize_hex_color(&theme.accent, DEFAULT_ACCENT);
    let cfg = app().mailer.config();
    let email = mailer::digest(
        cfg,
        &user.business_name,
        &user.email,
        &items,
        total.max(0) as usize,
        &label,
        &accent,
    );
    app().mail.send(mail::Job {
        user_id: Some(user.id),
        kind: "digest",
        email,
    });
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------------------------

/// Build and store a theme, never panicking: "photo", "gradient" or "failed" (for the stats).
fn build_theme_safely(
    ai: &deepseek::Client,
    sink: &Sink,
    user: &User,
    profile: &Profile,
    today: &str,
    cancel: &AtomicBool,
) -> &'static str {
    match catch_unwind(AssertUnwindSafe(|| {
        build_theme(ai, sink, user, profile, today, cancel)
    })) {
        Ok(Ok(true)) => "photo",
        Ok(Ok(false)) => "gradient",
        Ok(Err(e)) => {
            warn!("theme for user {}: {e}", user.id);
            "failed"
        }
        Err(_) => {
            error!("theme for user {} panicked", user.id);
            "failed"
        }
    }
}

/// Label, accent and photo for the profile; returns whether a photo was found.
fn build_theme(
    ai: &deepseek::Client,
    sink: &Sink,
    user: &User,
    profile: &Profile,
    today: &str,
    cancel: &AtomicBool,
) -> Result<bool, String> {
    let note = |p: Progress| sink.note(&p.stage, p.message);
    let ctx = engine::Ctx {
        ai,
        progress: &note,
        today: today.to_string(),
        localization: user.last_ip.clone(),
        cancel,
    };
    sink.note("theming", "Choosing a look for your page");
    let hint = engine::theme_hint(&ctx, profile);
    let label = model::text::truncate(&model::text::squash(&hint.label), 40);
    let label = if label.is_empty() {
        "Opportunity".to_string()
    } else {
        label
    };
    let accent = model::sanitize_hex_color(&hint.accent, DEFAULT_ACCENT);
    sink.note("theming", "Looking for a background photo");
    let background = imagery::find_background(&app().imagery, ai, &hint);
    let photo = store_theme(user.id, label, accent, background, subject_key(profile))?;
    sink.note(
        "theming",
        if photo {
            "Your page has a new background"
        } else {
            "Your page has a new colour scheme"
        },
    );
    Ok(photo)
}

/// The image type from its first bytes (trusting the bytes, not the declared type).
fn sniff_image(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("image/jpeg", "jpg"))
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "png"))
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(("image/webp", "webp"))
    } else {
        None
    }
}

fn store_theme(
    user_id: i64,
    label: String,
    accent: String,
    background: Result<imagery::Background, imagery::Error>,
    built_for: String,
) -> Result<bool, String> {
    let c = db::conn().map_err(|e| e.to_string())?;
    let previous = User::load(&c, user_id)
        .map_err(|e| e.to_string())?
        .ok_or("the account is gone")?
        .theme();
    let version = previous.as_ref().map_or(0, |t| t.version) + 1;
    let dir = app().cfg.data_dir.join("themes");
    let (image, file) = match background {
        Ok(bg) => match sniff_image(&bg.bytes) {
            Some((mime, ext)) if bg.bytes.len() <= 15 * 1024 * 1024 => {
                let name = format!("{user_id}-{version}.{ext}");
                let tmp = dir.join(format!("{name}.tmp"));
                std::fs::write(&tmp, &bg.bytes)
                    .and_then(|_| std::fs::rename(&tmp, dir.join(&name)))
                    .map_err(|e| format!("couldn't save the photo: {e}"))?;
                (
                    Some(model::ThemeImage {
                        mime: mime.to_string(),
                        ..bg.image
                    }),
                    Some(name),
                )
            }
            _ => {
                warn!(
                    "theme for user {user_id}: the photo isn't a usable JPEG, PNG or WebP; using a gradient"
                );
                (None, None)
            }
        },
        Err(e) => {
            debug!("theme for user {user_id}: {e}; using a gradient");
            (None, None)
        }
    };
    let photo = image.is_some();
    let record = ThemeRecord {
        label,
        accent,
        image,
        file: file.clone(),
        version,
        built_for,
    };
    let n = c
        .execute(
            "UPDATE users SET theme_json = ?2 WHERE id = ?1",
            (user_id, serde_json::to_string(&record).unwrap_or_default()),
        )
        .map_err(|e| e.to_string())?;
    if n == 0 {
        if let Some(f) = &file {
            let _ = std::fs::remove_file(dir.join(f));
        }
        return Err("the account is gone".into());
    }
    if let Some(old) = previous
        .and_then(|p| p.file)
        .filter(|old| Some(old) != file.as_ref())
    {
        let _ = std::fs::remove_file(dir.join(old));
    }
    Ok(photo)
}

fn theme(run: &RunRow, sink: &Sink, cancel: &AtomicBool) -> Result<(), RunError> {
    let c = db::conn()?;
    let user = User::load(&c, run.user_id)?.ok_or(RunError::Gone)?;
    let ai = ai_client(sink, &user)?;
    if over_budget(&c)? {
        return Err(RunError::failed(MSG_BUDGET, Retry::No));
    }
    let profile = user.profile();
    let today = model::date::today();
    sink.report(Progress::new("theming", "Choosing a look for your page", 5));
    let result = with_flusher(sink, || {
        build_theme(&ai, sink, &user, &profile, &today, cancel)
    });
    let photo = match result {
        Ok(photo) => photo,
        Err(e) if e == "the account is gone" => return Err(RunError::Gone),
        Err(e) => {
            warn!("theme for user {}: {e}", user.id);
            return Err(RunError::failed(MSG_THEME_FAILED, Retry::No));
        }
    };
    let usage = ai.usage();
    let stats = json!({ "photo": photo, "usage": usage_json(&usage), "cost_usd": usage.cost_usd });
    sink.finish(Progress::new(
        "done",
        if photo {
            "Your page has a fresh look"
        } else {
            "Your page has fresh colours"
        },
        100,
    ));
    let tx = db::write_tx(&c)?;
    finish_row(&tx, run.id, "done", sink, Some(&stats), None)?;
    let follow = start_followup(&tx, user.id)?;
    tx.commit()?;
    if follow {
        wake_workers();
    }
    log_calls(run.id, &usage);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Research
// ---------------------------------------------------------------------------------------------

/// Research the business. During onboarding (`researching`) the drafted profile and intel are
/// stored and the account moves to `review`; afterwards (an intel refresh) the intel is replaced
/// and the profile only filled where empty.
fn research(run: &RunRow, sink: &Sink, cancel: &AtomicBool) -> Result<(), RunError> {
    let c = db::conn()?;
    let user = User::load(&c, run.user_id)?.ok_or(RunError::Gone)?;
    let ai = ai_client(sink, &user)?;
    if over_budget(&c)? {
        return Err(RunError::failed(MSG_BUDGET, Retry::No));
    }
    let hints = run
        .input_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v["hints"].as_str().map(str::to_string))
        .unwrap_or_default();
    sink.report(Progress::new(
        "researching",
        format!("Reading about {}", user.business_name),
        3,
    ));
    let input = engine::BusinessInput {
        business_name: user.business_name.clone(),
        website: user.website.clone(),
        hints,
    };
    let report = |p: Progress| sink.report(p);
    let ctx = engine::Ctx {
        ai: &ai,
        progress: &report,
        today: model::date::today(),
        localization: user.last_ip.clone(),
        cancel,
    };
    let result = with_flusher(sink, || engine::research_business(&ctx, &input));
    let (found, intel) = result.map_err(|e| engine_error(e, MSG_RESEARCH_FAILED))?;
    let mut found = found.sanitized();
    if found.organization.is_empty() {
        found.organization = user.business_name.clone();
    }
    if found.website.is_empty() {
        found.website = user.website.clone();
    }
    if let Some(r) = found.research.as_mut()
        && r.researched_at <= 0
    {
        r.researched_at = util::now();
    }
    let mut intel = intel.sanitized();
    if intel.updated_at <= 0 {
        intel.updated_at = util::now();
    }
    let usage = ai.usage();
    let stats = json!({ "confidence": intel.confidence, "usage": usage_json(&usage), "cost_usd": usage.cost_usd });
    let intel_text = serde_json::to_string(&intel).unwrap_or_else(|_| "{}".into());
    sink.finish(Progress::new("done", "Your business profile is ready", 100));
    let tx = db::write_tx(&c)?;
    if !finish_row(&tx, run.id, "done", sink, Some(&stats), None)? {
        return Err(RunError::Cancelled);
    }
    let onboarding: String = tx.query_row(
        "SELECT onboarding FROM users WHERE id = ?1",
        [user.id],
        |r| r.get(0),
    )?;
    let profile = if onboarding == "researching" {
        found
    } else {
        fill_empty(user.profile(), found)
    };
    let profile_text = serde_json::to_string(&profile).unwrap_or_else(|_| "{}".into());
    tx.execute(
        "UPDATE users SET profile_json = ?2, intel_json = ?3, onboarding = CASE WHEN onboarding = 'researching' THEN 'review' ELSE onboarding END WHERE id = ?1",
        (user.id, &profile_text, &intel_text),
    )?;
    let follow = start_followup(&tx, user.id)?;
    tx.commit()?;
    if follow {
        wake_workers();
    }
    log_calls(run.id, &usage);
    Ok(())
}

/// `current` with every empty field taken from `found` (an intel refresh never overwrites what
/// the business entered or confirmed).
fn fill_empty(current: Profile, found: Profile) -> Profile {
    fn text(a: String, b: String) -> String {
        if a.trim().is_empty() { b } else { a }
    }
    fn list(a: Vec<String>, b: Vec<String>) -> Vec<String> {
        if a.is_empty() { b } else { a }
    }
    Profile {
        headline: text(current.headline, found.headline),
        stage: text(current.stage, found.stage),
        organization: text(current.organization, found.organization),
        website: text(current.website, found.website),
        industry: text(current.industry, found.industry),
        location: text(current.location, found.location),
        reach: text(current.reach, found.reach),
        seeking: list(current.seeking, found.seeking),
        interests: list(current.interests, found.interests),
        skills: list(current.skills, found.skills),
        goals: text(current.goals, found.goals),
        avoid: text(current.avoid, found.avoid),
        links: list(current.links, found.links),
        languages: list(current.languages, found.languages),
        research: current.research.or(found.research),
    }
    .sanitized()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffing() {
        assert_eq!(
            sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0, 0]),
            Some(("image/jpeg", "jpg"))
        );
        assert_eq!(
            sniff_image(b"\x89PNG\r\n\x1a\n...."),
            Some(("image/png", "png"))
        );
        assert_eq!(
            sniff_image(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(("image/webp", "webp"))
        );
        assert_eq!(sniff_image(b"<html>"), None);
        assert_eq!(sniff_image(b""), None);
    }

    #[test]
    fn log_keeps_the_last_distinct_steps() {
        let mut log = VecDeque::new();
        for i in 0..20 {
            push_log(
                &mut log,
                &Progress::new("searching", format!("step {i}"), i),
            );
            push_log(
                &mut log,
                &Progress::new("searching", format!("step {i}"), i),
            );
        }
        assert_eq!(log.len(), LOG_LEN);
        assert_eq!(log.front().unwrap().message, "step 4");
        assert_eq!(log.back().unwrap().message, "step 19");
    }
}
