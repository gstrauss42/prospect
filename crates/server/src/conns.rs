//! Connections to the business's own tools (Slack, Microsoft 365, Confluence), hard read-only:
//! the `connectors` crate enforces reads; this module stores credentials sealed by the vault
//! (bound to the account and provider), keeps OAuth state + PKCE verifiers for ten minutes,
//! writes every request's metadata to `audit_events` (never content), and fetches recent items
//! for discovery. A connector failure marks the connection and never fails anything else.

use connectors::{AuditEvent, Credential, FetchQuery, Provider};
use model::ContextItem;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

use crate::{app, db, util};

/// OAuth state lifetime.
pub const STATE_TTL: i64 = 600;

/// Writes audit events for one account to the database.
pub struct DbAudit {
    pub user_id: i64,
}

impl connectors::Audit for DbAudit {
    fn record(&self, e: AuditEvent) {
        let written = db::conn().and_then(|c| {
            c.execute(
                "INSERT INTO audit_events(user_id, provider, action, method, target, status, bytes, ms, at, note) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                (
                    self.user_id,
                    &e.provider,
                    &e.action,
                    &e.method,
                    model::text::truncate(&e.target, 300),
                    e.status,
                    e.bytes as i64,
                    e.ms as i64,
                    if e.at > 0 { e.at } else { util::now() },
                    model::text::truncate(&e.note, 300),
                ),
            )
        });
        if let Err(err) = written {
            warn!("couldn't write an audit event: {err}");
        }
    }
}

fn vault_context(user_id: i64, provider: &str) -> String {
    format!("user:{user_id}:connector:{provider}")
}

/// Keep a credential (sealed) as the account's connection to its provider.
pub fn store(c: &Connection, user_id: i64, cred: &Credential) -> rusqlite::Result<()> {
    let plain = serde_json::to_string(cred).unwrap_or_default();
    let sealed = app()
        .vault
        .seal(&plain, &vault_context(user_id, &cred.provider));
    let account = serde_json::to_string(&cred.account).unwrap_or_else(|_| "{}".into());
    c.execute(
        "INSERT INTO connections(user_id, provider, credential, mode, enforcement, account_json, status, error, connected_at) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'ok', NULL, ?7) \
         ON CONFLICT(user_id, provider) DO UPDATE SET credential = excluded.credential, mode = excluded.mode, \
         enforcement = excluded.enforcement, account_json = excluded.account_json, status = 'ok', error = NULL, \
         connected_at = excluded.connected_at",
        (user_id, &cred.provider, sealed, &cred.mode, cred.enforcement(), account, util::now()),
    )?;
    Ok(())
}

/// Replace the stored credential (after a token refresh), keeping the connection's other fields.
fn replace_credential(c: &Connection, user_id: i64, cred: &Credential) -> rusqlite::Result<()> {
    let sealed = app().vault.seal(
        &serde_json::to_string(cred).unwrap_or_default(),
        &vault_context(user_id, &cred.provider),
    );
    c.execute(
        "UPDATE connections SET credential = ?3 WHERE user_id = ?1 AND provider = ?2",
        (user_id, &cred.provider, sealed),
    )?;
    Ok(())
}

fn mark(c: &Connection, user_id: i64, provider: &str, err: &connectors::Error) {
    let status = if err.needs_reconnect() {
        "needs_reconnect"
    } else {
        "error"
    };
    let message = model::text::truncate(&err.to_string(), 300);
    if let Err(e) = c.execute(
        "UPDATE connections SET status = ?3, error = ?4 WHERE user_id = ?1 AND provider = ?2",
        (user_id, provider, status, message),
    ) {
        warn!("couldn't mark a connection: {e}");
    }
}

/// The API's connection objects for an account.
pub fn list(c: &Connection, user_id: i64) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = c.prepare_cached(
        "SELECT provider, account_json, mode, enforcement, status, connected_at, last_used_at, error FROM connections WHERE user_id = ?1 ORDER BY provider",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        let account: Value =
            serde_json::from_str(&r.get::<_, String>(1)?).unwrap_or_else(|_| json!({}));
        Ok(json!({
            "provider": r.get::<_, String>(0)?,
            "account": { "name": account["name"], "user": account["user"], "url": account["url"] },
            "mode": r.get::<_, String>(2)?,
            "enforcement": r.get::<_, String>(3)?,
            "status": r.get::<_, String>(4)?,
            "connected_at": r.get::<_, i64>(5)?,
            "last_used_at": r.get::<_, Option<i64>>(6)?,
            "error": r.get::<_, Option<String>>(7)?,
        }))
    })?;
    rows.collect()
}

/// Remember an OAuth state and its PKCE verifier (sealed) for ten minutes.
pub fn save_state(
    c: &Connection,
    user_id: i64,
    provider: Provider,
    state: &str,
    verifier: &str,
) -> rusqlite::Result<()> {
    let now = util::now();
    c.execute("DELETE FROM oauth_states WHERE expires_at <= ?1", [now])?;
    let sealed = app().vault.seal(verifier, &format!("oauth:{state}"));
    c.execute(
        "INSERT INTO oauth_states(state, user_id, provider, verifier, expires_at) VALUES(?1, ?2, ?3, ?4, ?5)",
        (state, user_id, provider.id(), sealed, now + STATE_TTL),
    )?;
    Ok(())
}

/// Take (once) the PKCE verifier for a state this account started for this provider.
pub fn take_state(
    c: &Connection,
    user_id: i64,
    provider: Provider,
    state: &str,
) -> rusqlite::Result<Option<String>> {
    let sealed: Option<String> = c
        .query_row(
            "DELETE FROM oauth_states WHERE state = ?1 AND user_id = ?2 AND provider = ?3 AND expires_at > ?4 RETURNING verifier",
            (state, user_id, provider.id(), util::now()),
            |r| r.get(0),
        )
        .optional()?;
    Ok(sealed.and_then(|s| app().vault.unseal(&s, &format!("oauth:{state}")).ok()))
}

/// Recent items from every working connection of an account, for discovery. Never fails: a
/// broken connection is marked (`needs_reconnect` or `error`) and skipped.
pub fn fetch_context(user_id: i64) -> Vec<ContextItem> {
    let Ok(c) = db::conn() else { return Vec::new() };
    let rows: Vec<(String, String)> = match c
        .prepare_cached(
            "SELECT provider, credential FROM connections WHERE user_id = ?1 AND status = 'ok'",
        )
        .and_then(|mut s| {
            s.query_map([user_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect()
        }) {
        Ok(rows) => rows,
        Err(e) => {
            warn!("couldn't read connections: {e}");
            return Vec::new();
        }
    };
    let audit = DbAudit { user_id };
    let mut items = Vec::new();
    for (provider, sealed) in rows {
        let cred = app()
            .vault
            .unseal(&sealed, &vault_context(user_id, &provider))
            .ok()
            .and_then(|plain| serde_json::from_str::<Credential>(&plain).ok());
        let Some(mut cred) = cred else {
            mark(
                &c,
                user_id,
                &provider,
                &connectors::Error::Unauthorized("the stored credential can't be read".into()),
            );
            continue;
        };
        let fetched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(fresh) = app()
                .connectors
                .refresh_if_needed(&cred, util::now(), &audit)?
            {
                if let Err(e) = replace_credential(&c, user_id, &fresh) {
                    warn!("couldn't store a refreshed {provider} token: {e}");
                }
                cred = fresh;
            }
            app()
                .connectors
                .fetch(&cred, &FetchQuery::default(), &audit)
        }));
        match fetched {
            Ok(Ok(found)) => {
                let _ = c.execute(
                    "UPDATE connections SET last_used_at = ?3 WHERE user_id = ?1 AND provider = ?2",
                    (user_id, &provider, util::now()),
                );
                items.extend(found);
            }
            Ok(Err(e)) => {
                warn!("{provider} connection of account {user_id}: {e}");
                mark(&c, user_id, &provider, &e);
            }
            Err(_) => {
                error!("{provider} connector panicked for account {user_id}");
                mark(
                    &c,
                    user_id,
                    &provider,
                    &connectors::Error::Invalid("the connector failed".into()),
                );
            }
        }
    }
    items
}
