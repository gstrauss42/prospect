//! The Prospect web server: one small binary that serves the web client and the JSON API,
//! keeps everything in SQLite, runs discovery/research/theme jobs on worker threads and refreshes
//! everyone on a schedule. See docs/ARCHITECTURE.md and docs/API.md.
//!
//! Threads: `HTTP_THREADS` request workers on tiny_http, `MAX_CONCURRENT_RUNS` run workers, one
//! scheduler, one mail sender, and the main thread, which waits for SIGINT/SIGTERM and then shuts
//! everything down gracefully (unfinished runs go back to the queue and resume on the next start).

#[macro_use]
mod util;
mod ai;
mod api;
mod assets;
mod auth;
mod config;
mod conns;
mod db;
mod gate;
mod http;
mod mail;
mod runs;
mod scheduler;
mod sys;
mod users;

use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use config::Config;

/// Everything the threads share. Built once at start-up; see [`app`].
pub struct App {
    pub cfg: Config,
    /// None without `DEEPSEEK_API_KEY`: the app runs, AI work fails with a clear message.
    pub ai: Option<deepseek::Client>,
    pub imagery: imagery::Config,
    pub mailer: mailer::Mailer,
    pub limiter: auth::Limiter,
    pub runs: runs::Registry,
    pub mail: mail::Queue,
    /// Seals and opens secrets at rest (Bedrock keys, connector credentials).
    pub vault: vault::Vault,
    pub connectors: connectors::Connectors,
    /// Set when the process is shutting down: workers stop taking new work.
    pub stopping: AtomicBool,
}

static APP: OnceLock<App> = OnceLock::new();

/// The shared state. Only called after start-up has built it.
pub fn app() -> &'static App {
    APP.get()
        .expect("app state is initialised before any thread starts")
}

impl App {
    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::Relaxed)
    }
}

fn main() {
    // Real environment variables win over the .env file; this runs before any thread exists.
    let loaded = config::load_dotenv(Path::new(".env"));
    platform_env();
    let cfg = Config::from_env();
    util::set_debug(cfg.debug);
    sys::tune_malloc();
    if loaded > 0 {
        info!("loaded {loaded} setting(s) from .env");
    }
    if let Err(e) = run(cfg) {
        error!("{e}");
        std::process::exit(1);
    }
}

fn run(cfg: Config) -> Result<(), String> {
    std::fs::create_dir_all(cfg.data_dir.join("themes")).map_err(|e| {
        format!(
            "can't create the data directory {}: {e}",
            cfg.data_dir.display()
        )
    })?;
    let db_path = cfg.data_dir.join("prospect.db");
    move_legacy_db(&cfg.data_dir, &db_path);
    db::init(db_path).map_err(|e| format!("can't open the database: {e}"))?;
    runs::recover().map_err(|e| format!("can't recover runs: {e}"))?;
    gate::init();

    let ai = deepseek::Config::from_env().map(deepseek::Client::new);
    if ai.is_none() {
        warn!(
            "DEEPSEEK_API_KEY is not set: the app runs, but discovery, research and themes are off"
        );
    }
    let mailer = mailer::Mailer::new(mailer::Config::from_env(&cfg.data_dir));
    let vault = vault::Vault::open(&cfg.data_dir)
        .map_err(|e| format!("can't open the secrets vault: {e}"))?;
    let connectors = connectors::Connectors::from_env(&cfg.public_url);
    let listener = http::bind(&cfg.addr)?;
    // The address actually bound (ADDR may say port 0: tests let the OS pick a free port).
    let bound = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| cfg.addr.clone());
    let state = App {
        ai,
        imagery: imagery::Config::from_env(),
        mailer,
        limiter: auth::Limiter::default(),
        runs: runs::Registry::default(),
        mail: mail::Queue::start(),
        vault,
        connectors,
        stopping: AtomicBool::new(false),
        cfg,
    };
    if APP.set(state).is_err() {
        return Err("app state initialised twice".into());
    }
    let app = app();

    let workers = runs::start_workers(app.cfg.max_concurrent_runs);
    let scheduler = scheduler::start();
    let server = http::start(listener, app.cfg.http_threads)?;
    info!(
        "prospect {} listening on http://{} (public URL {}, data in {}, AI {}, email {})",
        env!("CARGO_PKG_VERSION"),
        bound,
        app.cfg.public_url,
        app.cfg.data_dir.display(),
        if app.ai.is_some() {
            "ready"
        } else {
            "not configured"
        },
        if app.mailer.is_live() {
            "via Mailgun"
        } else {
            "to the outbox"
        },
    );

    sys::install_signal_handlers();
    while !sys::shutdown_requested() {
        std::thread::sleep(Duration::from_millis(200));
    }

    info!("shutting down");
    app.stopping.store(true, Ordering::SeqCst);
    app.runs.cancel_all();
    runs::wake_workers();
    let deadline = Instant::now() + Duration::from_secs(6);
    for handle in workers.into_iter().chain(std::iter::once(scheduler)) {
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    server.stop(Duration::from_secs(2));
    match runs::requeue_unfinished() {
        Ok(0) => {}
        Ok(n) => info!("{n} unfinished run(s) will resume on the next start"),
        Err(e) => error!("couldn't requeue unfinished runs: {e}"),
    }
    info!("bye");
    Ok(())
}

/// Databases created before the rename to Prospect are called `opportunity.db`: move one (and its
/// WAL files) into place once, so existing accounts carry over.
fn move_legacy_db(dir: &Path, new: &Path) {
    let old = dir.join("opportunity.db");
    if new.exists() || !old.exists() {
        return;
    }
    for suffix in ["", "-wal", "-shm"] {
        let from = dir.join(format!("opportunity.db{suffix}"));
        if from.exists()
            && let Err(e) = std::fs::rename(&from, dir.join(format!("prospect.db{suffix}")))
        {
            warn!("couldn't move {} into place: {e}", from.display());
        }
    }
    info!("moved the database from opportunity.db to prospect.db");
}

/// Hosting platforms such as Render assign the port (`PORT`) and publish the service's address
/// (`RENDER_EXTERNAL_URL`). Use them when `ADDR` and `PUBLIC_URL` aren't set explicitly, so the
/// container needs no extra configuration. Runs during start-up, before any thread exists.
fn platform_env() {
    let unset = |k: &str| {
        std::env::var(k)
            .map(|v| v.trim().is_empty())
            .unwrap_or(true)
    };
    if unset("ADDR")
        && let Ok(port) = std::env::var("PORT")
        && !port.trim().is_empty()
    {
        // SAFETY: single-threaded start-up, as for the .env loader.
        unsafe { std::env::set_var("ADDR", format!("0.0.0.0:{}", port.trim())) };
    }
    if unset("PUBLIC_URL")
        && let Ok(url) = std::env::var("RENDER_EXTERNAL_URL")
        && !url.trim().is_empty()
    {
        // SAFETY: as above.
        unsafe { std::env::set_var("PUBLIC_URL", url.trim()) };
    }
    // Render: every request comes through Cloudflare and then Render's own proxy, each adding
    // an X-Forwarded-For entry, and Cloudflare sets CF-Connecting-IP to the visitor's address.
    // Without these, every visitor looks like a Cloudflare server and shares its rate limits.
    if std::env::var("RENDER").is_ok_and(|v| v.trim().eq_ignore_ascii_case("true")) {
        for (key, value) in [
            ("TRUST_PROXY", "true"),
            ("TRUST_PROXY_HOPS", "2"),
            ("CLIENT_IP_HEADER", "CF-Connecting-IP"),
        ] {
            if unset(key) {
                // SAFETY: as above.
                unsafe { std::env::set_var(key, value) };
            }
        }
    }
}
