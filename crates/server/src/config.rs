//! Configuration: environment variables, optionally seeded from a `.env` file in the working
//! directory (real environment variables always win). See the table in docs/ARCHITECTURE.md.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use model::Frequency;

#[derive(Clone, Debug)]
pub struct Config {
    /// `ADDR`: where to listen.
    pub addr: String,
    /// `PUBLIC_URL` without a trailing slash: links in emails, the cookie's `Secure` flag, CSRF.
    pub public_url: String,
    /// `DATA_DIR`: the database, theme images and the email outbox.
    pub data_dir: PathBuf,
    /// `DEMO_PASSWORD`: when set, the demo padlock blocks everything until it is entered.
    pub demo_password: Option<String>,
    /// `ADMIN_PASSWORD`: unlocks the hidden feature-flag page at `/__admin`; without it the page doesn't exist.
    pub admin_password: Option<String>,
    pub http_threads: usize,
    pub max_concurrent_runs: usize,
    pub daily_spend_limit_usd: f64,
    pub manual_runs_per_day: i64,
    pub default_frequency: Frequency,
    /// `TRUST_PROXY`: take the client IP from `X-Forwarded-For` (and the host from
    /// `X-Forwarded-Host` for the CSRF origin check).
    pub trust_proxy: bool,
    /// `WEB_DIR`: serve the web client from disk on every request (development).
    pub web_dir: Option<PathBuf>,
    /// `LOG=debug`.
    pub debug: bool,
    /// `SCHEDULER_TICK_SECS` (default 30; tests use 1).
    pub scheduler_tick: Duration,
}

impl Config {
    pub fn from_env() -> Config {
        let public_url = var("PUBLIC_URL")
            .unwrap_or_else(|| "http://localhost:8080".into())
            .trim_end_matches('/')
            .to_string();
        Config {
            addr: var("ADDR").unwrap_or_else(|| "127.0.0.1:8080".into()),
            public_url,
            data_dir: PathBuf::from(var("DATA_DIR").unwrap_or_else(|| "./data".into())),
            demo_password: var("DEMO_PASSWORD")
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty()),
            admin_password: var("ADMIN_PASSWORD")
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty()),
            http_threads: parse("HTTP_THREADS", 8, |n: &usize| (1..=256).contains(n)),
            max_concurrent_runs: parse("MAX_CONCURRENT_RUNS", 2, |n: &usize| (1..=64).contains(n)),
            daily_spend_limit_usd: parse("DAILY_SPEND_LIMIT_USD", 1.0, |n: &f64| {
                n.is_finite() && *n >= 0.0
            }),
            manual_runs_per_day: parse("MANUAL_RUNS_PER_DAY", 6, |n: &i64| *n >= 0),
            default_frequency: match var("DEFAULT_FREQUENCY") {
                None => Frequency::Weekly,
                Some(v) => Frequency::parse(&v).unwrap_or_else(|| {
                    eprintln!("config: DEFAULT_FREQUENCY={v} isn't a frequency; using weekly");
                    Frequency::Weekly
                }),
            },
            trust_proxy: flag("TRUST_PROXY"),
            web_dir: var("WEB_DIR").map(PathBuf::from),
            debug: var("LOG").is_some_and(|v| v.eq_ignore_ascii_case("debug")),
            scheduler_tick: Duration::from_secs(parse("SCHEDULER_TICK_SECS", 30, |n: &u64| {
                (1..=3600).contains(n)
            })),
        }
    }

    /// True when the public URL is https, so cookies get the `Secure` flag.
    pub fn secure(&self) -> bool {
        self.public_url.to_ascii_lowercase().starts_with("https://")
    }

    /// `scheme://host[:port]` of the public URL, lowercased, for the CSRF origin check.
    pub fn public_origin(&self) -> String {
        let lower = self.public_url.to_ascii_lowercase();
        match lower.find("://") {
            Some(i) => {
                let rest = &lower[i + 3..];
                let authority = rest.split('/').next().unwrap_or(rest);
                format!("{}://{}", &lower[..i], authority)
            }
            None => lower,
        }
    }
}

/// A trimmed, non-empty environment variable.
fn var(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn flag(key: &str) -> bool {
    var(key).is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// A parsed variable, or the default (with a warning) when it is missing or out of range.
fn parse<T: FromStr + std::fmt::Display>(key: &str, default: T, valid: impl Fn(&T) -> bool) -> T {
    match var(key) {
        None => default,
        Some(raw) => match raw.parse::<T>() {
            Ok(v) if valid(&v) => v,
            _ => {
                eprintln!("config: {key}={raw} isn't valid; using {default}");
                default
            }
        },
    }
}

/// Load `KEY=value` lines from a `.env` file into the process environment, without overriding
/// anything already set. Returns how many variables it set. Supports comments, blank lines,
/// `export KEY=value`, and single or double quotes. Call before any thread starts.
pub fn load_dotenv(path: &Path) -> usize {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    let mut count = 0;
    for (key, value) in parse_dotenv(&text) {
        if std::env::var_os(&key).is_none() {
            // SAFETY: called from main before any other thread exists, so nothing can be reading
            // the environment concurrently.
            unsafe { std::env::set_var(&key, &value) };
            count += 1;
        }
    }
    count
}

fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let value = value.trim();
        let quoted = value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')));
        let value = if quoted {
            &value[1..value.len() - 1]
        } else {
            // An unquoted value may end in a comment: KEY=value # note
            value
                .find(" #")
                .map(|i| value[..i].trim_end())
                .unwrap_or(value)
        };
        out.push((key.to_string(), value.to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_lines() {
        let text = "# comment\n\nA=1\nexport B = two words \nC=\"quoted # not a comment\"\nD='single'\nE=x # trailing\nbad line\n=nokey\nF-G=no\nH=\n";
        let got = parse_dotenv(text);
        let get = |k: &str| {
            got.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("A"), Some("1"));
        assert_eq!(get("B"), Some("two words"));
        assert_eq!(get("C"), Some("quoted # not a comment"));
        assert_eq!(get("D"), Some("single"));
        assert_eq!(get("E"), Some("x"));
        assert_eq!(get("F-G"), None);
        assert_eq!(get("H"), Some(""));
        assert_eq!(got.len(), 6);
    }

    #[test]
    fn public_origin() {
        let mut c = Config::from_env();
        c.public_url = "HTTPS://Opp.Example.com:8443/app".into();
        assert_eq!(c.public_origin(), "https://opp.example.com:8443");
        assert!(c.secure());
        c.public_url = "http://localhost:8080".into();
        assert_eq!(c.public_origin(), "http://localhost:8080");
        assert!(!c.secure());
    }
}
