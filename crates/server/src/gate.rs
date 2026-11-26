//! The demo padlock. With `DEMO_PASSWORD` set, every visitor gets a password page until they enter
//! it: the web client isn't served and the API answers "locked". It sits in front of the app and
//! has nothing to do with accounts; leave the variable unset and it isn't there.
//!
//! A feature flag switches it off and on for everyone without a deploy: the hidden page at
//! `/__admin` (only when `ADMIN_PASSWORD` is set) stores `flag:padlock` in the `meta` table, so
//! the choice survives restarts. It's cached in memory; requests never touch the database for it.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use crate::app;
use crate::http::{ApiError, Req, Resp};

const COOKIE: &str = "prospect_gate";
const TTL_SECS: i64 = 30 * 86_400;

static PAGE: &[u8] = include_bytes!("../../../web/padlock.html");
static ADMIN_PAGE: &[u8] = include_bytes!("../../../web/admin.html");

/// The `padlock` feature flag (on unless switched off on the admin page).
static ENABLED: AtomicBool = AtomicBool::new(true);
const FLAG_KEY: &str = "flag:padlock";

/// Load the stored flag. Call once at start-up, after the database is ready.
pub fn init() {
    let stored = crate::db::conn().ok().and_then(|c| {
        c.query_row("SELECT value FROM meta WHERE key = ?1", [FLAG_KEY], |r| {
            r.get::<_, String>(0)
        })
        .ok()
    });
    ENABLED.store(stored.as_deref() != Some("off"), Ordering::Relaxed);
}

/// True when visitors must get past the padlock: a password is configured and the flag is on.
pub fn active() -> bool {
    app().cfg.demo_password.is_some() && ENABLED.load(Ordering::Relaxed)
}

fn store_flag(on: bool) -> bool {
    let saved = crate::db::conn().ok().is_some_and(|c| {
        c.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [FLAG_KEY, if on { "on" } else { "off" }],
        )
        .is_ok()
    });
    if saved {
        ENABLED.store(on, Ordering::Relaxed);
    }
    saved
}

fn token(password: &str) -> String {
    crate::util::sha256_hex(format!("prospect-demo-gate\0{password}").as_bytes())
}

/// Requests that work while locked: the padlock page's own files, the unlock call, the health check.
pub fn open(path: &str) -> bool {
    matches!(
        path,
        "/healthz"
            | "/api/gate"
            | "/padlock.css"
            | "/padlock.js"
            | "/admin.js"
            | "/favicon.svg"
            | "/__admin"
            | "/__admin/flags"
    )
}

/// True when there is no padlock, or the request carries the unlock cookie.
pub fn unlocked(req: &Req) -> bool {
    let Some(password) = app().cfg.demo_password.as_deref() else {
        return true;
    };
    if !active() {
        return true;
    }
    let want = token(password);
    req.header("cookie").is_some_and(|h| {
        h.split(';').any(|part| {
            part.trim()
                .split_once('=')
                .is_some_and(|(k, v)| k.trim() == COOKIE && v.trim() == want)
        })
    })
}

/// What a locked visitor gets: the padlock page, or "locked" from the API.
pub fn locked(req: &Req) -> Resp {
    if req.path == "/api" || req.path.starts_with("/api/") {
        return Resp::json(
            401,
            &json!({ "error": { "code": "locked", "message": "This preview is locked." } }),
        );
    }
    Resp::bytes(200, "text/html; charset=utf-8", PAGE.to_vec())
        .with_header("Cache-Control", "no-store")
}

/// `POST /api/gate {"password"}`: the unlock cookie, or 401.
pub fn unlock(req: &Req) -> Resp {
    let Some(password) = app().cfg.demo_password.as_deref() else {
        return Resp::empty(204);
    };
    if let Err(wait) = app()
        .limiter
        .hit(&format!("gate:ip:{}", req.ip_key()), 10, 600)
    {
        return Resp::error(&ApiError::rate_limited(
            wait,
            "Too many tries. Wait a few minutes and try again.",
        ));
    }
    let given = serde_json::from_slice::<Value>(&req.body)
        .ok()
        .and_then(|v| v["password"].as_str().map(str::to_string))
        .unwrap_or_default();
    if token(&given) != token(password) {
        return Resp::json(
            401,
            &json!({ "error": { "code": "unauthorized", "message": "That’s not the password.", "field": "password" } }),
        );
    }
    let secure = if app().cfg.secure() { "; Secure" } else { "" };
    Resp::empty(204).with_header(
        "Set-Cookie",
        format!(
            "{COOKIE}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={TTL_SECS}{secure}",
            token(password)
        ),
    )
}

/// `GET /__admin`: the hidden feature-flag page. Doesn't exist without `ADMIN_PASSWORD`.
pub fn admin_page() -> Resp {
    if app().cfg.admin_password.is_none() {
        return Resp::error(&ApiError::not_found());
    }
    Resp::bytes(200, "text/html; charset=utf-8", ADMIN_PAGE.to_vec())
        .with_header("Cache-Control", "no-store")
        .with_header("X-Robots-Tag", "noindex")
}

/// `POST /__admin/flags {"password", "padlock"?: bool}`: switch the padlock (when `padlock` is
/// given) and report the current state.
pub fn admin_flags(req: &Req) -> Resp {
    let Some(admin) = app().cfg.admin_password.as_deref() else {
        return Resp::error(&ApiError::not_found());
    };
    if let Err(wait) = app()
        .limiter
        .hit(&format!("admin:ip:{}", req.ip_key()), 10, 600)
    {
        return Resp::error(&ApiError::rate_limited(
            wait,
            "Too many tries. Wait a few minutes and try again.",
        ));
    }
    let body = serde_json::from_slice::<Value>(&req.body).unwrap_or(Value::Null);
    let given = body["password"].as_str().unwrap_or_default();
    if token(given) != token(admin) {
        return Resp::json(
            401,
            &json!({ "error": { "code": "unauthorized", "message": "That’s not the admin password.", "field": "password" } }),
        );
    }
    if let Some(on) = body["padlock"].as_bool()
        && !store_flag(on)
    {
        return Resp::json(
            500,
            &json!({ "error": { "code": "server_error", "message": "Couldn’t save the setting. Try again." } }),
        );
    }
    Resp::json(
        200,
        &json!({
            "padlock": ENABLED.load(Ordering::Relaxed),
            "password_set": app().cfg.demo_password.is_some(),
            "active": active(),
        }),
    )
}
