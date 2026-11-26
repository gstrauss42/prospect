//! The scheduler thread. Every tick (30 s by default): stop runs that hang, queue discovery for
//! people whose refresh is due (respecting the daily spend guard), and every ten minutes sweep
//! expired sessions, reset tokens, old runs and old email logs.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use model::{Frequency, Progress};
use rusqlite::OptionalExtension;

use crate::runs::{self, BUDGET_DELAY_SECS, Enqueued, MSG_TIMEOUT, RUN_TIMEOUT_SECS};
use crate::{app, db, util};

const SWEEP_EVERY: Duration = Duration::from_secs(600);
/// How many due people are queued per tick (the rest follow on the next ticks).
const BATCH: i64 = 50;

pub fn start() -> JoinHandle<()> {
    let spawned = std::thread::Builder::new()
        .name("scheduler".into())
        .spawn(run);
    match spawned {
        Ok(h) => h,
        Err(e) => {
            error!("couldn't start the scheduler: {e}");
            std::thread::spawn(|| {})
        }
    }
}

fn run() {
    let tick = app().cfg.scheduler_tick;
    let mut last_sweep: Option<Instant> = None;
    while !app().stopping() {
        let sweep = last_sweep.is_none_or(|t| t.elapsed() >= SWEEP_EVERY);
        match catch_unwind(AssertUnwindSafe(|| tick_once(sweep))) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!("scheduler: {e}"),
            Err(_) => error!("scheduler: a tick panicked"),
        }
        if sweep {
            last_sweep = Some(Instant::now());
        }
        let until = Instant::now() + tick;
        while Instant::now() < until && !app().stopping() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn tick_once(sweep: bool) -> rusqlite::Result<()> {
    watchdog()?;
    schedule_due()?;
    if sweep {
        self::sweep()?;
    }
    Ok(())
}

/// Fail runs that have been running far too long (and ask them to stop), so nobody is stuck
/// behind a hung run.
fn watchdog() -> rusqlite::Result<()> {
    let c = db::conn()?;
    let now = util::now();
    let stuck: Vec<(i64, i64, String)> = c
        .prepare_cached(
            "SELECT id, user_id, kind FROM runs WHERE status = 'running' AND started_at < ?1",
        )?
        .query_map([now - RUN_TIMEOUT_SECS], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (id, user_id, kind) in stuck {
        warn!(
            "run {id} ({kind}, user {user_id}) has been running for over {} minutes; stopping it",
            RUN_TIMEOUT_SECS / 60
        );
        app().runs.cancel_run(id);
        let tx = db::write_tx(&c)?;
        let progress =
            serde_json::to_string(&Progress::new("failed", MSG_TIMEOUT, 0)).unwrap_or_default();
        let n = tx.execute(
            "UPDATE runs SET status = 'failed', finished_at = ?2, error = ?3, progress_json = ?4 WHERE id = ?1 AND status = 'running'",
            (id, now, MSG_TIMEOUT, progress),
        )?;
        if n == 1 && kind == "research" {
            tx.execute("UPDATE users SET onboarding = 'review' WHERE id = ?1 AND onboarding = 'researching'", [user_id])?;
        }
        tx.commit()?;
    }
    Ok(())
}

/// Queue discovery for ready people whose `next_run_at` has passed and who have no active run.
fn schedule_due() -> rusqlite::Result<()> {
    if app().ai.is_none() {
        return Ok(());
    }
    let c = db::conn()?;
    let now = util::now();
    let due: Vec<(i64, String)> = c
        .prepare_cached(
            "SELECT id, frequency FROM users u WHERE onboarding = 'ready' AND next_run_at IS NOT NULL AND next_run_at <= ?1 \
             AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.user_id = u.id AND r.status IN ('queued', 'running')) \
             ORDER BY next_run_at LIMIT ?2",
        )?
        .query_map((now, BATCH), |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    if due.is_empty() {
        return Ok(());
    }
    if runs::over_budget(&c)? {
        warn!(
            "daily spend limit reached; {} scheduled refresh(es) pushed back an hour",
            due.len()
        );
        for (id, _) in &due {
            c.execute(
                "UPDATE users SET next_run_at = ?2 WHERE id = ?1",
                (id, now + BUDGET_DELAY_SECS),
            )?;
        }
        return Ok(());
    }
    let mut queued = 0;
    for (id, frequency) in &due {
        let tx = db::write_tx(&c)?;
        let last: Option<(String, String)> = tx
            .query_row(
                "SELECT status, trigger FROM runs WHERE user_id = ?1 AND kind = 'discover' ORDER BY id DESC LIMIT 1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let trigger = match last {
            Some((status, trigger)) if status == "failed" && trigger == "schedule" => "retry",
            _ => "schedule",
        };
        if let Enqueued::New(_) = runs::enqueue(&tx, *id, "discover", trigger, None)? {
            queued += 1;
        }
        let next = Frequency::parse(frequency)
            .and_then(Frequency::interval_secs)
            .map(|i| now + i);
        tx.execute(
            "UPDATE users SET next_run_at = ?2 WHERE id = ?1",
            (id, next),
        )?;
        tx.commit()?;
    }
    if queued > 0 {
        info!("scheduled {queued} refresh(es)");
        runs::wake_workers();
    }
    Ok(())
}

fn sweep() -> rusqlite::Result<()> {
    let c = db::conn()?;
    let now = util::now();
    let sessions = c.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now])?;
    let resets = c.execute(
        "DELETE FROM password_resets WHERE expires_at <= ?1 OR used = 1",
        [now],
    )?;
    let runs = c.execute(
        "DELETE FROM runs WHERE finished_at IS NOT NULL AND finished_at < ?1",
        [now - 90 * 86_400],
    )?;
    let emails = c.execute(
        "DELETE FROM emails WHERE created_at < ?1",
        [now - 180 * 86_400],
    )?;
    c.execute(
        "DELETE FROM audit_events WHERE at < ?1",
        [now - 90 * 86_400],
    )?;
    c.execute("DELETE FROM oauth_states WHERE expires_at <= ?1", [now])?;
    app().limiter.prune();
    if sessions + resets + runs + emails > 0 {
        debug!(
            "swept {sessions} session(s), {resets} reset token(s), {runs} old run(s), {emails} email log(s)"
        );
    }
    Ok(())
}
