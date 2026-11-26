//! Business research: from a business name and website to its profile and a sourced
//! intelligence dossier ([`model::Intel`]).
//!
//! 1. Read the website (Fetch API): the home page, then up to five key pages found in its links
//!    (about, products/services, customers, pricing, news/careers). In parallel, search for recent
//!    news about the business and for its competitors.
//! 2. `business_profile` (thinking off): draft the profile from the site and plan follow-up
//!    searches for comparable companies, market trends and regulation in its industry and country.
//! 3. Run those searches.
//! 4. `business_intel` (thinking low): build the dossier strictly from the numbered sources.
//!    Every item must cite a source number; items citing anything else are dropped, competitor
//!    names and named customers must appear in the cited text, and `Intel::sanitized` drops
//!    anything left unsourced. Unknown stays empty.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use deepseek::{ChatRequest, Page, SearchRequest, SearchResult, Thinking};
use model::links::{host, is_http_url, normalize_url};
use model::text::{squash, truncate};
use model::{
    Competitor, Intel, NewsItem, Offering, Overview, Profile, Regulation, Research, Segment,
    Source, Trend,
};
use serde_json::Value;

use crate::shortlist::clean_url;
use crate::util::{
    Reporter, array, clip, excerpt, list, number, parallel_map, parse_number, public_ip, text,
    with_heartbeat,
};
use crate::{BusinessInput, Ctx, Error, plan, prompt};

/// Key pages read besides the home page.
const MAX_PAGES: usize = 5;
/// Characters of each website page given to the models.
const PAGE_CHARS: usize = 3000;
/// Characters of each search result given to the dossier call.
const RESULT_CHARS: usize = 450;
/// Search results given to the dossier call at most.
const MAX_RESULTS: usize = 44;
/// Results asked of each search.
const RESULTS_PER_SEARCH: u32 = 8;

/// The website as an absolute https URL, or None when it can't be one.
pub(crate) fn normalize_site(raw: &str) -> Option<String> {
    let t = raw.trim().trim_end_matches('/');
    if t.is_empty() {
        return None;
    }
    let url = if t.to_ascii_lowercase().starts_with("http://")
        || t.to_ascii_lowercase().starts_with("https://")
    {
        t.to_string()
    } else {
        format!("https://{t}")
    };
    is_http_url(&url).then_some(url)
}

/// Same site: equal hosts, or one a subdomain of the other (www, shop…).
fn same_site(a: &str, b: &str) -> bool {
    a == b || a.ends_with(&format!(".{b}")) || b.ends_with(&format!(".{a}"))
}

/// Links in a Markdown page, absolute, with their anchor text.
pub(crate) fn page_links(content: &str, base: &str) -> Vec<(String, String)> {
    let origin = base.split('/').take(3).collect::<Vec<_>>().join("/");
    let mut out: Vec<(String, String)> = Vec::new();
    let mut rest = content;
    while let Some(i) = rest.find("](") {
        let before = &rest[..i];
        let anchor = before
            .rfind('[')
            .map(|j| squash(&before[j + 1..]))
            .unwrap_or_default();
        let after = &rest[i + 2..];
        let end = after.find([')', ' ', '"', '\n']).unwrap_or(after.len());
        let raw = after[..end].trim();
        rest = &after[end.min(after.len())..];
        if raw.is_empty()
            || raw.starts_with('#')
            || raw.starts_with("mailto:")
            || raw.starts_with("tel:")
            || raw.starts_with("javascript:")
        {
            continue;
        }
        let url = if raw.starts_with("http://") || raw.starts_with("https://") {
            raw.to_string()
        } else if raw.starts_with("//") {
            format!("https:{raw}")
        } else if raw.starts_with('/') {
            format!("{origin}{raw}")
        } else {
            continue;
        };
        let url = clean_url(&url);
        if is_http_url(&url) && !out.iter().any(|(u, _)| u == &url) {
            out.push((url, truncate(&anchor, 80)));
        }
    }
    out
}

/// Up to `max` key pages of the same site, one per kind, in order of usefulness.
pub(crate) fn key_pages(links: &[(String, String)], site: &str, max: usize) -> Vec<String> {
    const GROUPS: &[&[&str]] = &[
        &[
            "about",
            "company",
            "who-we-are",
            "our-story",
            "wie-wir-sind",
            "quienes-somos",
            "nosotros",
            "ueber-uns",
        ],
        &[
            "product",
            "service",
            "solution",
            "what-we-do",
            "platform",
            "features",
            "offering",
            "capabilit",
            "servicios",
            "leistungen",
        ],
        &[
            "customer",
            "case-stud",
            "client",
            "industries",
            "sector",
            "portfolio",
            "project",
            "success",
            "clientes",
            "referenzen",
        ],
        &["pricing", "plans", "price", "precios", "preise"],
        &["news", "press", "media", "blog", "insights", "noticias"],
        &[
            "career",
            "jobs",
            "join",
            "work-with-us",
            "empleo",
            "karriere",
        ],
    ];
    let Some(site_host) = host(site) else {
        return Vec::new();
    };
    let home_key = normalize_url(site);
    let candidates: Vec<&(String, String)> = links
        .iter()
        .filter(|(u, _)| host(u).is_some_and(|h| same_site(&h, &site_host)))
        .filter(|(u, _)| normalize_url(u) != home_key)
        .filter(|(u, _)| {
            let l = u.to_ascii_lowercase();
            ![
                ".pdf",
                ".jpg",
                ".png",
                ".zip",
                ".docx",
                "/login",
                "/signin",
                "/cart",
                "/wp-admin",
            ]
            .iter()
            .any(|x| l.contains(x))
        })
        .collect();
    let mut out: Vec<String> = Vec::new();
    for words in GROUPS {
        if out.len() >= max {
            break;
        }
        let hit = candidates.iter().find(|(u, a)| {
            let path = u.splitn(4, '/').nth(3).unwrap_or("").to_ascii_lowercase();
            let anchor = a.to_ascii_lowercase();
            !out.contains(u) && words.iter().any(|w| path.contains(w) || anchor.contains(w))
        });
        if let Some((u, _)) = hit {
            out.push(u.clone());
        }
    }
    out
}

/// A source the dossier may cite.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Src {
    pub url: String,
    pub title: String,
    pub date: Option<String>,
    /// The text the model saw (for checking that names it cites really appear there).
    pub text: String,
    /// True for the business's own website pages.
    pub own: bool,
}

impl Src {
    fn source(&self) -> Source {
        Source {
            url: self.url.clone(),
            title: self.title.clone(),
            date: self.date.clone(),
        }
    }
    fn haystack(&self) -> String {
        format!("{} {} {}", self.title, self.text, self.url).to_lowercase()
    }
}

const PROFILE_SYSTEM: &str = r#"You read a business's own website and draft its profile for a business-intelligence service. Reply in json.

Use only what the pages and the hints say; leave a field empty when they don't say it. Never invent.
- headline: what the business does, in one line of at most 90 characters ("Commercial cleaning for offices and schools in Cape Town").
- industry: its industry in 1 to 4 plain words ("Commercial cleaning", "B2B payments software").
- stage: one id: startup, growth (a scale-up), established, enterprise, nonprofit, public.
- location: its headquarters as "City, Country".
- reach: "local", "national" or "international": the markets it serves.
- seeking: 3 to 6 focus areas that would most help it grow (ids below).
- interests: up to 8 markets, sectors and topics it cares about (the segments it serves, its regions).
- skills: up to 10 of its products, services and capabilities, in its own words.
- goals: one sentence in its own voice when the site states an ambition ("Expand into Gauteng"); otherwise "".
- languages: the languages its site is written in.
- links: its official LinkedIn and other social profile URLs found on the pages.

Then plan follow-up web searches (plain words; no operators):
- competitors: 2 queries that would surface genuinely comparable companies in its category and country ("commercial cleaning companies Cape Town").
- market: 2 queries for current trends and demand in its industry and region ("South Africa facilities management market trends 2026").
- regulation: 2 queries for rules and upcoming changes that apply to its industry in its jurisdiction, aimed at official sources ("South Africa cleaning sector minimum wage 2027 gazette").
- country: the main country it operates in.

Reply with json only, in exactly this shape:
{"profile": {"headline": "", "industry": "", "stage": "", "location": "", "reach": "", "seeking": [], "interests": [], "skills": [], "goals": "", "languages": [], "links": []}, "country": "", "searches": {"competitors": [], "market": [], "regulation": []}}

Focus areas (id: label (what it covers)):
"#;

const INTEL_SYSTEM: &str = r#"You build a business-intelligence dossier about one business, strictly from numbered sources. Reply in json.

Hard rules:
- Every item cites exactly one source number ("S3") that supports it, and the overview cites the sources it uses. Use only the sources listed. If nothing supports something, leave it out: an empty section is fine, an invented item is not.
- The business's own website pages are the best source for what it does, its offerings and who it serves. Search results are for news, competitors, market trends and regulation.
- Before using a search result about the business itself, make sure it is about THIS business (same name and website). Companies with similar names don't count.
- Customers are segments ("Office parks and corporate campuses"), never customer lists. Put a customer's name in examples only when the business itself names it publicly (case studies, logos, testimonials) in the cited source.
- Competitors: the 4 to 8 most genuinely comparable companies (same category, overlapping customers and region), each named in its cited source. Directories, marketplaces and the business itself are not competitors. Give website only when the source shows it.
- Trends: up to 6 current market trends or drivers that matter to it; impact is "tailwind", "headwind" or "mixed".
- Regulation: only rules that truly apply to its industry and jurisdiction, preferring official sources; status is "proposed", "consultation", "passed" or "in force"; effective is the date it takes effect (YYYY-MM-DD) when stated; impact is "risk", "opportunity" or "both".
- News: up to 6 recent developments about the business itself, newest first.
- Write plainly and specifically; no hype. Leave unknown fields "".
- confidence (0 to 1): how sure you are that the sources describe this business and the facts hold.
- summary: one or two plain sentences to the business's team about what we found.

Reply with json only, in exactly this shape:
{"overview": {"summary": "two or three sentences: what it does, for whom, where", "industry": "", "headquarters": "", "founded": "", "size": "", "business_model": "", "markets": [], "sources": ["S1"]},
 "offerings": [{"name": "", "description": "", "audience": "", "pricing": "", "source": "S1"}],
 "segments": [{"name": "", "description": "", "examples": [], "source": "S2"}],
 "competitors": [{"name": "", "website": "", "summary": "", "difference": "", "overlap": "high", "source": "S9"}],
 "trends": [{"title": "", "summary": "", "impact": "tailwind", "source": "S12"}],
 "regulation": [{"title": "", "summary": "", "jurisdiction": "", "status": "in force", "effective": "", "impact": "opportunity", "source": "S15"}],
 "news": [{"title": "", "summary": "", "source": "S20"}],
 "confidence": 0.8,
 "summary": ""}"#;

fn pages_block(pages: &[(String, Page)]) -> String {
    pages
        .iter()
        .map(|(url, p)| {
            format!(
                "Page {url} - {}\n\"\"\"\n{}\n\"\"\"",
                truncate(&squash(&p.title), 140),
                excerpt(&p.content, PAGE_CHARS, &[])
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(crate) fn profile_request(
    today: &str,
    name: &str,
    site: &str,
    hints: &str,
    pages: &[(String, Page)],
    site_results: &[SearchResult],
) -> ChatRequest {
    let mut user = format!(
        "Today is {today}.\nBusiness: {name}\nWebsite: {site}\nHints: {}\n\n",
        if hints.is_empty() { "none" } else { hints }
    );
    if pages.is_empty() {
        user.push_str("Its website couldn't be read.\n");
    } else {
        user.push_str("Its website:\n");
        user.push_str(&pages_block(pages));
        user.push('\n');
    }
    if !site_results.is_empty() {
        user.push_str("\nSearch results from its site:\n");
        for r in site_results.iter().take(8) {
            user.push_str(&format!(
                "- {} | {}\n  {}\n",
                truncate(&squash(&r.title), 120),
                r.url,
                excerpt(&squash(&r.summary), 300, &[])
            ));
        }
    }
    ChatRequest::new(
        "business_profile",
        format!("{PROFILE_SYSTEM}{}", prompt::categories_block()),
        user,
    )
    .json()
    .thinking(Thinking::Off)
    .temperature(0.2)
    .max_tokens(2500)
}

pub(crate) fn intel_request(
    today: &str,
    name: &str,
    site: &str,
    hints: &str,
    profile: &Profile,
    sources: &[Src],
) -> ChatRequest {
    let mut user = format!(
        "Today is {today}.\nBusiness: {name}\nWebsite: {site}\nHints: {}\n\nWhat its website says about it (draft):\n{}\n\nSources:\n",
        if hints.is_empty() { "none" } else { hints },
        prompt::profile_block(profile)
    );
    for (i, s) in sources.iter().enumerate() {
        let date = s
            .date
            .as_deref()
            .map(|d| format!(" | {d}"))
            .unwrap_or_default();
        let own = if s.own { " (its own website)" } else { "" };
        user.push_str(&format!(
            "\n[S{}]{own} {}{date}\n{}\n{}\n",
            i + 1,
            truncate(&s.title, 160),
            s.url,
            s.text
        ));
    }
    ChatRequest::new("business_intel", INTEL_SYSTEM, user)
        .json()
        .thinking(Thinking::Low)
        .max_tokens(12_000)
}

/// The source a reference ("S3", "s3", 3) points at.
fn src_ref<'s>(v: &Value, key: &str, sources: &'s [Src]) -> Option<&'s Src> {
    let raw = match v.get(key)? {
        Value::String(s) => s
            .trim()
            .trim_start_matches(['S', 's', '['])
            .trim_end_matches(']')
            .to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let n = parse_number(&raw)?;
    if n < 1.0 || n.fract() != 0.0 {
        return None;
    }
    sources.get(n as usize - 1)
}

/// True when a company name (or its distinctive word) appears in the source's text.
fn named_in(name: &str, src: &Src) -> bool {
    const GENERIC: &[&str] = &[
        "group",
        "services",
        "service",
        "solutions",
        "company",
        "holdings",
        "limited",
        "the",
        "and",
        "pty",
        "ltd",
        "inc",
        "gmbh",
        "llc",
        "cleaning",
        "software",
        "technologies",
        "systems",
        "international",
        "global",
    ];
    let hay = src.haystack();
    let n = name.to_lowercase();
    if hay.contains(&n) {
        return true;
    }
    // Otherwise a distinctive word of the name must appear as a whole word.
    let hay_words: HashSet<&str> = hay
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    n.split(|c: char| !c.is_alphanumeric())
        .filter(|w| {
            w.chars().count() >= 4
                && !GENERIC.contains(w)
                && !["clean", "corp", "partners"].contains(w)
        })
        .any(|w| hay_words.contains(w))
}

/// The validated dossier from the model's reply: every item's source must be one of `sources`.
pub(crate) fn assemble_intel(v: &Value, sources: &[Src], business_name: &str, now: i64) -> Intel {
    let mut intel = Intel::default();
    let o = v.get("overview").cloned().unwrap_or(Value::Null);
    let mut ov_sources: Vec<Source> = Vec::new();
    for r in list(&o, "sources") {
        let wrapped = serde_json::json!({ "s": r });
        if let Some(s) = src_ref(&wrapped, "s", sources)
            && !ov_sources.iter().any(|x| x.url == s.url)
        {
            ov_sources.push(s.source());
        }
    }
    intel.overview = Overview {
        summary: clip(&text(&o, "summary"), 700),
        industry: clip(&text(&o, "industry"), 80),
        headquarters: clip(&text(&o, "headquarters"), 120),
        founded: clip(&text(&o, "founded"), 10),
        size: clip(&text(&o, "size"), 40),
        business_model: clip(&text(&o, "business_model"), 120),
        markets: list(&o, "markets"),
        sources: ov_sources,
    };
    for item in array(v, &["offerings"]) {
        if let Some(s) = src_ref(item, "source", sources) {
            intel.offerings.push(Offering {
                name: clip(&text(item, "name"), 100),
                description: clip(&text(item, "description"), 400),
                audience: clip(&text(item, "audience"), 160),
                pricing: clip(&text(item, "pricing"), 120),
                source: s.source(),
            });
        }
    }
    for item in array(v, &["segments"]) {
        if let Some(s) = src_ref(item, "source", sources) {
            let hay = s.haystack();
            let examples: Vec<String> = list(item, "examples")
                .into_iter()
                .filter(|e| hay.contains(&e.to_lowercase()))
                .collect();
            intel.segments.push(Segment {
                name: clip(&text(item, "name"), 100),
                description: clip(&text(item, "description"), 400),
                examples,
                source: s.source(),
            });
        }
    }
    let own = business_name.to_lowercase();
    for item in array(v, &["competitors"]) {
        let name = clip(&text(item, "name"), 100);
        let Some(s) = src_ref(item, "source", sources) else {
            continue;
        };
        if name.is_empty() || name.to_lowercase() == own || !named_in(&name, s) {
            continue;
        }
        // A website only when the sources show it.
        let site = clean_url(&text(item, "website"));
        let shown = host(&site).is_some_and(|h| {
            sources.iter().any(|x| {
                host(&x.url).is_some_and(|xh| same_site(&xh, &h))
                    || x.text.to_lowercase().contains(&h)
            })
        });
        intel.competitors.push(Competitor {
            name,
            website: if shown { site } else { String::new() },
            summary: clip(&text(item, "summary"), 300),
            difference: clip(&text(item, "difference"), 300),
            overlap: text(item, "overlap"),
            source: s.source(),
        });
    }
    for item in array(v, &["trends"]) {
        if let Some(s) = src_ref(item, "source", sources) {
            intel.trends.push(Trend {
                title: clip(&text(item, "title"), 140),
                summary: clip(&text(item, "summary"), 400),
                impact: text(item, "impact"),
                source: s.source(),
            });
        }
    }
    for item in array(v, &["regulation"]) {
        if let Some(s) = src_ref(item, "source", sources) {
            intel.regulation.push(Regulation {
                title: clip(&text(item, "title"), 160),
                summary: clip(&text(item, "summary"), 400),
                jurisdiction: clip(&text(item, "jurisdiction"), 60),
                status: text(item, "status"),
                effective: model::date::ymd_prefix(&text(item, "effective")),
                impact: text(item, "impact"),
                source: s.source(),
            });
        }
    }
    for item in array(v, &["news"]) {
        if let Some(s) = src_ref(item, "source", sources) {
            intel.news.push(NewsItem {
                title: clip(&text(item, "title"), 160),
                summary: clip(&text(item, "summary"), 300),
                source: s.source(),
            });
        }
    }
    let mut confidence = number(v, "confidence").unwrap_or(0.5);
    if confidence > 1.0 && confidence <= 100.0 {
        confidence /= 100.0;
    }
    intel.confidence = confidence as f32;
    intel.updated_at = now;
    intel.sanitized()
}

/// The drafted profile from the profile reply, with the name and website the account gave.
pub(crate) fn assemble_profile(
    v: &Value,
    name: &str,
    site: &str,
    allowed_links: &HashSet<String>,
) -> Profile {
    let p = v.get("profile").filter(|p| p.is_object()).unwrap_or(v);
    let links: Vec<String> = list(p, "links")
        .into_iter()
        .map(|l| clean_url(&l))
        .filter(|l| normalize_url(l).is_some_and(|k| allowed_links.contains(&k)))
        .collect();
    Profile {
        headline: clip(&text(p, "headline"), 200),
        stage: text(p, "stage").to_ascii_lowercase(),
        organization: name.to_string(),
        website: site.to_string(),
        industry: clip(&text(p, "industry"), 80),
        location: clip(&text(p, "location"), 120),
        reach: text(p, "reach").to_ascii_lowercase(),
        seeking: list(p, "seeking")
            .into_iter()
            .map(|s| s.to_ascii_lowercase())
            .collect(),
        interests: list(p, "interests"),
        skills: list(p, "skills"),
        goals: clip(&text(p, "goals"), 600),
        avoid: String::new(),
        links,
        languages: list(p, "languages"),
        research: None,
    }
    .sanitized()
}

enum Job {
    Fetch(String),
    Search(String, String),
}

enum Done {
    Page(String, Result<Page, deepseek::Error>),
    Results(String, Result<Vec<SearchResult>, deepseek::Error>),
    Skipped,
}

fn run_jobs(
    ctx: &Ctx,
    rep: &Reporter,
    jobs: &[Job],
    pct: (u8, u8),
    abort: &AtomicBool,
) -> Vec<Done> {
    let localization = ctx.localization.as_deref().and_then(public_ip);
    let n = jobs.len().max(1);
    let started = std::sync::atomic::AtomicUsize::new(0);
    parallel_map(jobs, 8, abort, |_, job| {
        if ctx.cancel.load(Ordering::Relaxed) {
            return Done::Skipped;
        }
        let k = started.fetch_add(1, Ordering::Relaxed);
        let p = pct.0 + ((pct.1 - pct.0) as usize * k / n) as u8;
        let out = match job {
            Job::Fetch(url) => {
                rep.report("researching", format!("Reading {}", display_url(url)), p);
                Done::Page(url.clone(), ctx.ai.fetch(url))
            }
            Job::Search(q, label) => {
                rep.report("researching", label.clone(), p);
                let mut req = SearchRequest::new(q.clone(), RESULTS_PER_SEARCH);
                req.localization = localization.clone();
                Done::Results(q.clone(), ctx.ai.search(&req))
            }
        };
        let account = match &out {
            Done::Page(_, Err(e)) | Done::Results(_, Err(e)) => e.is_account_problem(),
            _ => false,
        };
        if account {
            abort.store(true, Ordering::Relaxed);
        }
        out
    })
    .into_iter()
    .map(|d| d.unwrap_or(Done::Skipped))
    .collect()
}

/// "acme.com/about" for the feed.
fn display_url(url: &str) -> String {
    let h = host(url).unwrap_or_default();
    let path = url
        .splitn(4, '/')
        .nth(3)
        .unwrap_or("")
        .trim_end_matches('/');
    if path.is_empty() {
        h
    } else {
        truncate(&format!("{h}/{path}"), 60)
    }
}

/// Research a business and build its profile and dossier.
pub(crate) fn run(ctx: &Ctx, input: &BusinessInput) -> Result<(Profile, Intel), Error> {
    ctx.check_cancel()?;
    let rep = Reporter::new(ctx);
    let site = normalize_site(&input.website);
    let site_host = site.as_deref().and_then(host).unwrap_or_default();
    let name = {
        let n = squash(&input.business_name).replace('"', "");
        if n.is_empty() {
            site_host.split('.').next().unwrap_or("").to_string()
        } else {
            n
        }
    };
    if name.is_empty() && site.is_none() {
        return Err(Error::EmptyProfile);
    }
    let hints = truncate(&squash(&input.hints), 300);
    let year_ago = model::date::add_days(&ctx.today, -365).unwrap_or_else(|| ctx.today.clone());
    let abort = AtomicBool::new(false);
    let mut last_error: Option<deepseek::Error> = None;

    // 1. The home page, the site's own pages in search, news and competitors, all at once.
    rep.report(
        "researching",
        if site_host.is_empty() {
            format!("Looking up {name}")
        } else {
            format!("Reading {site_host}")
        },
        3,
    );
    let mut jobs: Vec<Job> = Vec::new();
    if let Some(s) = &site {
        jobs.push(Job::Fetch(s.clone()));
        jobs.push(Job::Search(
            format!("site:{site_host}"),
            format!("Reading what {site_host} publishes"),
        ));
    }
    if !name.is_empty() {
        jobs.push(Job::Search(
            format!("\"{name}\" after:{year_ago}"),
            format!("Looking for recent news about {name}"),
        ));
        jobs.push(Job::Search(
            format!("\"{name}\" competitors"),
            "Finding competitors".into(),
        ));
        jobs.push(Job::Search(
            format!("alternatives to \"{name}\""),
            "Finding alternatives buyers compare".into(),
        ));
    }
    let mut pages: Vec<(String, Page)> = Vec::new();
    let mut results: Vec<(String, Vec<SearchResult>)> = Vec::new();
    fn absorb(
        done: Vec<Done>,
        pages: &mut Vec<(String, Page)>,
        results: &mut Vec<(String, Vec<SearchResult>)>,
        last_error: &mut Option<deepseek::Error>,
    ) -> Result<(), Error> {
        for d in done {
            match d {
                Done::Page(url, Ok(p)) => pages.push((url, p)),
                Done::Results(q, Ok(r)) => results.push((q, r)),
                Done::Page(_, Err(e)) | Done::Results(_, Err(e)) => {
                    if e.is_account_problem() {
                        return Err(Error::Ai(e));
                    }
                    *last_error = Some(e);
                }
                Done::Skipped => {}
            }
        }
        Ok(())
    }
    absorb(
        run_jobs(ctx, &rep, &jobs, (5, 18), &abort),
        &mut pages,
        &mut results,
        &mut last_error,
    )?;
    ctx.check_cancel()?;

    // 2. Key pages linked from the home page.
    if let Some(s) = &site {
        let mut links: Vec<(String, String)> = pages
            .first()
            .map(|(u, p)| page_links(&p.content, u))
            .unwrap_or_default();
        if let Some((_, site_results)) = results.iter().find(|(q, _)| q.starts_with("site:")) {
            links.extend(
                site_results
                    .iter()
                    .map(|r| (clean_url(&r.url), r.title.clone())),
            );
        }
        // The Fetch API returns text without link targets, so common paths are tried too (a
        // missing page costs nothing).
        let origin = s.split('/').take(3).collect::<Vec<_>>().join("/");
        for path in [
            "about",
            "about-us",
            "products",
            "services",
            "solutions",
            "customers",
            "pricing",
        ] {
            links.push((format!("{origin}/{path}"), path.to_string()));
        }
        let more: Vec<Job> = key_pages(&links, s, MAX_PAGES)
            .into_iter()
            .map(Job::Fetch)
            .collect();
        if !more.is_empty() {
            let pages_word = if more.len() == 1 { "page" } else { "pages" };
            rep.report(
                "researching",
                format!("Reading {} more {pages_word} from {site_host}", more.len()),
                20,
            );
            absorb(
                run_jobs(ctx, &rep, &more, (20, 32), &abort),
                &mut pages,
                &mut results,
                &mut last_error,
            )?;
            ctx.check_cancel()?;
        }
    }
    let site_results: Vec<SearchResult> = results
        .iter()
        .find(|(q, _)| q.starts_with("site:"))
        .map(|(_, r)| r.clone())
        .unwrap_or_default();
    if pages.is_empty() && results.iter().all(|(_, r)| r.is_empty()) {
        return Err(Error::Ai(last_error.unwrap_or_else(|| {
            deepseek::Error::Unavailable(404, "nothing about this business could be read".into())
        })));
    }

    // 3. Draft the profile and plan the follow-up searches.
    rep.report("researching", format!("Understanding what {name} does"), 34);
    let site_str = site.clone().unwrap_or_default();
    let mut allowed_links: HashSet<String> = HashSet::new();
    for (_, p) in &pages {
        for (u, _) in page_links(&p.content, &site_str) {
            if let Some(k) = normalize_url(&u) {
                allowed_links.insert(k);
            }
        }
    }
    let (mut profile, country, follow_ups) = match ctx.ai.chat_json_value(&profile_request(
        &ctx.today,
        &name,
        &site_str,
        &hints,
        &pages,
        &site_results,
    )) {
        Ok(v) => {
            let profile = assemble_profile(&v, &name, &site_str, &allowed_links);
            let country = clip(&text(&v, "country"), 60);
            let s = v.get("searches").cloned().unwrap_or(Value::Null);
            let mut follow: Vec<(String, String)> = Vec::new();
            for (key, label) in [
                ("competitors", "Finding comparable companies".to_string()),
                (
                    "market",
                    format!(
                        "Checking market trends in {}",
                        if profile.industry.is_empty() {
                            "its industry".into()
                        } else {
                            profile.industry.to_lowercase()
                        }
                    ),
                ),
                (
                    "regulation",
                    if country.is_empty() {
                        "Checking the regulation that applies".to_string()
                    } else {
                        format!("Checking regulation in {country}")
                    },
                ),
            ] {
                for q in list(&s, key).into_iter().take(2) {
                    if let Some(q) = plan::clean_query(&q, &ctx.today)
                        && !follow.iter().any(|(x, _)| x.eq_ignore_ascii_case(&q))
                    {
                        follow.push((q, label.clone()));
                    }
                }
            }
            (profile, country, follow)
        }
        Err(e) if e.is_account_problem() => return Err(Error::Ai(e)),
        Err(e) => {
            last_error = Some(e);
            let p = Profile {
                organization: name.clone(),
                website: site_str.clone(),
                ..Default::default()
            }
            .sanitized();
            (p, String::new(), Vec::new())
        }
    };
    ctx.check_cancel()?;

    // 4. Follow-up searches: comparable companies, market, regulation.
    if !follow_ups.is_empty() {
        let jobs: Vec<Job> = follow_ups
            .into_iter()
            .map(|(q, l)| Job::Search(q, l))
            .collect();
        absorb(
            run_jobs(ctx, &rep, &jobs, (40, 55), &abort),
            &mut pages,
            &mut results,
            &mut last_error,
        )?;
        ctx.check_cancel()?;
    }

    // 5. The dossier, strictly from numbered sources.
    let mut sources: Vec<Src> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (url, p) in &pages {
        if let Some(k) = normalize_url(url)
            && seen.insert(k)
        {
            sources.push(Src {
                url: url.clone(),
                title: truncate(&squash(&p.title), 160),
                date: None,
                text: excerpt(&p.content, PAGE_CHARS, &[]),
                own: true,
            });
        }
    }
    let longest = results.iter().map(|(_, r)| r.len()).max().unwrap_or(0);
    'outer: for rank in 0..longest {
        for (_, rs) in &results {
            if sources.len() >= MAX_RESULTS + pages.len() {
                break 'outer;
            }
            if let Some(r) = rs.get(rank) {
                let url = clean_url(&r.url);
                if let Some(k) = normalize_url(&url)
                    && seen.insert(k)
                {
                    let own = host(&url)
                        .is_some_and(|h| !site_host.is_empty() && same_site(&h, &site_host));
                    let summary = if r.summary.trim().is_empty() {
                        &r.snippet
                    } else {
                        &r.summary
                    };
                    sources.push(Src {
                        url,
                        title: truncate(&squash(&r.title), 160),
                        date: r.date.as_deref().and_then(model::date::ymd_prefix),
                        text: excerpt(&squash(summary), RESULT_CHARS, &[]),
                        own,
                    });
                }
            }
        }
    }
    rep.report(
        "researching",
        format!("Writing the dossier from {} sources", sources.len()),
        58,
    );
    let messages = [
        "Checking each fact against its source",
        "Separating competitors from directories and namesakes",
        "Keeping only regulation that applies",
        "Leaving out anything no source supports",
    ];
    let req = intel_request(&ctx.today, &name, &site_str, &hints, &profile, &sources);
    let reply = with_heartbeat(&rep, "researching", 92, &messages, || {
        ctx.ai.chat_json_value(&req)
    });
    ctx.check_cancel()?;
    let (intel, owner_summary) = match reply {
        Ok(v) => (
            assemble_intel(&v, &sources, &name, model::date::now_unix()),
            clip(&text(&v, "summary"), 600),
        ),
        Err(e) if e.is_account_problem() => return Err(Error::Ai(e)),
        Err(e) => {
            if profile.headline.is_empty() {
                return Err(Error::Ai(e));
            }
            (
                Intel {
                    updated_at: model::date::now_unix(),
                    ..Default::default()
                },
                String::new(),
            )
        }
    };

    // Fill profile gaps from the dossier, and record where it all came from.
    if profile.industry.is_empty() {
        profile.industry = intel.overview.industry.clone();
    }
    if profile.location.is_empty() {
        profile.location = intel.overview.headquarters.clone();
    }
    let mut used: Vec<String> = pages.iter().map(|(u, _)| u.clone()).collect();
    for s in intel
        .overview
        .sources
        .iter()
        .chain(intel.news.iter().map(|n| &n.source))
    {
        if !used.contains(&s.url) {
            used.push(s.url.clone());
        }
    }
    let summary = if !owner_summary.is_empty() {
        owner_summary
    } else if intel.is_ready() {
        format!(
            "We read {site_host} and {} other sources to build {name}'s profile.",
            sources.len().saturating_sub(pages.len())
        )
    } else {
        format!(
            "We couldn't read enough about {name} to be sure, so please check the profile and fill in what's missing."
        )
    };
    let confidence = if intel.is_ready() {
        intel.confidence
    } else {
        intel.confidence.min(0.3)
    };
    profile.research = Some(Research {
        confidence,
        summary,
        sources: used,
        researched_at: model::date::now_unix(),
    });
    let profile = profile.sanitized();
    let counts = [
        (intel.offerings.len(), "offerings"),
        (intel.competitors.len(), "competitors"),
        (intel.trends.len(), "trends"),
        (intel.regulation.len(), "rules"),
        (intel.news.len(), "news items"),
    ];
    let found: Vec<String> = counts
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, l)| format!("{n} {l}"))
        .collect();
    let done = if found.is_empty() {
        "Research complete".to_string()
    } else {
        format!("Research complete: {}", found.join(", "))
    };
    rep.report("researching", done, 95);
    let _ = country;
    Ok((profile, intel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sites_and_links() {
        assert_eq!(
            normalize_site("acme.co.za/").as_deref(),
            Some("https://acme.co.za")
        );
        assert_eq!(
            normalize_site("http://acme.com").as_deref(),
            Some("http://acme.com")
        );
        assert_eq!(normalize_site("not a site"), None);
        let md = "[Home](/) [About us](/about-us) [Services](https://www.acme.co.za/services) [Blog](https://acme.co.za/blog/) [Mail](mailto:x@y.z) [LinkedIn](https://linkedin.com/company/acme) [Case studies](/case-studies \"t\") [Pricing](#pricing) [PDF](/brochure.pdf)";
        let links = page_links(md, "https://acme.co.za");
        assert!(links.contains(&(
            "https://acme.co.za/about-us".to_string(),
            "About us".to_string()
        )));
        assert!(
            links
                .iter()
                .any(|(u, _)| u == "https://linkedin.com/company/acme")
        );
        assert!(!links.iter().any(|(u, _)| u.starts_with("mailto")));
        let keys = key_pages(&links, "https://acme.co.za", 5);
        assert_eq!(
            keys,
            vec![
                "https://acme.co.za/about-us",
                "https://www.acme.co.za/services",
                "https://acme.co.za/case-studies",
                "https://acme.co.za/blog/"
            ]
        );
        assert_eq!(
            display_url("https://acme.co.za/about-us/"),
            "acme.co.za/about-us"
        );
    }

    fn src(url: &str, title: &str, text: &str, own: bool) -> Src {
        Src {
            url: url.into(),
            title: title.into(),
            date: Some("2026-10-01".into()),
            text: text.into(),
            own,
        }
    }

    #[test]
    fn intel_keeps_only_sourced_and_supported_items() {
        let sources = vec![
            src(
                "https://acme.co.za",
                "Acme Cleaning",
                "Office cleaning for corporate parks. Trusted by Growthpoint and Redefine.",
                true,
            ),
            src(
                "https://news.example/acme-wins",
                "Acme wins contract",
                "Acme Cleaning won a three-year contract.",
                false,
            ),
            src(
                "https://lists.example/top-cleaners",
                "Top cleaning companies in Cape Town",
                "Spotless Cape and Bidvest Prestige lead the market.",
                false,
            ),
            src(
                "https://gov.example/wage",
                "Sectoral determination: cleaning",
                "The cleaning sector minimum wage rises on 1 March 2027.",
                false,
            ),
        ];
        let v = json!({
            "overview": {"summary": "Acme cleans offices in Cape Town.", "industry": "Commercial cleaning", "headquarters": "Cape Town, South Africa", "markets": ["South Africa"], "sources": ["S1", "S9", "S1"]},
            "offerings": [{"name": "Office cleaning", "description": "Daily cleaning", "source": "S1"}, {"name": "Invented", "source": "S99"}, {"name": "No source"}],
            "segments": [{"name": "Corporate parks", "description": "Office parks", "examples": ["Growthpoint", "Made Up Corp"], "source": "S1"}],
            "competitors": [
                {"name": "Spotless Cape", "website": "https://spotless.example", "summary": "Office cleaning", "overlap": "HIGH", "source": "S3"},
                {"name": "Imaginary Clean Co", "source": "S3"},
                {"name": "Acme Cleaning", "source": "S1"}
            ],
            "trends": [{"title": "Hybrid work cuts office cleaning frequency", "impact": "headwind", "source": "s3"}],
            "regulation": [{"title": "Cleaning sector minimum wage increase", "jurisdiction": "South Africa", "status": "passed", "effective": "2027-03-01", "impact": "risk", "source": 4}],
            "news": [{"title": "Acme wins three-year contract", "source": "S2"}],
            "confidence": 0.82
        });
        let i = assemble_intel(&v, &sources, "Acme Cleaning", 1);
        assert_eq!(
            i.overview.sources.len(),
            1,
            "unknown and repeated sources dropped"
        );
        assert_eq!(i.overview.sources[0].url, "https://acme.co.za");
        assert_eq!(i.offerings.len(), 1);
        assert_eq!(
            i.segments[0].examples,
            vec!["Growthpoint"],
            "only customers the source names"
        );
        let names: Vec<&str> = i.competitors.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Spotless Cape"],
            "invented and self competitors dropped"
        );
        assert_eq!(
            i.competitors[0].website, "",
            "a website the sources don't show is blanked"
        );
        assert_eq!(i.competitors[0].overlap, "high");
        assert_eq!(i.trends.len(), 1);
        assert_eq!(i.regulation[0].effective.as_deref(), Some("2027-03-01"));
        assert_eq!(i.news[0].source.date.as_deref(), Some("2026-10-01"));
        assert!((i.confidence - 0.82).abs() < 1e-6);
        assert!(i.is_ready());
    }

    #[test]
    fn profile_keeps_given_name_and_site() {
        let allowed: HashSet<String> = ["linkedin.com/company/acme".to_string()].into();
        let v = json!({"profile": {"headline": "Office cleaning", "industry": "Commercial cleaning", "stage": "Established", "reach": "national", "seeking": ["sales", "funding", "jobs"], "links": ["https://linkedin.com/company/acme", "https://invented.example"]}});
        let p = assemble_profile(&v, "Acme Cleaning", "https://acme.co.za", &allowed);
        assert_eq!(p.organization, "Acme Cleaning");
        assert_eq!(p.website, "https://acme.co.za");
        assert_eq!(p.stage, "established");
        assert_eq!(p.seeking, vec!["sales", "funding"]);
        assert_eq!(p.links, vec!["https://linkedin.com/company/acme"]);
    }
}
