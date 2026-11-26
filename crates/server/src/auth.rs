//! Accounts: argon2id password hashing, sessions, reset tokens, in-memory rate limits and input
//! validation.
//!
//! * Passwords: argon2id with the crate defaults (19 MiB, t=2, p=1), PHC strings. Hashing is
//!   CPU- and memory-heavy, so at most [`HASH_SLOTS`] run at once whatever the number of HTTP
//!   threads: memory stays bounded and a burst of sign-ins can't starve the machine.
//! * Sessions: 32 random bytes in the `opp_session` cookie; only their SHA-256 is stored, so a
//!   lookup by hash is also a constant-time comparison. 30 days, sliding.
//! * Reset tokens: the same scheme, one hour, single use.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex};

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use rusqlite::{Connection, OptionalExtension};

use crate::util::{self, lock};

pub const COOKIE: &str = "opp_session";
pub const SESSION_TTL: i64 = 30 * 86_400;
/// A session is extended (and its cookie re-sent) at most this often.
pub const SESSION_REFRESH: i64 = 3600;
pub const RESET_TTL: i64 = 3600;

// ---------------------------------------------------------------------------------------------
// Passwords
// ---------------------------------------------------------------------------------------------

const HASH_SLOTS: usize = 2;

struct Slots {
    used: Mutex<usize>,
    free: Condvar,
}

static SLOTS: Slots = Slots {
    used: Mutex::new(0),
    free: Condvar::new(),
};

struct Permit;

impl Permit {
    fn acquire() -> Permit {
        let mut used = lock(&SLOTS.used);
        while *used >= HASH_SLOTS {
            used = SLOTS.free.wait(used).unwrap_or_else(|e| e.into_inner());
        }
        *used += 1;
        Permit
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        *lock(&SLOTS.used) -= 1;
        SLOTS.free.notify_one();
    }
}

/// An argon2id PHC string for `password`, or None if hashing failed (it doesn't in practice).
pub fn hash_password(password: &str) -> Option<String> {
    let _permit = Permit::acquire();
    match Argon2::default().hash_password(password.as_bytes()) {
        Ok(h) => Some(h.to_string()),
        Err(e) => {
            error!("password hashing failed: {e}");
            None
        }
    }
}

/// True when `password` matches the stored PHC string.
pub fn verify_password(password: &str, phc: &str) -> bool {
    let _permit = Permit::acquire();
    Argon2::default()
        .verify_password(password.as_bytes(), phc)
        .is_ok()
}

/// Spend the same effort as a real verification, so a sign-in for an unknown email takes as long
/// as one with a wrong password.
pub fn burn_verification(password: &str) {
    static DUMMY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    if let Some(phc) = DUMMY.get_or_init(|| hash_password("not a real password, only for timing")) {
        let _ = verify_password(password, phc);
    }
}

// ---------------------------------------------------------------------------------------------
// Sessions and reset tokens
// ---------------------------------------------------------------------------------------------

pub fn token_hash(token: &str) -> String {
    util::sha256_hex(token.as_bytes())
}

/// Create a session for `user_id`; returns the token for the cookie.
pub fn create_session(c: &Connection, user_id: i64) -> Result<String, crate::http::ApiError> {
    let token = util::random_hex(32).ok_or_else(crate::http::ApiError::internal)?;
    let now = util::now();
    c.execute(
        "INSERT INTO sessions(token_hash, user_id, created_at, expires_at, last_seen_at) VALUES(?1, ?2, ?3, ?4, ?3)",
        (token_hash(&token), user_id, now, now + SESSION_TTL),
    )?;
    Ok(token)
}

/// The user behind a session token, if it is valid. Slides the expiry when the session wasn't
/// seen for a while; the second value is true when the cookie should be re-sent.
pub fn session_user(c: &Connection, token: &str) -> rusqlite::Result<Option<(i64, bool)>> {
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let hash = token_hash(token);
    let now = util::now();
    let found: Option<(i64, i64)> = c
        .prepare_cached(
            "SELECT user_id, last_seen_at FROM sessions WHERE token_hash = ?1 AND expires_at > ?2",
        )?
        .query_row((&hash, now), |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((user_id, last_seen)) = found else {
        return Ok(None);
    };
    let refresh = now - last_seen >= SESSION_REFRESH;
    if refresh {
        c.execute(
            "UPDATE sessions SET last_seen_at = ?2, expires_at = ?3 WHERE token_hash = ?1",
            (&hash, now, now + SESSION_TTL),
        )?;
    }
    Ok(Some((user_id, refresh)))
}

pub fn delete_session(c: &Connection, token: &str) -> rusqlite::Result<()> {
    c.execute(
        "DELETE FROM sessions WHERE token_hash = ?1",
        [token_hash(token)],
    )?;
    Ok(())
}

/// Revoke every session of a user except `keep` (a token), if given.
pub fn revoke_sessions(
    c: &Connection,
    user_id: i64,
    keep: Option<&str>,
) -> rusqlite::Result<usize> {
    match keep {
        Some(token) => c.execute(
            "DELETE FROM sessions WHERE user_id = ?1 AND token_hash != ?2",
            (user_id, token_hash(token)),
        ),
        None => c.execute("DELETE FROM sessions WHERE user_id = ?1", [user_id]),
    }
}

/// `Set-Cookie` value for a session token.
pub fn session_cookie(token: &str, secure: bool) -> String {
    format!(
        "{COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={SESSION_TTL}{}",
        if secure { "; Secure" } else { "" }
    )
}

/// `Set-Cookie` value that removes the session cookie.
pub fn clear_cookie(secure: bool) -> String {
    format!(
        "{COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}

/// The session token from a `Cookie` header, if any.
pub fn cookie_token(cookie_header: &str) -> Option<&str> {
    cookie_header.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name.trim() == COOKIE && !value.trim().is_empty()).then(|| value.trim())
    })
}

// ---------------------------------------------------------------------------------------------
// Rate limits
// ---------------------------------------------------------------------------------------------

/// Fixed-window counters keyed by strings like `login:ip:203.0.113.9`, in memory.
#[derive(Default)]
pub struct Limiter {
    windows: Mutex<HashMap<String, (i64, u32)>>,
}

impl Limiter {
    /// Count one attempt; Err(seconds to wait) when the limit was already reached.
    pub fn hit(&self, key: &str, limit: u32, window: i64) -> Result<(), i64> {
        let now = util::now();
        let mut map = lock(&self.windows);
        if map.len() > 50_000 {
            map.retain(|_, (start, _)| now - *start < 86_400);
        }
        let entry = map.entry(key.to_string()).or_insert((now, 0));
        if now - entry.0 >= window {
            *entry = (now, 0);
        }
        if entry.1 >= limit {
            return Err((entry.0 + window - now).max(1));
        }
        entry.1 += 1;
        Ok(())
    }

    /// Like [`Limiter::hit`] but without counting.
    pub fn check(&self, key: &str, limit: u32, window: i64) -> Result<(), i64> {
        let now = util::now();
        match lock(&self.windows).get(key) {
            Some((start, count)) if now - start < window && *count >= limit => {
                Err((start + window - now).max(1))
            }
            _ => Ok(()),
        }
    }

    pub fn clear(&self, key: &str) {
        lock(&self.windows).remove(key);
    }

    /// Forget windows older than a day (the longest window used).
    pub fn prune(&self) {
        let now = util::now();
        lock(&self.windows).retain(|_, (start, _)| now - *start < 86_400);
    }
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// A plausible email address: one @, a non-empty local part, a dotted domain, no spaces,
/// at most 254 characters.
pub fn valid_email(email: &str) -> bool {
    if email.len() > 254
        || email
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '<' || c == '>' || c == ',')
    {
        return false;
    }
    let Some((local, domain)) = email.rsplit_once('@') else {
        return false;
    };
    !local.is_empty()
        && local.len() <= 64
        && !local.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
        && domain
            .chars()
            .all(|c| c.is_alphanumeric() || c == '.' || c == '-')
}

/// Password length rule: 8–200 characters.
pub fn password_problem(password: &str) -> Option<&'static str> {
    let n = password.chars().count();
    if n < 8 {
        Some("Use at least 8 characters for your password.")
    } else if n > 200 {
        Some("Use at most 200 characters for your password.")
    } else {
        None
    }
}

/// The business name trimmed and squashed; Err with a message unless it has 1–120 characters.
pub fn clean_business_name(raw: &str) -> Result<String, String> {
    let name = model::text::squash(raw);
    match name.chars().count() {
        0 => Err("Enter your business's name.".into()),
        n if n > 120 => Err("The business name can be at most 120 characters.".into()),
        _ => Ok(name),
    }
}

/// An http(s) website, with `https://` added when the scheme is missing; None when unusable.
pub fn clean_website(raw: &str) -> Option<String> {
    let w = raw.trim();
    if w.is_empty() || w.len() > 300 {
        return None;
    }
    let lower = w.to_ascii_lowercase();
    let url = if lower.starts_with("http://") || lower.starts_with("https://") {
        w.to_string()
    } else {
        format!("https://{w}")
    };
    model::links::is_http_url(&url).then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails() {
        for ok in [
            "a@b.co",
            "first.last+tag@example.co.za",
            "x@sub.domain.org",
            "ünï@exämple.com",
        ] {
            assert!(valid_email(ok), "{ok}");
        }
        for bad in [
            "",
            "a",
            "a@b",
            "@b.com",
            "a@.com",
            "a@b.com.",
            "a b@c.com",
            "a@b..com",
            "a@@b.com",
            "<a@b.com>",
            &format!("{}@b.com", "x".repeat(65)),
        ] {
            assert!(!valid_email(bad), "{bad}");
        }
    }

    #[test]
    fn business_fields_and_passwords() {
        assert_eq!(
            clean_business_name("  Acme \n Cleaning ").unwrap(),
            "Acme Cleaning"
        );
        assert!(clean_business_name("   ").is_err());
        assert!(clean_business_name(&"x".repeat(121)).is_err());
        assert!(clean_business_name(&"é".repeat(120)).is_ok());
        assert_eq!(
            clean_website("acme.example").as_deref(),
            Some("https://acme.example")
        );
        assert_eq!(
            clean_website(" HTTP://acme.example/x ").as_deref(),
            Some("HTTP://acme.example/x")
        );
        assert_eq!(clean_website("not a site"), None);
        assert_eq!(clean_website("javascript:alert(1)"), None);
        assert_eq!(clean_website(""), None);
        assert!(password_problem("1234567").is_some());
        assert!(password_problem("12345678").is_none());
        assert!(password_problem(&"x".repeat(201)).is_some());
    }

    #[test]
    fn cookies() {
        assert_eq!(cookie_token("a=1; opp_session=abc; b=2"), Some("abc"));
        assert_eq!(cookie_token("opp_session="), None);
        assert_eq!(cookie_token("other=1"), None);
        assert!(session_cookie("t", true).ends_with("; Secure"));
        assert!(clear_cookie(false).contains("Max-Age=0"));
    }

    #[test]
    fn limiter_counts_and_resets() {
        let l = Limiter::default();
        assert!(l.hit("k", 2, 60).is_ok());
        assert!(l.hit("k", 2, 60).is_ok());
        let wait = l.hit("k", 2, 60).unwrap_err();
        assert!((1..=60).contains(&wait));
        assert!(l.check("k", 2, 60).is_err());
        l.clear("k");
        assert!(l.check("k", 2, 60).is_ok());
        assert!(l.hit("k", 2, 60).is_ok());
    }

    #[test]
    fn hashing_roundtrip() {
        let h = hash_password("correct horse").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("wrong horse", &h));
        assert!(!verify_password("x", "not a phc string"));
    }
}
