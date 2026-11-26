//! Step 3 of discovery, without the model: raw search results become the candidates worth the
//! model's attention. Duplicates merge by normalised URL, dismissed pages and obvious noise
//! (social posts, Q&A, shopping, dictionaries) go, and the rest are ranked by search relevance,
//! authority, recency and how many searches found them, with every query guaranteed a few
//! places so no wanted kind of opportunity is crowded out.

use std::collections::{HashMap, HashSet};

use deepseek::SearchResult;
use model::KnownOpportunity;
use model::links::{host, normalize_url};
use model::text::{squash, truncate};

use crate::plan::Query;

/// One search result and the query that found it.
#[derive(Clone, Debug)]
pub(crate) struct Hit {
    pub query: usize,
    pub result: SearchResult,
}

/// A unique search result, ready for review.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Candidate {
    pub key: String,
    pub url: String,
    pub title: String,
    pub site_name: String,
    pub site_icon: String,
    pub host: String,
    /// Publication date (YYYY-MM-DD), if the search engine knew it.
    pub date: Option<String>,
    pub summary: String,
    pub rerank: f64,
    pub authority: i32,
    /// How many searches returned this page.
    pub hits: u32,
    /// The query that found it (the best-ranked hit's).
    pub query: usize,
    /// That query's category.
    pub category: String,
    /// Its index in the known list when the person already has it.
    pub known: Option<usize>,
    pub score: f64,
}

/// What the shortlist produced.
#[derive(Debug, Default)]
pub(crate) struct Shortlist {
    pub candidates: Vec<Candidate>,
    /// Unique, usable results before the cap.
    pub unique: usize,
}

/// The candidates, best first, at most `max`. `known` is the person's active and saved items in
/// the order the review prompt numbers them; `dismissed` holds keys that must never come back.
pub(crate) fn shortlist(
    hits: Vec<Hit>,
    queries: &[Query],
    known: &[KnownOpportunity],
    dismissed: &HashSet<String>,
    today: &str,
    max: usize,
) -> Shortlist {
    let today_days = model::date::parse_ymd(today);
    let known_index: HashMap<&str, usize> = known
        .iter()
        .enumerate()
        .map(|(i, k)| (k.key.as_str(), i))
        .collect();
    let mut cands: Vec<Candidate> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for hit in hits {
        let r = hit.result;
        let url = clean_url(&r.url);
        let Some(key) = normalize_url(&url) else {
            continue;
        };
        let Some(h) = host(&url) else { continue };
        let title = squash(&r.title);
        if title.is_empty() || dismissed.contains(&key) || is_noise(&h, &url) {
            continue;
        }
        let category = queries
            .get(hit.query)
            .map(|q| q.category.clone())
            .unwrap_or_else(|| "other".into());
        let rerank = if r.rerank_score.is_finite() {
            r.rerank_score.clamp(0.0, 3.0)
        } else {
            0.0
        };
        if let Some(&i) = by_key.get(&key) {
            let c = &mut cands[i];
            c.hits += 1;
            c.authority = c.authority.max(r.authority);
            if rerank > c.rerank {
                c.rerank = rerank;
                c.query = hit.query;
                c.category = category;
                if r.summary.len() > c.summary.len() {
                    c.summary = r.summary.clone();
                }
            }
            continue;
        }
        let site_name = r
            .site_name
            .as_deref()
            .map(squash)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| h.clone());
        let summary = if r.summary.trim().is_empty() {
            r.snippet.clone()
        } else {
            r.summary.clone()
        };
        by_key.insert(key.clone(), cands.len());
        cands.push(Candidate {
            known: known_index.get(key.as_str()).copied(),
            key,
            url,
            title: truncate(&title, 200),
            site_name: truncate(&site_name, 80),
            site_icon: r.site_icon.clone().unwrap_or_default(),
            host: h,
            date: r.date.as_deref().and_then(model::date::ymd_prefix),
            summary,
            rerank,
            authority: r.authority,
            hits: 1,
            query: hit.query,
            category,
            score: 0.0,
        });
    }
    for c in &mut cands {
        c.score = score(c, today_days);
    }
    let unique = cands.len();
    Shortlist {
        candidates: select(cands, queries.len(), max),
        unique,
    }
}

/// Every query keeps its best few (so each wanted kind is represented), then the rest fill up by
/// score. Returned best first.
fn select(cands: Vec<Candidate>, n_queries: usize, max: usize) -> Vec<Candidate> {
    if cands.len() <= max {
        return sorted(cands);
    }
    let guarantee = (max / (2 * n_queries.max(1))).clamp(1, 3);
    let mut order: Vec<usize> = (0..cands.len()).collect();
    order.sort_by(|&a, &b| cands[b].score.total_cmp(&cands[a].score));
    let mut per_query: HashMap<usize, usize> = HashMap::new();
    let mut chosen: Vec<bool> = vec![false; cands.len()];
    let mut count = 0;
    for &i in &order {
        let n = per_query.entry(cands[i].query).or_insert(0);
        if *n < guarantee && count < max {
            *n += 1;
            chosen[i] = true;
            count += 1;
        }
    }
    for &i in &order {
        if count >= max {
            break;
        }
        if !chosen[i] {
            chosen[i] = true;
            count += 1;
        }
    }
    sorted(
        cands
            .into_iter()
            .zip(chosen)
            .filter(|(_, keep)| *keep)
            .map(|(c, _)| c)
            .collect(),
    )
}

fn sorted(mut cands: Vec<Candidate>) -> Vec<Candidate> {
    cands.sort_by(|a, b| b.score.total_cmp(&a.score));
    cands
}

/// Relevance first, then authority, freshness, agreement between searches; generic advice
/// pages sink.
fn score(c: &Candidate, today_days: Option<i64>) -> f64 {
    let mut s = c.rerank;
    s += f64::from(c.authority.clamp(0, 4)) * 0.15;
    s += recency(c.date.as_deref(), today_days);
    s += f64::from(c.hits.saturating_sub(1).min(3)) * 0.25;
    if is_generic_title(&c.title) {
        s -= 0.8;
    }
    s
}

fn recency(date: Option<&str>, today_days: Option<i64>) -> f64 {
    let (Some(d), Some(t)) = (date.and_then(model::date::parse_ymd), today_days) else {
        return 0.0;
    };
    match t - d {
        age if age < 0 => 0.1,
        0..=30 => 0.5,
        31..=90 => 0.35,
        91..=180 => 0.15,
        181..=365 => 0.0,
        366..=730 => -0.4,
        _ => -0.8,
    }
}

/// Advice, definitions and paginated listings rarely are an opportunity themselves.
pub(crate) fn is_generic_title(title: &str) -> bool {
    let t = title.to_lowercase();
    const STARTS: &[&str] = &[
        "how to ",
        "how do ",
        "how can ",
        "what is ",
        "what are ",
        "why ",
        "should you ",
        "can you ",
    ];
    const CONTAINS: &[&str] = &[
        " tips",
        "tips for",
        "ways to ",
        "guide to ",
        "things to know",
        "everything you need to know",
        "beginner's guide",
        "pros and cons",
        " meaning",
        "definition of",
        "step-by-step guide",
    ];
    if STARTS.iter().any(|s| t.starts_with(s)) || CONTAINS.iter().any(|s| t.contains(s)) {
        return true;
    }
    // "… - Page 56 of 2767"
    t.match_indices("page ").any(|(i, _)| {
        let rest = &t[i + 5..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        !digits.is_empty() && rest[digits.len()..].starts_with(" of ")
    })
}

/// The URL as a person should get it: HTML entities decoded and junk fragments ("#content",
/// "#1") removed. Hash routes ("#/jobs/12") are kept.
pub(crate) fn clean_url(raw: &str) -> String {
    let url = raw.trim().replace("&amp;", "&");
    match url.split_once('#') {
        Some((base, frag)) if !frag.starts_with('/') && !frag.starts_with('!') => base.to_string(),
        _ => url,
    }
}

/// Sites whose pages are posts, answers, products or references, never an opportunity page.
const NOISE_HOSTS: &[&str] = &[
    "facebook.com",
    "fb.com",
    "fb.watch",
    "m.facebook.com",
    "instagram.com",
    "tiktok.com",
    "twitter.com",
    "x.com",
    "threads.net",
    "reddit.com",
    "quora.com",
    "youtube.com",
    "youtu.be",
    "vimeo.com",
    "tumblr.com",
    "snapchat.com",
    "t.me",
    "telegram.me",
    "whatsapp.com",
    "discord.com",
    "discord.gg",
    "medium.com",
    "scribd.com",
    "slideshare.net",
    "coursehero.com",
    "chegg.com",
    "brainly.com",
    "answers.com",
    "stackoverflow.com",
    "stackexchange.com",
    "superuser.com",
    "wikipedia.org",
    "wiktionary.org",
    "wikihow.com",
    "merriam-webster.com",
    "dictionary.com",
    "thesaurus.com",
    "collinsdictionary.com",
    "imdb.com",
    "yelp.com",
    "tripadvisor.com",
    "trustpilot.com",
    "soundcloud.com",
    "spotify.com",
    "apps.apple.com",
    "play.google.com",
    "news.google.com",
    "aliexpress.com",
    "temu.com",
    "shein.com",
    "walmart.com",
    "etsy.com",
    "pinterest.com",
];

/// Brands with a site per country (amazon.de, ebay.co.uk…).
const NOISE_FAMILIES: &[&str] = &["amazon", "ebay", "pinterest", "facebook", "wikipedia"];

pub(crate) fn is_noise(host: &str, url: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if NOISE_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
    {
        return true;
    }
    if let Some((first, rest)) = host.split_once('.')
        && NOISE_FAMILIES.contains(&first)
        && rest.len() <= 6
    {
        return true;
    }
    // Link aggregators of the fediverse: posts about pages, not the pages.
    if host
        .split('.')
        .any(|label| label == "lemmy" || label == "kbin")
    {
        return true;
    }
    if host == "linkedin.com" || host.ends_with(".linkedin.com") {
        let path = url
            .split("linkedin.com")
            .nth(1)
            .unwrap_or("")
            .to_ascii_lowercase();
        return [
            "/posts/",
            "/pulse/",
            "/feed/",
            "/in/",
            "/school/",
            "/learning/",
            "/company/",
            "/groups/",
        ]
        .iter()
        .any(|p| path.starts_with(p));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(url: &str, title: &str, rerank: f64, authority: i32, date: Option<&str>) -> SearchResult {
        SearchResult {
            url: url.into(),
            title: title.into(),
            rerank_score: rerank,
            authority,
            date: date.map(str::to_string),
            summary: format!("Summary of {title}"),
            site_name: Some("Site".into()),
            ..Default::default()
        }
    }

    fn q(cat: &str) -> Query {
        Query {
            q: format!("{cat} query"),
            category: cat.into(),
            about: cat.into(),
        }
    }

    #[test]
    fn dedupes_drops_noise_and_dismissed() {
        let hits = vec![
            Hit {
                query: 0,
                result: r(
                    "https://www.grants.org/a/?utm_source=x#content",
                    "Grant A",
                    1.0,
                    2,
                    Some("2026-11-01"),
                ),
            },
            Hit {
                query: 1,
                result: r("http://grants.org/a", "Grant A again", 2.0, 1, None),
            },
            Hit {
                query: 0,
                result: r("https://www.facebook.com/events/1", "Party", 2.9, 3, None),
            },
            Hit {
                query: 0,
                result: r(
                    "https://www.linkedin.com/posts/someone_activity-1",
                    "Post",
                    2.9,
                    3,
                    None,
                ),
            },
            Hit {
                query: 0,
                result: r(
                    "https://www.linkedin.com/jobs/view/123",
                    "Chef job",
                    1.5,
                    3,
                    None,
                ),
            },
            Hit {
                query: 0,
                result: r("https://amazon.co.za/knives", "Knives", 2.9, 3, None),
            },
            Hit {
                query: 0,
                result: r("https://lemmy.world/post/1", "CFP", 2.9, 3, None),
            },
            Hit {
                query: 0,
                result: r("https://dismissed.org/x", "Old", 2.9, 3, None),
            },
            Hit {
                query: 0,
                result: r("not a url", "Bad", 2.9, 3, None),
            },
            Hit {
                query: 0,
                result: r("https://blank.org/x", "  ", 2.9, 3, None),
            },
            Hit {
                query: 1,
                result: r("https://known.org/k", "Known one", 1.0, 1, None),
            },
        ];
        let dismissed: HashSet<String> = ["dismissed.org/x".to_string()].into();
        let known = vec![KnownOpportunity {
            key: "known.org/k".into(),
            status: "active".into(),
            ..Default::default()
        }];
        let s = shortlist(
            hits,
            &[q("funding"), q("jobs")],
            &known,
            &dismissed,
            "2026-11-26",
            10,
        );
        let urls: Vec<&str> = s.candidates.iter().map(|c| c.url.as_str()).collect();
        assert_eq!(s.unique, 3, "{urls:?}");
        let a = s
            .candidates
            .iter()
            .find(|c| c.key == "grants.org/a")
            .unwrap();
        assert_eq!(a.hits, 2);
        assert_eq!(a.rerank, 2.0, "the better hit wins");
        assert_eq!(a.category, "jobs", "category follows the better hit");
        assert_eq!(a.authority, 2, "authority keeps the best");
        assert_eq!(a.date.as_deref(), Some("2026-11-01"));
        assert!(urls.contains(&"https://www.linkedin.com/jobs/view/123"));
        assert_eq!(
            s.candidates
                .iter()
                .find(|c| c.key == "known.org/k")
                .unwrap()
                .known,
            Some(0)
        );
        assert!(!urls.iter().any(|u| u.contains("facebook")
            || u.contains("amazon")
            || u.contains("lemmy")
            || u.contains("dismissed")));
    }

    #[test]
    fn ranking_prefers_relevance_authority_recency() {
        let hits = vec![
            Hit {
                query: 0,
                result: r("https://a.org/1", "Old", 1.5, 1, Some("2023-01-01")),
            },
            Hit {
                query: 0,
                result: r("https://a.org/2", "Fresh", 1.5, 1, Some("2026-11-20")),
            },
            Hit {
                query: 0,
                result: r("https://a.org/3", "Authoritative", 1.5, 4, None),
            },
            Hit {
                query: 0,
                result: r(
                    "https://a.org/4",
                    "How to win a grant: 10 tips",
                    1.9,
                    1,
                    Some("2026-11-20"),
                ),
            },
            Hit {
                query: 0,
                result: r("https://a.org/5", "Very relevant", 2.6, 1, None),
            },
        ];
        let s = shortlist(
            hits,
            &[q("funding")],
            &[],
            &HashSet::new(),
            "2026-11-26",
            10,
        );
        let titles: Vec<&str> = s.candidates.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Very relevant",
                "Fresh",
                "Authoritative",
                "How to win a grant: 10 tips",
                "Old"
            ]
        );
    }

    #[test]
    fn every_query_keeps_a_place_under_the_cap() {
        let mut hits = Vec::new();
        for i in 0..30 {
            hits.push(Hit {
                query: 0,
                result: r(
                    &format!("https://strong.org/{i}"),
                    &format!("Strong {i}"),
                    2.8,
                    3,
                    None,
                ),
            });
        }
        for i in 0..5 {
            hits.push(Hit {
                query: 1,
                result: r(
                    &format!("https://weak.org/{i}"),
                    &format!("Weak {i}"),
                    0.5,
                    0,
                    None,
                ),
            });
        }
        let s = shortlist(
            hits,
            &[q("jobs"), q("events")],
            &[],
            &HashSet::new(),
            "2026-11-26",
            12,
        );
        assert_eq!(s.candidates.len(), 12);
        assert_eq!(s.unique, 35);
        let weak = s.candidates.iter().filter(|c| c.query == 1).count();
        assert_eq!(weak, 3, "the weak query keeps its guaranteed places");
        assert!(
            s.candidates.windows(2).all(|w| w[0].score >= w[1].score),
            "best first"
        );
    }

    #[test]
    fn urls_and_titles() {
        assert_eq!(
            clean_url("https://x.org/a?b=1&amp;c=2#content"),
            "https://x.org/a?b=1&c=2"
        );
        assert_eq!(
            clean_url("https://x.org/app#/jobs/12"),
            "https://x.org/app#/jobs/12"
        );
        assert!(is_generic_title("How to apply for a bursary"));
        assert!(is_generic_title(
            "StudentRoom.co.za - Page 56 of 2767 - Internships"
        ));
        assert!(is_generic_title("7 tips for chefs"));
        assert!(!is_generic_title("Chef bursary 2027: applications open"));
        assert!(!is_generic_title("Page 3 Restaurant hiring sous chef"));
        assert!(is_noise("x.com", "https://x.com/a"));
        assert!(!is_noise("box.com", "https://box.com/a"));
        assert!(is_noise(
            "de.wikipedia.org",
            "https://de.wikipedia.org/wiki/A"
        ));
        assert!(is_noise("ebay.co.uk", "https://ebay.co.uk/itm/1"));
        assert!(!is_noise("ebaycareers.com", "https://ebaycareers.com/job"));
        assert!(!is_noise(
            "linkedin.com",
            "https://www.linkedin.com/jobs/view/1"
        ));
    }
}
