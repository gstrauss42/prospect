//! The JSON API of docs/API.md: routing and handlers. Requests reach here with their body read
//! and the CSRF rule already checked (see `http`).

use std::net::IpAddr;

use model::{Frequency, Opportunity, Profile, Research};
use rusqlite::{Connection, OptionalExtension, Row};
use serde_json::{Value, json};

use crate::http::{ApiError, Req, Resp};
use crate::runs::{self, Enqueued, RunRow};
use crate::users::{self, User};
use crate::{ai, app, auth, conns, db, mail, util};

type Result<T> = std::result::Result<T, ApiError>;

const RESEARCH_PER_DAY: i64 = 3;
const THEME_PER_DAY: i64 = 5;
const MAX_NOTES: i64 = 20;
/// Discoveries a person starts themselves (counted against `MANUAL_RUNS_PER_DAY`).
const USER_TRIGGERS: &[&str] = &["manual", "profile", "steer"];
const TOO_MANY: &str = "Too many attempts. Please wait a few minutes and try again.";

struct Ctx<'a> {
    req: &'a Req,
    body: Value,
    /// A `Set-Cookie` for the response: a new, extended or cleared session.
    cookie: Option<String>,
    /// The signed-in user's session token, once [`Ctx::user`] has checked it.
    token: Option<String>,
}

pub fn handle(req: &Req) -> Resp {
    let mut ctx = Ctx {
        req,
        body: Value::Null,
        cookie: None,
        token: None,
    };
    let result = parse_body(req).and_then(|body| {
        ctx.body = body;
        route(&mut ctx)
    });
    let mut resp = result.unwrap_or_else(|e| Resp::error(&e));
    if let Some(cookie) = ctx.cookie.take()
        && !resp.has_header("Set-Cookie")
    {
        resp.headers.push(("Set-Cookie", cookie));
    }
    if !resp.has_header("Cache-Control") {
        resp.headers.push(("Cache-Control", "no-store".into()));
    }
    resp
}

fn route(ctx: &mut Ctx) -> Result<Resp> {
    let req = ctx.req;
    let segs: Vec<&str> = req
        .path
        .strip_prefix("/api")
        .unwrap_or("")
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    match (req.method.as_str(), segs.as_slice()) {
        ("GET", ["meta"]) => Ok(meta()),
        ("POST", ["auth", "signup"]) => signup(ctx),
        ("POST", ["auth", "login"]) => login(ctx),
        ("POST", ["auth", "logout"]) => logout(ctx),
        ("POST", ["auth", "forgot"]) => forgot(ctx),
        ("POST", ["auth", "reset"]) => reset(ctx),
        ("GET", ["me"]) => me(ctx),
        ("POST", ["onboarding", "research"]) => research(ctx),
        ("POST", ["intel", "refresh"]) => intel_refresh(ctx),
        ("PUT", ["profile"]) => put_profile(ctx),
        ("GET", ["opportunities"]) => list_opportunities(ctx),
        ("POST", ["opportunities", id, action]) => opportunity_action(ctx, id, action),
        ("POST", ["runs"]) => start_run(ctx),
        ("GET", ["runs", "current"]) => current_run(ctx),
        ("GET", ["steering"]) => list_steering(ctx),
        ("POST", ["steering"]) => add_steering(ctx),
        ("DELETE", ["steering", id]) => delete_steering(ctx, id),
        ("PUT", ["settings"]) => put_settings(ctx),
        ("GET", ["settings", "ai"]) => get_ai(ctx),
        ("PUT", ["settings", "ai"]) => put_ai(ctx),
        ("POST", ["settings", "ai", "test"]) => test_ai(ctx),
        ("GET", ["connectors"]) => list_connectors(ctx),
        ("GET", ["connectors", "audit"]) => connector_audit(ctx),
        ("POST", ["connectors", provider, "start"]) => connector_start(ctx, provider),
        ("GET", ["connectors", provider, "callback"]) => connector_callback(ctx, provider),
        ("POST", ["connectors", provider, "token"]) => connector_token(ctx, provider),
        ("DELETE", ["connectors", provider]) => connector_delete(ctx, provider),
        ("GET", ["theme", "image"]) => theme_image(ctx),
        ("POST", ["theme", "refresh"]) => theme_refresh(ctx),
        ("PUT", ["account"]) => put_account(ctx),
        ("POST", ["account", "password"]) => change_password(ctx),
        ("DELETE", ["account"]) => delete_account(ctx),
        (_, segs) if known_path(segs) => Err(ApiError::method_not_allowed()),
        _ => Err(ApiError::not_found()),
    }
}

fn known_path(segs: &[&str]) -> bool {
    matches!(
        segs,
        ["meta"]
            | ["auth", "signup" | "login" | "logout" | "forgot" | "reset"]
            | ["me"]
            | ["onboarding", "research"]
            | ["intel", "refresh"]
            | ["profile"]
            | ["opportunities"]
            | ["opportunities", _, "save" | "vote" | "dismiss" | "restore"]
            | ["runs"]
            | ["runs", "current"]
            | ["steering"]
            | ["steering", _]
            | ["settings"]
            | ["settings", "ai"]
            | ["settings", "ai", "test"]
            | ["connectors"]
            | ["connectors", _]
            | ["connectors", _, "start" | "callback" | "token"]
            | ["theme", "image" | "refresh"]
            | ["account"]
            | ["account", "password"]
    )
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The JSON object body of a state-changing request (`{}` when empty).
fn parse_body(req: &Req) -> Result<Value> {
    if !matches!(req.method.as_str(), "POST" | "PUT" | "DELETE" | "PATCH")
        || req.body.iter().all(u8::is_ascii_whitespace)
    {
        return Ok(json!({}));
    }
    match serde_json::from_slice::<Value>(&req.body) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => Err(ApiError::bad_request("Send a JSON object.")),
        Err(_) => Err(ApiError::bad_request("The request body isn't valid JSON.")),
    }
}

impl Ctx<'_> {
    /// A string field, or "" when missing or not a string.
    fn text(&self, name: &str) -> &str {
        self.body.get(name).and_then(Value::as_str).unwrap_or("")
    }

    /// An optional string field; 422 when present with another type.
    fn opt_text(&self, name: &'static str) -> Result<Option<&str>> {
        match self.body.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(ApiError::validation(name, "This should be text.")),
        }
    }

    /// An optional boolean field; 422 when present with another type.
    fn opt_bool(&self, name: &'static str) -> Result<Option<bool>> {
        match self.body.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(ApiError::validation(name, "This should be true or false.")),
        }
    }

    /// The signed-in user, or 401. Extends the session (and re-sends the cookie) now and then.
    fn user(&mut self) -> Result<User> {
        let req = self.req;
        let token = req
            .header("cookie")
            .and_then(auth::cookie_token)
            .ok_or_else(ApiError::unauthorized)?;
        let c = db::conn()?;
        let (user_id, refreshed) =
            auth::session_user(&c, token)?.ok_or_else(ApiError::unauthorized)?;
        let user = User::load(&c, user_id)?.ok_or_else(ApiError::unauthorized)?;
        if refreshed {
            self.cookie = Some(auth::session_cookie(token, app().cfg.secure()));
            remember_ip(&c, &user, req.ip)?;
        }
        self.token = Some(token.to_string());
        Ok(user)
    }
}

fn limit(key: String, max: u32, window: i64) -> Result<()> {
    app()
        .limiter
        .hit(&key, max, window)
        .map_err(|wait| ApiError::rate_limited(wait, TOO_MANY))
}

fn public_ip(ip: Option<IpAddr>) -> Option<String> {
    ip.filter(|ip| util::is_public_ip(*ip))
        .map(|ip| ip.to_string())
}

/// Keep the person's last public IP, to localise their web searches.
fn remember_ip(c: &Connection, user: &User, ip: Option<IpAddr>) -> Result<()> {
    if let Some(ip) = public_ip(ip)
        && user.last_ip.as_deref() != Some(ip.as_str())
    {
        c.execute("UPDATE users SET last_ip = ?2 WHERE id = ?1", (user.id, ip))?;
    }
    Ok(())
}

fn reload(c: &Connection, id: i64) -> Result<User> {
    User::load(c, id)?.ok_or_else(ApiError::unauthorized)
}

fn run_resp(status: u16, run: &RunRow) -> Resp {
    Resp::json(status, &json!({ "run": run.json() }))
}

fn not_found_item() -> ApiError {
    ApiError::new(404, "not_found", "We couldn't find that opportunity.")
}

fn rate_limited_runs(wait: i64) -> ApiError {
    ApiError::rate_limited(
        wait,
        "You've refreshed a lot today. Your page also refreshes on its own; try again tomorrow.",
    )
}

/// Start the discovery a person asked for (directly, or by changing their profile or steering),
/// or join the one under way. None when today's allowance of refreshes is used up.
fn request_discover(c: &Connection, user_id: i64, trigger: &'static str) -> Result<Option<RunRow>> {
    let limited = trigger != "signup"
        && runs::daily_limit(
            c,
            user_id,
            "discover",
            USER_TRIGGERS,
            app().cfg.manual_runs_per_day,
        )?
        .is_some();
    if let Some(active) = runs::active(c, user_id)? {
        // A queued discovery reads the latest profile when it starts; anything already under way
        // gets a follow-up discovery when it finishes.
        if !limited && (active.status == "running" || active.kind != "discover") {
            app().runs.want_followup(user_id, trigger);
        }
        return Ok(Some(active));
    }
    if limited {
        return Ok(None);
    }
    Ok(Some(
        runs::enqueue(c, user_id, "discover", trigger, None)?.into_run(),
    ))
}

// ---------------------------------------------------------------------------------------------
// Meta
// ---------------------------------------------------------------------------------------------

fn meta() -> Resp {
    let mut m = model::meta_json();
    m["ai_ready"] = json!(app().ai.is_some());
    m["email_live"] = json!(app().mailer.is_live());
    m["version"] = json!(env!("CARGO_PKG_VERSION"));
    Resp::json(200, &m)
}

// ---------------------------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------------------------

fn email_taken() -> ApiError {
    ApiError::conflict("An account with this email already exists. Try signing in instead.")
        .field("email")
}

fn signup(ctx: &mut Ctx) -> Result<Resp> {
    let business_name = auth::clean_business_name(ctx.text("business_name"))
        .map_err(|m| ApiError::validation("business_name", m))?;
    let website = auth::clean_website(ctx.text("website")).ok_or_else(|| {
        ApiError::validation(
            "website",
            "Enter your business's website, like https://example.com.",
        )
    })?;
    let email = ctx.text("email").trim().to_string();
    if !auth::valid_email(&email) || !mailer::is_valid_email(&email) {
        return Err(ApiError::validation(
            "email",
            "Enter a valid email address.",
        ));
    }
    let password = ctx.text("password").to_string();
    if let Some(problem) = auth::password_problem(&password) {
        return Err(ApiError::validation("password", problem));
    }
    let hints = model::text::squash(ctx.opt_text("hints")?.unwrap_or(""));
    if hints.chars().count() > 300 {
        return Err(ApiError::validation(
            "hints",
            "Keep this under 300 characters.",
        ));
    }
    limit(format!("signup:ip:{}", ctx.req.ip_key()), 20, 3600)?;
    let c = db::conn()?;
    let taken = c
        .query_row("SELECT 1 FROM users WHERE email = ?1", [&email], |_| Ok(()))
        .optional()?
        .is_some();
    if taken {
        return Err(email_taken());
    }
    let hash = auth::hash_password(&password).ok_or_else(ApiError::internal)?;
    let profile = Profile {
        organization: business_name.clone(),
        website: website.clone(),
        ..Profile::default()
    }
    .sanitized();
    let profile = serde_json::to_string(&profile).map_err(|_| ApiError::internal())?;
    // The account and its first research run are created together: no account is ever left
    // "researching" without a run.
    let tx = db::write_tx(&c)?;
    let inserted = tx.execute(
        "INSERT INTO users(email, password_hash, business_name, website, created_at, onboarding, profile_json, frequency, last_ip) \
         VALUES(?1, ?2, ?3, ?4, ?5, 'researching', ?6, ?7, ?8)",
        (&email, &hash, &business_name, &website, util::now(), &profile, app().cfg.default_frequency.as_str(), public_ip(ctx.req.ip)),
    );
    match inserted {
        Err(e) if db::is_constraint(&e) => return Err(email_taken()),
        other => other?,
    };
    let id = tx.last_insert_rowid();
    let token = auth::create_session(&tx, id)?;
    runs::enqueue(
        &tx,
        id,
        "research",
        "signup",
        Some(&json!({ "hints": hints })),
    )?;
    tx.commit()?;
    runs::wake_workers();
    ctx.cookie = Some(auth::session_cookie(&token, app().cfg.secure()));
    let welcome = mailer::welcome(app().mailer.config(), &business_name, &email);
    app().mail.send(mail::Job {
        user_id: Some(id),
        kind: "welcome",
        email: welcome,
    });
    info!("account {id} created");
    Ok(Resp::json(201, &json!({ "user": reload(&c, id)?.json() })))
}

fn login(ctx: &mut Ctx) -> Result<Resp> {
    let email = ctx.text("email").trim().to_string();
    let password = ctx.text("password").to_string();
    if email.is_empty() {
        return Err(ApiError::validation("email", "Enter your email address."));
    }
    if password.is_empty() {
        return Err(ApiError::validation("password", "Enter your password."));
    }
    limit(format!("login:ip:{}", ctx.req.ip_key()), 30, 600)?;
    let email_key = format!("login:email:{}", email.to_lowercase());
    app()
        .limiter
        .check(&email_key, 10, 600)
        .map_err(|wait| ApiError::rate_limited(wait, TOO_MANY))?;
    let c = db::conn()?;
    let found: Option<(i64, String)> = c
        .query_row(
            "SELECT id, password_hash FROM users WHERE email = ?1",
            [&email],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let ok = match &found {
        Some((_, phc)) => auth::verify_password(&password, phc),
        None => {
            auth::burn_verification(&password);
            false
        }
    };
    let Some((user_id, _)) = found.filter(|_| ok) else {
        let _ = app().limiter.hit(&email_key, 10, 600);
        return Err(ApiError::new(
            401,
            "unauthorized",
            "Email or password is incorrect.",
        ));
    };
    app().limiter.clear(&email_key);
    let token = auth::create_session(&c, user_id)?;
    ctx.cookie = Some(auth::session_cookie(&token, app().cfg.secure()));
    let user = reload(&c, user_id)?;
    remember_ip(&c, &user, ctx.req.ip)?;
    Ok(Resp::json(200, &json!({ "user": user.json() })))
}

fn logout(ctx: &mut Ctx) -> Result<Resp> {
    if let Some(token) = ctx.req.header("cookie").and_then(auth::cookie_token) {
        auth::delete_session(&*db::conn()?, token)?;
    }
    ctx.cookie = Some(auth::clear_cookie(app().cfg.secure()));
    Ok(Resp::empty(204))
}

fn forgot(ctx: &mut Ctx) -> Result<Resp> {
    let email = ctx.text("email").trim().to_string();
    if !auth::valid_email(&email) {
        return Err(ApiError::validation(
            "email",
            "Enter a valid email address.",
        ));
    }
    limit(format!("forgot:ip:{}", ctx.req.ip_key()), 10, 3600)?;
    // A few emails per address per hour at most, silently, so nobody can flood an inbox.
    if app()
        .limiter
        .hit(&format!("forgot:email:{}", email.to_lowercase()), 3, 3600)
        .is_ok()
    {
        let c = db::conn()?;
        let found: Option<(i64, String, String)> = c
            .query_row(
                "SELECT id, business_name, email FROM users WHERE email = ?1",
                [&email],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((user_id, business_name, to)) = found {
            let token = util::random_hex(32).ok_or_else(ApiError::internal)?;
            c.execute(
                "INSERT INTO password_resets(token_hash, user_id, expires_at) VALUES(?1, ?2, ?3)",
                (
                    auth::token_hash(&token),
                    user_id,
                    util::now() + auth::RESET_TTL,
                ),
            )?;
            let url = format!("{}/reset?token={token}", app().cfg.public_url);
            let email = mailer::password_reset(app().mailer.config(), &business_name, &to, &url);
            app().mail.send(mail::Job {
                user_id: Some(user_id),
                kind: "reset",
                email,
            });
        }
    }
    Ok(Resp::json(202, &json!({})))
}

fn reset(ctx: &mut Ctx) -> Result<Resp> {
    let token = ctx.text("token").trim().to_string();
    let password = ctx.text("password").to_string();
    if let Some(problem) = auth::password_problem(&password) {
        return Err(ApiError::validation("password", problem));
    }
    limit(format!("reset:ip:{}", ctx.req.ip_key()), 20, 3600)?;
    let expired = || ApiError::bad_request("This link has expired or was already used.");
    if token.is_empty() || token.len() > 200 {
        return Err(expired());
    }
    let hash = auth::token_hash(&token);
    let c = db::conn()?;
    let valid = "SELECT user_id FROM password_resets WHERE token_hash = ?1 AND used = 0 AND expires_at > ?2";
    let user_id: i64 = c
        .query_row(valid, (&hash, util::now()), |r| r.get(0))
        .optional()?
        .ok_or_else(expired)?;
    let phc = auth::hash_password(&password).ok_or_else(ApiError::internal)?;
    let tx = db::write_tx(&c)?;
    if tx.execute("UPDATE password_resets SET used = 1 WHERE token_hash = ?1 AND used = 0 AND expires_at > ?2", (&hash, util::now()))? == 0 {
        return Err(expired());
    }
    tx.execute(
        "UPDATE users SET password_hash = ?2 WHERE id = ?1",
        (user_id, &phc),
    )?;
    tx.execute(
        "UPDATE password_resets SET used = 1 WHERE user_id = ?1",
        [user_id],
    )?;
    auth::revoke_sessions(&tx, user_id, None)?;
    let session = auth::create_session(&tx, user_id)?;
    tx.commit()?;
    ctx.cookie = Some(auth::session_cookie(&session, app().cfg.secure()));
    let user = reload(&c, user_id)?;
    remember_ip(&c, &user, ctx.req.ip)?;
    Ok(Resp::json(200, &json!({ "user": user.json() })))
}

// ---------------------------------------------------------------------------------------------
// Me, onboarding, profile
// ---------------------------------------------------------------------------------------------

fn me(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let c = db::conn()?;
    let active = runs::active(&c, user.id)?;
    let last = runs::last_finished(&c, user.id)?;
    Ok(Resp::json(
        200,
        &json!({
            "user": user.json(),
            "profile": user.profile(),
            "intel": user.intel(),
            "settings": user.settings_json(),
            "theme": users::theme_json(&user, active.as_ref()),
            "ai": ai::settings_json(&user),
            "run": active.as_ref().map(RunRow::json),
            "last_run": last.as_ref().map(RunRow::json),
        }),
    ))
}

/// Research the business again from `review` (e.g. after a failed first attempt).
fn research(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let hints = model::text::squash(ctx.opt_text("hints")?.unwrap_or(""));
    if hints.chars().count() > 300 {
        return Err(ApiError::validation(
            "hints",
            "Keep this under 300 characters.",
        ));
    }
    if user.onboarding != "review" {
        let message = if user.onboarding == "researching" {
            "We're already researching your business. This takes a minute or two."
        } else {
            "Your business is already set up. Refresh its intelligence from your page instead."
        };
        return Err(ApiError::conflict(message));
    }
    let c = db::conn()?;
    if let Some(active) = runs::active(&c, user.id)? {
        return Ok(run_resp(200, &active));
    }
    if let Some(wait) = runs::daily_limit(&c, user.id, "research", &[], RESEARCH_PER_DAY + 1)? {
        return Err(ApiError::rate_limited(
            wait,
            "You've researched your business a few times today. Fill in the profile instead, or try again tomorrow.",
        ));
    }
    let tx = db::write_tx(&c)?;
    let (status, run) = match runs::enqueue(
        &tx,
        user.id,
        "research",
        "signup",
        Some(&json!({ "hints": hints })),
    )? {
        Enqueued::New(run) => {
            tx.execute(
                "UPDATE users SET onboarding = 'researching' WHERE id = ?1",
                [user.id],
            )?;
            (202, run)
        }
        Enqueued::Existing(run) => (200, run),
    };
    tx.commit()?;
    runs::wake_workers();
    Ok(run_resp(status, &run))
}

/// Re-research the business: the intel is replaced, the profile only filled where empty.
fn intel_refresh(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let c = db::conn()?;
    if let Some(active) = runs::active(&c, user.id)? {
        return Ok(run_resp(200, &active));
    }
    if user.onboarding == "researching" {
        return Err(ApiError::conflict(
            "We're already researching your business.",
        ));
    }
    if app().ai.is_none() {
        return Err(ApiError::ai_unavailable(runs::MSG_NO_AI));
    }
    if let Some(wait) = runs::daily_limit(&c, user.id, "research", &["manual"], RESEARCH_PER_DAY)? {
        return Err(ApiError::rate_limited(
            wait,
            "You've refreshed your business intelligence a few times today. Try again tomorrow.",
        ));
    }
    Ok(
        match runs::enqueue(&c, user.id, "research", "manual", None)? {
            Enqueued::New(run) => run_resp(202, &run),
            Enqueued::Existing(run) => run_resp(200, &run),
        },
    )
}

fn put_profile(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    if user.onboarding == "researching" {
        return Err(ApiError::conflict(
            "We're still looking you up. This takes about a minute.",
        ));
    }
    let Some(incoming) = ctx.body.get("profile").and_then(Value::as_object) else {
        return Err(ApiError::validation("profile", "Send the profile to save."));
    };
    let stored = user.profile();
    // Missing fields keep their current value.
    let mut merged = serde_json::to_value(&stored).unwrap_or_else(|_| json!({}));
    for (key, value) in incoming {
        if key != "research" {
            merged[key.as_str()] = value.clone();
        }
    }
    merged["research"] = Value::Null;
    let mut profile: Profile = serde_json::from_value(merged)
        .map_err(|_| ApiError::validation("profile", "Some profile fields have the wrong type."))?;
    // Research notes are kept only as the server stored them: the client can't forge them.
    profile.research = match incoming.get("research") {
        None => stored.research.clone(),
        Some(claimed) => {
            let claimed: Option<Research> = serde_json::from_value(claimed.clone()).unwrap_or(None);
            let claimed = Profile {
                research: claimed,
                ..Profile::default()
            }
            .sanitized()
            .research;
            claimed.filter(|r| Some(r) == stored.research.as_ref())
        }
    };
    let profile = profile.sanitized();
    if profile.is_empty() {
        return Err(ApiError::validation(
            "headline",
            "Tell us a little about yourself: a headline, a few interests, or what you're looking for.",
        ));
    }
    let changed = profile != stored;
    let text = serde_json::to_string(&profile).map_err(|_| ApiError::internal())?;
    let c = db::conn()?;
    let tx = db::write_tx(&c)?;
    let run = if user.onboarding == "ready" {
        tx.execute(
            "UPDATE users SET profile_json = ?2 WHERE id = ?1",
            (user.id, &text),
        )?;
        if changed {
            request_discover(&tx, user.id, "profile")?
        } else {
            runs::active(&tx, user.id)?
        }
    } else {
        tx.execute(
            "UPDATE users SET profile_json = ?2, onboarding = 'ready' WHERE id = ?1",
            (user.id, &text),
        )?;
        request_discover(&tx, user.id, "signup")?
    };
    tx.commit()?;
    runs::wake_workers();
    Ok(Resp::json(
        200,
        &json!({ "profile": profile, "user": reload(&c, user.id)?.json(), "run": run.as_ref().map(RunRow::json) }),
    ))
}

// ---------------------------------------------------------------------------------------------
// Opportunities
// ---------------------------------------------------------------------------------------------

const ITEM_COLS: &str = "id, data_json, status, vote, first_seen_at, first_seen_run";

/// The id of the user's latest completed discovery: its first finds are "new".
fn latest_discover(c: &Connection, user_id: i64) -> rusqlite::Result<Option<i64>> {
    c.query_row(
        "SELECT MAX(id) FROM runs WHERE user_id = ?1 AND kind = 'discover' AND status = 'done'",
        [user_id],
        |r| r.get(0),
    )
}

/// The API's Item: the opportunity plus its bookkeeping.
fn item_json(r: &Row, latest: Option<i64>) -> rusqlite::Result<Value> {
    let data: String = r.get(1)?;
    let opportunity: Opportunity = serde_json::from_str(&data).unwrap_or_default();
    let mut item = serde_json::to_value(opportunity).unwrap_or_else(|_| json!({}));
    let first_seen_run: Option<i64> = r.get(5)?;
    item["id"] = json!(r.get::<_, i64>(0)?);
    item["status"] = json!(r.get::<_, String>(2)?);
    item["is_new"] = json!(latest.is_some() && first_seen_run == latest);
    item["vote"] = json!(r.get::<_, i64>(3)?);
    item["first_seen_at"] = json!(r.get::<_, i64>(4)?);
    Ok(item)
}

fn list_opportunities(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let c = db::conn()?;
    let latest = latest_discover(&c, user.id)?;
    let only_saved = if ctx.req.query_param("view") == Some("saved") {
        "AND status = 'saved'"
    } else {
        ""
    };
    let sql = format!(
        "SELECT {ITEM_COLS} FROM opportunities WHERE user_id = ?1 AND status IN ('active', 'saved') {only_saved} \
         ORDER BY COALESCE(first_seen_run = ?2, 0) DESC, score DESC, last_seen_at DESC, id DESC"
    );
    let items = c
        .prepare(&sql)?
        .query_map((user.id, latest), |r| item_json(r, latest))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let (active, saved, new): (i64, i64, i64) = c.query_row(
        "SELECT COALESCE(SUM(status = 'active'), 0), COALESCE(SUM(status = 'saved'), 0), COALESCE(SUM(COALESCE(first_seen_run = ?2, 0)), 0) \
         FROM opportunities WHERE user_id = ?1 AND status IN ('active', 'saved')",
        (user.id, latest),
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(Resp::json(
        200,
        &json!({
            "items": items,
            "counts": { "active": active, "saved": saved, "new": new },
            "last_refresh": user.last_run_at,
            "next_refresh": user.next_run_at,
        }),
    ))
}

fn opportunity_action(ctx: &mut Ctx, id: &str, action: &str) -> Result<Resp> {
    let user = ctx.user()?;
    if !matches!(action, "save" | "vote" | "dismiss" | "restore") {
        return Err(ApiError::not_found());
    }
    let id: i64 = id.parse().map_err(|_| not_found_item())?;
    let c = db::conn()?;
    let status: String = c
        .query_row(
            "SELECT status FROM opportunities WHERE id = ?1 AND user_id = ?2",
            (id, user.id),
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(not_found_item)?;
    let listed = status == "active" || status == "saved";
    let now = util::now();
    match action {
        "save" => {
            let saved = match ctx.body.get("saved") {
                None => true,
                Some(Value::Bool(b)) => *b,
                Some(_) => {
                    return Err(ApiError::validation(
                        "saved",
                        "This should be true or false.",
                    ));
                }
            };
            if !listed {
                return Err(not_found_item());
            }
            c.execute(
                "UPDATE opportunities SET status = ?2, updated_at = ?3 WHERE id = ?1",
                (id, if saved { "saved" } else { "active" }, now),
            )?;
        }
        "vote" => {
            let vote = ctx
                .body
                .get("vote")
                .and_then(Value::as_i64)
                .filter(|v| (-1..=1).contains(v))
                .ok_or_else(|| ApiError::validation("vote", "Vote with 1, 0 or -1."))?;
            if !listed {
                return Err(not_found_item());
            }
            c.execute(
                "UPDATE opportunities SET vote = ?2, updated_at = ?3 WHERE id = ?1",
                (id, vote, now),
            )?;
        }
        "dismiss" => {
            if status == "expired" {
                return Err(not_found_item());
            }
            c.execute(
                "UPDATE opportunities SET status = 'dismissed', updated_at = ?2 WHERE id = ?1",
                (id, now),
            )?;
            return Ok(Resp::json(200, &json!({ "ok": true })));
        }
        _ => {
            // restore: undo a dismiss
            if status == "expired" {
                return Err(not_found_item());
            }
            if status == "dismissed" {
                c.execute(
                    "UPDATE opportunities SET status = 'active', updated_at = ?2 WHERE id = ?1",
                    (id, now),
                )?;
            }
        }
    }
    let latest = latest_discover(&c, user.id)?;
    let item = c.query_row(
        &format!("SELECT {ITEM_COLS} FROM opportunities WHERE id = ?1"),
        [id],
        |r| item_json(r, latest),
    )?;
    Ok(Resp::json(200, &json!({ "item": item })))
}

// ---------------------------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------------------------

fn start_run(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    if ctx.opt_text("kind")?.unwrap_or("discover") != "discover" {
        return Err(ApiError::validation(
            "kind",
            "Only discovery runs can be started here.",
        ));
    }
    if user.onboarding != "ready" {
        return Err(ApiError::conflict("Finish setting up your profile first."));
    }
    let c = db::conn()?;
    if let Some(active) = runs::active(&c, user.id)? {
        return Ok(run_resp(200, &active));
    }
    if app().ai.is_none() {
        return Err(ApiError::ai_unavailable(runs::MSG_NO_AI));
    }
    if let Some(wait) = runs::daily_limit(
        &c,
        user.id,
        "discover",
        USER_TRIGGERS,
        app().cfg.manual_runs_per_day,
    )? {
        return Err(rate_limited_runs(wait));
    }
    if runs::over_budget(&c)? {
        let mut e = ApiError::ai_unavailable(runs::MSG_BUDGET);
        e.retry_after = Some(runs::BUDGET_DELAY_SECS);
        return Err(e);
    }
    Ok(
        match runs::enqueue(&c, user.id, "discover", "manual", None)? {
            Enqueued::New(run) => run_resp(202, &run),
            Enqueued::Existing(run) => run_resp(200, &run),
        },
    )
}

fn current_run(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let c = db::conn()?;
    let active = runs::active(&c, user.id)?;
    let last = runs::last_finished(&c, user.id)?;
    Ok(Resp::json(
        200,
        &json!({
            "run": active.as_ref().map(RunRow::json),
            "last_run": last.as_ref().map(RunRow::json),
            "theme": users::theme_json(&user, active.as_ref()),
        }),
    ))
}

// ---------------------------------------------------------------------------------------------
// Steering
// ---------------------------------------------------------------------------------------------

fn list_steering(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let c = db::conn()?;
    let notes = c
        .prepare_cached("SELECT id, text, created_at FROM steering WHERE user_id = ?1 ORDER BY id DESC LIMIT ?2")?
        .query_map((user.id, MAX_NOTES), |r| {
            Ok(json!({ "id": r.get::<_, i64>(0)?, "text": r.get::<_, String>(1)?, "created_at": r.get::<_, i64>(2)? }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Resp::json(200, &json!({ "notes": notes })))
}

fn add_steering(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let text = model::text::squash(ctx.opt_text("text")?.unwrap_or(""));
    match text.chars().count() {
        0 => {
            return Err(ApiError::validation(
                "text",
                "Write a short note, like \u{201c}more remote roles\u{201d}.",
            ));
        }
        n if n > 300 => {
            return Err(ApiError::validation(
                "text",
                "Keep notes under 300 characters.",
            ));
        }
        _ => {}
    }
    let refresh = ctx.opt_bool("refresh")?.unwrap_or(false);
    let c = db::conn()?;
    let now = util::now();
    let tx = db::write_tx(&c)?;
    tx.execute(
        "INSERT INTO steering(user_id, text, created_at) VALUES(?1, ?2, ?3)",
        (user.id, &text, now),
    )?;
    let id = tx.last_insert_rowid();
    tx.execute(
        "DELETE FROM steering WHERE user_id = ?1 AND id NOT IN (SELECT id FROM steering WHERE user_id = ?1 ORDER BY id DESC LIMIT ?2)",
        (user.id, MAX_NOTES),
    )?;
    let run = if refresh && user.onboarding == "ready" {
        request_discover(&tx, user.id, "steer")?
    } else {
        None
    };
    tx.commit()?;
    runs::wake_workers();
    Ok(Resp::json(
        201,
        &json!({ "note": { "id": id, "text": text, "created_at": now }, "run": run.as_ref().map(RunRow::json) }),
    ))
}

fn delete_steering(ctx: &mut Ctx, id: &str) -> Result<Resp> {
    let user = ctx.user()?;
    let id: i64 = id.parse().map_err(|_| ApiError::not_found())?;
    if db::conn()?.execute(
        "DELETE FROM steering WHERE id = ?1 AND user_id = ?2",
        (id, user.id),
    )? == 0
    {
        return Err(ApiError::not_found());
    }
    Ok(Resp::empty(204))
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

fn put_settings(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let bad_frequency = || {
        ApiError::validation(
            "frequency",
            "Choose daily, weekly, fortnightly, monthly or off.",
        )
    };
    let frequency = match ctx.body.get("frequency") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(Frequency::parse(s).ok_or_else(bad_frequency)?),
        Some(_) => return Err(bad_frequency()),
    };
    let digest = ctx.opt_bool("email_digest")?;
    let c = db::conn()?;
    if let Some(f) = frequency.filter(|f| *f != user.frequency()) {
        // The next refresh counts from the last one (never in the past); none until onboarded.
        let now = util::now();
        let next = (user.onboarding == "ready")
            .then(|| {
                f.interval_secs()
                    .map(|i| (user.last_run_at.unwrap_or(now) + i).max(now))
            })
            .flatten();
        c.execute(
            "UPDATE users SET frequency = ?2, next_run_at = ?3 WHERE id = ?1",
            (user.id, f.as_str(), next),
        )?;
    }
    if let Some(d) = digest {
        c.execute(
            "UPDATE users SET email_digest = ?2 WHERE id = ?1",
            (user.id, d),
        )?;
    }
    Ok(Resp::json(
        200,
        &json!({ "settings": reload(&c, user.id)?.settings_json() }),
    ))
}

// ---------------------------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------------------------

fn theme_image(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let theme = user.theme().unwrap_or_default();
    let (Some(file), Some(image)) = (theme.file, theme.image) else {
        return Err(ApiError::not_found());
    };
    if file.contains('/') || file.contains('\\') || file.starts_with('.') {
        return Err(ApiError::not_found());
    }
    let bytes = std::fs::read(app().cfg.data_dir.join("themes").join(&file))
        .map_err(|_| ApiError::not_found())?;
    let content_type = match image.mime.as_str() {
        "image/png" => "image/png",
        "image/webp" => "image/webp",
        _ => "image/jpeg",
    };
    Ok(Resp::bytes(200, content_type, bytes)
        .with_header("Cache-Control", "private, max-age=31536000, immutable"))
}

fn theme_refresh(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    if user.onboarding != "ready" {
        return Err(ApiError::conflict("Finish setting up your profile first."));
    }
    let c = db::conn()?;
    if let Some(active) = runs::active(&c, user.id)? {
        return Ok(run_resp(200, &active));
    }
    if app().ai.is_none() {
        return Err(ApiError::ai_unavailable(runs::MSG_NO_AI));
    }
    if let Some(wait) = runs::daily_limit(&c, user.id, "theme", &[], THEME_PER_DAY)? {
        return Err(ApiError::rate_limited(
            wait,
            "You've changed the look of your page a few times today. Try again tomorrow.",
        ));
    }
    Ok(match runs::enqueue(&c, user.id, "theme", "manual", None)? {
        Enqueued::New(run) => run_resp(202, &run),
        Enqueued::Existing(run) => run_resp(200, &run),
    })
}

// ---------------------------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------------------------

fn put_account(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let business_name = match ctx.opt_text("business_name")? {
        Some(s) => {
            auth::clean_business_name(s).map_err(|m| ApiError::validation("business_name", m))?
        }
        None => user.business_name.clone(),
    };
    let website = match ctx.opt_text("website")? {
        Some(s) => auth::clean_website(s).ok_or_else(|| {
            ApiError::validation(
                "website",
                "Enter your business's website, like https://example.com.",
            )
        })?,
        None => user.website.clone(),
    };
    // The profile names the same business.
    let mut profile = user.profile();
    profile.organization = business_name.clone();
    profile.website = website.clone();
    let profile = serde_json::to_string(&profile.sanitized()).map_err(|_| ApiError::internal())?;
    let c = db::conn()?;
    c.execute(
        "UPDATE users SET business_name = ?2, website = ?3, profile_json = ?4 WHERE id = ?1",
        (user.id, &business_name, &website, &profile),
    )?;
    Ok(Resp::json(
        200,
        &json!({ "user": reload(&c, user.id)?.json() }),
    ))
}

fn password_matches(c: &Connection, user_id: i64, password: &str) -> Result<bool> {
    let phc: String = c.query_row(
        "SELECT password_hash FROM users WHERE id = ?1",
        [user_id],
        |r| r.get(0),
    )?;
    Ok(auth::verify_password(password, &phc))
}

fn change_password(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let current = ctx.text("current_password").to_string();
    let new = ctx.text("new_password").to_string();
    if let Some(problem) = auth::password_problem(&new) {
        return Err(ApiError::validation("new_password", problem));
    }
    limit(format!("password:user:{}", user.id), 10, 3600)?;
    let c = db::conn()?;
    if !password_matches(&c, user.id, &current)? {
        return Err(
            ApiError::new(401, "unauthorized", "Your current password isn't right.")
                .field("current_password"),
        );
    }
    let phc = auth::hash_password(&new).ok_or_else(ApiError::internal)?;
    let tx = db::write_tx(&c)?;
    tx.execute(
        "UPDATE users SET password_hash = ?2 WHERE id = ?1",
        (user.id, &phc),
    )?;
    auth::revoke_sessions(&tx, user.id, ctx.token.as_deref())?;
    tx.commit()?;
    Ok(Resp::empty(204))
}

fn delete_account(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let password = ctx.text("password").to_string();
    limit(format!("password:user:{}", user.id), 10, 3600)?;
    let c = db::conn()?;
    if !password_matches(&c, user.id, &password)? {
        return Err(
            ApiError::new(401, "unauthorized", "Your password isn't right.").field("password"),
        );
    }
    app().runs.cancel_user(user.id);
    c.execute("DELETE FROM users WHERE id = ?1", [user.id])?;
    let dir = app().cfg.data_dir.join("themes");
    let prefix = format!("{}-", user.id);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    ctx.cookie = Some(auth::clear_cookie(app().cfg.secure()));
    info!("account {} deleted", user.id);
    Ok(Resp::empty(204))
}

// ---------------------------------------------------------------------------------------------
// AI model
// ---------------------------------------------------------------------------------------------

fn get_ai(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    Ok(Resp::json(200, &ai::settings_json(&user)))
}

fn put_ai(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let settings = ai::update(&user, &ctx.body)?;
    let c = db::conn()?;
    c.execute(
        "UPDATE users SET ai_json = ?2 WHERE id = ?1",
        (
            user.id,
            serde_json::to_string(&settings).map_err(|_| ApiError::internal())?,
        ),
    )?;
    Ok(Resp::json(200, &ai::settings_json(&reload(&c, user.id)?)))
}

fn test_ai(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    limit(format!("ai-test:user:{}", user.id), 10, 3600)?;
    let (ok, message) = ai::test(&user);
    Ok(Resp::json(200, &json!({ "ok": ok, "message": message })))
}

// ---------------------------------------------------------------------------------------------
// Connectors
// ---------------------------------------------------------------------------------------------

fn provider(name: &str) -> Result<connectors::Provider> {
    connectors::Provider::parse(name)
        .ok_or_else(|| ApiError::new(404, "not_found", "We don't connect to that service."))
}

fn connector_error(e: connectors::Error) -> ApiError {
    match e {
        connectors::Error::Unsupported(m) | connectors::Error::NotConfigured(m) => {
            ApiError::conflict(m)
        }
        other => ApiError::validation("token", other.to_string()),
    }
}

fn list_connectors(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let connections = conns::list(&*db::conn()?, user.id)?;
    Ok(Resp::json(
        200,
        &json!({ "available": app().connectors.availability(), "connections": connections }),
    ))
}

fn connector_start(ctx: &mut Ctx, name: &str) -> Result<Resp> {
    let user = ctx.user()?;
    let p = provider(name)?;
    limit(format!("connect:user:{}", user.id), 20, 3600)?;
    let state = util::random_hex(24).ok_or_else(ApiError::internal)?;
    let (url, verifier) = app().connectors.begin(p, &state).map_err(connector_error)?;
    conns::save_state(&*db::conn()?, user.id, p, &state, &verifier)?;
    Ok(Resp::json(200, &json!({ "url": url })))
}

/// The OAuth redirect back from the provider: finish, then send the browser to the page.
fn connector_callback(ctx: &mut Ctx, name: &str) -> Result<Resp> {
    let redirect =
        |query: String| Ok(Resp::empty(302).with_header("Location", format!("/?{query}")));
    let fail =
        |message: &str| redirect(format!("connector_error={}", util::percent_encode(message)));
    let Ok(user) = ctx.user() else {
        return fail("Please sign in, then connect again.");
    };
    let Ok(p) = provider(name) else {
        return fail("We don't connect to that service.");
    };
    let param = |k: &str| {
        ctx.req
            .query_param(k)
            .map(util::percent_decode)
            .unwrap_or_default()
    };
    if !param("error").is_empty() {
        return fail(&format!(
            "{} wasn't connected: {}",
            p.label(),
            param("error_description")
                .chars()
                .take(200)
                .collect::<String>()
        ));
    }
    let (code, state) = (param("code"), param("state"));
    let c = db::conn()?;
    let Some(verifier) = conns::take_state(&c, user.id, p, &state)? else {
        return fail("That sign-in link expired. Please connect again.");
    };
    if code.is_empty() {
        return fail("The service didn't send a sign-in code. Please connect again.");
    }
    match app()
        .connectors
        .finish(p, &code, &verifier, &conns::DbAudit { user_id: user.id })
    {
        Ok(cred) => {
            conns::store(&c, user.id, &cred)?;
            info!("account {} connected {}", user.id, p.id());
            redirect(format!("connected={}", p.id()))
        }
        Err(e) => fail(&e.to_string()),
    }
}

fn connector_token(ctx: &mut Ctx, name: &str) -> Result<Resp> {
    let user = ctx.user()?;
    let p = provider(name)?;
    let input = connectors::TokenInput {
        token: ctx.text("token").trim().to_string(),
        email: ctx.text("email").trim().to_string(),
        site: ctx.text("site").trim().to_string(),
    };
    if input.token.is_empty() {
        return Err(ApiError::validation(
            "token",
            "Paste the token to connect with.",
        ));
    }
    limit(format!("connect:user:{}", user.id), 20, 3600)?;
    let cred = app()
        .connectors
        .connect_token(p, &input, &conns::DbAudit { user_id: user.id })
        .map_err(connector_error)?;
    let c = db::conn()?;
    conns::store(&c, user.id, &cred)?;
    let connection = conns::list(&c, user.id)?
        .into_iter()
        .find(|v| v["provider"] == p.id())
        .unwrap_or(Value::Null);
    info!("account {} connected {} with a token", user.id, p.id());
    Ok(Resp::json(201, &json!({ "connection": connection })))
}

fn connector_delete(ctx: &mut Ctx, name: &str) -> Result<Resp> {
    let user = ctx.user()?;
    let p = provider(name)?;
    if db::conn()?.execute(
        "DELETE FROM connections WHERE user_id = ?1 AND provider = ?2",
        (user.id, p.id()),
    )? == 0
    {
        return Err(ApiError::not_found());
    }
    Ok(Resp::empty(204))
}

fn connector_audit(ctx: &mut Ctx) -> Result<Resp> {
    let user = ctx.user()?;
    let limit: i64 = ctx
        .req
        .query_param("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
        .clamp(1, 500);
    let c = db::conn()?;
    let events = c
        .prepare_cached("SELECT provider, action, method, target, status, bytes, ms, at, note FROM audit_events WHERE user_id = ?1 ORDER BY id DESC LIMIT ?2")?
        .query_map((user.id, limit), |r| {
            Ok(json!({
                "provider": r.get::<_, String>(0)?,
                "action": r.get::<_, String>(1)?,
                "method": r.get::<_, String>(2)?,
                "target": r.get::<_, String>(3)?,
                "status": r.get::<_, i64>(4)?,
                "bytes": r.get::<_, i64>(5)?,
                "ms": r.get::<_, i64>(6)?,
                "at": r.get::<_, i64>(7)?,
                "note": r.get::<_, String>(8)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Resp::json(200, &json!({ "events": events })))
}
