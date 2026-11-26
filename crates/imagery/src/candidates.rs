//! Candidate photos: what Openverse and Wikimedia Commons return, normalised to one shape, then
//! filtered (big, wide, openly licensed, not mature) and ranked by cheap heuristics before any
//! image is downloaded.

use model::ThemeHint;
use model::text::{squash, truncate};
use serde_json::Value;

/// Which API a candidate came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Openverse,
    Commons,
}

/// One photo that might become the background.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub provider: Provider,
    /// Where it is hosted, lowercase: "stocksnap", "rawpixel", "flickr", "wikimedia"…
    pub source: String,
    pub title: String,
    pub creator: String,
    /// The licence as people read it: "CC0", "Public domain", "CC BY 2.0", "CC BY-SA 4.0".
    pub license: String,
    /// True for licences we may use for a background: CC0, public domain, CC BY, CC BY-SA.
    pub license_ok: bool,
    /// The photo's own page, for the credit link.
    pub landing_url: String,
    /// Small versions (~1000 px) to judge, in order of preference.
    pub preview_urls: Vec<String>,
    /// Large versions (~1920 px) to download if it wins, in order of preference.
    pub full_urls: Vec<String>,
    /// Size of the original.
    pub width: u32,
    pub height: u32,
    /// Expected width of the best large download, 0 when unknown.
    pub full_width: u32,
    pub mature: bool,
    /// Openverse category ("photograph", "illustration"…) or file type; empty when unknown.
    pub kind: String,
    pub tags: Vec<String>,
    /// From Commons' "Quality images" category (peer-reviewed, technically excellent photos).
    pub quality: bool,
    /// Which image query found it and its rank in that query's results (0 = first).
    pub query: usize,
    pub rank: usize,
    /// Heuristic score, set by [`shortlist`].
    pub score: f64,
}

/// Openverse sources that are mostly scans of museum objects and documents rather than photos.
const ARCHIVE_SOURCES: &[&str] = &[
    "smithsonian",
    "museum",
    "met",
    "library",
    "archive",
    "europeana",
    "rijksmuseum",
    "nypl",
    "brooklyn",
    "cleveland",
    "sciencemuseum",
    "thorvaldsens",
    "statensmuseum",
    "geographorguk",
    "biodiversity",
    "inaturalist",
];

/// Words in a title or tag that point to a poor background: text, people as the subject,
/// catastrophes, graphics.
const BAD_WORDS: &[&str] = &[
    "logo",
    "logos",
    "diagram",
    "chart",
    "map",
    "poster",
    "flyer",
    "leaflet",
    "screenshot",
    "infographic",
    "sign",
    "signage",
    "document",
    "letter",
    "newspaper",
    "cover",
    "label",
    "text",
    "portrait",
    "selfie",
    "headshot",
    "flood",
    "flooded",
    "flooding",
    "submerged",
    "fire",
    "burning",
    "burnt",
    "disaster",
    "accident",
    "crash",
    "ruin",
    "ruins",
    "damage",
    "damaged",
    "destroyed",
    "dead",
    "death",
    "corpse",
    "war",
    "riot",
    "protest",
    "funeral",
    "blood",
    "injury",
    "wound",
    "nude",
    "naked",
    "illustration",
    "drawing",
    "vector",
    "icon",
    "clipart",
    "cartoon",
    "scan",
    "scanned",
    "mine",
    "shaft",
    "x-ray",
    "mugshot",
    "garbage",
    "trash",
    "litter",
    "toilet",
];

/// Words that carry no meaning for matching a photo to a theme.
const STOP_WORDS: &[&str] = &[
    "the",
    "and",
    "with",
    "for",
    "from",
    "into",
    "over",
    "under",
    "near",
    "photo",
    "photos",
    "image",
    "images",
    "picture",
    "background",
    "close",
    "view",
    "beautiful",
    "stunning",
    "atmospheric",
    "moody",
    "dark",
    "light",
    "shot",
    "wide",
    "landscape",
    "scene",
    "style",
];

// ---------------------------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------------------------

/// Plain text from a snippet of HTML (Commons' `Artist` field is HTML): tags removed, list items
/// and line breaks turned into "; ", entities decoded, whitespace squashed.
pub fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after = &rest[lt..];
        let Some(gt) = after.find('>') else {
            // A lone "<" is text.
            out.push_str(after);
            rest = "";
            break;
        };
        let tag = after[1..gt].trim().to_ascii_lowercase();
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        let closing = tag.starts_with('/');
        if name == "br"
            || (closing && matches!(name.as_str(), "li" | "p" | "div" | "tr" | "dd" | "dt"))
        {
            out.push_str("; ");
        } else {
            out.push(' ');
        }
        rest = &after[gt + 1..];
    }
    out.push_str(rest);
    let text = squash(&decode_entities(&out));
    // Tidy the separators the block tags left behind.
    let parts: Vec<&str> = text
        .split(';')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    let joined = parts.join("; ");
    // A tag between a word and its punctuation leaves a stray space: "Name , 2020" → "Name, 2020".
    joined
        .replace(" ,", ",")
        .replace(" .", ".")
        .replace("( ", "(")
        .replace(" )", ")")
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let end = after.find(';').filter(|&e| e <= 10);
        let decoded = end.and_then(|e| {
            let name = &after[1..e];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                _ if name.starts_with("#x") || name.starts_with("#X") => {
                    u32::from_str_radix(&name[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                }
                _ if name.starts_with('#') => {
                    name[1..].parse::<u32>().ok().and_then(char::from_u32)
                }
                _ => None,
            };
            c.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &after[e + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `%C3%BC` → `ü`; invalid escapes are kept as they are.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && b[i + 1].is_ascii_hexdigit()
            && b[i + 2].is_ascii_hexdigit()
        {
            let digit = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            out.push(digit(b[i + 1]) * 16 + digit(b[i + 2]));
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A display title: no "File:" prefix, no file extension, squashed, at most 140 characters.
fn clean_title(raw: &str) -> String {
    let t = squash(&strip_html(raw));
    let t = t.strip_prefix("File:").unwrap_or(&t);
    let lower = t.to_ascii_lowercase();
    let t = [".jpg", ".jpeg", ".png", ".webp", ".tif", ".tiff"]
        .iter()
        .find(|ext| lower.ends_with(*ext))
        .map(|ext| &t[..t.len() - ext.len()])
        .unwrap_or(t);
    truncate(&squash(&t.replace('_', " ")), 140)
}

fn clean_creator(raw: &str) -> String {
    truncate(&strip_html(raw), 70)
}

fn http_url(v: &Value) -> Option<String> {
    let s = v.as_str()?.trim();
    model::links::is_http_url(s).then(|| s.to_string())
}

fn host_of(url: &str) -> String {
    model::links::host(url).unwrap_or_default()
}

/// Lowercase words for matching, with a plural "s" dropped: "Herbs, chopped" → ["herb", "chopped"].
pub(crate) fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| {
            let w = w.to_lowercase();
            if w.chars().count() > 3 && w.ends_with('s') && !w.ends_with("ss") {
                w[..w.len() - 1].to_string()
            } else {
                w
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Licences
// ---------------------------------------------------------------------------------------------

/// An Openverse licence code and version as people read it, or None when it isn't one we may use.
/// `("by", "2.0")` → "CC BY 2.0"; `("cc0", "1.0")` → "CC0"; `("by-nc", "2.0")` → None.
pub fn openverse_license(code: &str, version: &str) -> Option<String> {
    let v = version.trim();
    let with_version = |name: &str| {
        if v.is_empty() {
            name.to_string()
        } else {
            format!("{name} {v}")
        }
    };
    match code.trim().to_ascii_lowercase().as_str() {
        "cc0" => Some("CC0".into()),
        "pdm" => Some("Public domain".into()),
        "by" => Some(with_version("CC BY")),
        "by-sa" => Some(with_version("CC BY-SA")),
        _ => None,
    }
}

/// A Commons licence (`LicenseShortName` such as "CC BY-SA 4.0", and the machine `License` code
/// such as "cc-by-sa-4.0") as people read it, or None when it isn't one we may use (GFDL-only,
/// fair use, anything non-commercial or no-derivatives…).
pub fn commons_license(short: &str, code: &str) -> Option<String> {
    let short = squash(&strip_html(short));
    let basis = if code.trim().is_empty() {
        short.clone()
    } else {
        code.trim().to_string()
    };
    let norm = basis.to_ascii_lowercase().replace([' ', '_'], "-");
    let tokens: Vec<&str> = norm.split('-').filter(|t| !t.is_empty()).collect();
    if tokens.iter().any(|t| matches!(*t, "nc" | "nd")) {
        return None;
    }
    let label = |fallback: String| {
        if short.is_empty() {
            fallback
        } else {
            truncate(&short, 40)
        }
    };
    if norm.starts_with("cc0") || norm == "cc-zero" {
        return Some("CC0".into());
    }
    if norm.starts_with("pd") || norm.contains("public-domain") {
        return Some(label("Public domain".into()));
    }
    let version: String = tokens
        .iter()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|v| v.to_string())
        .unwrap_or_default();
    let named = |name: &str| {
        if version.is_empty() {
            name.to_string()
        } else {
            format!("{name} {version}")
        }
    };
    if norm.starts_with("cc-by-sa") {
        return Some(label(named("CC BY-SA")));
    }
    if norm.starts_with("cc-by") {
        return Some(label(named("CC BY")));
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Source-specific URL knowledge
// ---------------------------------------------------------------------------------------------

/// The standard-width thumbnail of a Wikimedia Commons original:
/// `…/wikipedia/commons/a/ab/Name.jpg` → `…/wikipedia/commons/thumb/a/ab/Name.jpg/960px-Name.jpg`.
/// Commons only serves standard widths (250, 330, 500, 960, 1280, 1920, 3840…). Only for JPEG and
/// PNG originals, whose thumbnails keep the file name.
pub fn wikimedia_thumb(url: &str, width: u32) -> Option<String> {
    let clean = url.split(['?', '#']).next().unwrap_or(url);
    let at = clean.find("/wikipedia/commons/")?;
    let (base, rest) = clean.split_at(at + "/wikipedia/commons/".len());
    let parts: Vec<&str> = rest.split('/').collect();
    let [a, ab, name] = parts.as_slice() else {
        return None;
    };
    let hex = |s: &str| s.chars().all(|c| c.is_ascii_hexdigit());
    if a.len() != 1 || ab.len() != 2 || !hex(a) || !hex(ab) || !ab.starts_with(a) || name.is_empty()
    {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    if ![".jpg", ".jpeg", ".png"].iter().any(|e| lower.ends_with(e)) {
        return None;
    }
    // Wikimedia serves thumbnails from thumb.wikimedia.org (the host its API hands out); the
    // upload host rate-limits bursts of thumbnail requests much sooner.
    let base = base.replacen("://upload.wikimedia.org/", "://thumb.wikimedia.org/", 1);
    Some(format!("{base}thumb/{a}/{ab}/{name}/{width}px-{name}"))
}

/// The width of a Flickr static image from its size suffix (`_b.jpg` = 1024 px).
fn flickr_width(url: &str) -> Option<u32> {
    let file = url.split(['?', '#']).next()?.rsplit('/').next()?;
    let stem = file.rsplit_once('.')?.0;
    let suffix = stem.rsplit_once('_')?.1;
    Some(match suffix {
        "z" => 640,
        "c" => 800,
        "b" => 1024,
        "h" => 1600,
        "k" => 2048,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------------

/// Candidates from an Openverse `/v1/images/` reply.
pub fn parse_openverse(v: &Value, query: usize) -> Vec<Candidate> {
    let Some(results) = v["results"].as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (rank, r) in results.iter().enumerate() {
        let Some(url) = http_url(&r["url"]) else {
            continue;
        };
        let source = r["source"]
            .as_str()
            .or_else(|| r["provider"].as_str())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let width = r["width"].as_u64().unwrap_or(0).min(u32::MAX as u64) as u32;
        let height = r["height"].as_u64().unwrap_or(0).min(u32::MAX as u64) as u32;
        let code = r["license"].as_str().unwrap_or("");
        let license = openverse_license(code, r["license_version"].as_str().unwrap_or(""));
        let thumbnail = http_url(&r["thumbnail"]);
        let filesize = r["filesize"].as_u64().unwrap_or(0);
        let sensitive = r["unstable__sensitivity"]
            .as_array()
            .is_some_and(|a| !a.is_empty());

        let mut preview_urls = Vec::new();
        let mut full_urls = Vec::new();
        let full_width;
        if let Some(thumb) = (width > 960).then(|| wikimedia_thumb(&url, 960)).flatten() {
            preview_urls.push(thumb);
            if width > 1920 {
                full_urls.extend(wikimedia_thumb(&url, 1920));
                full_width = 1920;
            } else {
                full_urls.push(url.clone());
                full_width = width;
            }
        } else {
            // Stock sources link a ~1000 px version; an original that is many megabytes is only
            // fetched as the large version, with Openverse's own thumbnail as the preview.
            if filesize == 0 || filesize <= 3 * 1024 * 1024 {
                preview_urls.push(url.clone());
            }
            // (rawpixel's larger "image_1300" versions carry a watermark: keep the clean
            // 1024 px one Openverse links.)
            if host_of(&url) == "images.rawpixel.com" {
                full_width = 1024;
            } else if source == "stocksnap" || url.contains("/img-thumbs/960w/") {
                full_width = 960;
            } else if let Some(w) = flickr_width(&url) {
                full_width = w;
            } else {
                full_width = 0;
            }
            full_urls.push(url.clone());
        }
        if let Some(t) = thumbnail {
            preview_urls.push(t);
        }
        let mut kind = r["category"].as_str().unwrap_or("").to_ascii_lowercase();
        if let Some(ft) = r["filetype"].as_str().filter(|f| !f.is_empty())
            && (kind.is_empty()
                || !matches!(
                    ft.to_ascii_lowercase().as_str(),
                    "jpg" | "jpeg" | "png" | "webp"
                ))
        {
            kind = ft.to_ascii_lowercase();
        }
        let landing_url = http_url(&r["foreign_landing_url"])
            .or_else(|| http_url(&r["detail_url"]))
            .unwrap_or_else(|| url.clone());
        let tags = r["tags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| t["name"].as_str())
                    .map(|t| truncate(&squash(t), 40))
                    .take(30)
                    .collect()
            })
            .unwrap_or_default();
        out.push(Candidate {
            provider: Provider::Openverse,
            source,
            title: clean_title(r["title"].as_str().unwrap_or("")),
            creator: clean_creator(r["creator"].as_str().unwrap_or("")),
            license_ok: license.is_some(),
            license: license.unwrap_or_else(|| truncate(&squash(code), 20)),
            landing_url,
            preview_urls,
            full_urls,
            width,
            height,
            full_width,
            mature: r["mature"].as_bool() == Some(true) || sensitive,
            kind,
            tags,
            quality: false,
            query,
            rank,
            score: 0.0,
        });
    }
    out
}

/// Candidates from a Commons `action=query&generator=search&prop=imageinfo` reply (either
/// `formatversion`). `quality` says the search was limited to Quality images.
pub fn parse_commons(v: &Value, quality: bool, query: usize) -> Vec<Candidate> {
    let pages: Vec<&Value> = match &v["query"]["pages"] {
        Value::Array(a) => a.iter().collect(),
        Value::Object(o) => o.values().collect(),
        _ => return Vec::new(),
    };
    let mut pages = pages;
    // Search order is in `index`, not in the order pages are listed.
    pages.sort_by_key(|p| p["index"].as_u64().unwrap_or(u64::MAX));
    let mut out = Vec::new();
    for (rank, p) in pages.into_iter().enumerate() {
        let ii = &p["imageinfo"][0];
        let Some(thumb) = http_url(&ii["thumburl"]) else {
            continue;
        };
        let width = ii["width"].as_u64().unwrap_or(0).min(u32::MAX as u64) as u32;
        let height = ii["height"].as_u64().unwrap_or(0).min(u32::MAX as u64) as u32;
        let em = &ii["extmetadata"];
        let meta = |k: &str| em[k]["value"].as_str().unwrap_or("").to_string();
        let license = commons_license(&meta("LicenseShortName"), &meta("License"));
        let mut full_urls = Vec::new();
        let mut full_width = ii["thumbwidth"].as_u64().unwrap_or(960) as u32;
        let responsive = &ii["responsiveUrls"];
        if let Some(big) = http_url(&responsive["2"]).or_else(|| http_url(&responsive["1.5"])) {
            full_urls.push(big);
            full_width = width.min(1920);
        } else if let (Some(orig), true) = (
            http_url(&ii["url"]),
            ii["size"].as_u64().unwrap_or(u64::MAX) <= 8 * 1024 * 1024 && width <= 2600,
        ) {
            full_urls.push(orig);
            full_width = width;
        }
        let file_title = p["title"].as_str().unwrap_or("");
        let object_name = meta("ObjectName");
        let title = if object_name.trim().is_empty() {
            clean_title(file_title)
        } else {
            clean_title(&object_name)
        };
        let landing_url = http_url(&ii["descriptionurl"]).unwrap_or_else(|| {
            format!(
                "https://commons.wikimedia.org/wiki/{}",
                file_title.replace(' ', "_")
            )
        });
        out.push(Candidate {
            provider: Provider::Commons,
            source: "wikimedia".into(),
            title,
            creator: clean_creator(&meta("Artist")),
            license_ok: license.is_some(),
            license: license.unwrap_or_else(|| truncate(&squash(&meta("LicenseShortName")), 30)),
            landing_url,
            preview_urls: vec![thumb],
            full_urls,
            width,
            height,
            full_width,
            mature: false,
            kind: ii["mime"].as_str().unwrap_or("").to_ascii_lowercase(),
            tags: Vec::new(),
            quality,
            query,
            rank,
            score: 0.0,
        });
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Filtering, ranking, credit
// ---------------------------------------------------------------------------------------------

/// Smallest original width worth considering, and the landscape aspect range.
pub const MIN_WIDTH: u32 = 1600;
pub const MIN_ASPECT: f64 = 1.3;
pub const MAX_ASPECT: f64 = 2.4;

/// Why a candidate can't be used, or None when it can.
pub fn rejection(c: &Candidate) -> Option<&'static str> {
    if c.mature {
        return Some("mature");
    }
    if !c.license_ok {
        return Some("licence");
    }
    if c.width < MIN_WIDTH || c.height == 0 {
        return Some("too small");
    }
    let aspect = c.width as f64 / c.height as f64;
    if !(MIN_ASPECT..=MAX_ASPECT).contains(&aspect) {
        return Some("not landscape");
    }
    match c.kind.as_str() {
        "" | "photograph" | "jpg" | "jpeg" | "png" | "webp" | "image/jpeg" | "image/png"
        | "image/webp" => {}
        "illustration" | "digitized_artwork" => return Some("not a photo"),
        _ => return Some("file type"),
    }
    if c.preview_urls.is_empty() {
        return Some("no preview");
    }
    None
}

/// The words a theme is about, from its label and photo searches.
pub(crate) fn theme_terms(hint: &ThemeHint) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for w in words(&hint.label)
        .into_iter()
        .chain(hint.image_queries.iter().flat_map(|q| words(q)))
    {
        if w.chars().count() >= 3 && !STOP_WORDS.contains(&w.as_str()) && !terms.contains(&w) {
            terms.push(w);
        }
    }
    terms
}

/// Cheap prior for how good a background a candidate will be, before anyone looks at it.
pub fn heuristic_score(c: &Candidate, terms: &[String], label_terms: &[String]) -> f64 {
    let mut s = match (c.provider, c.source.as_str()) {
        (Provider::Commons, _) if c.quality => 2.5,
        (Provider::Commons, _) => 0.8,
        (_, "stocksnap") => 2.5,
        (_, "rawpixel" | "nappy") => 2.0,
        (_, "wikimedia") => 1.2,
        (_, "flickr") => 0.8,
        (_, src) if ARCHIVE_SOURCES.iter().any(|a| src.contains(a)) => -1.0,
        _ => 0.0,
    };
    let mut hay: Vec<String> = words(&c.title);
    for t in &c.tags {
        hay.extend(words(t));
    }
    if !terms.is_empty() {
        let matched = terms.iter().filter(|t| hay.contains(t)).count();
        s += 3.0 * matched as f64 / terms.len() as f64;
    }
    let title_words = words(&c.title);
    if !label_terms.is_empty() && label_terms.iter().all(|t| title_words.contains(t)) {
        s += 1.0;
    }
    // The source's own relevance order.
    s += (1.0 - c.rank as f64 / 20.0).max(0.0);
    s += match c.full_width {
        0 => 0.0,
        w if w >= 1800 => 1.0,
        w if w >= 1250 => 0.5,
        w if w < 1100 => -0.5,
        _ => 0.0,
    };
    if c.height > 0 {
        let a = c.width as f64 / c.height as f64;
        if (1.45..=1.9).contains(&a) {
            s += 0.4;
        }
    }
    let bad = hay
        .iter()
        .filter(|w| BAD_WORDS.contains(&w.as_str()))
        .count()
        .min(2);
    s -= 2.0 * bad as f64;
    s
}

/// The same photo found twice (Openverse also indexes Commons): keyed by Commons file name, or by
/// landing page.
fn dedupe_key(c: &Candidate) -> String {
    let file = if c.provider == Provider::Commons {
        c.landing_url
            .rsplit_once("File:")
            .map(|(_, f)| f.to_string())
    } else if c.source == "wikimedia" {
        c.full_urls
            .iter()
            .chain(c.preview_urls.iter())
            .find(|u| u.contains("/wikipedia/commons/"))
            .and_then(|u| {
                let clean = u.split(['?', '#']).next().unwrap_or(u);
                let segs: Vec<&str> = clean.split('/').collect();
                // …/thumb/a/ab/Name.jpg/960px-Name.jpg → Name.jpg; …/a/ab/Name.jpg → Name.jpg
                match segs.iter().position(|s| *s == "thumb") {
                    Some(i) => segs.get(i + 3).map(|s| s.to_string()),
                    None => segs.last().map(|s| s.to_string()),
                }
            })
    } else {
        None
    };
    match file {
        Some(f) => format!(
            "commons:{}",
            percent_decode(&f).replace('_', " ").to_lowercase()
        ),
        None => model::links::normalize_url(&c.landing_url)
            .unwrap_or_else(|| c.landing_url.to_lowercase()),
    }
}

/// A title with numbers and punctuation removed, to spot a photographer's series ("Market -- 2018
/// -- 2219", "Market -- 2018 -- 2221").
fn series_key(c: &Candidate) -> String {
    let t: String = c
        .title
        .chars()
        .filter(|ch| ch.is_alphabetic() || ch.is_whitespace())
        .collect();
    format!("{}|{}", squash(&t).to_lowercase(), c.creator.to_lowercase())
}

/// The usable candidates, scored, deduplicated and ordered for judging: best first, alternating
/// between the two sources so the vision model sees variety, at most two per photographer and
/// one per series.
pub fn shortlist(cands: Vec<Candidate>, hint: &ThemeHint, max: usize) -> Vec<Candidate> {
    let terms = theme_terms(hint);
    let label_terms: Vec<String> = words(&hint.label)
        .into_iter()
        .filter(|w| w.chars().count() >= 3)
        .collect();
    let mut scored: Vec<Candidate> = cands
        .into_iter()
        .filter(|c| rejection(c).is_none())
        .map(|mut c| {
            c.score = heuristic_score(&c, &terms, &label_terms);
            c
        })
        .collect();
    scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.rank.cmp(&b.rank)));

    let mut keys: Vec<String> = Vec::new();
    let mut series: Vec<String> = Vec::new();
    let mut per_creator: Vec<(String, usize)> = Vec::new();
    let mut queues: [Vec<Candidate>; 2] = [Vec::new(), Vec::new()];
    for c in scored {
        let key = dedupe_key(&c);
        let ser = series_key(&c);
        if keys.contains(&key) || series.contains(&ser) {
            continue;
        }
        let creator = c.creator.to_lowercase();
        if !creator.is_empty() {
            match per_creator.iter_mut().find(|(n, _)| *n == creator) {
                Some((_, count)) if *count >= 2 => continue,
                Some((_, count)) => *count += 1,
                None => per_creator.push((creator, 1)),
            }
        }
        keys.push(key);
        series.push(ser);
        let q = if c.provider == Provider::Openverse {
            0
        } else {
            1
        };
        queues[q].push(c);
    }
    // Alternate, starting with whichever source has the best candidate.
    let mut out = Vec::new();
    let [mut a, mut b] = queues;
    if b.first().map(|c| c.score) > a.first().map(|c| c.score) {
        std::mem::swap(&mut a, &mut b);
    }
    let (mut ia, mut ib) = (a.into_iter(), b.into_iter());
    while out.len() < max {
        let (x, y) = (ia.next(), ib.next());
        if x.is_none() && y.is_none() {
            break;
        }
        out.extend(x);
        if out.len() < max {
            out.extend(y);
        }
    }
    out
}

/// The attribution shown on the page: "Photo: Jane Doe · CC BY 2.0 · via Openverse",
/// "Photo: Dietmar Rabich · CC BY-SA 4.0 · Wikimedia Commons".
pub fn credit_line(c: &Candidate) -> String {
    let who = if c.creator.is_empty() {
        "Photo".to_string()
    } else {
        format!("Photo: {}", c.creator)
    };
    let place = if c.provider == Provider::Commons || c.source == "wikimedia" {
        "Wikimedia Commons".to_string()
    } else if c.creator.is_empty() && !c.source.is_empty() {
        // No photographer named: at least say where it is from.
        format!("{} via Openverse", source_name(&c.source))
    } else {
        "via Openverse".to_string()
    };
    let mut parts = vec![who];
    if !c.license.is_empty() {
        parts.push(c.license.clone());
    }
    parts.push(place);
    truncate(&parts.join(" · "), 160)
}

/// How a source is written: "rawpixel", "StockSnap", "Flickr"…
fn source_name(source: &str) -> String {
    match source {
        "stocksnap" => "StockSnap".into(),
        "rawpixel" => "rawpixel".into(),
        "flickr" => "Flickr".into(),
        "nappy" => "nappy".into(),
        other => {
            let words: Vec<String> = other
                .split('_')
                .filter(|w| !w.is_empty())
                .map(|w| {
                    let mut c = w.chars();
                    c.next()
                        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                        .unwrap_or_default()
                })
                .collect();
            truncate(&words.join(" "), 40)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OPENVERSE: &str = include_str!("../tests/fixtures/openverse_cooking.json");
    const COMMONS: &str = include_str!("../tests/fixtures/commons_cooking.json");

    fn hint() -> ThemeHint {
        ThemeHint {
            label: "Cooking".into(),
            image_queries: vec!["cooking kitchen".into(), "chef plating food".into()],
            mood: "Warm, inviting kitchen light".into(),
            accent: "#c2410c".into(),
        }
    }

    #[test]
    fn parses_a_real_openverse_reply() {
        let v: Value = serde_json::from_str(OPENVERSE).unwrap();
        let c = parse_openverse(&v, 0);
        assert_eq!(c.len(), v["results"].as_array().unwrap().len());
        let first = &c[0];
        assert_eq!(first.provider, Provider::Openverse);
        assert_eq!(first.source, "stocksnap");
        assert_eq!(first.title, "Cooking Kitchen");
        assert_eq!(first.creator, "JESHOOTS.com");
        assert_eq!(first.license, "CC0");
        assert!(first.license_ok);
        assert_eq!((first.width, first.height), (4800, 3200));
        assert_eq!(
            first.landing_url,
            "https://stocksnap.io/photo/cooking-kitchen-80C03C2071"
        );
        assert_eq!(
            first.preview_urls[0],
            "https://cdn.stocksnap.io/img-thumbs/960w/80C03C2071.jpg"
        );
        assert!(
            first.preview_urls[1].starts_with("https://api.openverse.org/v1/images/"),
            "Openverse thumbnail as the fallback"
        );
        assert_eq!(first.full_width, 960);
        assert_eq!(first.kind, "photograph");
        assert!(first.tags.contains(&"kitchen".to_string()));
        assert!(!first.mature);

        // rawpixel's larger versions are watermarked: the clean 1024 px one is used throughout.
        let rawpixel = c.iter().find(|c| c.source == "rawpixel").unwrap();
        assert_eq!(rawpixel.full_urls.len(), 1, "{:?}", rawpixel.full_urls);
        assert!(rawpixel.full_urls[0].starts_with("https://images.rawpixel.com/editor_1024/"));
        assert!(rawpixel.full_urls.iter().all(|u| !u.contains("image_1300")));
        assert_eq!(rawpixel.full_width, 1024);

        // Openverse links the multi-megabyte Commons original; we use standard-width thumbnails.
        let wiki = c.iter().find(|c| c.source == "wikimedia").unwrap();
        assert!(
            wiki.preview_urls[0].contains("/wikipedia/commons/thumb/")
                && wiki.preview_urls[0].contains("/960px-"),
            "{:?}",
            wiki.preview_urls
        );
        assert!(
            wiki.full_urls[0].contains("/1920px-"),
            "{:?}",
            wiki.full_urls
        );
        assert_eq!(wiki.credit_place(), "Wikimedia Commons");
    }

    impl Candidate {
        fn credit_place(&self) -> &'static str {
            if credit_line(self).ends_with("Wikimedia Commons") {
                "Wikimedia Commons"
            } else {
                "via Openverse"
            }
        }
    }

    #[test]
    fn parses_a_real_commons_reply_in_search_order() {
        let v: Value = serde_json::from_str(COMMONS).unwrap();
        let c = parse_commons(&v, true, 1);
        assert_eq!(c.len(), 6);
        assert!(
            c.iter()
                .enumerate()
                .all(|(i, c)| c.rank == i && c.query == 1 && c.quality)
        );
        let first = &c[0];
        assert_eq!(
            first.title,
            "Grandmother preparing food inside traditional kitchen (edited)"
        );
        assert_eq!(
            first.creator,
            "Original: Mithun Kunwar; Derivative work: Radomianin"
        );
        assert_eq!(first.license, "CC BY-SA 4.0");
        assert!(first.license_ok);
        assert_eq!((first.width, first.height), (5827, 4160));
        assert_eq!(first.kind, "image/jpeg");
        assert!(first.preview_urls[0].contains("/960px-"));
        assert!(
            first.full_urls[0].contains("/1920px-"),
            "the 2x responsive URL is the large version"
        );
        assert_eq!(first.full_width, 1920);
        assert!(
            first
                .landing_url
                .starts_with("https://commons.wikimedia.org/wiki/File:")
        );
        let rabich = c
            .iter()
            .find(|c| c.title.contains("Küchenutensilien"))
            .unwrap();
        assert_eq!(rabich.creator, "Dietmar Rabich");
        assert_eq!(
            credit_line(rabich),
            "Photo: Dietmar Rabich · CC BY-SA 4.0 · Wikimedia Commons"
        );
    }

    #[test]
    fn commons_formatversion_1_and_empty_replies() {
        let v1 = json!({ "query": { "pages": {
            "12": { "title": "File:B.jpg", "index": 2, "imageinfo": [{ "thumburl": "https://x.org/b960.jpg", "width": 3000, "height": 2000, "mime": "image/jpeg",
                "extmetadata": { "LicenseShortName": { "value": "CC0" } } }] },
            "11": { "title": "File:A_photo.png", "index": 1, "imageinfo": [{ "thumburl": "https://x.org/a960.png", "width": 2400, "height": 1600, "mime": "image/png",
                "url": "https://x.org/a.png", "size": 1000,
                "extmetadata": { "LicenseShortName": { "value": "Public domain" }, "License": { "value": "pd" } } }] }
        }}});
        let c = parse_commons(&v1, false, 0);
        assert_eq!(
            c.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(),
            vec!["A photo", "B"]
        );
        assert_eq!(c[0].license, "Public domain");
        assert_eq!(
            c[0].full_urls,
            vec!["https://x.org/a.png"],
            "a small original is the large version"
        );
        assert!(c[1].full_urls.is_empty());
        assert!(
            parse_commons(&json!({ "batchcomplete": true }), true, 0).is_empty(),
            "no results: no `query` key"
        );
        assert!(parse_openverse(&json!({ "result_count": 0, "results": [] }), 0).is_empty());
        assert!(parse_openverse(&json!("garbage"), 0).is_empty());
    }

    #[test]
    fn licences() {
        assert_eq!(openverse_license("cc0", "1.0").as_deref(), Some("CC0"));
        assert_eq!(
            openverse_license("pdm", "1.0").as_deref(),
            Some("Public domain")
        );
        assert_eq!(openverse_license("by", "2.0").as_deref(), Some("CC BY 2.0"));
        assert_eq!(
            openverse_license("BY-SA", "4.0").as_deref(),
            Some("CC BY-SA 4.0")
        );
        for bad in [
            "by-nc",
            "by-nd",
            "by-nc-sa",
            "by-nc-nd",
            "sampling+",
            "nc-sampling+",
            "",
        ] {
            assert_eq!(openverse_license(bad, "2.0"), None, "{bad}");
        }
        assert_eq!(
            commons_license("CC BY-SA 4.0", "cc-by-sa-4.0").as_deref(),
            Some("CC BY-SA 4.0")
        );
        assert_eq!(
            commons_license("CC BY 2.0", "cc-by-2.0").as_deref(),
            Some("CC BY 2.0")
        );
        assert_eq!(commons_license("CC0", "cc0").as_deref(), Some("CC0"));
        assert_eq!(
            commons_license("Public domain", "pd").as_deref(),
            Some("Public domain")
        );
        assert_eq!(
            commons_license("CC BY-SA 3.0 de", "cc-by-sa-3.0-de").as_deref(),
            Some("CC BY-SA 3.0 de")
        );
        assert_eq!(
            commons_license("", "cc-by-sa-2.5").as_deref(),
            Some("CC BY-SA 2.5")
        );
        assert_eq!(
            commons_license("CC BY-SA 4.0", "").as_deref(),
            Some("CC BY-SA 4.0")
        );
        assert_eq!(commons_license("GFDL", "gfdl"), None);
        assert_eq!(commons_license("CC BY-NC 2.0", "cc-by-nc-2.0"), None);
        assert_eq!(commons_license("CC BY-ND 4.0", ""), None);
        assert_eq!(commons_license("Fair use", "fair-use"), None);
        assert_eq!(commons_license("", ""), None);
    }

    #[test]
    fn html_and_text_cleanup() {
        assert_eq!(
            strip_html("<bdi><a href=\"x\"><span title=\"t\">Dietmar Rabich</span></a></bdi>"),
            "Dietmar Rabich"
        );
        assert_eq!(
            strip_html("Jane &amp; John &lt;3 &#233;t&#xE9; &nbsp;x"),
            "Jane & John <3 été x"
        );
        assert_eq!(strip_html("A<br>B<br/>C"), "A; B; C");
        assert_eq!(strip_html("<a href=\"x\">Name</a>, 2020"), "Name, 2020");
        assert_eq!(strip_html("1 < 2 and AT&T"), "1 < 2 and AT&T");
        assert_eq!(
            clean_title("File:Saxophone_quartet.jpg"),
            "Saxophone quartet"
        );
        assert_eq!(percent_decode("M%C3%BCnster%2C_x%zz"), "Münster,_x%zz");
        assert_eq!(
            words("Herbs, chopped; glass"),
            vec!["herb", "chopped", "glass"]
        );
    }

    #[test]
    fn source_url_knowledge() {
        assert_eq!(
            wikimedia_thumb(
                "https://upload.wikimedia.org/wikipedia/commons/e/ec/Saxophone_quartet.jpg",
                960
            )
            .as_deref(),
            Some(
                "https://thumb.wikimedia.org/wikipedia/commons/thumb/e/ec/Saxophone_quartet.jpg/960px-Saxophone_quartet.jpg"
            )
        );
        assert_eq!(
            wikimedia_thumb(
                "https://upload.wikimedia.org/wikipedia/commons/e/ec/Scan.tif",
                960
            ),
            None
        );
        assert_eq!(wikimedia_thumb("https://example.org/photo.jpg", 960), None);
        assert_eq!(
            wikimedia_thumb(
                "https://upload.wikimedia.org/wikipedia/commons/e/zz/A.jpg",
                960
            ),
            None
        );
        assert_eq!(
            flickr_width("https://live.staticflickr.com/65535/123_abc_b.jpg"),
            Some(1024)
        );
        assert_eq!(
            flickr_width("https://live.staticflickr.com/65535/123_abc.jpg"),
            None
        );
    }

    fn cand(
        provider: Provider,
        source: &str,
        title: &str,
        creator: &str,
        w: u32,
        h: u32,
    ) -> Candidate {
        Candidate {
            provider,
            source: source.into(),
            title: title.into(),
            creator: creator.into(),
            license: "CC0".into(),
            license_ok: true,
            landing_url: format!("https://example.org/{}", title.replace(' ', "-")),
            preview_urls: vec![format!(
                "https://example.org/{}.jpg",
                title.replace(' ', "-")
            )],
            full_urls: vec![],
            width: w,
            height: h,
            full_width: 0,
            mature: false,
            kind: String::new(),
            tags: vec![],
            quality: false,
            query: 0,
            rank: 0,
            score: 0.0,
        }
    }

    #[test]
    fn filters() {
        let ok = cand(Provider::Openverse, "stocksnap", "Kitchen", "A", 4000, 2667);
        assert_eq!(rejection(&ok), None);
        assert_eq!(
            rejection(&Candidate {
                mature: true,
                ..ok.clone()
            }),
            Some("mature")
        );
        assert_eq!(
            rejection(&Candidate {
                license_ok: false,
                ..ok.clone()
            }),
            Some("licence")
        );
        assert_eq!(
            rejection(&Candidate {
                width: 1200,
                height: 800,
                ..ok.clone()
            }),
            Some("too small")
        );
        assert_eq!(
            rejection(&Candidate {
                width: 3000,
                height: 3000,
                ..ok.clone()
            }),
            Some("not landscape"),
            "square"
        );
        assert_eq!(
            rejection(&Candidate {
                width: 2000,
                height: 3000,
                ..ok.clone()
            }),
            Some("not landscape"),
            "portrait"
        );
        assert_eq!(
            rejection(&Candidate {
                width: 6000,
                height: 2000,
                ..ok.clone()
            }),
            Some("not landscape"),
            "panorama"
        );
        assert_eq!(
            rejection(&Candidate {
                kind: "illustration".into(),
                ..ok.clone()
            }),
            Some("not a photo")
        );
        assert_eq!(
            rejection(&Candidate {
                kind: "image/tiff".into(),
                ..ok.clone()
            }),
            Some("file type")
        );
        assert_eq!(
            rejection(&Candidate {
                kind: "gif".into(),
                ..ok.clone()
            }),
            Some("file type")
        );
        assert_eq!(
            rejection(&Candidate {
                preview_urls: vec![],
                ..ok.clone()
            }),
            Some("no preview")
        );
        assert_eq!(
            rejection(&Candidate {
                kind: "image/jpeg".into(),
                ..ok.clone()
            }),
            None
        );
    }

    #[test]
    fn real_fixtures_filter_as_expected() {
        let ov: Value = serde_json::from_str(OPENVERSE).unwrap();
        let cm: Value = serde_json::from_str(COMMONS).unwrap();
        let all: Vec<Candidate> = parse_openverse(&ov, 0)
            .into_iter()
            .chain(parse_commons(&cm, true, 0))
            .collect();
        assert!(
            all.iter().all(|c| rejection(c).is_none()),
            "{:?}",
            all.iter()
                .map(|c| (c.title.clone(), rejection(c)))
                .collect::<Vec<_>>()
        );
        let list = shortlist(all, &hint(), 16);
        // The flooded kitchen and the mine shaft sink to the bottom; the series of Münster photos
        // keeps one.
        let titles: Vec<&str> = list.iter().map(|c| c.title.as_str()).collect();
        let pos = |needle: &str| titles.iter().position(|t| t.contains(needle));
        assert!(pos("submerged").unwrap() > list.len() / 2, "{titles:?}");
        assert!(
            pos("Engine Shaft").is_none_or(|p| p > list.len() / 2),
            "{titles:?}"
        );
        assert_eq!(
            titles.iter().filter(|t| t.contains("Mühlenhaus")).count(),
            1,
            "{titles:?}"
        );
        // A stock photo with "cooking" in its title leads.
        assert!(
            matches!(list[0].source.as_str(), "stocksnap" | "rawpixel")
                && list[0].title.to_lowercase().contains("cooking"),
            "{titles:?}"
        );
        assert!(pos("Cooking Kitchen").unwrap() < 4, "{titles:?}");
        // Both sources are represented near the top.
        assert!(list[..4].iter().any(|c| c.provider == Provider::Commons));
        assert!(list[..4].iter().any(|c| c.provider == Provider::Openverse));
    }

    #[test]
    fn ranking_prefers_relevant_stock_and_quality() {
        let h = hint();
        let terms = theme_terms(&h);
        let labels = words(&h.label);
        let stock = cand(
            Provider::Openverse,
            "stocksnap",
            "Cooking in a kitchen",
            "A",
            4000,
            2667,
        );
        let archive = Candidate {
            source: "smithsonian_museum".into(),
            ..cand(
                Provider::Openverse,
                "x",
                "Cooking in a kitchen",
                "B",
                4000,
                2667,
            )
        };
        let off_topic = cand(
            Provider::Openverse,
            "stocksnap",
            "Mountain lake",
            "C",
            4000,
            2667,
        );
        let flood = cand(
            Provider::Openverse,
            "stocksnap",
            "Flooded kitchen after the disaster",
            "D",
            4000,
            2667,
        );
        let quality = Candidate {
            quality: true,
            ..cand(
                Provider::Commons,
                "wikimedia",
                "Chef plating food",
                "E",
                4000,
                2667,
            )
        };
        let s = |c: &Candidate| heuristic_score(c, &terms, &labels);
        assert!(s(&stock) > s(&archive));
        assert!(s(&stock) > s(&off_topic));
        assert!(s(&stock) > s(&flood));
        assert!(s(&quality) > s(&off_topic));
        let big = Candidate {
            full_width: 1920,
            ..stock.clone()
        };
        let small = Candidate {
            full_width: 960,
            ..stock.clone()
        };
        assert!(s(&big) > s(&small));
    }

    #[test]
    fn shortlist_dedupes_caps_creators_and_alternates() {
        let mut v = Vec::new();
        for i in 0..5 {
            v.push(cand(
                Provider::Openverse,
                "stocksnap",
                &format!("Kitchen scene {}", ["a", "b", "c", "d", "e"][i]),
                "Same Person",
                4000,
                2667,
            ));
        }
        v.push(cand(
            Provider::Commons,
            "wikimedia",
            "Cooking pots",
            "Other",
            4000,
            2667,
        ));
        v.push(cand(
            Provider::Commons,
            "wikimedia",
            "Cooking pots",
            "Other",
            4000,
            2667,
        )); // duplicate
        v.push(Candidate {
            license_ok: false,
            ..cand(
                Provider::Commons,
                "wikimedia",
                "Kitchen NC",
                "Z",
                4000,
                2667,
            )
        });
        let list = shortlist(v, &hint(), 16);
        assert_eq!(
            list.iter().filter(|c| c.creator == "Same Person").count(),
            2,
            "at most two per photographer"
        );
        assert_eq!(list.iter().filter(|c| c.title == "Cooking pots").count(), 1);
        assert!(list.iter().all(|c| c.license_ok));
        assert_eq!(list.len(), 3);
        assert_ne!(list[0].provider, list[1].provider, "alternates sources");
        // A Commons original also indexed by Openverse is the same photo.
        let a = Candidate {
            preview_urls: vec!["https://upload.wikimedia.org/wikipedia/commons/thumb/e/ec/Saxophone_quartet.jpg/960px-Saxophone_quartet.jpg".into()],
            ..cand(Provider::Openverse, "wikimedia", "Saxophone quartet", "L", 2576, 1932)
        };
        let b = Candidate {
            landing_url: "https://commons.wikimedia.org/wiki/File:Saxophone_quartet.jpg".into(),
            ..cand(
                Provider::Commons,
                "wikimedia",
                "Saxophone quartet (2)",
                "M",
                2576,
                1932,
            )
        };
        assert_eq!(dedupe_key(&a), dedupe_key(&b));
    }

    #[test]
    fn credit_lines() {
        let mut c = cand(
            Provider::Openverse,
            "flickr",
            "Jazz",
            "Jane Doe",
            4000,
            2667,
        );
        c.license = "CC BY 2.0".into();
        assert_eq!(
            credit_line(&c),
            "Photo: Jane Doe · CC BY 2.0 · via Openverse"
        );
        c.creator.clear();
        assert_eq!(credit_line(&c), "Photo · CC BY 2.0 · Flickr via Openverse");
        c.source = "rawpixel".into();
        c.license = "CC0".into();
        assert_eq!(credit_line(&c), "Photo · CC0 · rawpixel via Openverse");
        c.source = "smithsonian_national_museum".into();
        assert_eq!(
            credit_line(&c),
            "Photo · CC0 · Smithsonian National Museum via Openverse"
        );
        let mut w = cand(
            Provider::Commons,
            "wikimedia",
            "Jazz",
            "Tore Sætre",
            4000,
            2667,
        );
        w.license = "CC BY-SA 4.0".into();
        assert_eq!(
            credit_line(&w),
            "Photo: Tore Sætre · CC BY-SA 4.0 · Wikimedia Commons"
        );
        let ov_wiki = Candidate {
            provider: Provider::Openverse,
            ..w.clone()
        };
        assert_eq!(
            credit_line(&ov_wiki),
            "Photo: Tore Sætre · CC BY-SA 4.0 · Wikimedia Commons"
        );
    }
}
