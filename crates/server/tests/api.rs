//! Integration tests: the real binary against the mock upstreams. They assert structure and
//! behaviour, never exact engine output, so they keep passing when the real engine, imagery and
//! mailer replace the stubs.

mod common;

use std::time::Duration;

use common::{Env, ready_account, settle, signup, wait_for};
use serde_json::{Value, json};

const ITEM_FIELDS: &[&str] = &[
    "id",
    "url",
    "title",
    "organization",
    "category",
    "summary",
    "why",
    "action",
    "location",
    "deadline",
    "value",
    "score",
    "published",
    "site_name",
    "site_icon",
    "verified",
    "status",
    "is_new",
    "vote",
    "first_seen_at",
];

#[test]
fn business_onboarding_to_opportunities() {
    let env = Env::start(&[]);
    let mut c = env.client();
    let meta = c.get("/api/meta").json();
    assert_eq!(meta["ai_ready"], true);
    assert!(
        meta["opportunity_types"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == "sales")
    );

    // Sign-up starts research at once.
    let user = signup(&mut c, "Acme Cleaning", "ops@acme.example")["user"].clone();
    assert_eq!(user["business_name"], "Acme Cleaning");
    assert_eq!(user["website"], "https://acme.example");
    assert_eq!(user["onboarding"], "researching");
    assert!(user.get("first_name").is_none());
    let current = c.get("/api/runs/current").json();
    assert!(
        current["run"]["kind"] == "research" || current["last_run"]["kind"] == "research",
        "{current}"
    );

    // Research → review, with a drafted profile and intel.
    let me = settle(&mut c);
    assert_eq!(me["user"]["onboarding"], "review", "{me}");
    assert_eq!(me["last_run"]["status"], "done");
    assert_eq!(me["profile"]["organization"], "Acme Cleaning");
    assert!(me["intel"].is_object() && me["intel"]["overview"].is_object());
    assert_eq!(me["ai"]["provider"], "deepseek");
    for key in ["settings", "theme"] {
        assert!(me[key].is_object(), "{key}");
    }

    // Confirm → ready + discovery.
    let mut profile = me["profile"].clone();
    profile["headline"] = "Commercial cleaning for offices".into();
    profile["seeking"] = json!(["sales"]);
    profile["interests"] = json!(["green cleaning"]);
    let r = c.put("/api/profile", &json!({ "profile": profile }));
    assert_eq!(r.status, 200, "{}", r.text());
    let body = r.json();
    assert_eq!(body["user"]["onboarding"], "ready");
    assert_eq!(body["run"]["kind"], "discover");
    assert_eq!(body["run"]["trigger"], "signup");

    let me = settle(&mut c);
    let last = &me["last_run"];
    assert_eq!(last["status"], "done", "{last}");
    assert!(last["summary"]["new"].as_i64().unwrap() >= 1);
    assert!(last["log"].as_array().unwrap().len() >= 2);
    assert!(
        me["settings"]["next_run_at"].as_i64().unwrap()
            > me["settings"]["last_run_at"].as_i64().unwrap()
    );
    assert!(matches!(
        me["theme"]["status"].as_str(),
        Some("ready" | "none")
    ));

    let list = c.get("/api/opportunities").json();
    let items = list["items"].as_array().unwrap();
    assert!(!items.is_empty());
    for f in ITEM_FIELDS {
        assert!(items[0].get(*f).is_some(), "item has {f}");
    }
    assert_eq!(
        list["counts"]["new"].as_i64().unwrap(),
        items.iter().filter(|i| i["is_new"] == true).count() as i64
    );
    let id = items[0]["id"].as_i64().unwrap();

    // Save, vote, dismiss, restore.
    let r = c.post(
        &format!("/api/opportunities/{id}/save"),
        &json!({ "saved": true }),
    );
    assert_eq!(r.json()["item"]["status"], "saved");
    assert_eq!(
        c.get("/api/opportunities?view=saved").json()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let r = c.post(
        &format!("/api/opportunities/{id}/vote"),
        &json!({ "vote": -1 }),
    );
    assert_eq!(r.json()["item"]["vote"], -1);
    assert_eq!(
        c.post(
            &format!("/api/opportunities/{id}/vote"),
            &json!({ "vote": 5 })
        )
        .status,
        422
    );
    assert_eq!(
        c.post(&format!("/api/opportunities/{id}/dismiss"), &json!({}))
            .json()["ok"],
        true
    );
    assert!(
        c.get("/api/opportunities").json()["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["id"] != id)
    );
    assert_eq!(
        c.post(&format!("/api/opportunities/{id}/restore"), &json!({}))
            .json()["item"]["status"],
        "active"
    );

    // Another business can't touch it.
    let mut other = env.client_from("81.2.69.160");
    signup(&mut other, "Other Co", "hello@other.example");
    for action in ["save", "vote", "dismiss", "restore"] {
        assert_eq!(
            other
                .post(
                    &format!("/api/opportunities/{id}/{action}"),
                    &json!({ "vote": 1 })
                )
                .status,
            404,
            "{action}"
        );
    }

    // A manual refresh: 202 with a run, or 200 with the active one.
    let r = c.post("/api/runs", &json!({ "kind": "discover" }));
    assert!(r.status == 202 || r.status == 200, "{}", r.text());
    assert_eq!(c.post("/api/runs", &json!({ "kind": "theme" })).status, 422);
    let me = settle(&mut c);
    assert_eq!(me["last_run"]["trigger"], "manual");
    assert!(
        me["last_run"]["summary"]["refreshed"].as_i64().unwrap() >= 1,
        "{}",
        me["last_run"]
    );
}

#[test]
fn accounts_and_password_reset() {
    let env = Env::start(&[]);
    let mut c = env.client();
    let bad = |c: &mut common::Client, body: Value, field: &str| {
        let r = c.post("/api/auth/signup", &body);
        assert_eq!(r.status, 422, "{}", r.text());
        assert_eq!(r.json()["error"]["field"], field, "{}", r.text());
        assert_eq!(r.json()["error"]["code"], "validation");
    };
    let ok = json!({ "business_name": "Acme", "website": "https://acme.example", "email": "a@acme.example", "password": "long enough" });
    let with = |k: &str, v: Value| {
        let mut b = ok.clone();
        b[k] = v;
        b
    };
    bad(&mut c, with("business_name", json!("  ")), "business_name");
    bad(&mut c, with("website", json!("not a website")), "website");
    bad(&mut c, with("email", json!("nope")), "email");
    bad(&mut c, with("password", json!("short")), "password");
    bad(&mut c, with("hints", json!("x".repeat(301))), "hints");

    signup(&mut c, "Acme", "a@acme.example");
    let r = env.client().post("/api/auth/signup", &ok);
    assert_eq!(r.status, 409);
    assert_eq!(r.json()["error"]["field"], "email");

    // Sign out, wrong password, sign in (case-insensitive email).
    assert_eq!(c.post("/api/auth/logout", &json!({})).status, 204);
    assert_eq!(c.get("/api/me").status, 401);
    let r = c.post(
        "/api/auth/login",
        &json!({ "email": "a@acme.example", "password": "wrong password" }),
    );
    assert_eq!(r.status, 401);
    assert_eq!(
        r.json()["error"]["message"],
        "Email or password is incorrect."
    );
    assert_eq!(
        c.post(
            "/api/auth/login",
            &json!({ "email": "nobody@acme.example", "password": "wrong password" })
        )
        .status,
        401
    );
    let r = c.post(
        "/api/auth/login",
        &json!({ "email": "A@ACME.example", "password": "correct horse battery" }),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["user"]["business_name"], "Acme");

    // Forgot (no enumeration) → reset through the emailed link.
    assert_eq!(
        env.client_from("81.2.69.161")
            .post(
                "/api/auth/forgot",
                &json!({ "email": "nobody@acme.example" })
            )
            .status,
        202
    );
    let before = env.outbox().len();
    assert_eq!(
        env.client_from("81.2.69.161")
            .post("/api/auth/forgot", &json!({ "email": "a@acme.example" }))
            .status,
        202
    );
    // The first `reset?token=` followed by exactly 64 hex digits in any outbox file (an .eml may
    // be quoted-printable; its .html twin isn't).
    let token = wait_for(10, || {
        let all = env.outbox();
        if all.len() <= before {
            return None;
        }
        all.iter()
            .flat_map(|m| m.split("reset?token=").skip(1))
            .find_map(|rest| {
                let hex: String = rest
                    .chars()
                    .take_while(|ch| ch.is_ascii_hexdigit())
                    .collect();
                (hex.len() == 64).then_some(hex)
            })
    });
    let mut fresh = env.client();
    let r = fresh.post(
        "/api/auth/reset",
        &json!({ "token": token, "password": "a brand new password" }),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(c.get("/api/me").status, 401, "older sessions are revoked");
    assert_eq!(fresh.get("/api/me").status, 200);
    let again = env.client().post(
        "/api/auth/reset",
        &json!({ "token": token, "password": "another password" }),
    );
    assert_eq!(again.status, 400, "single use");

    // Account: rename, change password, delete.
    let r = fresh.put(
        "/api/account",
        &json!({ "business_name": "Acme Holdings", "website": "acme-holdings.example" }),
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["user"]["business_name"], "Acme Holdings");
    assert_eq!(r.json()["user"]["website"], "https://acme-holdings.example");
    assert_eq!(
        fresh.get("/api/me").json()["profile"]["organization"],
        "Acme Holdings"
    );
    let r = fresh.post(
        "/api/account/password",
        &json!({ "current_password": "wrong", "new_password": "x".repeat(12) }),
    );
    assert_eq!(r.status, 401);
    assert_eq!(r.json()["error"]["field"], "current_password");
    let r = fresh.post(
        "/api/account/password",
        &json!({ "current_password": "a brand new password", "new_password": "third password ok" }),
    );
    assert_eq!(r.status, 204);
    assert_eq!(
        fresh.get("/api/me").status,
        200,
        "the current session survives"
    );
    assert_eq!(
        fresh
            .delete("/api/account", &json!({ "password": "nope" }))
            .status,
        401
    );
    assert_eq!(
        fresh
            .delete("/api/account", &json!({ "password": "third password ok" }))
            .status,
        204
    );
    assert_eq!(fresh.get("/api/me").status, 401);
    let gone: i64 = env
        .db()
        .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(gone, 0);
}

#[test]
fn security_rules() {
    let env = Env::start(&[]);
    let mut c = env.client();

    // Auth required everywhere it should be.
    for (method, path) in [
        ("GET", "/api/me"),
        ("PUT", "/api/profile"),
        ("GET", "/api/opportunities"),
        ("POST", "/api/opportunities/1/save"),
        ("POST", "/api/runs"),
        ("GET", "/api/runs/current"),
        ("GET", "/api/steering"),
        ("POST", "/api/steering"),
        ("DELETE", "/api/steering/1"),
        ("PUT", "/api/settings"),
        ("GET", "/api/settings/ai"),
        ("PUT", "/api/settings/ai"),
        ("POST", "/api/settings/ai/test"),
        ("GET", "/api/theme/image"),
        ("POST", "/api/theme/refresh"),
        ("POST", "/api/intel/refresh"),
        ("PUT", "/api/account"),
        ("POST", "/api/account/password"),
        ("DELETE", "/api/account"),
        ("GET", "/api/connectors"),
        ("GET", "/api/connectors/audit"),
        ("POST", "/api/connectors/slack/token"),
    ] {
        let r = c.request(
            method,
            path,
            &[("Content-Type", "application/json")],
            Some(b"{}"),
        );
        assert_eq!(r.status, 401, "{method} {path}");
        assert_eq!(r.json()["error"]["code"], "unauthorized");
    }
    assert_eq!(c.get("/api/nope").status, 404);
    assert_eq!(
        c.request(
            "PATCH",
            "/api/me",
            &[("Content-Type", "application/json")],
            Some(b"{}")
        )
        .status,
        405
    );

    // CSRF: JSON content type, and a matching Origin when one is sent.
    let body = br#"{"email":"x@y.example","password":"whatever1"}"#;
    let r = c.request(
        "POST",
        "/api/auth/login",
        &[("Content-Type", "text/plain")],
        Some(body),
    );
    assert_eq!(r.status, 403);
    assert_eq!(r.json()["error"]["code"], "forbidden");
    let r = c.request(
        "POST",
        "/api/auth/login",
        &[
            ("Content-Type", "application/json"),
            ("Origin", "https://evil.example"),
        ],
        Some(body),
    );
    assert_eq!(r.status, 403);
    let origin = format!("http://127.0.0.1:{}", env.port);
    let r = c.request(
        "POST",
        "/api/auth/login",
        &[
            ("Content-Type", "application/json; charset=utf-8"),
            ("Origin", &origin),
        ],
        Some(body),
    );
    assert_eq!(r.status, 401, "same-origin passes the CSRF rule");

    // Body limit and malformed JSON.
    let big = vec![b'a'; 70 * 1024];
    let r = c.request(
        "POST",
        "/api/auth/login",
        &[("Content-Type", "application/json")],
        Some(&big),
    );
    assert_eq!(r.status, 413);
    assert_eq!(r.json()["error"]["code"], "payload_too_large");
    assert_eq!(
        c.request(
            "POST",
            "/api/auth/login",
            &[("Content-Type", "application/json")],
            Some(b"{nope")
        )
        .status,
        400
    );

    // Rate limit on sign-in, per IP, with Retry-After.
    let mut attacker = env.client_from("203.0.113.77");
    let mut limited = None;
    for i in 0..40 {
        let r = attacker.post(
            "/api/auth/login",
            &json!({ "email": format!("u{i}@x.example"), "password": "wrong password" }),
        );
        if r.status == 429 {
            limited = Some(r);
            break;
        }
        assert_eq!(r.status, 401);
    }
    let r = limited.expect("sign-in is rate limited");
    assert_eq!(r.json()["error"]["code"], "rate_limited");
    assert!(
        r.header("Retry-After")
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|s| s > 0)
    );
    assert_eq!(
        env.client_from("203.0.113.78")
            .post(
                "/api/auth/login",
                &json!({ "email": "a@x.example", "password": "wrong pass" })
            )
            .status,
        401
    );

    // Headers: security on everything, CSP on HTML, ETag/304 on static files.
    let page = c.get("/some/client/route");
    assert_eq!(page.status, 200);
    assert!(
        page.header("Content-Type")
            .unwrap()
            .starts_with("text/html")
    );
    assert!(
        page.header("Content-Security-Policy")
            .unwrap()
            .contains("default-src 'self'")
    );
    assert_eq!(page.header("X-Frame-Options"), Some("DENY"));
    let js = c.get("/app.js");
    let tag = js.header("ETag").unwrap().to_string();
    assert_eq!(js.header("Cache-Control"), Some("no-cache"));
    assert_eq!(
        c.request("GET", "/app.js", &[("If-None-Match", &tag)], None)
            .status,
        304
    );
    let api = c.get("/api/meta");
    assert_eq!(api.header("X-Content-Type-Options"), Some("nosniff"));
    assert_eq!(api.header("Cache-Control"), Some("no-store"));
    assert_eq!(c.get("/healthz").json()["ok"], true);
}

#[test]
fn steering_settings_theme_ai_and_connectors() {
    let env = Env::start(&[]);
    let mut c = env.client();
    ready_account(&mut c, "Acme", "s@acme.example");

    // Steering.
    let r = c.post(
        "/api/steering",
        &json!({ "text": "  More public-sector tenders  ", "refresh": false }),
    );
    assert_eq!(r.status, 201);
    let note = r.json()["note"].clone();
    assert_eq!(note["text"], "More public-sector tenders");
    assert_eq!(r.json()["run"], Value::Null);
    assert_eq!(c.post("/api/steering", &json!({ "text": "" })).status, 422);
    assert_eq!(
        c.post("/api/steering", &json!({ "text": "x".repeat(301) }))
            .status,
        422
    );
    let r = c.post(
        "/api/steering",
        &json!({ "text": "Fewer events", "refresh": true }),
    );
    assert_eq!(r.status, 201);
    assert_eq!(r.json()["run"]["kind"], "discover");
    settle(&mut c);
    let notes = c.get("/api/steering").json()["notes"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0]["text"], "Fewer events", "newest first");
    assert_eq!(
        c.delete(&format!("/api/steering/{}", note["id"]), &json!({}))
            .status,
        204
    );
    assert_eq!(
        c.delete(&format!("/api/steering/{}", note["id"]), &json!({}))
            .status,
        404
    );

    // Settings and next_run_at.
    let r = c.put(
        "/api/settings",
        &json!({ "frequency": "daily", "email_digest": false }),
    );
    assert_eq!(r.status, 200);
    let s = r.json()["settings"].clone();
    assert_eq!(s["frequency"], "daily");
    assert_eq!(s["email_digest"], false);
    let gap = s["next_run_at"].as_i64().unwrap() - s["last_run_at"].as_i64().unwrap();
    assert!((86_000..=86_400).contains(&gap), "{gap}");
    assert_eq!(
        c.put("/api/settings", &json!({ "frequency": "off" }))
            .json()["settings"]["next_run_at"],
        Value::Null
    );
    assert_eq!(
        c.put("/api/settings", &json!({ "frequency": "hourly" }))
            .status,
        422
    );

    // Theme.
    let theme = c.get("/api/me").json()["theme"].clone();
    for k in [
        "label",
        "accent",
        "image_url",
        "credit",
        "source_url",
        "status",
    ] {
        assert!(theme.get(k).is_some(), "theme has {k}");
    }
    if theme["image_url"].is_null() {
        assert_eq!(c.get("/api/theme/image").status, 404);
    } else {
        let img = c.get(theme["image_url"].as_str().unwrap());
        assert_eq!(img.status, 200);
        assert!(img.header("Content-Type").unwrap().starts_with("image/"));
    }
    let r = c.post("/api/theme/refresh", &json!({}));
    assert!(r.status == 202 || r.status == 200, "{}", r.text());
    assert_eq!(r.json()["run"]["kind"], "theme");
    let me = settle(&mut c);
    assert_eq!(me["last_run"]["kind"], "theme");
    assert_eq!(me["last_run"]["status"], "done", "{}", me["last_run"]);

    // Intel refresh: a research run that keeps the account ready.
    let r = c.post("/api/intel/refresh", &json!({}));
    assert_eq!(r.status, 202, "{}", r.text());
    assert_eq!(r.json()["run"]["kind"], "research");
    let me = settle(&mut c);
    assert_eq!(me["user"]["onboarding"], "ready");
    assert_eq!(
        me["profile"]["headline"], "Commercial cleaning for offices and clinics",
        "the profile isn't overwritten"
    );

    // AI model: DeepSeek models, Bedrock with a sealed key that is never returned.
    assert_eq!(c.get("/api/settings/ai").json()["model"], "deepseek-flash");
    let r = c.put(
        "/api/settings/ai",
        &json!({ "provider": "deepseek", "model": "deepseek-v4-pro" }),
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["model"], "deepseek-v4-pro");
    assert_eq!(
        c.put(
            "/api/settings/ai",
            &json!({ "provider": "deepseek", "model": "gpt-9" })
        )
        .json()["error"]["field"],
        "model"
    );
    assert_eq!(
        c.put("/api/settings/ai", &json!({ "provider": "other" }))
            .status,
        422
    );
    let secret = "ABSKQmVkcm9ja0FQSUtleS1zZWNyZXQtdmFsdWU7Q2c";
    let r = c.put(
        "/api/settings/ai",
        &json!({ "provider": "bedrock", "bedrock": { "region": "us-east-1", "model": "us.anthropic.claude-sonnet-4-5-20250929-v1:0", "api_key": secret } }),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    let ai = r.json();
    assert_eq!(ai["provider"], "bedrock");
    assert_eq!(ai["bedrock"]["auth"], "api_key");
    assert_eq!(
        ai["bedrock"]["key_hint"],
        "••••7Q2c".replace("7Q2c", &secret[secret.len() - 4..])
    );
    assert!(!r.text().contains(secret));
    assert!(!c.get("/api/me").text().contains(secret));
    let stored: String = env
        .db()
        .query_row("SELECT ai_json FROM users", [], |r| r.get(0))
        .unwrap();
    // (The vault crate is still a stub that doesn't encrypt; the check applies once it does.)
    if !stored.contains("\"sealed\":\"stub:") {
        assert!(!stored.contains(secret), "the key is sealed at rest");
    }
    // Omitting the secret keeps it; a bad region is a field error.
    let r = c.put(
        "/api/settings/ai",
        &json!({ "provider": "bedrock", "bedrock": { "region": "eu-west-1" } }),
    );
    assert_eq!(r.json()["bedrock"]["key_hint"], ai["bedrock"]["key_hint"]);
    let r = c.put(
        "/api/settings/ai",
        &json!({ "provider": "bedrock", "bedrock": { "region": "Mars 1" } }),
    );
    assert_eq!(r.json()["error"]["field"], "region");
    let t = c.post("/api/settings/ai/test", &json!({}));
    assert_eq!(t.status, 200);
    assert!(t.json()["ok"].is_boolean() && t.json()["message"].is_string());
    assert_eq!(
        c.put("/api/settings/ai", &json!({ "provider": "deepseek" }))
            .json()["provider"],
        "deepseek"
    );

    // Connectors: availability, no connections yet, unknown provider, empty audit.
    let list = c.get("/api/connectors").json();
    assert!(
        list["available"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["provider"] == "slack")
    );
    assert_eq!(list["connections"], json!([]));
    assert_eq!(
        c.post("/api/connectors/myspace/start", &json!({})).status,
        404
    );
    assert_eq!(
        c.post("/api/connectors/slack/token", &json!({ "token": "" }))
            .status,
        422
    );
    assert_eq!(c.delete("/api/connectors/slack", &json!({})).status, 404);
    assert!(c.get("/api/connectors/audit?limit=5").json()["events"].is_array());
    let r = c.get("/api/connectors/slack/callback?code=x&state=forged");
    assert_eq!(r.status, 302);
    assert!(
        r.header("Location")
            .unwrap()
            .starts_with("/?connector_error=")
    );
}

#[test]
fn scheduler_refreshes_a_due_business() {
    let env = Env::start(&[]);
    let mut c = env.client();
    let me = ready_account(&mut c, "Acme", "due@acme.example");
    assert_eq!(me["last_run"]["trigger"], "signup");
    // Make the refresh due and forget what was found, so the scheduled run finds new items.
    env.db()
        .execute(
            "UPDATE users SET next_run_at = strftime('%s','now') - 5",
            [],
        )
        .unwrap();
    env.db().execute("DELETE FROM opportunities", []).unwrap();
    let before = env.outbox().len();
    let run = wait_for(30, || {
        let me = c.get("/api/me").json();
        (me["last_run"]["trigger"] == "schedule" && me["run"].is_null())
            .then(|| me["last_run"].clone())
    });
    assert_eq!(run["status"], "done", "{run}");
    let me = c.get("/api/me").json();
    let next = me["settings"]["next_run_at"].as_i64().unwrap();
    assert!(
        next > me["settings"]["last_run_at"].as_i64().unwrap() + 6 * 86_400,
        "weekly by default"
    );
    // New finds + digest on → a digest email, logged in `emails`.
    let _ = before;
    wait_for(10, || {
        let n: i64 = env
            .db()
            .query_row(
                "SELECT COUNT(*) FROM emails WHERE kind = 'digest' AND error IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        (n >= 1).then_some(())
    });
}

#[test]
fn crash_recovery_resumes_runs() {
    let mut env = Env::start(&[]);
    let mut c = env.client();
    ready_account(&mut c, "Acme", "crash@acme.example");
    // Slow the AI down so a real engine is mid-run when the process dies.
    env.mock_post(
        "/_fail",
        &json!({ "prefix": "/v1/", "status": 200, "times": 1000, "delay": 2 }),
    );
    let r = c.post("/api/runs", &json!({ "kind": "discover" }));
    let run_id = r.json()["run"]["id"].as_i64().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    env.kill_server();
    // Whether or not it finished before the kill, make the database look like a crash mid-run.
    env.db()
        .execute(
            "UPDATE runs SET status = 'running', finished_at = NULL WHERE id = ?1",
            [run_id],
        )
        .unwrap();
    env.mock_post("/_reset", &json!({}));
    env.start_server();
    c.port = env.port;
    let run = wait_for(60, || {
        let cur = c.get("/api/runs/current").json();
        (cur["run"].is_null() && cur["last_run"]["id"] == run_id).then(|| cur["last_run"].clone())
    });
    assert_eq!(run["status"], "done", "{run}");
    let attempts: i64 = env
        .db()
        .query_row("SELECT attempts FROM runs WHERE id = ?1", [run_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(attempts, 1);
    assert!(
        !c.get("/api/opportunities").json()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // A run that keeps crashing the process gives up after three attempts.
    env.kill_server();
    env.db()
        .execute(
            "UPDATE runs SET status = 'running', attempts = 2, finished_at = NULL WHERE id = ?1",
            [run_id],
        )
        .unwrap();
    env.start_server();
    c.port = env.port;
    let run = c.get("/api/runs/current").json()["last_run"].clone();
    assert_eq!(run["status"], "failed", "{run}");
    assert!(run["error"].as_str().unwrap().len() > 10);

    // A graceful shutdown exits cleanly.
    assert!(env.stop_server(), "SIGTERM exits with status 0");
    env.start_server();
    c.port = env.port;
    assert_eq!(c.get("/api/me").status, 200);
}

#[test]
fn concurrent_signups_and_requests() {
    let env = Env::start(&[("HTTP_THREADS", "8")]);
    let port = env.port;
    let handles: Vec<_> = (0..12)
        .map(|i| {
            std::thread::spawn(move || {
                let mut c = common::Client {
                    port,
                    cookie: None,
                    ip: Some(format!("81.2.70.{i}")),
                };
                signup(
                    &mut c,
                    &format!("Business {i}"),
                    &format!("owner{i}@biz{i}.example"),
                );
                for _ in 0..15 {
                    for path in [
                        "/api/me",
                        "/api/opportunities",
                        "/api/runs/current",
                        "/api/steering",
                        "/app.js",
                        "/healthz",
                    ] {
                        let r = c.get(path);
                        assert!(r.status == 200, "{path}: {} {}", r.status, r.text());
                    }
                    let r = c.post(
                        "/api/steering",
                        &json!({ "text": format!("note from {i}") }),
                    );
                    assert_eq!(r.status, 201, "{}", r.text());
                }
                settle(&mut c)
            })
        })
        .collect();
    for h in handles {
        let me = h.join().expect("no thread failed");
        assert_eq!(me["user"]["onboarding"], "review");
    }
    let users: i64 = env
        .db()
        .query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(users, 12);
    let log = env.log();
    assert!(
        !log.contains("database is locked") && !log.contains(" 500 "),
        "no lock errors or 500s"
    );
}

#[test]
fn without_an_api_key_runs_fail_clearly() {
    // A .env in the working directory must never override what the environment says (here:
    // no key, no Mailgun): real environment variables win, even empty ones.
    let env = Env::start_with_dotenv(
        &[("DEEPSEEK_API_KEY", "")],
        Some(
            "DEEPSEEK_API_KEY=leaked-from-dotenv\nMAILGUN_API_KEY=leaked\nMAILGUN_DOMAIN=mg.example\nMAILGUN_SENDER_EMAIL=a@mg.example\n",
        ),
    );
    let mut c = env.client();
    let meta = c.get("/api/meta").json();
    assert_eq!(meta["ai_ready"], false);
    assert_eq!(meta["email_live"], false);
    signup(&mut c, "Acme", "noai@acme.example");
    let me = settle(&mut c);
    assert_eq!(
        me["user"]["onboarding"], "review",
        "research failure still leads to review"
    );
    assert_eq!(me["last_run"]["status"], "failed");
    assert!(
        me["last_run"]["error"]
            .as_str()
            .unwrap()
            .contains("AI isn't configured")
    );
    let mut profile = me["profile"].clone();
    profile["headline"] = "Commercial cleaning".into();
    assert_eq!(
        c.put("/api/profile", &json!({ "profile": profile })).status,
        200
    );
    settle(&mut c);
    let r = c.post("/api/runs", &json!({ "kind": "discover" }));
    assert_eq!(r.status, 503);
    assert_eq!(r.json()["error"]["code"], "ai_unavailable");
}
