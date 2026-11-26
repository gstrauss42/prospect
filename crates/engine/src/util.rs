//! Helpers shared by the pipelines: forward-only progress, a small scoped thread pool, lenient
//! readers for model JSON, and text utilities.

use std::net::IpAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use model::text::{squash, truncate};
use serde_json::Value;

use crate::Ctx;

// ---------------------------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------------------------

/// Progress for the live feed. The percent never goes backwards, even when threads race.
pub(crate) struct Reporter<'r, 'a> {
    ctx: &'r Ctx<'a>,
    last: Mutex<u8>,
}

impl<'r, 'a> Reporter<'r, 'a> {
    pub(crate) fn new(ctx: &'r Ctx<'a>) -> Self {
        Reporter {
            ctx,
            last: Mutex::new(0),
        }
    }

    pub(crate) fn report(&self, stage: &str, message: impl Into<String>, percent: u8) {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let p = percent.max(*last).min(100);
        *last = p;
        self.ctx.report(stage, message, p);
    }

    pub(crate) fn percent(&self) -> u8 {
        *self.last.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Run `work` while a ticker reports `messages` in turn every few seconds, creeping from the
/// current percent towards `ceiling`, so a long model call still shows life on the loading
/// screen. The messages must describe what the call is genuinely doing.
pub(crate) fn with_heartbeat<R>(
    rep: &Reporter,
    stage: &str,
    ceiling: u8,
    messages: &[&str],
    work: impl FnOnce() -> R,
) -> R {
    let done = AtomicBool::new(false);
    let start_pct = rep.percent();
    std::thread::scope(|s| {
        let ticker = std::thread::Builder::new()
            .name("engine-heartbeat".into())
            .spawn_scoped(s, || {
                let started = Instant::now();
                let mut next_at = Duration::from_secs(5);
                let mut i = 0usize;
                while !done.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                    if messages.is_empty()
                        || started.elapsed() < next_at
                        || done.load(Ordering::Relaxed)
                    {
                        continue;
                    }
                    let secs = started.elapsed().as_secs_f64();
                    let span = f64::from(ceiling.saturating_sub(start_pct));
                    let pct = f64::from(start_pct) + span * (1.0 - (-secs / 45.0).exp());
                    rep.report(stage, messages[i % messages.len()], pct.floor() as u8);
                    i += 1;
                    next_at += Duration::from_secs(7);
                }
            });
        let out = work();
        done.store(true, Ordering::Relaxed);
        drop(ticker);
        out
    })
}

// ---------------------------------------------------------------------------------------------
// Parallelism
// ---------------------------------------------------------------------------------------------

/// Run `f` over `items` on up to `threads` scoped threads; results come back in input order.
/// An item yields None when its call panicked or when `stop` was set before it started.
pub(crate) fn parallel_map<T, R, F>(
    items: &[T],
    threads: usize,
    stop: &AtomicBool,
    f: F,
) -> Vec<Option<R>>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> R + Sync,
{
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..n).map(|_| None).collect());
    let worker = || {
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i >= n {
                break;
            }
            if let Ok(r) = catch_unwind(AssertUnwindSafe(|| f(i, &items[i]))) {
                results.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
            }
        }
    };
    std::thread::scope(|s| {
        let mut spawned = 0;
        for k in 0..threads.clamp(1, n) {
            if std::thread::Builder::new()
                .name(format!("engine-{k}"))
                .spawn_scoped(s, worker)
                .is_ok()
            {
                spawned += 1;
            }
        }
        if spawned == 0 {
            worker();
        }
    });
    results.into_inner().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------------------------
// Lenient readers for model JSON
// ---------------------------------------------------------------------------------------------

/// A field as squashed text: strings as they are, numbers and booleans spelled out, else "".
pub(crate) fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => squash(s),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// The first non-empty of several fields.
pub(crate) fn text_any(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .map(|k| text(v, k))
        .find(|s| !s.is_empty())
        .unwrap_or_default()
}

/// A number from a number or a numeric string ("12", "#12", "K3", "85%").
pub(crate) fn number(v: &Value, key: &str) -> Option<f64> {
    let x = match v.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_number(s),
        _ => None,
    };
    x.filter(|x| x.is_finite())
}

pub(crate) fn parse_number(s: &str) -> Option<f64> {
    let t = s
        .trim()
        .trim_start_matches(['#', 'K', 'k', 'n', 'N'])
        .trim_end_matches('%')
        .trim();
    t.parse::<f64>().ok().filter(|x| x.is_finite())
}

/// A list of strings from an array (strings or numbers) or a comma/semicolon separated string.
pub(crate) fn list(v: &Value, key: &str) -> Vec<String> {
    match v.get(key) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| match x {
                Value::String(s) => Some(squash(s)),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => s
            .split([',', ';', '\n'])
            .map(squash)
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// The array under the first of `keys` that holds one, or the value itself when it is an array.
pub(crate) fn array<'v>(v: &'v Value, keys: &[&str]) -> &'v [Value] {
    if let Value::Array(a) = v {
        return a;
    }
    for k in keys {
        if let Some(Value::Array(a)) = v.get(*k) {
            return a;
        }
    }
    &[]
}

/// True for the empty-ish placeholders models write instead of leaving a field blank.
pub(crate) fn is_placeholder(s: &str) -> bool {
    let t = s.trim().trim_end_matches('.').to_ascii_lowercase();
    matches!(
        t.as_str(),
        "" | "-"
            | "n/a"
            | "na"
            | "none"
            | "null"
            | "unknown"
            | "not stated"
            | "not specified"
            | "not given"
            | "not known"
            | "unspecified"
            | "tbc"
            | "tba"
            | "various"
            | "see page"
            | "see website"
    )
}

/// Squashed, capped text, with placeholders turned into "".
pub(crate) fn clip(s: &str, max: usize) -> String {
    let s = squash(s);
    if is_placeholder(&s) {
        String::new()
    } else {
        truncate(&s, max)
    }
}

// ---------------------------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------------------------

fn floor_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Byte position of the first ASCII-case-insensitive match of `needle` at or after `from`.
fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (from..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// At most about `max` characters of `text`: its start, plus short windows around the first
/// mentions of `keywords` further down (so a closing date deep in a page survives), joined
/// with " … ".
pub(crate) fn excerpt(text: &str, max: usize, keywords: &[&str]) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head_chars = max * 3 / 5;
    let head_end = text
        .char_indices()
        .nth(head_chars)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for kw in keywords {
        let mut from = head_end;
        let mut found = 0;
        while let Some(pos) = find_ci(text, kw, from) {
            let start = floor_boundary(text, pos.saturating_sub(160));
            let end = floor_boundary(text, pos + kw.len() + 240);
            windows.push((start.max(head_end), end));
            from = pos + kw.len();
            found += 1;
            if found >= 2 {
                break;
            }
        }
    }
    windows.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in windows {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    let mut out = text[..head_end].trim_end().to_string();
    let mut budget = max.saturating_sub(head_chars);
    for (s, e) in merged {
        if budget < 80 {
            break;
        }
        let piece = squash(&text[s..e]);
        let piece = truncate(&piece, budget);
        budget = budget.saturating_sub(piece.chars().count() + 3);
        out.push_str(" … ");
        out.push_str(&piece);
    }
    if out.len() < text.len() {
        out.push_str(" …");
    }
    out
}

/// Lowercase letters and digits only: a key for "the same title".
pub(crate) fn title_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Title case for a short label: "climate tech" → "Climate Tech" (words already capitalised,
/// like "AI", keep their case).
pub(crate) fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A public IP address for search localisation; None for private, loopback and other
/// non-routable addresses (a search engine can't place those).
pub(crate) fn public_ip(s: &str) -> Option<String> {
    let ip: IpAddr = s.trim().parse().ok()?;
    let public = match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || (o[0] == 100 && (64..128).contains(&o[1])))
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80)
        }
    };
    public.then(|| ip.to_string())
}

/// "15 Jan 2027" for a date, for progress messages.
pub(crate) fn short_date(ymd: &str) -> String {
    model::date::human(ymd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lenient_readers() {
        let v = json!({"n": "#12", "s": 85.6, "t": "  a \n b ", "b": true, "l": "x, y; z", "a": ["p", 3, null, ""]});
        assert_eq!(number(&v, "n"), Some(12.0));
        assert_eq!(number(&v, "s"), Some(85.6));
        assert_eq!(number(&v, "t"), None);
        assert_eq!(number(&v, "missing"), None);
        assert_eq!(text(&v, "t"), "a b");
        assert_eq!(text(&v, "b"), "true");
        assert_eq!(text(&v, "s"), "85.6");
        assert_eq!(list(&v, "l"), vec!["x", "y", "z"]);
        assert_eq!(list(&v, "a"), vec!["p", "3"]);
        assert_eq!(text_any(&v, &["missing", "t"]), "a b");
        assert_eq!(parse_number("K3"), Some(3.0));
        assert_eq!(parse_number("90%"), Some(90.0));
        assert_eq!(
            array(&json!({"picks": [1, 2]}), &["items", "picks"]).len(),
            2
        );
        assert_eq!(array(&json!([1]), &["picks"]).len(), 1);
        assert!(array(&json!({"picks": "no"}), &["picks"]).is_empty());
    }

    #[test]
    fn placeholders_are_blanked() {
        assert_eq!(clip("Unknown", 50), "");
        assert_eq!(clip(" N/A. ", 50), "");
        assert_eq!(clip("R35 000", 50), "R35 000");
        assert_eq!(clip("a very long value indeed", 10), "a very lo…");
    }

    #[test]
    fn excerpt_keeps_head_and_deadline_windows() {
        let filler = "Lorem ipsum dolor sit amet. ".repeat(400);
        let text = format!(
            "# Title\n\nIntro text. {filler} The closing date is 18 December 2026. {filler}"
        );
        let ex = excerpt(&text, 2000, &["closing date", "deadline"]);
        assert!(ex.starts_with("# Title"));
        assert!(ex.contains("18 December 2026"), "{ex}");
        assert!(ex.chars().count() < 2300);
        assert_eq!(excerpt("short", 100, &["x"]), "short");
        // Multibyte text never splits a character.
        let uni = "é".repeat(5000) + " deadline ünïcödé " + &"ß".repeat(5000);
        let ex = excerpt(&uni, 1000, &["deadline"]);
        assert!(ex.contains("deadline"));
    }

    #[test]
    fn ips() {
        assert_eq!(public_ip("102.65.1.2").as_deref(), Some("102.65.1.2"));
        assert_eq!(public_ip("127.0.0.1"), None);
        assert_eq!(public_ip("10.1.2.3"), None);
        assert_eq!(public_ip("192.168.0.1"), None);
        assert_eq!(public_ip("100.64.0.1"), None);
        assert_eq!(public_ip("::1"), None);
        assert_eq!(public_ip("fd00::1"), None);
        assert_eq!(public_ip("2a02:8070::1").as_deref(), Some("2a02:8070::1"));
        assert_eq!(public_ip("nonsense"), None);
    }

    #[test]
    fn parallel_map_keeps_order_survives_panics_and_stops() {
        let stop = AtomicBool::new(false);
        let items: Vec<u32> = (0..50).collect();
        let out = parallel_map(&items, 8, &stop, |i, x| {
            if *x == 7 {
                panic!("boom");
            }
            i as u32 * 2
        });
        assert_eq!(out.len(), 50);
        assert_eq!(out[3], Some(6));
        assert_eq!(out[7], None);
        assert_eq!(out[49], Some(98));
        stop.store(true, Ordering::Relaxed);
        assert!(
            parallel_map(&items, 4, &stop, |_, x| *x)
                .iter()
                .all(Option::is_none)
        );
        assert!(parallel_map(&Vec::<u8>::new(), 4, &stop, |_, x| *x).is_empty());
    }

    #[test]
    fn keys_and_case() {
        assert_eq!(
            title_key("The TokioConf 2027 — CFP!"),
            "thetokioconf2027cfp"
        );
        assert_eq!(title_case("climate tech AI"), "Climate Tech AI");
    }
}
