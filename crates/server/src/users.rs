//! The `users` row — one business account — as the rest of the server sees it, its JSON views
//! (Account, Settings, Theme), and the stored theme record.

use model::{Frequency, Intel, Profile, ThemeImage};
use rusqlite::{Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::runs::RunRow;

/// Accent used until a theme is built.
pub const DEFAULT_ACCENT: &str = "#3b5bdb";

#[derive(Clone, Debug)]
pub struct User {
    pub id: i64,
    pub email: String,
    pub business_name: String,
    pub website: String,
    pub created_at: i64,
    pub onboarding: String,
    pub profile_json: String,
    pub intel_json: Option<String>,
    pub theme_json: Option<String>,
    pub ai_json: Option<String>,
    pub frequency: String,
    pub email_digest: bool,
    pub next_run_at: Option<i64>,
    pub last_run_at: Option<i64>,
    pub last_ip: Option<String>,
}

const COLS: &str = "id, email, business_name, website, created_at, onboarding, profile_json, intel_json, theme_json, ai_json, \
                    frequency, email_digest, next_run_at, last_run_at, last_ip";

fn from_row(r: &Row) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        email: r.get(1)?,
        business_name: r.get(2)?,
        website: r.get(3)?,
        created_at: r.get(4)?,
        onboarding: r.get(5)?,
        profile_json: r.get(6)?,
        intel_json: r.get(7)?,
        theme_json: r.get(8)?,
        ai_json: r.get(9)?,
        frequency: r.get(10)?,
        email_digest: r.get(11)?,
        next_run_at: r.get(12)?,
        last_run_at: r.get(13)?,
        last_ip: r.get(14)?,
    })
}

impl User {
    pub fn load(c: &Connection, id: i64) -> rusqlite::Result<Option<User>> {
        c.prepare_cached(&format!("SELECT {COLS} FROM users WHERE id = ?1"))?
            .query_row([id], from_row)
            .optional()
    }

    pub fn profile(&self) -> Profile {
        serde_json::from_str(&self.profile_json).unwrap_or_default()
    }

    /// The intelligence dossier (empty until research has run).
    pub fn intel(&self) -> Intel {
        self.intel_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }

    pub fn theme(&self) -> Option<ThemeRecord> {
        self.theme_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
    }

    pub fn frequency(&self) -> Frequency {
        Frequency::parse(&self.frequency).unwrap_or_default()
    }

    /// The API's Account object (sent under the key `user`).
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "email": self.email,
            "business_name": self.business_name,
            "website": self.website,
            "created_at": self.created_at,
            "onboarding": self.onboarding,
        })
    }

    /// The API's Settings object.
    pub fn settings_json(&self) -> Value {
        json!({
            "frequency": self.frequency().as_str(),
            "email_digest": self.email_digest,
            "next_run_at": self.next_run_at,
            "last_run_at": self.last_run_at,
        })
    }
}

/// What `users.theme_json` holds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeRecord {
    pub label: String,
    pub accent: String,
    /// The photo's description; None means "use a gradient of the accent".
    pub image: Option<ThemeImage>,
    /// File name under `DATA_DIR/themes/`.
    pub file: Option<String>,
    /// Bumped with every rebuild; part of the image URL so browsers can cache it forever.
    pub version: i64,
    /// [`subject_key`] of the profile the theme was built for.
    pub built_for: String,
}

/// A fingerprint of what a page is "about" (industry, headline, stage, interests, skills). A new
/// theme is built when it changes; edits to goals, location or links keep the current one.
pub fn subject_key(p: &Profile) -> String {
    let norm = |v: &[String]| {
        let mut v: Vec<String> = v
            .iter()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        v.sort();
        v.join(",")
    };
    let text = format!(
        "{}|{}|{}|{}|{}",
        p.industry.trim().to_lowercase(),
        p.headline.trim().to_lowercase(),
        p.stage,
        norm(&p.interests),
        norm(&p.skills)
    );
    crate::util::sha256_hex(text.as_bytes())[..16].to_string()
}

/// The API's Theme object. `active` is the user's active run, if any: a theme run, or a discovery
/// that will (re)build a missing or outdated theme, means "building".
pub fn theme_json(user: &User, active: Option<&RunRow>) -> Value {
    let theme = user.theme();
    let building = match active {
        Some(r) if r.kind == "theme" => true,
        Some(r) if r.kind == "discover" => theme
            .as_ref()
            .is_none_or(|t| t.built_for != subject_key(&user.profile())),
        _ => false,
    };
    let status = if building {
        "building"
    } else if theme.is_some() {
        "ready"
    } else {
        "none"
    };
    let t = theme.unwrap_or_default();
    let photo = t.image.as_ref().filter(|_| t.file.is_some());
    json!({
        "label": t.label,
        "accent": model::sanitize_hex_color(&t.accent, DEFAULT_ACCENT),
        "image_url": photo.map(|_| format!("/api/theme/image?v={}", t.version)),
        "credit": photo.map(|i| i.credit.clone()).filter(|c| !c.is_empty()),
        "source_url": photo.map(|i| i.source_url.clone()).filter(|u| model::links::is_http_url(u)),
        "status": status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_ignores_order_case_and_other_fields() {
        let a = Profile {
            headline: "Chef".into(),
            interests: vec!["Cooking".into(), "street food".into()],
            goals: "x".into(),
            ..Default::default()
        };
        let b = Profile {
            headline: " chef ".into(),
            interests: vec!["street food".into(), "cooking".into()],
            goals: "y".into(),
            location: "Cape Town".into(),
            ..Default::default()
        };
        assert_eq!(subject_key(&a), subject_key(&b));
        let c = Profile {
            headline: "Baker".into(),
            ..a.clone()
        };
        assert_ne!(subject_key(&a), subject_key(&c));
    }
}
