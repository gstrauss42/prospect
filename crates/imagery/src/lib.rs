//! The background photo for someone's page.
//!
//! Given a [`ThemeHint`] (label, photo searches, mood), find candidate photos from keyless,
//! openly licensed sources (Openverse, Wikimedia Commons), shortlist the ones big and wide enough,
//! let `deepseek-flash` look at small versions and pick the most beautiful on-theme one (vision,
//! base64 images at low detail — DeepSeek can't always download third-party URLs itself), then
//! download the winner and return its bytes with a proper credit line.
//!
//! The pipeline, frugal by design (about four search requests, ten small downloads and one
//! vision call of ~2–3k tokens per theme):
//!
//! 1. **Search** both sources for the first two photo searches, in parallel; a second, smaller
//!    wave (the other searches, Commons beyond its Quality images) only when the first finds too
//!    little. A source that is down is skipped; only both down is an error.
//! 2. **Normalise** every result to a [`Candidate`] and **filter**: landscape (1.3–2.4), at least
//!    1600 px wide, CC0 / public domain / CC BY / CC BY-SA, not mature, a photo.
//! 3. **Rank** by heuristics (stock sources and Commons Quality images first, title relevance,
//!    resolution of the large version, no "flood"/"logo"/"portrait" titles), dedupe, at most two
//!    per photographer, alternating sources.
//! 4. **Download** up to ten previews (~1000 px) in parallel, size-capped and sniffed; keep the
//!    first eight valid ones.
//! 5. **Curate**: one vision call scores them like an art director and picks one, or none (then
//!    one more batch is tried, if there is one). If the call fails, the best-ranked preview wins.
//! 6. **Download the winner** at ~1920 px (falling back to its preview), sniff it, and build the
//!    [`ThemeImage`] with a credit line such as `Photo: Jane Doe · CC BY 2.0 · via Openverse`.
//!
//! See docs/ARCHITECTURE.md ("Theme"). Owner: the imagery & mail builder.

pub mod candidates;
mod curate;
mod formats;
mod net;

use std::time::{Duration, Instant};

use model::text::truncate;
use model::{ThemeHint, ThemeImage};

pub use candidates::{Candidate, Provider, credit_line};
pub use curate::Verdict;
pub use formats::{is_complete, sniff};

use net::{FetchError, Image, Net, parallel};

#[derive(Clone, Debug)]
pub struct Config {
    /// Openverse API base, default `https://api.openverse.org/v1`.
    pub openverse_url: String,
    /// Wikimedia Commons API endpoint, default `https://commons.wikimedia.org/w/api.php`.
    pub commons_api_url: String,
    /// Sent on every request (Wikimedia requires a descriptive one).
    pub user_agent: String,
    pub timeout: Duration,
    /// Largest image accepted, in bytes.
    pub max_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            openverse_url: "https://api.openverse.org/v1".into(),
            commons_api_url: "https://commons.wikimedia.org/w/api.php".into(),
            user_agent: concat!(
                "ProspectApp/",
                env!("CARGO_PKG_VERSION"),
                " (self-hosted business intelligence)"
            )
            .into(),
            timeout: Duration::from_secs(25),
            max_bytes: 12 * 1024 * 1024,
        }
    }
}

impl Config {
    /// Defaults, overridden by `OPENVERSE_URL` and `COMMONS_API_URL` (tests point them at the mock).
    /// When `PUBLIC_URL` is a public https address it is added to the User-Agent, so Wikimedia can
    /// tell who is calling, as its policy asks.
    pub fn from_env() -> Config {
        let mut c = Config::default();
        if let Ok(v) = std::env::var("OPENVERSE_URL")
            && !v.trim().is_empty()
        {
            c.openverse_url = v.trim().trim_end_matches('/').to_string();
        }
        if let Ok(v) = std::env::var("COMMONS_API_URL")
            && !v.trim().is_empty()
        {
            c.commons_api_url = v.trim().to_string();
        }
        if let Ok(public) = std::env::var("PUBLIC_URL") {
            let public = public.trim().trim_end_matches('/');
            let host = model::links::host(public).unwrap_or_default();
            if public.starts_with("https://")
                && host != "localhost"
                && !host.starts_with("127.")
                && !public.contains(char::is_whitespace)
            {
                c.user_agent = format!(
                    "ProspectApp/{} (+{public}; self-hosted business intelligence)",
                    env!("CARGO_PKG_VERSION")
                );
            }
        }
        c
    }
}

/// A chosen photo: the bytes to store and serve, and its description.
#[derive(Clone, Debug, PartialEq)]
pub struct Background {
    pub bytes: Vec<u8>,
    pub image: ThemeImage,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// No candidate was good enough (the page falls back to a gradient of the accent colour).
    NothingSuitable,
    /// The image sources couldn't be reached.
    Network(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NothingSuitable => write!(f, "No suitable background photo was found"),
            Error::Network(m) => write!(f, "Couldn't reach the image sources: {m}"),
        }
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------------------------------------
// Tuning
// ---------------------------------------------------------------------------------------------

/// Photo searches used in the first wave (per source).
const FIRST_WAVE_QUERIES: usize = 2;
/// Fewer usable candidates than this after the first wave triggers a second, smaller one.
const ENOUGH_CANDIDATES: usize = 6;
const OPENVERSE_PAGE_SIZE: &str = "20";
const COMMONS_LIMIT: &str = "15";
/// Candidates kept after ranking (enough for two batches).
const SHORTLIST: usize = 16;
/// Previews downloaded per batch, and the most sent to the vision model.
const BATCH_DOWNLOADS: usize = 10;
const MAX_PREVIEWS: usize = 8;
/// Caps for one preview and for all previews in one vision call.
const PREVIEW_MAX_BYTES: usize = 3 * 1024 * 1024;
const PREVIEW_BUDGET_BYTES: usize = 6 * 1024 * 1024;
/// A preview narrower than this is too small to end up as a page background.
const MIN_PREVIEW_WIDTH: u32 = 600;
/// Previews keep the original's shape; a little slack for rounding and stale metadata.
const PREVIEW_MIN_ASPECT: f64 = 1.2;
const PREVIEW_MAX_ASPECT: f64 = 2.6;
/// Nothing this large in either dimension is served to a browser.
const MAX_DIMENSION: u32 = 10_000;
/// Vision batches: a second is judged only when the first had nothing good enough.
const MAX_ROUNDS: usize = 2;

// ---------------------------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------------------------

/// What one search for a background did, for logs, run diagnostics and the live example.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// The photo searches used, cleaned up.
    pub queries: Vec<String>,
    /// Search requests made to Openverse and Commons.
    pub search_requests: u32,
    /// Sources that failed, with why (the search carries on with the other).
    pub source_errors: Vec<String>,
    /// Candidates the sources returned, how many passed the filters, how many were shortlisted.
    pub found: usize,
    pub suitable: usize,
    pub shortlisted: usize,
    /// Every valid preview that was judged.
    pub previews: Vec<PreviewInfo>,
    /// Downloads that produced no usable preview: "title: why".
    pub failed_downloads: Vec<String>,
    /// One per vision call.
    pub verdicts: Vec<Verdict>,
    /// Why the vision call failed, when it did (the best-ranked preview is used instead).
    pub vision_error: Option<String>,
    /// How the winner was chosen: "vision" or "ranking".
    pub decision: String,
    pub winner: Option<Candidate>,
    /// True when the large version was downloaded, false when the preview was kept.
    pub used_full_size: bool,
    pub elapsed_ms: u64,
}

/// A preview that was downloaded and judged.
#[derive(Clone, Debug)]
pub struct PreviewInfo {
    pub candidate: Candidate,
    pub url: String,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub size: usize,
    /// The bytes, only when asked for with `keep_previews`.
    pub bytes: Vec<u8>,
}

impl Report {
    /// One line for a log.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} searches, {} found, {} suitable, {} previews",
            self.search_requests,
            self.found,
            self.suitable,
            self.previews.len()
        );
        if !self.source_errors.is_empty() {
            s.push_str(&format!(
                ", source errors: {}",
                self.source_errors.join("; ")
            ));
        }
        if let Some(e) = &self.vision_error {
            s.push_str(&format!(", vision failed: {e}"));
        }
        if let Some(w) = &self.winner {
            s.push_str(&format!(
                ", chose \"{}\" ({:?}/{}) by {}{}",
                w.title,
                w.provider,
                w.source,
                self.decision,
                if self.used_full_size {
                    ", full size"
                } else {
                    ", preview size"
                }
            ));
        } else {
            s.push_str(", no winner");
        }
        s.push_str(&format!(" in {} ms", self.elapsed_ms));
        s
    }
}

// ---------------------------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------------------------

/// Find, judge and download the best background photo for a theme.
pub fn find_background(
    cfg: &Config,
    ai: &deepseek::Client,
    hint: &ThemeHint,
) -> Result<Background, Error> {
    find_background_report(cfg, ai, hint, false).0
}

/// [`find_background`], also returning a [`Report`] of what it did. With `keep_previews` the
/// report carries the preview bytes too (for inspecting the vision model's choice).
pub fn find_background_report(
    cfg: &Config,
    ai: &deepseek::Client,
    hint: &ThemeHint,
    keep_previews: bool,
) -> (Result<Background, Error>, Report) {
    let started = Instant::now();
    let mut report = Report::default();
    let result = run(cfg, ai, hint, keep_previews, &mut report);
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    (result, report)
}

/// Up to four photo searches, cleaned of search-syntax characters, with the label as a last resort.
fn clean_queries(hint: &ThemeHint) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in hint
        .image_queries
        .iter()
        .chain(std::iter::once(&hint.label))
    {
        let cleaned: String = raw
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '\'' || c == '-' {
                    c
                } else {
                    ' '
                }
            })
            .collect();
        let mut q = String::new();
        for w in cleaned
            .split_whitespace()
            .map(|w| w.trim_matches(['-', '\'']))
            .filter(|w| !w.is_empty())
        {
            if q.chars().count() + w.chars().count() + 1 > 60 {
                break;
            }
            if !q.is_empty() {
                q.push(' ');
            }
            q.push_str(w);
        }
        if q.is_empty() || out.iter().any(|o| o.to_lowercase() == q.to_lowercase()) {
            continue;
        }
        out.push(q);
        if out.len() >= 4 {
            break;
        }
    }
    out
}

#[derive(Clone, Debug)]
enum Search {
    Openverse {
        query: String,
        index: usize,
    },
    Commons {
        query: String,
        index: usize,
        quality: bool,
    },
}

impl Search {
    fn is_openverse(&self) -> bool {
        matches!(self, Search::Openverse { .. })
    }
}

fn search(net: &Net, cfg: &Config, s: &Search) -> Result<Vec<Candidate>, String> {
    match s {
        Search::Openverse { query, index } => {
            let url = format!("{}/images/", cfg.openverse_url.trim_end_matches('/'));
            let v = net.get_json(
                &url,
                &[
                    ("q", query.as_str()),
                    ("aspect_ratio", "wide"),
                    ("size", "large"),
                    ("mature", "false"),
                    ("license", "cc0,pdm,by,by-sa"),
                    ("page_size", OPENVERSE_PAGE_SIZE),
                ],
            )?;
            if !v["results"].is_array() {
                return Err(format!(
                    "unexpected reply: {}",
                    truncate(&v.to_string(), 160)
                ));
            }
            Ok(candidates::parse_openverse(&v, *index))
        }
        Search::Commons {
            query,
            index,
            quality,
        } => {
            let search = format!(
                "{query} filetype:bitmap filew:>1800{}",
                if *quality {
                    " incategory:Quality_images"
                } else {
                    ""
                }
            );
            let v = net.get_json(
                &cfg.commons_api_url,
                &[
                    ("action", "query"),
                    ("format", "json"),
                    ("formatversion", "2"),
                    ("generator", "search"),
                    ("gsrnamespace", "6"),
                    ("gsrlimit", COMMONS_LIMIT),
                    ("gsrsearch", search.as_str()),
                    ("prop", "imageinfo"),
                    ("iiprop", "url|size|mime|extmetadata"),
                    (
                        "iiextmetadatafilter",
                        "LicenseShortName|License|Artist|ObjectName",
                    ),
                    ("iiurlwidth", "960"),
                ],
            )?;
            if let Some(info) = v["error"]["info"].as_str() {
                return Err(format!("API error: {}", truncate(info, 160)));
            }
            Ok(candidates::parse_commons(&v, *quality, *index))
        }
    }
}

/// Run a wave of searches in parallel. Returns the candidates and, per source, whether any
/// request succeeded.
fn search_wave(
    net: &Net,
    cfg: &Config,
    wave: Vec<Search>,
    report: &mut Report,
) -> (Vec<Candidate>, bool, bool) {
    report.search_requests += wave.len() as u32;
    let results = parallel(wave.clone(), |s| search(net, cfg, &s));
    let (mut found, mut ov_ok, mut cm_ok) = (Vec::new(), false, false);
    for (s, r) in wave.iter().zip(results) {
        let name = if s.is_openverse() {
            "Openverse"
        } else {
            "Commons"
        };
        match r {
            Some(Ok(c)) => {
                if s.is_openverse() {
                    ov_ok = true;
                } else {
                    cm_ok = true;
                }
                found.extend(c);
            }
            Some(Err(e)) => report.source_errors.push(format!("{name}: {e}")),
            None => report.source_errors.push(format!("{name}: search crashed")),
        }
    }
    (found, ov_ok, cm_ok)
}

fn preview_usable(img: &Image) -> bool {
    let a = img.aspect();
    img.width >= MIN_PREVIEW_WIDTH
        && img.width <= MAX_DIMENSION
        && img.height <= MAX_DIMENSION
        && (PREVIEW_MIN_ASPECT..=PREVIEW_MAX_ASPECT).contains(&a)
}

/// Download previews for a batch in parallel; the valid ones in rank order, within the count and
/// byte budget. The bool is true when every failure was a network failure.
fn download_previews(
    net: &Net,
    cfg: &Config,
    batch: &[Candidate],
    report: &mut Report,
) -> (Vec<(Candidate, Image)>, bool) {
    let cap = cfg.max_bytes.min(PREVIEW_MAX_BYTES);
    let results = parallel(batch.to_vec(), |c| {
        let mut why: Vec<String> = Vec::new();
        let mut network_only = true;
        for url in &c.preview_urls {
            match net.get_image(url, cap) {
                Ok(img) if preview_usable(&img) => return (c, Ok(img)),
                Ok(img) => {
                    network_only = false;
                    why.push(format!("{}×{} preview not usable", img.width, img.height));
                }
                Err(e) => {
                    network_only &= matches!(e, FetchError::Network(_));
                    why.push(e.to_string());
                }
            }
        }
        (c, Err((why.join("; "), network_only)))
    });
    let mut out: Vec<(Candidate, Image)> = Vec::new();
    let mut total = 0usize;
    let mut all_network = true;
    for r in results {
        match r {
            Some((c, Ok(img))) => {
                if out.len() < MAX_PREVIEWS && total + img.bytes.len() <= PREVIEW_BUDGET_BYTES {
                    total += img.bytes.len();
                    out.push((c, img));
                }
            }
            Some((c, Err((why, network_only)))) => {
                all_network &= network_only;
                report.failed_downloads.push(format!("{}: {why}", c.title));
            }
            None => all_network = false,
        }
    }
    (out, all_network)
}

/// Ask the vision model. Ok(Some(i)) = preview i won, Ok(None) = none good enough.
fn judge(
    ai: &deepseek::Client,
    hint: &ThemeHint,
    previews: &[(Candidate, Image)],
    report: &mut Report,
) -> Result<Option<usize>, String> {
    let images: Vec<(&'static str, &[u8])> = previews
        .iter()
        .map(|(_, img)| (img.mime, img.bytes.as_slice()))
        .collect();
    let req = curate::request(hint, &images);
    let v = ai.chat_json_value(&req).map_err(|e| e.to_string())?;
    let verdict = curate::parse_verdict(&v, previews.len())?;
    let best = verdict.best;
    report.verdicts.push(verdict);
    Ok(best)
}

/// The large version of the winner, when it downloads and is the same picture as the preview
/// (same shape, at least as wide).
fn download_full(net: &Net, cfg: &Config, c: &Candidate, preview: &Image) -> Option<Image> {
    for url in c.full_urls.iter().filter(|u| **u != preview.url) {
        let Ok(img) = net.get_image(url, cfg.max_bytes) else {
            continue;
        };
        let same_shape = (img.aspect() - preview.aspect()).abs() / preview.aspect() <= 0.03;
        if same_shape
            && img.width >= preview.width
            && img.width <= MAX_DIMENSION
            && img.height <= MAX_DIMENSION
        {
            return Some(img);
        }
    }
    None
}

fn run(
    cfg: &Config,
    ai: &deepseek::Client,
    hint: &ThemeHint,
    keep_previews: bool,
    report: &mut Report,
) -> Result<Background, Error> {
    let queries = clean_queries(hint);
    report.queries = queries.clone();
    if queries.is_empty() {
        return Err(Error::NothingSuitable);
    }
    let net = Net::new(cfg);

    // 1. Search: the first two photo searches on both sources.
    let mut wave = Vec::new();
    for (index, q) in queries.iter().enumerate().take(FIRST_WAVE_QUERIES) {
        wave.push(Search::Openverse {
            query: q.clone(),
            index,
        });
        wave.push(Search::Commons {
            query: q.clone(),
            index,
            quality: true,
        });
    }
    let (mut found, ov_ok, cm_ok) = search_wave(&net, cfg, wave, report);
    let (mut any_ov, mut any_cm) = (ov_ok, cm_ok);
    let usable = |found: &[Candidate]| {
        found
            .iter()
            .filter(|c| candidates::rejection(c).is_none())
            .count()
    };
    if usable(&found) < ENOUGH_CANDIDATES && (ov_ok || cm_ok) {
        // A second, smaller wave, only to sources that answered.
        let mut wave = Vec::new();
        for (index, q) in queries.iter().enumerate().skip(FIRST_WAVE_QUERIES).take(2) {
            if ov_ok {
                wave.push(Search::Openverse {
                    query: q.clone(),
                    index,
                });
            }
            if cm_ok {
                wave.push(Search::Commons {
                    query: q.clone(),
                    index,
                    quality: true,
                });
            }
        }
        if cm_ok {
            wave.push(Search::Commons {
                query: queries[0].clone(),
                index: 0,
                quality: false,
            });
        }
        if !wave.is_empty() {
            let (more, ov2, cm2) = search_wave(&net, cfg, wave, report);
            found.extend(more);
            any_ov |= ov2;
            any_cm |= cm2;
        }
    }
    if !any_ov && !any_cm {
        return Err(Error::Network(report.source_errors.join("; ")));
    }
    report.found = found.len();
    report.suitable = usable(&found);

    // 2–3. Filter, rank, dedupe, diversify.
    let ranked = candidates::shortlist(found, hint, SHORTLIST);
    report.shortlisted = ranked.len();
    if ranked.is_empty() {
        return Err(Error::NothingSuitable);
    }

    // 4–5. Previews and the vision pick, in up to two batches.
    let mut winner: Option<(Candidate, Image)> = None;
    let mut downloads_all_network = true;
    let mut offset = 0;
    for _round in 0..MAX_ROUNDS {
        let batch: Vec<Candidate> = ranked
            .iter()
            .skip(offset)
            .take(BATCH_DOWNLOADS)
            .cloned()
            .collect();
        if batch.is_empty() {
            break;
        }
        offset += batch.len();
        let (previews, all_network) = download_previews(&net, cfg, &batch, report);
        if previews.is_empty() {
            downloads_all_network &= all_network;
            continue;
        }
        downloads_all_network = false;
        report
            .previews
            .extend(previews.iter().map(|(c, img)| PreviewInfo {
                candidate: c.clone(),
                url: img.url.clone(),
                mime: img.mime,
                width: img.width,
                height: img.height,
                size: img.bytes.len(),
                bytes: if keep_previews {
                    img.bytes.clone()
                } else {
                    Vec::new()
                },
            }));
        match judge(ai, hint, &previews, report) {
            Ok(Some(i)) => {
                report.decision = "vision".into();
                winner = previews.into_iter().nth(i);
                break;
            }
            Ok(None) => {
                // Nothing good enough here; try the next batch if there are a few more.
                if ranked.len().saturating_sub(offset) < 3 {
                    break;
                }
            }
            Err(e) => {
                report.vision_error = Some(truncate(&e, 300));
                report.decision = "ranking".into();
                winner = previews.into_iter().next();
                break;
            }
        }
    }
    let Some((cand, preview)) = winner else {
        if report.previews.is_empty()
            && downloads_all_network
            && !report.failed_downloads.is_empty()
        {
            return Err(Error::Network(
                "none of the candidate photos could be downloaded".into(),
            ));
        }
        return Err(Error::NothingSuitable);
    };

    // 6. The large version, or the preview if that fails.
    let full = download_full(&net, cfg, &cand, &preview);
    report.used_full_size = full.is_some();
    let img = full.unwrap_or(preview);
    let image = ThemeImage {
        mime: img.mime.to_string(),
        width: img.width,
        height: img.height,
        credit: credit_line(&cand),
        source_url: cand.landing_url.clone(),
    };
    report.winner = Some(cand);
    Ok(Background {
        bytes: img.bytes,
        image,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_cleaned_capped_and_deduped() {
        let h = ThemeHint {
            label: "Cooking".into(),
            image_queries: vec![
                "  chef plating food ".into(),
                "-site:example.com \"kitchen\" (herbs) | fire".into(),
                "Chef plating FOOD".into(),
                "".into(),
                "a very long query that goes on and on about kitchens and markets and spices and more".into(),
                "fifth".into(),
            ],
            ..Default::default()
        };
        let q = clean_queries(&h);
        assert_eq!(q[0], "chef plating food");
        assert_eq!(q[1], "site example com kitchen herbs fire");
        assert!(
            q[2].chars().count() <= 60 && q[2].starts_with("a very long query"),
            "{q:?}"
        );
        assert_eq!(q.len(), 4);
        assert_eq!(q[3], "fifth");
        let only_label = ThemeHint {
            label: "Jazz".into(),
            ..Default::default()
        };
        assert_eq!(clean_queries(&only_label), vec!["Jazz"]);
        assert!(clean_queries(&ThemeHint::default()).is_empty());
    }

    #[test]
    fn nothing_to_search_is_nothing_suitable_without_any_request() {
        let ai = deepseek::Client::new(deepseek::Config::new("unused"));
        let cfg = Config {
            openverse_url: "http://127.0.0.1:9/ov".into(),
            commons_api_url: "http://127.0.0.1:9/c".into(),
            ..Config::default()
        };
        let (r, report) = find_background_report(&cfg, &ai, &ThemeHint::default(), false);
        assert_eq!(r, Err(Error::NothingSuitable));
        assert_eq!(report.search_requests, 0);
    }

    #[test]
    fn unreachable_sources_are_a_network_error() {
        let ai = deepseek::Client::new(deepseek::Config::new("unused"));
        // Port 9 (discard) is closed on test machines: connections are refused at once.
        let cfg = Config {
            openverse_url: "http://127.0.0.1:9/ov".into(),
            commons_api_url: "http://127.0.0.1:9/c".into(),
            timeout: Duration::from_secs(3),
            ..Config::default()
        };
        let hint = ThemeHint {
            label: "Cooking".into(),
            image_queries: vec!["kitchen".into()],
            ..Default::default()
        };
        let (r, report) = find_background_report(&cfg, &ai, &hint, false);
        assert!(matches!(r, Err(Error::Network(_))), "{r:?}");
        // "kitchen" and the label "Cooking", on both sources; no second wave to dead sources.
        assert_eq!(report.queries, vec!["kitchen", "Cooking"]);
        assert_eq!(report.search_requests, 4);
        assert_eq!(report.source_errors.len(), 4);
        assert!(
            ai.usage().chat_calls == 0 && ai.usage().failed_calls == 0,
            "no vision call without candidates"
        );
    }

    #[test]
    fn report_summary_reads_well() {
        let r = Report {
            search_requests: 4,
            found: 40,
            suitable: 22,
            decision: "vision".into(),
            elapsed_ms: 1234,
            ..Default::default()
        };
        let s = r.summary();
        assert!(
            s.starts_with("4 searches, 40 found, 22 suitable, 0 previews"),
            "{s}"
        );
        assert!(s.ends_with("no winner in 1234 ms"), "{s}");
    }
}
