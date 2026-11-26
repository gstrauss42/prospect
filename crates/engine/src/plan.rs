//! Step 1 of discovery: turn the business profile, its intelligence dossier and steering into
//! diverse Search API queries, per focus area.
//!
//! The model writes the queries (thinking off: a creative, fast step). Everything it returns is
//! validated: known focus areas, sane operators, no duplicates, nothing that searches for the
//! business itself. If the call fails, a deterministic plan built from the profile takes over.

use deepseek::{ChatRequest, Thinking};
use model::{Intel, Profile, Steering};
use serde_json::Value;

use crate::prompt;
use crate::util::{array, text, text_any};

/// One planned search.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Query {
    /// What goes to the Search API (operators included), at most 128 characters.
    pub q: String,
    /// The [`model::OPPORTUNITY_TYPES`] id it targets.
    pub category: String,
    /// 3–7 plain words for the live feed: "Searching: cleaning tenders in the Western Cape".
    pub about: String,
}

/// How far back "recent" reaches for `after:`.
pub(crate) const RECENT_DAYS: i64 = 120;

/// `today` minus [`RECENT_DAYS`], for `after:`.
pub(crate) fn after_date(today: &str) -> String {
    model::date::add_days(today, -RECENT_DAYS).unwrap_or_else(|| today.to_string())
}

/// The year of the next application cycle: next year from July on.
pub(crate) fn coming_year(today: &str) -> i64 {
    let year: i64 = today.get(0..4).and_then(|y| y.parse().ok()).unwrap_or(2026);
    let month: u32 = today.get(5..7).and_then(|m| m.parse().ok()).unwrap_or(1);
    if month >= 7 { year + 1 } else { year }
}

const SYSTEM: &str = r#"You plan web searches that find real, current business opportunities for one business. Reply in json.

The searches go to a web search engine like Google. It supports these operators:
"exact phrase"   -word   site:example.org   -site:example.org   lang:xx (ISO 639-1 code)   after:YYYY-MM-DD   before:YYYY-MM-DD

Look in each focus area the business wants, using what the intelligence dossier says about it:
- sales: public tenders, RFPs, RFQs, expressions of interest, supplier and vendor calls, buyer programmes and buying signals (expansions, new projects, funding rounds) in the segments it serves. Use the words buyers use: "tender", "RFP", "request for proposals", "RFQ", "expression of interest", "supplier registration", "procurement".
- funding: grants, incentives, tax credits, rebates, subsidised finance and public programmes in its jurisdiction and industry; investors and accelerators if it is a startup or scale-up.
- regulation: new or changing rules, standards and deadlines in its industry and jurisdiction that create demand for what it sells or require it to act.
- market: openings against its competitors, new segments and new regions it could enter, distributors in those regions.
- partners: partner, reseller, channel, integration and marketplace programmes; complementary companies.
- visibility: its industry's trade shows and expos (exhibitor and hosted-buyer programmes), awards taking entries, calls for speakers, press opportunities.
- talent: training subsidies, learnerships and apprenticeships, graduate and skills programmes for its industry.
- operations: suppliers, shared services, technology and cost-saving programmes.

What makes a great query:
- It finds pages that ARE opportunities (an open tender, a grant round, an awards call, an exhibitor application), or the official portals and programme pages that list them; not articles about them.
- One clear idea in 3 to 9 words plus operators; no questions, no filler ("best", "top 10", "how to").
- Specific to the business's industry, offerings, segments and places.
- Local queries name the place (city, province, country) and use local terms ("tender" and "RFQ" in South Africa, "Ausschreibung" in Germany, "licitación" in Mexico); international ones name regions.
- Recency: add the after: date you are given to queries for things announced recently (tenders, calls, news, rule changes). For yearly programmes (grants, awards, trade shows), put the coming year in the query instead.
- Never search for the business itself: never include its name, website or email. Searching for a competitor by name is fine for market and partner queries.

Choose the mix by business model (you are told which it is):
- Sells to consumers, or runs a marketplace (B2C): partnerships and distribution (corporates with employee-benefit or wellness schemes, property managers and estate agents, insurers, retailers, platforms and marketplaces that onboard providers, published "become a partner/supplier" pages); market expansion (new cities and regions, adjacent services its competitors offer); funding (SME funds, development finance and accelerators in its country); visibility (industry awards with open entries, consumer shows, media features); openings that regulation creates; talent and skills programmes. Public tenders rarely fit a consumer business: use at most one sales query, for corporate or institutional buyers.
- Sells to businesses or government (B2B): tenders, RFPs, RFQs and procurement in the segments it serves, plus partner and channel programmes, then funding, regulation, market and visibility.

The plan as a whole:
- Spread the queries over the focus areas it wants, weighted by what matters most for its goals, with at least one query per wanted area. If none are given, follow the mix for its business model.
- Mix places by the markets it serves. local: mostly its city and province. national: its country and main cities. international: its home country plus the regions it sells to.
- Vary the angle so results overlap as little as possible.
- If it works in languages other than English, write the local queries in those languages, with lang:xx.
- Steering notes are direct instructions and override the profile when they conflict. Search for more like what they liked; avoid what they dismissed and anything they said to avoid.

Reply with json only, in exactly this shape:
{"queries": [{"q": "office cleaning tender Western Cape after:2026-07-29", "category": "sales", "about": "cleaning tenders in the Western Cape"}]}
- q: the query, at most 120 characters including operators.
- category: one focus area id from the list below.
- about: what it looks for, in 3 to 7 plain words (shown as "Searching: ...").

Focus areas (id: label (what it covers)):
"#;

/// The second-wave request: `n` broader queries from new angles, avoiding those already tried.
pub(crate) fn request_more(
    today: &str,
    profile: &Profile,
    intel: &Intel,
    steering: &Steering,
    tried: &[Query],
    n: usize,
) -> ChatRequest {
    let mut req = request(today, profile, intel, steering, n);
    req.stage = "plan_more".into();
    let tried_list: Vec<String> = tried.iter().map(|q| format!("- {}", q.q)).collect();
    req = req.text(format!(
        "The first round of searches found too little that fits. Write exactly {n} NEW, broader queries from different angles: plain words, at most one operator, no after: and no site:. Try partner, supplier and provider programme pages, companies expanding into its area, funds and accelerators, awards with open entries, and adjacent services. Don't repeat these:\n{}",
        tried_list.join("\n")
    ));
    req.temperature(0.9)
}

/// The plan request for `n` queries.
pub(crate) fn request(
    today: &str,
    profile: &Profile,
    intel: &Intel,
    steering: &Steering,
    n: usize,
) -> ChatRequest {
    let system = format!("{SYSTEM}{}", prompt::categories_block());
    let mut user = format!(
        "Today is {today}. For recent material use after:{after}. The coming year for programmes is {year}.\nWrite exactly {n} queries.\nBusiness model: {model}\n\nThe business:\n{profile}",
        after = after_date(today),
        year = coming_year(today),
        model = prompt::business_model(profile, intel).describe(),
        profile = prompt::profile_block(profile),
    );
    let dossier = prompt::intel_block(intel);
    if !dossier.is_empty() {
        user.push_str("\n\nWhat research established about it:\n");
        user.push_str(&dossier);
    }
    let steer = prompt::steering_block(steering);
    if !steer.is_empty() {
        user.push_str("\n\n");
        user.push_str(&steer);
    }
    ChatRequest::new("plan", system, user)
        .json()
        .thinking(Thinking::Off)
        .temperature(0.7)
        .max_tokens(3000)
}

/// Validated queries from the model's reply: known categories, sane operators, nothing about the
/// business itself, no duplicates, at most `n`.
pub(crate) fn parse(v: &Value, profile: &Profile, today: &str, n: usize) -> Vec<Query> {
    let mut out: Vec<Query> = Vec::new();
    for item in array(v, &["queries", "searches", "plan"]) {
        let (raw_q, raw_cat, raw_about) = match item {
            Value::String(s) => (s.clone(), String::new(), String::new()),
            Value::Object(_) => (
                text_any(item, &["q", "query", "search"]),
                text(item, "category").to_ascii_lowercase(),
                text(item, "about"),
            ),
            _ => continue,
        };
        let Some(q) = clean_query(&raw_q, today) else {
            continue;
        };
        if leaks_identity(&q, profile) {
            continue;
        }
        let category = if model::is_opportunity_type(&raw_cat) {
            raw_cat
        } else {
            infer_category(&format!("{q} {raw_about}"))
                .map(str::to_string)
                .unwrap_or_else(|| default_category(profile))
        };
        let about = clean_about(&raw_about, &q);
        push_unique(&mut out, Query { q, category, about });
        if out.len() >= n {
            break;
        }
    }
    out
}

fn push_unique(out: &mut Vec<Query>, q: Query) -> bool {
    let key = q.q.to_lowercase();
    if out.iter().any(|o| o.q.to_lowercase() == key) {
        return false;
    }
    out.push(q);
    true
}

/// A query with broken operators removed and its length capped; None when nothing searchable
/// is left.
pub(crate) fn clean_query(raw: &str, today: &str) -> Option<String> {
    let mut s = model::text::squash(raw);
    // Unbalanced quotes would turn the rest of the query into one phrase.
    if s.matches('"').count() % 2 == 1 {
        s = s.replace('"', "");
    }
    let today_days = model::date::parse_ymd(today);
    let mut words: Vec<&str> = Vec::new();
    let mut plain_words = 0;
    for w in s.split(' ') {
        let lower = w.to_ascii_lowercase();
        if let Some(d) = lower
            .strip_prefix("after:")
            .or_else(|| lower.strip_prefix("before:"))
        {
            let ok = match (model::date::parse_ymd(d), today_days) {
                (Some(day), Some(t)) => !lower.starts_with("after:") || day <= t,
                (Some(_), None) => true,
                _ => false,
            };
            if ok {
                words.push(w);
            }
            continue;
        }
        if let Some(code) = lower.strip_prefix("lang:") {
            if (2..=3).contains(&code.len()) && code.chars().all(|c| c.is_ascii_lowercase()) {
                words.push(w);
            }
            continue;
        }
        if lower.starts_with("site:") || lower.starts_with("-site:") {
            if lower.len() > 6 {
                words.push(w);
            }
            continue;
        }
        if w.trim_matches(|c: char| !c.is_alphanumeric()).is_empty() {
            continue;
        }
        if !w.starts_with('-') {
            plain_words += 1;
        }
        words.push(w);
    }
    if plain_words == 0 {
        return None;
    }
    deepseek::clamp_query(&words.join(" "))
}

/// The query without operators, for display.
pub(crate) fn plain_words(q: &str) -> String {
    let kept: Vec<&str> = q
        .split_whitespace()
        .filter(|w| {
            let l = w.to_ascii_lowercase();
            !(l.starts_with("after:")
                || l.starts_with("before:")
                || l.starts_with("lang:")
                || l.starts_with("site:")
                || l.starts_with("-site:")
                || l.starts_with('-'))
        })
        .collect();
    kept.join(" ").replace('"', "")
}

fn clean_about(raw: &str, q: &str) -> String {
    let a = model::text::squash(raw);
    let a = if a.is_empty() { plain_words(q) } else { a };
    // The model's casing is kept: lowercasing the first letter mangles names ("south African").
    model::text::truncate(a.trim_end_matches('.'), 70)
}

/// True when a query would search for the business itself (its name, website, email or phone).
pub(crate) fn leaks_identity(q: &str, profile: &Profile) -> bool {
    let lower = q.to_lowercase();
    if lower.contains('@') || looks_like_phone(&lower) {
        return true;
    }
    for link in profile
        .links
        .iter()
        .chain(std::iter::once(&profile.website))
    {
        if let Some(host) = model::links::host(link)
            && host.len() >= 4
            && lower.contains(&host)
        {
            return true;
        }
    }
    let org = profile.organization.trim().to_lowercase();
    org.chars().count() >= 3 && contains_phrase(&lower, &org)
}

/// Seven or more digits in a row, or nine or more split by single spaces or dashes
/// ("082 123 4567"). Years ("2027 2028") and prices stay below that.
fn looks_like_phone(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let (mut plain, mut grouped) = (0, 0);
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_digit() {
            plain += 1;
            grouped += 1;
        } else {
            plain = 0;
            let joins_digits = (c == ' ' || c == '-')
                && i > 0
                && chars[i - 1].is_ascii_digit()
                && chars.get(i + 1).is_some_and(char::is_ascii_digit);
            if !joins_digits {
                grouped = 0;
            }
        }
        if plain >= 7 || grouped >= 9 {
            return true;
        }
    }
    false
}

fn contains_phrase(hay: &str, phrase: &str) -> bool {
    hay.match_indices(phrase).any(|(i, _)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + phrase.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// The focus area a query or title most likely targets, from its words.
pub(crate) fn infer_category(text: &str) -> Option<&'static str> {
    const RULES: &[(&str, &[&str])] = &[
        (
            "sales",
            &[
                "tender",
                "rfp",
                "rfq",
                "procurement",
                "request for proposal",
                "request for quotation",
                "expression of interest",
                "bid ",
                "supplier registration",
                "vendor",
                "buyer",
                "contract",
            ],
        ),
        (
            "regulation",
            &[
                "regulation",
                "compliance",
                "directive",
                "legislation",
                "act ",
                "bill ",
                "standard",
                "deadline for compliance",
                "rule",
                "law",
            ],
        ),
        (
            "funding",
            &[
                "grant",
                "funding",
                "incentive",
                "subsidy",
                "loan",
                "financing",
                "tax credit",
                "rebate",
                "accelerator",
                "investor",
                "venture",
            ],
        ),
        (
            "visibility",
            &[
                "trade show",
                "expo",
                "exhibition",
                "conference",
                "summit",
                "award",
                "call for speakers",
                "fair",
                "festival",
            ],
        ),
        (
            "partners",
            &[
                "partner",
                "reseller",
                "channel",
                "distributor",
                "integration",
                "marketplace",
                "alliance",
                "joint venture",
            ],
        ),
        (
            "talent",
            &[
                "learnership",
                "apprentice",
                "internship",
                "graduate",
                "skills",
                "training",
                "hiring",
                "talent",
            ],
        ),
        (
            "operations",
            &[
                "supplier",
                "equipment",
                "cost saving",
                "energy efficiency",
                "logistics",
                "software for",
            ],
        ),
        (
            "market",
            &[
                "market",
                "expansion",
                "export",
                "competitor",
                "demand",
                "growth",
                "segment",
            ],
        ),
    ];
    let t = format!("{} ", text.to_lowercase());
    RULES
        .iter()
        .find(|(_, words)| words.iter().any(|w| t.contains(w)))
        .map(|(id, _)| *id)
}

fn default_category(profile: &Profile) -> String {
    profile
        .seeking
        .first()
        .cloned()
        .unwrap_or_else(|| "other".into())
}

/// The focus areas to cover when the business didn't choose.
pub(crate) fn default_kinds(stage: &str) -> &'static [&'static str] {
    match stage {
        "startup" | "growth" => &["sales", "funding", "partners", "visibility", "market"],
        "nonprofit" => &["funding", "partners", "visibility", "regulation"],
        "public" => &["partners", "funding", "regulation", "operations"],
        _ => &["sales", "funding", "regulation", "market", "visibility"],
    }
}

/// Words that make a search land on actual opportunities in a focus area.
fn kind_terms(kind: &str, year: i64) -> String {
    match kind {
        "sales" => "tender request for proposals".into(),
        "partners" => "partner programme resellers".into(),
        "funding" => format!("grant incentive applications open {year}"),
        "regulation" => "new regulation compliance deadline".into(),
        "market" => "market demand growth".into(),
        "visibility" => format!("trade show expo {year}"),
        "talent" => "skills development learnership programme".into(),
        "operations" => "supplier programme".into(),
        _ => "opportunities".into(),
    }
}

/// Recency suits announcements; yearly programmes carry the year in their terms instead.
fn kind_wants_after(kind: &str) -> bool {
    matches!(
        kind,
        "sales" | "regulation" | "market" | "partners" | "operations"
    )
}

/// The topics to search around, most specific first.
fn topics(profile: &Profile) -> Vec<String> {
    let mut t: Vec<String> = Vec::new();
    let candidates = std::iter::once(profile.industry.clone())
        .chain(profile.skills.iter().cloned())
        .chain(profile.interests.iter().cloned());
    for s in candidates {
        let s = model::text::squash(&s);
        if !s.is_empty() && s.chars().count() <= 40 && !t.iter().any(|x| x.eq_ignore_ascii_case(&s))
        {
            t.push(s);
        }
    }
    if t.is_empty() {
        const STOP: &[&str] = &[
            "a", "an", "the", "and", "or", "of", "in", "at", "for", "with", "to", "our", "we",
            "on", "from", "by",
        ];
        let words: Vec<&str> = profile
            .headline
            .split_whitespace()
            .filter(|w| !STOP.contains(&w.to_lowercase().as_str()))
            .take(3)
            .collect();
        if !words.is_empty() {
            t.push(words.join(" "));
        }
    }
    t
}

/// The business's city and country, from "City, Country" free text.
fn places(profile: &Profile) -> (String, String) {
    let parts: Vec<&str> = profile
        .location
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    match parts.as_slice() {
        [] => (String::new(), String::new()),
        [one] => (one.to_string(), one.to_string()),
        [first, .., last] => (first.to_string(), last.to_string()),
    }
}

/// A plan built without the model: focus areas × topics × places, with sensible operators.
pub(crate) fn fallback(profile: &Profile, today: &str, n: usize) -> Vec<Query> {
    let kinds: Vec<String> = if profile.seeking.is_empty() {
        default_kinds(&profile.stage)
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        profile.seeking.clone()
    };
    let topics = topics(profile);
    let (city, country) = places(profile);
    let reach = prompt::reach_id(profile);
    let year = coming_year(today);
    let after = after_date(today);
    let mut out: Vec<Query> = Vec::new();
    let mut i = 0usize;
    while out.len() < n && i < n * 6 {
        let k = i % kinds.len();
        let round = i / kinds.len();
        let kind = kinds[k].as_str();
        let topic = topics
            .get(round % topics.len().max(1))
            .cloned()
            .unwrap_or_default();
        // Where to look follows the markets served, rotating per round and kind.
        let place = match (reach, (round / topics.len().max(1) + k) % 3) {
            ("local", 0 | 1) | ("national", 0) => city.clone(),
            ("local", _) | ("national", 1) => country.clone(),
            ("national", _) => String::new(),
            (_, 0) => country.clone(),
            (_, 1) => "international".into(),
            _ => "Europe".into(),
        };
        let mut q = [
            topic.as_str(),
            kind_terms(kind, year).as_str(),
            place.as_str(),
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
        if kind_wants_after(kind) {
            q.push_str(&format!(" after:{after}"));
        }
        if let Some(q) = clean_query(&q, today)
            && !leaks_identity(&q, profile)
        {
            let noun = prompt::category_noun(kind);
            let about = match (topic.is_empty(), place.is_empty()) {
                (false, false) => format!("{topic} {noun} in {place}"),
                (false, true) => format!("{topic} {noun}"),
                (true, false) => format!("{noun} in {place}"),
                (true, true) => noun.to_string(),
            };
            push_unique(
                &mut out,
                Query {
                    q,
                    category: kind.to_string(),
                    about: model::text::truncate(&about, 70),
                },
            );
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn business() -> Profile {
        Profile {
            organization: "Khumalo Precision".into(),
            headline: "Steel fabrication and precision machining".into(),
            industry: "Steel fabrication".into(),
            stage: "established".into(),
            website: "https://www.khumaloprecision.co.za".into(),
            location: "Durban, South Africa".into(),
            reach: "national".into(),
            seeking: vec!["sales".into(), "funding".into(), "regulation".into()],
            skills: vec!["CNC machining".into()],
            ..Default::default()
        }
    }

    #[test]
    fn dates() {
        assert_eq!(after_date("2026-11-26"), "2026-07-29");
        assert_eq!(coming_year("2026-11-26"), 2027);
        assert_eq!(coming_year("2027-03-01"), 2027);
    }

    #[test]
    fn parse_validates_everything() {
        let v = json!({"queries": [
            {"q": "steel fabrication tender KwaZulu-Natal after:2026-07-29", "category": "sales", "about": "Steel tenders in KwaZulu-Natal"},
            {"q": "Steel fabrication tender KwaZulu-Natal after:2026-07-29", "category": "sales"},
            {"q": "manufacturing incentive South Africa 2027", "category": "grants"},
            {"q": "machining tender Durban after:2026-07-29 after:2027-12-01 after:garbage", "category": "sales"},
            {"q": "\"Khumalo Precision\" reviews", "category": "market"},
            {"q": "info@khumalo.co.za", "category": "sales"},
            {"q": "site:khumaloprecision.co.za", "category": "other"},
            {"q": "call 0821234567 for steel", "category": "sales"},
            {"q": "after:2026-01-01 lang:en", "category": "sales"},
            {"q": "x".repeat(300), "category": "visibility"},
            "metal fabrication trade show Johannesburg 2027",
            42
        ]});
        let qs = parse(&v, &business(), "2026-11-26", 20);
        let texts: Vec<&str> = qs.iter().map(|q| q.q.as_str()).collect();
        assert_eq!(qs[0].about, "Steel tenders in KwaZulu-Natal");
        assert_eq!(
            qs.iter()
                .filter(|q| q.q.to_lowercase().starts_with("steel fabrication tender"))
                .count(),
            1,
            "duplicates dropped"
        );
        assert_eq!(
            qs[1].category, "funding",
            "unknown category inferred from the words"
        );
        assert_eq!(
            qs[2].q, "machining tender Durban after:2026-07-29",
            "future and broken dates removed: {texts:?}"
        );
        assert!(
            texts.iter().all(|q| !q.contains("Khumalo")
                && !q.contains('@')
                && !q.contains("khumaloprecision")
                && !q.contains("0821234567")),
            "{texts:?}"
        );
        assert!(texts.iter().all(|q| q.chars().count() <= 128));
        assert!(texts.contains(&"metal fabrication trade show Johannesburg 2027"));
        assert_eq!(qs.last().unwrap().category, "visibility");
        assert_eq!(parse(&v, &business(), "2026-11-26", 2).len(), 2, "capped");
        assert!(parse(&json!({"nope": 1}), &business(), "2026-11-26", 5).is_empty());
    }

    #[test]
    fn identity_and_phones() {
        let p = business();
        assert!(leaks_identity("khumalo precision news", &p));
        assert!(!leaks_identity("precision machining tender", &p));
        assert!(leaks_identity("catering 082 123 4567", &p));
        assert!(!leaks_identity("scholarships 2027 2028", &p));
        assert!(!leaks_identity("prize R100 000 for fabricators", &p));
    }

    #[test]
    fn fallback_is_diverse_and_clean() {
        let qs = fallback(&business(), "2026-11-26", 14);
        assert_eq!(qs.len(), 14);
        let cats: std::collections::HashSet<&str> =
            qs.iter().map(|q| q.category.as_str()).collect();
        assert_eq!(cats.len(), 3, "every wanted focus area is covered");
        assert!(qs.iter().any(|q| q.q.contains("after:2026-07-29")));
        assert!(qs.iter().any(|q| q.q.contains("2027")));
        assert!(
            qs.iter()
                .any(|q| q.q.contains("South Africa") || q.q.contains("Durban"))
        );
        assert!(qs.iter().all(|q| !q.q.contains("Khumalo")
            && !q.about.is_empty()
            && q.q.chars().count() <= 128));
        let thin = Profile {
            headline: "Family bakery".into(),
            stage: "established".into(),
            ..Default::default()
        };
        let qs = fallback(&thin, "2026-11-26", 5);
        assert_eq!(qs.len(), 5);
        assert!(qs[0].q.contains("Family bakery"));
    }

    #[test]
    fn categories_inferred() {
        assert_eq!(
            infer_category("office cleaning tender Western Cape"),
            Some("sales")
        );
        assert_eq!(
            infer_category("manufacturing incentive grant"),
            Some("funding")
        );
        assert_eq!(
            infer_category("food safety regulation 2027"),
            Some("regulation")
        );
        assert_eq!(
            infer_category("packaging trade show 2027"),
            Some("visibility")
        );
        assert_eq!(infer_category("something odd"), None);
    }

    #[test]
    fn request_has_today_json_intel_and_counts() {
        let mut intel = Intel::default();
        intel.overview.summary = "Fabricates steel for mines and ports.".into();
        let r = request(
            "2026-11-26",
            &business(),
            &intel,
            &Steering {
                notes: vec!["more port work".into()],
                ..Default::default()
            },
            14,
        );
        assert_eq!(r.stage, "plan");
        assert!(r.json && r.system.contains("json"));
        assert_eq!(r.thinking, Thinking::Off);
        let user = match &r.user[0] {
            deepseek::Part::Text(t) => t.clone(),
            _ => String::new(),
        };
        assert!(user.contains("Today is 2026-11-26"));
        assert!(user.contains("after:2026-07-29"));
        assert!(user.contains("Write exactly 14 queries."));
        assert!(user.contains("Overview: Fabricates steel"));
        assert!(user.contains("more port work"));
        assert!(!user.contains("khumaloprecision"));
        assert!(r.system.contains("- regulation: Regulation & compliance"));
    }
}
