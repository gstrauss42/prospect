//! Auto-onboarding: find a person on the public web from their name, email and optional hints,
//! and draft their opportunity profile.
//!
//! Searches: the quoted name; the name with their organisation and that organisation's own site
//! when the email domain isn't a free provider; the name with their hints; a distinctive email
//! username; pages they linked are read with the Fetch API. Then one judgement call decides which
//! results are really about *this* person (never merging namesakes) and drafts the profile. With
//! nothing reliable, the draft comes from the hints alone with low confidence and an honest note.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use deepseek::{ChatRequest, Page, SearchRequest, SearchResult, Thinking};
use model::links::normalize_url;
use model::text::{squash, truncate};
use model::{Profile, Research};
use serde_json::Value;

use crate::shortlist::clean_url;
use crate::util::{
    Reporter, clip, excerpt, list, number, parallel_map, public_ip, text, with_heartbeat,
};
use crate::{Ctx, Error, ResearchInput, prompt};

/// Results given to the judgement call at most.
const MAX_RESULTS: usize = 40;
/// Pages from the hints read at most.
const MAX_LINKS: usize = 2;
/// Characters of a linked page given to the model.
const LINK_CHARS: usize = 5000;
/// Characters of each result's summary given to the model.
const RESULT_CHARS: usize = 500;

/// Free email providers: their domain says nothing about the person.
const FREE_MAIL: &[&str] = &[
    "gmail.com",
    "googlemail.com",
    "outlook.com",
    "hotmail.com",
    "live.com",
    "msn.com",
    "passport.com",
    "yahoo.com",
    "ymail.com",
    "rocketmail.com",
    "icloud.com",
    "me.com",
    "mac.com",
    "aol.com",
    "aim.com",
    "proton.me",
    "protonmail.com",
    "protonmail.ch",
    "pm.me",
    "gmx.com",
    "gmx.net",
    "gmx.de",
    "gmx.at",
    "gmx.ch",
    "web.de",
    "mail.ru",
    "inbox.ru",
    "list.ru",
    "bk.ru",
    "internet.ru",
    "rambler.ru",
    "yandex.ru",
    "yandex.com",
    "yandex.ua",
    "yandex.kz",
    "ya.ru",
    "zoho.com",
    "zohomail.com",
    "zohomail.eu",
    "mail.com",
    "email.com",
    "usa.com",
    "qq.com",
    "foxmail.com",
    "163.com",
    "126.com",
    "yeah.net",
    "sina.com",
    "sina.cn",
    "sohu.com",
    "aliyun.com",
    "naver.com",
    "daum.net",
    "hanmail.net",
    "kakao.com",
    "tutanota.com",
    "tutanota.de",
    "tuta.io",
    "tuta.com",
    "fastmail.com",
    "fastmail.fm",
    "hey.com",
    "duck.com",
    "hushmail.com",
    "mailfence.com",
    "posteo.de",
    "posteo.net",
    "seznam.cz",
    "email.cz",
    "centrum.cz",
    "wp.pl",
    "o2.pl",
    "interia.pl",
    "onet.pl",
    "libero.it",
    "virgilio.it",
    "tiscali.it",
    "alice.it",
    "orange.fr",
    "free.fr",
    "laposte.net",
    "sfr.fr",
    "wanadoo.fr",
    "neuf.fr",
    "t-online.de",
    "freenet.de",
    "arcor.de",
    "rediffmail.com",
    "btinternet.com",
    "sky.com",
    "talktalk.net",
    "virginmedia.com",
    "blueyonder.co.uk",
    "comcast.net",
    "verizon.net",
    "att.net",
    "sbcglobal.net",
    "bellsouth.net",
    "cox.net",
    "charter.net",
    "earthlink.net",
    "shaw.ca",
    "rogers.com",
    "sympatico.ca",
    "bigpond.com",
    "bigpond.net.au",
    "optusnet.com.au",
    "xtra.co.nz",
    "telkomsa.net",
    "vodamail.co.za",
    "mweb.co.za",
    "webmail.co.za",
    "afrihost.co.za",
    "uol.com.br",
    "bol.com.br",
    "terra.com.br",
    "ig.com.br",
    "prodigy.net.mx",
    "netvigator.com",
    "singnet.com.sg",
    "hotmail.co.uk",
    "yahoo.co.uk",
    "yahoo.co.in",
    "yahoo.co.jp",
];

/// Providers with a domain per country (yahoo.fr, hotmail.co.za, outlook.com.br…).
const FREE_MAIL_FAMILIES: &[&str] = &[
    "gmail",
    "googlemail",
    "yahoo",
    "ymail",
    "hotmail",
    "outlook",
    "live",
    "msn",
    "aol",
    "gmx",
    "yandex",
    "icloud",
    "proton",
    "protonmail",
    "zoho",
    "zohomail",
    "mail",
    "email",
    "web",
];

/// True when an email domain belongs to a free mail provider rather than an organisation.
pub(crate) fn is_free_mail(domain: &str) -> bool {
    let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if FREE_MAIL.contains(&d.as_str()) {
        return true;
    }
    match d.split_once('.') {
        Some((first, rest)) => {
            FREE_MAIL_FAMILIES.contains(&first)
                && rest.split('.').all(|label| {
                    (2..=3).contains(&label.len()) && label.chars().all(|c| c.is_ascii_alphabetic())
                })
        }
        None => false,
    }
}

/// The domain of an email address, lowercased.
pub(crate) fn email_domain(email: &str) -> Option<String> {
    let (local, domain) = email.trim().rsplit_once('@')?;
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let valid = !local.is_empty()
        && domain.contains('.')
        && domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    valid.then_some(domain)
}

/// The email domain when it points at an organisation (not a free provider).
pub(crate) fn org_domain(email: &str) -> Option<String> {
    email_domain(email).filter(|d| !is_free_mail(d))
}

/// The organisation-ish part of a domain: "heyhi" for heyhi.co.za, "ox" for ox.ac.uk.
fn domain_label(domain: &str) -> String {
    const SUFFIX: &[&str] = &[
        "co", "com", "org", "net", "ac", "edu", "gov", "gob", "or", "ne", "go", "mil", "nic",
    ];
    let labels: Vec<&str> = domain
        .split('.')
        .filter(|l| !l.is_empty() && *l != "www")
        .collect();
    let mut i = labels.len().saturating_sub(1);
    // Walk left past the TLD and second-level suffixes.
    while i > 0 && (labels[i].len() <= 2 || SUFFIX.contains(&labels[i])) {
        i -= 1;
    }
    if i == labels.len().saturating_sub(1) && labels.len() > 1 {
        i -= 1;
    }
    labels.get(i).map(|s| s.to_string()).unwrap_or_default()
}

/// A distinctive email username worth searching ("gstrauss18"), or None when it is generic
/// ("info") or just the person's name (the name searches cover it).
pub(crate) fn handle(email: &str, first: &str, last: &str) -> Option<String> {
    let local = email
        .trim()
        .rsplit_once('@')?
        .0
        .split('+')
        .next()?
        .to_ascii_lowercase();
    if local.chars().count() < 5 {
        return None;
    }
    const GENERIC: &[&str] = &[
        "info",
        "contact",
        "hello",
        "admin",
        "office",
        "mail",
        "sales",
        "support",
        "team",
        "jobs",
        "careers",
        "enquiries",
        "inquiries",
        "accounts",
        "billing",
        "noreply",
        "no-reply",
        "webmaster",
        "reception",
        "bookings",
        "orders",
        "hr",
        "help",
        "marketing",
        "press",
        "media",
    ];
    if GENERIC.contains(&local.as_str()) {
        return None;
    }
    let alnum: String = local.chars().filter(|c| c.is_alphanumeric()).collect();
    let f: String = first
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    let l: String = last
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    let initial = f.chars().next().map(String::from).unwrap_or_default();
    let name_only = [
        format!("{f}{l}"),
        format!("{l}{f}"),
        f.clone(),
        l.clone(),
        format!("{initial}{l}"),
        format!("{l}{initial}"),
    ]
    .contains(&alnum);
    let has_digit = alnum.chars().any(|c| c.is_ascii_digit());
    if name_only && !has_digit {
        return None;
    }
    Some(local)
}

/// Links in the hints: http(s) URLs and bare domains ("heyhi.co.za", "www.x.org/me").
pub(crate) fn hint_links(hints: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in
        hints.split(|c: char| c.is_whitespace() || c == '·' || c == '|' || c == ',' || c == ';')
    {
        let t = raw.trim_matches(|c: char| {
            matches!(
                c,
                '(' | ')' | '[' | ']' | '<' | '>' | '"' | '\'' | '.' | ','
            )
        });
        if t.is_empty() || t.contains('@') {
            continue;
        }
        let url = if model::links::is_http_url(t) {
            t.to_string()
        } else {
            let host = t.split('/').next().unwrap_or("");
            let labels: Vec<&str> = host.split('.').collect();
            let looks_domain = labels.len() >= 2
                && labels.iter().all(|l| {
                    !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                })
                && labels.last().is_some_and(|tld| {
                    tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
                });
            if !looks_domain {
                continue;
            }
            format!("https://{t}")
        };
        if model::links::is_http_url(&url) && !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

/// The hints without their links, as search words.
pub(crate) fn hint_words(hints: &str, links: &[String]) -> String {
    let mut s = hints.to_string();
    for l in links {
        s = s.replace(l.as_str(), " ");
        if let Some(bare) = l.strip_prefix("https://") {
            s = s.replace(bare, " ");
        }
    }
    let s: String = s
        .chars()
        .map(|c| {
            if matches!(c, '·' | '|' | ';' | '"' | '(' | ')' | '[' | ']') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let s = squash(&s).replace(" ,", ",");
    truncate(s.trim_matches(|c: char| c == ',' || c == ' '), 80)
        .trim_end_matches('…')
        .trim()
        .to_string()
}

/// One research search: the query and the feed line shown while it runs.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Lookup {
    pub q: String,
    pub label: String,
}

pub(crate) fn full_name(input: &ResearchInput) -> String {
    squash(&format!(
        "{} {}",
        input.first_name.trim(),
        input.last_name.trim()
    ))
    .replace('"', "")
}

/// The searches for a person, most telling first, at most six.
pub(crate) fn lookups(input: &ResearchInput) -> Vec<Lookup> {
    let name = full_name(input);
    let links = hint_links(&input.hints);
    let words = hint_words(&input.hints, &links);
    let mut out: Vec<Lookup> = Vec::new();
    fn push(out: &mut Vec<Lookup>, q: String, label: String) {
        if let Some(q) = deepseek::clamp_query(&q)
            && !out.iter().any(|l| l.q.eq_ignore_ascii_case(&q))
        {
            out.push(Lookup { q, label });
        }
    }
    if !name.is_empty() {
        push(
            &mut out,
            format!("\"{name}\""),
            format!("Searching for “{name}”"),
        );
    }
    if let Some(domain) = org_domain(&input.email) {
        let label = domain_label(&domain);
        if !name.is_empty() && !label.is_empty() {
            push(
                &mut out,
                format!("\"{name}\" {label}"),
                format!("Searching for “{name}” with {label}"),
            );
        }
        push(
            &mut out,
            format!("site:{domain}"),
            format!("Looking at {domain}"),
        );
    }
    if !name.is_empty() && !words.is_empty() {
        push(
            &mut out,
            format!("\"{name}\" {words}"),
            format!("Searching for “{name}” with your hints"),
        );
    }
    if let Some(h) = handle(&input.email, &input.first_name, &input.last_name) {
        push(
            &mut out,
            format!("\"{h}\""),
            format!("Searching for the username “{h}”"),
        );
    }
    if !name.is_empty() && out.len() < 4 {
        push(
            &mut out,
            format!("\"{name}\" LinkedIn"),
            format!("Searching for “{name}” on LinkedIn"),
        );
    }
    if name.is_empty() && !words.is_empty() {
        push(&mut out, words.clone(), "Searching with your hints".into());
    }
    out.truncate(6);
    out
}

/// Words that make an interest or skill a sensitive personal trait rather than a field.
const SENSITIVE: &[&str] = &[
    "religio",
    "church",
    "mosque",
    "synagogue",
    "muslim",
    "islam",
    "christian",
    "catholic",
    "jewish",
    "judaism",
    "hindu",
    "buddhis",
    "atheis",
    "sexual",
    "lgbt",
    "gay",
    "lesbian",
    "bisexual",
    "transgender",
    "queer",
    "ethnic",
    "racial",
    "pregnan",
    "disabilit",
    "illness",
    "diagnos",
    "depression",
    "anxiety",
    "adhd",
    "autism",
    "cancer",
    "diabetes",
    "debt",
    "bankrupt",
    "political party",
    "democrat",
    "republican",
    "conservative party",
    "labour party",
    "divorce",
];

pub(crate) fn is_sensitive(item: &str) -> bool {
    let t = item.to_lowercase();
    SENSITIVE.iter().any(|w| t.contains(w))
}

const SYSTEM: &str = r#"You help a person set up their opportunity profile by looking them up on the public web. They gave us their name and email and asked us to do this. Reply in json.

Who is who (the most important rule):
- Many people share a name. A result is about this person only when the name matches AND something else agrees: their email domain or organisation, a hint they gave, a username matching their email, a page they linked, or details consistent with a result that is already certainly them. A result where only the name matches is not enough, unless the name is truly unusual and nothing contradicts it.
- Never merge different people. If the results describe several people with this name (a dentist in Ohio, a student in Lagos), use only those that match the other signals; if you can't tell which one is them, use none.
- When in doubt, leave it out. A sparse, correct profile is far better than a rich, wrong one.

Privacy and honesty:
- Use only professional, public information that helps find opportunities: work, study, skills, projects, interests, city and country.
- Never infer or mention health, religion, politics, sexuality, ethnicity, family, age or finances.
- Never invent anything. Every fact must come from a result you matched, a page they linked, or their hints.

Draft the profile, leaving a field empty when nothing supports it:
- headline: who they are in one line, at most 90 characters ("Product lead at HeyHi, building voice ordering for restaurants").
- stage: one id: student, early, mid, senior, changer, freelancer, founder, business, nonprofit, retired, other.
- organization: their current company, school or organisation.
- location: "City, Country".
- reach: "local", "country" or "global": where opportunities could suit them; "global" if unsure.
- seeking: 2 to 5 kinds of opportunity that would most likely help them (ids below).
- interests: 3 to 8 topics or fields they're into.
- skills: 3 to 8 things they're good at or offer.
- goals: one sentence in their own voice suggesting what they may want next ("Grow HeyHi's restaurant customers across Europe"), only when the evidence supports it; they will edit it.
- languages: only when evident.
- links: URLs of their own profiles or sites among the results you matched (LinkedIn, GitHub, portfolio, company page).

matched: the numbers (#) of the results that are about them.
confidence, from 0 to 1, that the matched results really are this person: 0.9 or more when several independent sources agree; 0.6 to 0.85 for one strong source plus a second signal; 0.3 to 0.55 when plausible but thin; below 0.3 when there is essentially nothing. With no matches, 0.1 or less.
summary: one or two warm, plain sentences addressed to them ("We found you at HeyHi in Cape Town, where you're building voice ordering for restaurants."). If nothing reliable turned up, say so kindly and that the draft starts from their hints.

Reply with json only, in exactly this shape:
{"matched": [2, 5], "confidence": 0.8, "summary": "...", "profile": {"headline": "", "stage": "", "organization": "", "location": "", "reach": "", "seeking": [], "interests": [], "skills": [], "goals": "", "languages": [], "links": []}}

Kinds of opportunity (id: label (what it covers)):
"#;

/// The judgement request.
pub(crate) fn request(
    today: &str,
    input: &ResearchInput,
    results: &[SearchResult],
    pages: &[(String, Page)],
) -> ChatRequest {
    let name = full_name(input);
    let email = input.email.trim();
    let email_line = match email_domain(email) {
        Some(d) if is_free_mail(&d) => format!(
            "Email: {email} ({d} is a free email provider, so the domain says nothing about them; the username might)"
        ),
        Some(d) => format!("Email: {email} (domain {d}: probably their organisation or school)"),
        None => "Email: not usable".to_string(),
    };
    let hints = squash(&input.hints);
    let mut user = format!(
        "Today is {today}.\nName: {name}\n{email_line}\nTheir hints: {}\n",
        if hints.is_empty() {
            "none".to_string()
        } else {
            format!("\"{}\"", truncate(&hints, 500))
        }
    );
    if !pages.is_empty() {
        user.push_str("\nPages they linked (certainly about them or their work):\n");
        for (i, (url, page)) in pages.iter().enumerate() {
            user.push_str(&format!(
                "[L{}] {url} - {}\n\"\"\"\n{}\n\"\"\"\n",
                i + 1,
                truncate(&squash(&page.title), 140),
                excerpt(&page.content, LINK_CHARS, &[])
            ));
        }
    }
    if results.is_empty() {
        user.push_str("\nSearch results: none were found.\n");
    } else {
        user.push_str("\nSearch results:\n");
        for (i, r) in results.iter().enumerate() {
            let host = model::links::host(&r.url).unwrap_or_default();
            let site = r
                .site_name
                .as_deref()
                .map(squash)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| host.clone());
            let mut head = format!(
                "#{} {} | {site} ({host})",
                i + 1,
                truncate(&squash(&r.title), 160)
            );
            if let Some(d) = r.date.as_deref().and_then(model::date::ymd_prefix) {
                head.push_str(&format!(" | {d}"));
            }
            let summary = if r.summary.trim().is_empty() {
                &r.snippet
            } else {
                &r.summary
            };
            user.push_str(&format!(
                "\n{head}\n{}\n{}\n",
                r.url,
                excerpt(&squash(summary), RESULT_CHARS, &[])
            ));
        }
    }
    let system = format!("{SYSTEM}{}", prompt::categories_block());
    ChatRequest::new("research", system, user)
        .json()
        .thinking(Thinking::Low)
        .max_tokens(10_000)
}

fn honest_summary(has_hints: bool) -> String {
    if has_hints {
        "We couldn't find public pages we were sure were about you, so this draft starts from your hints. Add anything that's missing.".into()
    } else {
        "We couldn't find public pages we were sure were about you, so there's little to go on yet. Fill in a few details below and we'll take it from there.".into()
    }
}

/// The drafted, validated profile from the model's reply. `pages` are the linked pages that
/// were read; `links` all links the person gave.
pub(crate) fn assemble(
    v: &Value,
    results: &[SearchResult],
    pages: &[(String, Page)],
    links: &[String],
    has_hints: bool,
) -> Profile {
    let p = v.get("profile").filter(|p| p.is_object()).unwrap_or(v);
    let mut matched: Vec<usize> = Vec::new();
    for m in list(v, "matched") {
        if let Some(n) = crate::util::parse_number(&m)
            && n >= 1.0
            && n.fract() == 0.0
            && (n as usize) <= results.len()
            && !matched.contains(&(n as usize - 1))
        {
            matched.push(n as usize - 1);
        }
    }
    let found_something = !matched.is_empty() || !pages.is_empty();
    let mut confidence = number(v, "confidence").unwrap_or(0.0);
    if confidence > 1.0 && confidence <= 100.0 {
        confidence /= 100.0;
    }
    let mut confidence = confidence.clamp(0.0, 1.0) as f32;
    if !found_something {
        confidence = confidence.min(0.2);
    }
    let mut sources: Vec<String> = matched
        .iter()
        .map(|&i| clean_url(&results[i].url))
        .collect();
    sources.extend(pages.iter().map(|(u, _)| u.clone()));
    let allowed: HashSet<String> = sources
        .iter()
        .chain(links.iter())
        .filter_map(|u| normalize_url(u))
        .collect();
    let mut profile_links: Vec<String> = links.to_vec();
    for l in list(p, "links") {
        let l = clean_url(&l);
        if normalize_url(&l).is_some_and(|k| allowed.contains(&k)) && !profile_links.contains(&l) {
            profile_links.push(l);
        }
    }
    let summary = if found_something {
        Some(clip(&text(v, "summary"), 600)).filter(|s| !s.is_empty()).unwrap_or_else(|| "We found a few pages about you and drafted your profile from them. Check it over and change anything that's off.".into())
    } else {
        honest_summary(has_hints)
    };
    let keep = |items: Vec<String>| -> Vec<String> {
        items.into_iter().filter(|i| !is_sensitive(i)).collect()
    };
    Profile {
        headline: clip(&text(p, "headline"), 200),
        stage: text(p, "stage").to_ascii_lowercase(),
        organization: clip(&text(p, "organization"), 120),
        location: clip(&text(p, "location"), 120),
        reach: text(p, "reach").to_ascii_lowercase(),
        seeking: list(p, "seeking")
            .into_iter()
            .map(|s| s.to_ascii_lowercase())
            .collect(),
        interests: keep(list(p, "interests")),
        skills: keep(list(p, "skills")),
        goals: clip(&text(p, "goals"), 600),
        avoid: String::new(),
        website: String::new(),
        industry: String::new(),
        links: profile_links,
        languages: list(p, "languages"),
        research: Some(Research {
            confidence,
            summary,
            sources,
            researched_at: model::date::now_unix(),
        }),
    }
    .sanitized()
}

/// The profile when there is nothing at all to go on.
fn empty_draft(has_hints: bool) -> Profile {
    Profile {
        research: Some(Research {
            confidence: 0.0,
            summary: honest_summary(has_hints),
            sources: Vec::new(),
            researched_at: model::date::now_unix(),
        }),
        ..Default::default()
    }
    .sanitized()
}

enum Job {
    Search(Lookup),
    Fetch(String),
}

enum Done {
    Results(Result<Vec<SearchResult>, deepseek::Error>),
    Page(String, Result<Page, deepseek::Error>),
    Skipped,
}

/// Research a person and draft their profile.
pub(crate) fn run(ctx: &Ctx, input: &ResearchInput) -> Result<Profile, Error> {
    ctx.check_cancel()?;
    let rep = Reporter::new(ctx);
    let name = full_name(input);
    let hints = squash(&input.hints);
    let links = hint_links(&hints);
    let has_hints = !hints.is_empty();
    let lookups = lookups(input);
    if lookups.is_empty() && links.is_empty() {
        rep.report(
            "researching",
            "There's nothing to look up yet, so you'll start from a blank profile",
            95,
        );
        return Ok(empty_draft(has_hints));
    }
    let who = if name.is_empty() {
        "you".to_string()
    } else {
        name.clone()
    };
    rep.report(
        "researching",
        format!("Looking for public information about {who}"),
        5,
    );
    let mut jobs: Vec<Job> = lookups.into_iter().map(Job::Search).collect();
    jobs.extend(links.iter().take(MAX_LINKS).cloned().map(Job::Fetch));
    let n_searches = jobs.iter().filter(|j| matches!(j, Job::Search(_))).count();
    let abort = AtomicBool::new(false);
    let started = AtomicUsize::new(0);
    let localization = ctx.localization.as_deref().and_then(public_ip);
    let outcomes = parallel_map(&jobs, 8, &abort, |_, job| {
        if ctx.cancel.load(Ordering::Relaxed) {
            return Done::Skipped;
        }
        let k = started.fetch_add(1, Ordering::Relaxed);
        let pct = 8 + (40 * k / jobs.len().max(1)) as u8;
        let out = match job {
            Job::Search(l) => {
                rep.report("researching", l.label.clone(), pct);
                let mut req = SearchRequest::new(l.q.clone(), 10);
                req.localization = localization.clone();
                Done::Results(ctx.ai.search(&req))
            }
            Job::Fetch(url) => {
                rep.report("researching", "Reading the page you linked", pct);
                Done::Page(url.clone(), ctx.ai.fetch(url))
            }
        };
        let account = match &out {
            Done::Results(Err(e)) | Done::Page(_, Err(e)) => e.is_account_problem(),
            _ => false,
        };
        if account {
            abort.store(true, Ordering::Relaxed);
        }
        out
    });
    ctx.check_cancel()?;
    // Results interleaved across searches, so each search's best results make the cut.
    let mut per_search: Vec<Vec<SearchResult>> = Vec::new();
    let mut pages: Vec<(String, Page)> = Vec::new();
    let mut failed = 0usize;
    let mut last_error: Option<deepseek::Error> = None;
    for out in outcomes.into_iter().flatten() {
        match out {
            Done::Results(Ok(r)) => per_search.push(r),
            Done::Results(Err(e)) => {
                if e.is_account_problem() {
                    return Err(Error::Ai(e));
                }
                failed += 1;
                last_error = Some(e);
            }
            Done::Page(url, Ok(page)) => pages.push((url, page)),
            Done::Page(_, Err(e)) if e.is_account_problem() => return Err(Error::Ai(e)),
            Done::Page(..) | Done::Skipped => {}
        }
    }
    if n_searches > 0 && failed == n_searches {
        return Err(Error::Ai(last_error.unwrap_or_else(|| {
            deepseek::Error::Network("every search failed".into())
        })));
    }
    let results = interleave(per_search, MAX_RESULTS);
    if results.is_empty() && pages.is_empty() && !has_hints {
        rep.report(
            "researching",
            "We couldn't find public pages about you, so you'll start from a blank profile",
            95,
        );
        return Ok(empty_draft(false));
    }
    rep.report(
        "researching",
        format!(
            "Found {} pages, checking which are really about you",
            results.len() + pages.len()
        ),
        55,
    );
    let messages = [
        "Matching pages against your name, email and hints",
        "Leaving out anyone who just shares your name",
        "Drafting your profile from what's certain",
    ];
    let req = request(&ctx.today, input, &results, &pages);
    let reply = with_heartbeat(&rep, "researching", 90, &messages, || {
        ctx.ai.chat_json_value(&req)
    });
    ctx.check_cancel()?;
    let v = reply.map_err(Error::Ai)?;
    let profile = assemble(&v, &results, &pages, &links, has_hints);
    let confidence = profile
        .research
        .as_ref()
        .map(|r| r.confidence)
        .unwrap_or(0.0);
    let msg = if confidence >= 0.5 {
        "Drafted your profile from what we found"
    } else {
        "Drafted a starting profile; please check it over"
    };
    rep.report("researching", msg, 95);
    Ok(profile)
}

/// Round-robin across searches, unique by URL, at most `max`.
fn interleave(per_search: Vec<Vec<SearchResult>>, max: usize) -> Vec<SearchResult> {
    let mut out: Vec<SearchResult> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let longest = per_search.iter().map(Vec::len).max().unwrap_or(0);
    for rank in 0..longest {
        for results in &per_search {
            if out.len() >= max {
                return out;
            }
            if let Some(r) = results.get(rank) {
                let mut r = r.clone();
                r.url = clean_url(&r.url);
                if let Some(key) = normalize_url(&r.url)
                    && seen.insert(key)
                {
                    out.push(r);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(first: &str, last: &str, email: &str, hints: &str) -> ResearchInput {
        ResearchInput {
            first_name: first.into(),
            last_name: last.into(),
            email: email.into(),
            hints: hints.into(),
        }
    }

    #[test]
    fn free_mail_providers() {
        for d in [
            "gmail.com",
            "GoogleMail.com",
            "outlook.com",
            "hotmail.co.uk",
            "hotmail.fr",
            "live.co.za",
            "yahoo.com.br",
            "icloud.com",
            "me.com",
            "aol.com",
            "proton.me",
            "protonmail.com",
            "gmx.de",
            "mail.ru",
            "yandex.ru",
            "zoho.com",
            "qq.com",
        ] {
            assert!(is_free_mail(d), "{d}");
        }
        for d in [
            "heyhi.co.za",
            "uct.ac.za",
            "mail.heyhi.co.za",
            "ox.ac.uk",
            "unilag.edu.ng",
            "liveperson.com",
            "gmailer.io",
            "outlookcareers.org",
        ] {
            assert!(!is_free_mail(d), "{d}");
        }
        assert_eq!(
            org_domain("jane@HeyHi.co.za").as_deref(),
            Some("heyhi.co.za")
        );
        assert_eq!(org_domain("jane@gmail.com"), None);
        assert_eq!(email_domain("nonsense"), None);
    }

    #[test]
    fn domain_labels() {
        assert_eq!(domain_label("heyhi.co.za"), "heyhi");
        assert_eq!(domain_label("uct.ac.za"), "uct");
        assert_eq!(domain_label("ox.ac.uk"), "ox");
        assert_eq!(domain_label("stripe.com"), "stripe");
        assert_eq!(domain_label("mail.example.org"), "example");
        assert_eq!(domain_label("example.de"), "example");
    }

    #[test]
    fn handles() {
        assert_eq!(
            handle("gstrauss18@gmail.com", "Gabriel", "Strauss").as_deref(),
            Some("gstrauss18")
        );
        assert_eq!(
            handle("gabriel.strauss@gmail.com", "Gabriel", "Strauss"),
            None,
            "just the name"
        );
        assert_eq!(handle("gstrauss@gmail.com", "Gabriel", "Strauss"), None);
        assert_eq!(handle("info@heyhi.co.za", "Gabriel", "Strauss"), None);
        assert_eq!(handle("ab@x.com", "A", "B"), None);
        assert_eq!(
            handle("chefmaria+opps@gmail.com", "Maria", "Ngcobo").as_deref(),
            Some("chefmaria")
        );
    }

    #[test]
    fn links_and_words_from_hints() {
        let h = "Cape Town · chef at Mama's Kitchen · https://mamaskitchen.co.za/about, also www.mychef.blog";
        let links = hint_links(h);
        assert_eq!(
            links,
            vec![
                "https://mamaskitchen.co.za/about",
                "https://www.mychef.blog"
            ]
        );
        assert_eq!(
            hint_words(h, &links),
            "Cape Town chef at Mama's Kitchen, also"
        );
        assert!(hint_links("email me at x@y.com, I'm 3.5 years in").is_empty());
    }

    #[test]
    fn lookups_cover_name_domain_hints_and_handle() {
        let l = lookups(&input(
            "Jane",
            "Doe",
            "jane@heyhi.co.za",
            "Cape Town, product lead",
        ));
        let qs: Vec<&str> = l.iter().map(|x| x.q.as_str()).collect();
        assert_eq!(
            qs,
            vec![
                "\"Jane Doe\"",
                "\"Jane Doe\" heyhi",
                "site:heyhi.co.za",
                "\"Jane Doe\" Cape Town, product lead"
            ]
        );
        assert!(l.iter().all(|x| !x.label.is_empty()));
        let g = lookups(&input("Gabriel", "Strauss", "gstrauss18@gmail.com", ""));
        let qs: Vec<&str> = g.iter().map(|x| x.q.as_str()).collect();
        assert_eq!(
            qs,
            vec![
                "\"Gabriel Strauss\"",
                "\"gstrauss18\"",
                "\"Gabriel Strauss\" LinkedIn"
            ],
            "free mail: no domain search"
        );
        assert!(lookups(&input("", "", "", "")).is_empty());
        let hints_only = lookups(&input("", "", "", "pastry chef Durban"));
        assert_eq!(hints_only[0].q, "pastry chef Durban");
    }

    fn result(url: &str, title: &str) -> SearchResult {
        SearchResult {
            url: url.into(),
            title: title.into(),
            summary: "s".into(),
            ..Default::default()
        }
    }

    #[test]
    fn assemble_trusts_only_matched_evidence() {
        let results = vec![
            result("https://heyhi.co.za/team#jane", "Jane Doe - HeyHi"),
            result("https://linkedin.com/in/janedoe", "Jane Doe | LinkedIn"),
            result("https://dentist.example/jane-doe", "Dr Jane Doe, dentist"),
        ];
        let v = json!({
            "matched": [1, 2, 2, 7, "x"],
            "confidence": 85,
            "summary": "We found you at HeyHi in Cape Town.",
            "profile": {
                "headline": "Product lead at HeyHi", "stage": "Established", "organization": "HeyHi", "location": "Cape Town, South Africa",
                "reach": "global", "seeking": ["sales", "partners", "bogus"], "interests": ["voice AI", "Christian worship music", "restaurants"],
                "skills": ["product management"], "goals": "Grow HeyHi", "languages": ["English"],
                "links": ["https://linkedin.com/in/janedoe", "https://dentist.example/jane-doe", "https://invented.example/jane"]
            }
        });
        let p = assemble(
            &v,
            &results,
            &[],
            &["https://janedoe.dev".to_string()],
            true,
        );
        let r = p.research.clone().unwrap();
        assert!((r.confidence - 0.85).abs() < 1e-6, "percent scale accepted");
        assert_eq!(
            r.sources,
            vec![
                "https://heyhi.co.za/team",
                "https://linkedin.com/in/janedoe"
            ]
        );
        assert_eq!(r.summary, "We found you at HeyHi in Cape Town.");
        assert_eq!(p.stage, "established");
        assert_eq!(p.seeking, vec!["sales", "partners"]);
        assert_eq!(
            p.interests,
            vec!["voice AI", "restaurants"],
            "sensitive traits are dropped"
        );
        assert_eq!(
            p.links,
            vec!["https://janedoe.dev", "https://linkedin.com/in/janedoe"],
            "only the person's own and matched links"
        );
        assert!(r.researched_at > 0);
    }

    #[test]
    fn assemble_without_matches_is_honest() {
        let results = vec![result("https://a.org/x", "Someone else")];
        let v = json!({"matched": [], "confidence": 0.6, "summary": "We found you!", "profile": {"headline": "Chef", "location": "Cape Town"}});
        let p = assemble(&v, &results, &[], &[], true);
        let r = p.research.unwrap();
        assert!(r.confidence <= 0.2);
        assert!(r.summary.starts_with("We couldn't find"), "{}", r.summary);
        assert!(r.sources.is_empty());
        assert_eq!(p.headline, "Chef");
        let blank = empty_draft(false);
        assert_eq!(blank.research.unwrap().confidence, 0.0);
    }

    #[test]
    fn sensitive_words() {
        assert!(is_sensitive("LGBT activism"));
        assert!(is_sensitive("Church choir"));
        assert!(!is_sensitive("health tech"));
        assert!(!is_sensitive("political science"));
        assert!(!is_sensitive("machine learning"));
    }

    #[test]
    fn interleave_round_robins_and_dedupes() {
        let a = vec![
            result("https://a.org/1", "a1"),
            result("https://a.org/2", "a2"),
        ];
        let b = vec![
            result("https://a.org/1#x", "dup"),
            result("https://b.org/2", "b2"),
        ];
        let out = interleave(vec![a, b], 10);
        let urls: Vec<&str> = out.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            urls,
            vec!["https://a.org/1", "https://a.org/2", "https://b.org/2"]
        );
        assert_eq!(
            interleave(vec![vec![result("https://a.org/1", "a")]], 0).len(),
            0
        );
    }

    #[test]
    fn request_states_the_email_situation() {
        let r = request(
            "2026-11-26",
            &input("Gabriel", "Strauss", "gstrauss18@gmail.com", ""),
            &[result("https://x.org", "X")],
            &[],
        );
        let user = match &r.user[0] {
            deepseek::Part::Text(t) => t.clone(),
            _ => String::new(),
        };
        assert_eq!(r.stage, "research");
        assert!(r.system.contains("json"));
        assert!(user.contains("Today is 2026-11-26."));
        assert!(user.contains("Name: Gabriel Strauss"));
        assert!(user.contains("free email provider"));
        assert!(user.contains("Their hints: none"));
        assert!(user.contains("#1 X | x.org (x.org)\nhttps://x.org\n"));
    }
}
