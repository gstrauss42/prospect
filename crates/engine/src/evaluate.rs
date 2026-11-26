//! Step 4 of discovery: the model reads the numbered candidates and picks the genuine, open,
//! fitting opportunities, writing each card for the person.
//!
//! Picks refer to candidates **by number**, so every URL shown is one the search engine actually
//! returned; the model never writes a URL. Big shortlists are reviewed in interleaved batches in
//! parallel (faster, and each batch sees a mix of strong and weaker leads). Everything returned is
//! validated: numbers in range and in the batch, known categories, real dates, no expired
//! deadlines, sane lengths, no duplicates.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use deepseek::{ChatRequest, Thinking};
use model::{KnownOpportunity, Opportunity, Profile, Steering};
use serde_json::Value;

use crate::shortlist::Candidate;
use crate::util::{
    Reporter, array, clip, excerpt, list, number, parse_number, text, text_any, title_key,
    with_heartbeat,
};
use crate::{Ctx, Error, prompt};

/// Candidates per review call. Smaller batches come back sooner; all batches run in parallel.
pub(crate) const BATCH: usize = 80;

/// Picks below this score aren't worth showing.
pub(crate) const MIN_SCORE: u8 = 45;

/// Characters of each candidate's search summary shown to the model.
const SUMMARY_CHARS: usize = 650;

/// Words near which closing dates and status usually appear.
pub(crate) const DATE_WORDS: &[&str] = &[
    "deadline",
    "closing date",
    "closes",
    "close on",
    "apply by",
    "applications close",
    "applications open",
    "due date",
    "submission",
    "until",
    "expires",
    "last date",
    "date limite",
    "bewerbungsfrist",
    "frist",
    "fecha límite",
    "prazo",
];

/// How sure the review was that it's open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Open {
    Yes,
    Likely,
    Unknown,
}

/// One chosen opportunity and the candidate it came from.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pick {
    pub cand: usize,
    pub opp: Opportunity,
    pub open: Open,
}

#[derive(Debug, Default)]
pub(crate) struct Evaluation {
    pub picks: Vec<Pick>,
    /// Keys of known active items that no longer fit.
    pub stale: Vec<String>,
    pub failed_batches: usize,
}

const SYSTEM: &str = r#"You are a sharp business-development analyst. You read numbered web search results and choose the genuine, current, actionable opportunities for one business, then write each one up for its team. Reply in json.

Keep a candidate only if ALL of these hold:
1. It is a specific opportunity the business can act on: a tender, RFP, RFQ or supplier call; a buyer's brief or a buying signal from a likely customer (an expansion, a new site, a funded project) in a segment it serves; a grant, incentive, rebate or public programme it could apply to; a new rule or deadline that creates demand for what it sells or requires it to act; a market opening against its competitors or in a new region; a partner, reseller, integration or marketplace programme; a trade show, expo or awards call with applications open; a talent or skills programme for its industry; a supplier or cost-saving programme. A programme's official page counts when its next round is open or announced.
Genuine informal openings count too, when the page itself lets the business act and the next step is concrete: a partner, affiliate or provider programme page; a published "become a supplier", "become a partner" or "list your services" page; a company announcing an expansion (new estates, offices, sites, cities) that creates a need for what the business sells; an employee-benefit, wellness or insurance scheme that takes on service providers; an award with open entries; a media or speaking opportunity inviting submissions. For a consumer-facing business these are often its best opportunities.
2. It is current: open now, or opening with dates still ahead. A deadline or event date before today means it is over. Reject anything that says applications are closed, a past edition, or an old page with nothing suggesting a current round.
3. It fits THIS business: its industry, offerings, segments, size, location and the markets it serves, as the profile and the research dossier describe them; a tender it could realistically win, an incentive a business like it qualifies for. Respect eligibility (company type, size, registration, location; registering on a supplier database is a normal step, not a barrier) and the markets it serves. Never pick something it said to avoid.
4. It is legitimate: no pay-to-play schemes, MLM, scams or content farms.

Reject: news and articles that don't let the business act, advice and listicles, directories or job boards with no specific opportunity (keep a curated, current list only when it is squarely on target and nothing better covers it), pages behind a login, and duplicates: when two candidates are the same opportunity (the same tender on several tender sites), keep the most official page. Something already on its page may be picked again only with the same URL; never pick a copy of it from another site.
The team's own tools may show things they are already working on ("Already in the team's own tools"). If a candidate is clearly the same opportunity, leave it out, unless the page adds something they need (a deadline, the documents); then keep it and start its why with "Already on your radar: ".

For each pick write, in {language}:
- n: the candidate's number, exactly as listed.
- title: specific and scannable, at most 90 characters: the opportunity's real name and its hook ("Transnet RFQ: S07 steel structure for Richards Bay port terminal", "Cape Town SMME Growth Fund: up to R500 000 for equipment"). No site names, no clickbait.
- organization: who runs, issues or offers it.
- category: one focus area id from the list below, by what the opportunity is.
- summary: one or two plain sentences: what it is, who it is for, what is on offer or required.
- why: one sentence to the team on why it matters for THIS business, naming what in its profile or dossier it connects to (an offering, a segment, a competitor, a market, a rule): "Your conveyor maintenance work matches the scope, and Transnet is already in the port segment you serve." Vary the openings; never just restate the title.
- action: the concrete next step with its date when known ("Download the RFQ pack on eTenders and submit by 9 December 2026", "Register as an exhibitor before 31 January 2027").
- location: where it happens or who may apply ("Richards Bay, South Africa", "EU-wide", "Online").
- deadline: YYYY-MM-DD when the closing date is stated (for an event without one, the event date); "" when unknown. Never guess.
- value: money or other value when stated ("R2.5m contract", "Up to €100k grant", "Free exhibitor stand"); "" when unknown.
- open: "yes" if the result says it is open now or gives a future deadline; "likely" if it recurs and the next round is expected but not confirmed.
- score: 0 to 100, how worthwhile it is for this business right now (fit, value and certainty together).

Scoring: use the whole range and be honest.
- 90-100: rare. An outstanding, clearly open match for its core business, high value.
- 75-89: a strong match the team should act on soon.
- 60-74: a good option with a partial fit, smaller value or less certainty.
- 45-59: worth a look but a stretch.
- Below 45: don't pick it.
Usually only one or two picks earn 90 or more, and most land between 55 and 85. Things like those they liked score higher; things like those they dismissed score lower or are left out. Follow their steering notes.

Variety: cover the focus areas it wants. Don't fill the list with near-identical items; keep the best few of a kind. Return fewer picks rather than weak ones.

stale: the K-numbers of items marked [active] under "Already on their page" that clearly no longer fit the business, or that the results show have closed. Be conservative: when unsure, leave it out. Never list [saved] items.

Reply with json only, in exactly this shape, picks best first:
{"picks": [{"n": 3, "title": "...", "organization": "...", "category": "sales", "summary": "...", "why": "...", "action": "...", "location": "...", "deadline": "2027-01-15", "value": "R2.5m contract", "open": "yes", "score": 84}], "stale": ["K2"]}

Focus areas (id: label (what it covers)):
"#;

/// Everything about the business the review needs.
#[derive(Clone, Copy)]
pub(crate) struct Scope<'a> {
    pub profile: &'a Profile,
    pub intel: &'a model::Intel,
    pub steering: &'a Steering,
    pub known: &'a [KnownOpportunity],
    pub context: &'a [model::ContextItem],
}

/// The "Already on their page" lines, numbered K1…
fn known_block(known: &[KnownOpportunity]) -> String {
    if known.is_empty() {
        return "Already on their page: nothing yet.".into();
    }
    let mut out = vec!["Already on their page:".to_string()];
    for (i, k) in known.iter().enumerate() {
        let status = if k.status == "saved" {
            "saved"
        } else {
            "active"
        };
        let mut line = format!(
            "K{} [{status}] {}",
            i + 1,
            model::text::truncate(&k.title, 110)
        );
        let mut extra: Vec<String> = Vec::new();
        if model::is_opportunity_type(&k.category) {
            extra.push(k.category.clone());
        }
        if let Some(d) = k.deadline.as_deref().filter(|d| !d.is_empty()) {
            extra.push(format!("closes {d}"));
        }
        if !extra.is_empty() {
            line.push_str(&format!(" ({})", extra.join(", ")));
        }
        line.push_str(&format!(" - {}", model::text::truncate(&k.key, 90)));
        out.push(line);
    }
    out.join("\n")
}

/// One candidate as the model sees it.
pub(crate) fn candidate_block(n: usize, c: &Candidate) -> String {
    let mut head = format!(
        "#{n} [{}] {} | {} ({})",
        c.category, c.title, c.site_name, c.host
    );
    if let Some(d) = &c.date {
        head.push_str(&format!(" | {d}"));
    }
    if let Some(k) = c.known {
        head.push_str(&format!(" | already on their page as K{}", k + 1));
    }
    let summary = excerpt(&model::text::squash(&c.summary), SUMMARY_CHARS, DATE_WORDS);
    format!("{head}\n{}\n{summary}", c.url)
}

/// The review request for one batch (`members` are candidate indices).
pub(crate) fn request(
    today: &str,
    scope: &Scope,
    cands: &[Candidate],
    members: &[usize],
    max_picks: usize,
) -> ChatRequest {
    let Scope {
        profile,
        intel,
        steering,
        known,
        context,
    } = *scope;
    let system = format!(
        "{}{}",
        SYSTEM.replace("{language}", &prompt::output_language(profile)),
        prompt::categories_block()
    );
    let mut user = format!(
        "Today is {today}.\nPick at most {max_picks} opportunities from the {} candidates below.\n\nThe business:\n{}",
        members.len(),
        prompt::profile_block(profile)
    );
    let dossier = prompt::intel_block(intel);
    if !dossier.is_empty() {
        user.push_str("\n\nWhat research established about it:\n");
        user.push_str(&dossier);
    }
    let radar = prompt::radar_block(context);
    if !radar.is_empty() {
        user.push_str("\n\n");
        user.push_str(&radar);
    }
    let steer = prompt::steering_block(steering);
    if !steer.is_empty() {
        user.push_str("\n\n");
        user.push_str(&steer);
    }
    user.push_str("\n\n");
    user.push_str(&known_block(known));
    user.push_str("\n\nCandidates:\n");
    for &i in members {
        user.push('\n');
        user.push_str(&candidate_block(i + 1, &cands[i]));
        user.push('\n');
    }
    ChatRequest::new("evaluate", system, user)
        .json()
        .thinking(Thinking::Low)
        .max_tokens(16_000)
}

/// Validated picks and stale keys from one batch's reply.
pub(crate) fn parse(
    v: &Value,
    cands: &[Candidate],
    members: &[usize],
    known: &[KnownOpportunity],
    today: &str,
) -> (Vec<Pick>, Vec<String>) {
    let allowed: HashSet<usize> = members.iter().copied().collect();
    let mut seen: HashSet<usize> = HashSet::new();
    let mut picks: Vec<Pick> = Vec::new();
    for item in array(v, &["picks", "opportunities", "results"]) {
        let Some(n) = ["n", "number", "candidate", "id"]
            .iter()
            .find_map(|k| number(item, k))
        else {
            continue;
        };
        if n < 1.0 || n.fract() != 0.0 {
            continue;
        }
        let idx = n as usize - 1;
        if !allowed.contains(&idx) || !seen.insert(idx) {
            continue;
        }
        if let Some(p) = pick_from(item, idx, &cands[idx], today) {
            picks.push(p);
        }
    }
    let mut stale: Vec<String> = Vec::new();
    for s in list(v, "stale") {
        let Some(k) = parse_number(&s) else { continue };
        if k < 1.0 || k.fract() != 0.0 {
            continue;
        }
        if let Some(item) = known.get(k as usize - 1)
            && item.status == "active"
            && !item.key.is_empty()
            && !stale.contains(&item.key)
        {
            stale.push(item.key.clone());
        }
    }
    (picks, stale)
}

fn pick_from(item: &Value, idx: usize, c: &Candidate, today: &str) -> Option<Pick> {
    let score = number(item, "score")
        .map(|s| s.round().clamp(0.0, 100.0) as u8)
        .unwrap_or(50);
    if score < MIN_SCORE {
        return None;
    }
    let category = {
        let raw = text(item, "category").to_ascii_lowercase();
        if model::is_opportunity_type(&raw) {
            raw
        } else {
            c.category.clone()
        }
    };
    let deadline_raw = text(item, "deadline");
    let deadline = model::date::ymd_prefix(&deadline_raw);
    if let (Some(d), Some(t)) = (
        deadline.as_deref().and_then(model::date::parse_ymd),
        model::date::parse_ymd(today),
    ) && d < t
    {
        return None;
    }
    let open = match text(item, "open").to_ascii_lowercase().as_str() {
        "yes" | "true" | "open" => Open::Yes,
        "likely" | "probably" | "expected" => Open::Likely,
        _ => Open::Unknown,
    };
    let title = Some(clip(&text(item, "title"), 140))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| clip(&c.title, 140));
    let summary = Some(clip(&text(item, "summary"), 420))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| clip(&c.summary, 240));
    let action = Some(clip(&text(item, "action"), 200))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Open the page for the details and how to apply".into());
    let organization = Some(clip(
        &text_any(item, &["organization", "organisation", "org"]),
        120,
    ))
    .filter(|t| !t.is_empty())
    .unwrap_or_else(|| c.site_name.clone());
    let opp = Opportunity {
        url: c.url.clone(),
        title,
        organization,
        category,
        summary,
        why: clip(&text(item, "why"), 300),
        action,
        location: clip(&text(item, "location"), 100),
        deadline,
        value: clip(&text(item, "value"), 80),
        score,
        published: c.date.clone(),
        site_name: c.site_name.clone(),
        site_icon: c.site_icon.clone(),
        verified: false,
        source: String::new(),
    }
    .sanitized(today)?;
    Some(Pick {
        cand: idx,
        opp,
        open,
    })
}

/// Merge batches: one pick per opportunity (same title means same opportunity; the higher
/// score wins), best first, at most `max`.
pub(crate) fn merge(mut picks: Vec<Pick>, max: usize) -> Vec<Pick> {
    picks.sort_by(|a, b| b.opp.score.cmp(&a.opp.score).then(a.cand.cmp(&b.cand)));
    let mut titles: HashSet<String> = HashSet::new();
    let mut cands: HashSet<usize> = HashSet::new();
    picks.retain(|p| cands.insert(p.cand) && titles.insert(title_key(&p.opp.title)));
    picks.truncate(max);
    picks
}

/// Interleaved batches over `n` candidates: batch b holds b, b+k, b+2k…
pub(crate) fn batches(n: usize, size: usize) -> Vec<Vec<usize>> {
    if n == 0 {
        return Vec::new();
    }
    let k = n.div_ceil(size.max(1));
    (0..k).map(|b| (b..n).step_by(k).collect()).collect()
}

/// Review every candidate and return the validated, merged picks.
pub(crate) fn evaluate(
    ctx: &Ctx,
    rep: &Reporter,
    scope: &Scope,
    cands: &[Candidate],
    members: &[usize],
    max_picks: usize,
) -> Result<Evaluation, Error> {
    if members.is_empty() {
        return Ok(Evaluation::default());
    }
    let known = scope.known;
    let groups: Vec<Vec<usize>> = batches(members.len(), BATCH)
        .into_iter()
        .map(|g| g.into_iter().map(|i| members[i]).collect())
        .collect();
    let per_batch = |len: usize| (max_picks * len).div_ceil(members.len().max(1)) + 3;
    let abort = AtomicBool::new(false);
    let messages = [
        "Checking each lead's dates against today",
        "Weighing how well each one fits the business",
        "Setting aside expired calls, listicles and duplicates",
        "Writing why each one matters for the business",
    ];
    let replies = with_heartbeat(rep, "reviewing", 72, &messages, || {
        crate::util::parallel_map(
            &groups,
            groups.len(),
            &AtomicBool::new(false),
            |_, members| {
                if abort.load(Ordering::Relaxed) || ctx.cancel.load(Ordering::Relaxed) {
                    return Err(None);
                }
                let req = request(&ctx.today, scope, cands, members, per_batch(members.len()));
                ctx.ai.chat_json_value(&req).map_err(|e| {
                    if e.is_account_problem() {
                        abort.store(true, Ordering::Relaxed);
                    }
                    Some(e)
                })
            },
        )
    });
    ctx.check_cancel()?;
    let mut out = Evaluation::default();
    let mut first_error: Option<deepseek::Error> = None;
    let mut all: Vec<Pick> = Vec::new();
    for (members, reply) in groups.iter().zip(replies) {
        match reply {
            Some(Ok(v)) => {
                let (picks, stale) = parse(&v, cands, members, known, &ctx.today);
                all.extend(picks);
                for s in stale {
                    if !out.stale.contains(&s) {
                        out.stale.push(s);
                    }
                }
            }
            Some(Err(Some(e))) => {
                if e.is_account_problem() {
                    return Err(Error::Ai(e));
                }
                out.failed_batches += 1;
                first_error.get_or_insert(e);
            }
            _ => out.failed_batches += 1,
        }
    }
    if out.failed_batches == groups.len() {
        return Err(Error::Ai(first_error.unwrap_or_else(|| {
            deepseek::Error::InvalidResponse("the review step returned nothing".into())
        })));
    }
    out.picks = merge(all, max_picks);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn cand(i: usize, cat: &str) -> Candidate {
        Candidate {
            key: format!("site{i}.org/opp"),
            url: format!("https://site{i}.org/opp"),
            title: format!("Opportunity {i}"),
            site_name: format!("Site {i}"),
            site_icon: String::new(),
            host: format!("site{i}.org"),
            date: Some("2026-11-01".into()),
            summary: format!("Applications for opportunity {i} close on 31 January 2027."),
            rerank: 2.0,
            authority: 2,
            hits: 1,
            query: 0,
            category: cat.into(),
            known: None,
            score: 2.0,
        }
    }

    #[test]
    fn batches_interleave() {
        assert_eq!(batches(5, 35), vec![vec![0, 1, 2, 3, 4]]);
        assert_eq!(batches(70, 35).len(), 2);
        assert_eq!(batches(7, 3), vec![vec![0, 3, 6], vec![1, 4], vec![2, 5]]);
        assert!(batches(0, 35).is_empty());
    }

    #[test]
    fn parse_validates_picks_and_stale() {
        let cands: Vec<Candidate> = (0..6).map(|i| cand(i, "funding")).collect();
        let members = vec![0, 2, 4];
        let known = vec![
            KnownOpportunity {
                key: "old.org/a".into(),
                title: "Old A".into(),
                status: "active".into(),
                ..Default::default()
            },
            KnownOpportunity {
                key: "saved.org/b".into(),
                title: "Saved B".into(),
                status: "saved".into(),
                ..Default::default()
            },
        ];
        let v = json!({
            "picks": [
                {"n": 1, "title": "  Great grant  ", "category": "FUNDING", "summary": "S", "why": "Your menus fit.", "action": "Apply by 31 January 2027", "location": "Cape Town", "deadline": "2027-01-31", "value": "R35 000", "open": "yes", "score": 88},
                {"n": "#3", "title": "Contest", "category": "contests", "deadline": "unknown", "value": "Not stated", "score": "71"},
                {"n": 1, "title": "Duplicate number", "score": 99},
                {"n": 2, "title": "Not in this batch", "score": 80},
                {"n": 99, "title": "Out of range", "score": 80},
                {"n": 0, "title": "Zero", "score": 80},
                {"n": 5, "title": "Expired", "deadline": "2026-10-01", "score": 80},
                {"title": "No number", "score": 80},
                "junk"
            ],
            "stale": ["K1", "K2", "K9", 1, "nonsense"]
        });
        let (picks, stale) = parse(&v, &cands, &members, &known, "2026-11-26");
        assert_eq!(picks.len(), 2, "{picks:?}");
        let a = &picks[0];
        assert_eq!(
            (a.cand, a.opp.title.as_str(), a.opp.category.as_str()),
            (0, "Great grant", "funding")
        );
        assert_eq!(
            a.opp.url, "https://site0.org/opp",
            "the URL always comes from the candidate"
        );
        assert_eq!(a.opp.deadline.as_deref(), Some("2027-01-31"));
        assert_eq!(a.opp.published.as_deref(), Some("2026-11-01"));
        assert_eq!(a.open, Open::Yes);
        let b = &picks[1];
        assert_eq!(b.cand, 2);
        assert_eq!(
            b.opp.category, "funding",
            "unknown category falls back to the query's"
        );
        assert_eq!(b.opp.deadline, None);
        assert_eq!(b.opp.value, "", "placeholders are blanked");
        assert_eq!(b.opp.score, 71);
        assert_eq!(b.open, Open::Unknown);
        assert_eq!(
            b.opp.organization, "Site 2",
            "organisation falls back to the site"
        );
        assert!(!b.opp.action.is_empty());
        assert_eq!(stale, vec!["old.org/a"], "only active known items, once");
    }

    #[test]
    fn low_scores_are_dropped_and_merge_dedupes() {
        let cands: Vec<Candidate> = (0..4).map(|i| cand(i, "sales")).collect();
        let v = json!({"picks": [
            {"n": 1, "title": "Role A", "score": 40},
            {"n": 2, "title": "Role B", "score": 60},
            {"n": 3, "title": "Role B!", "score": 75},
            {"n": 4, "title": "Role C", "score": 101}
        ]});
        let (picks, _) = parse(&v, &cands, &[0, 1, 2, 3], &[], "2026-11-26");
        assert_eq!(picks.len(), 3);
        let merged = merge(picks, 10);
        let titles: Vec<&str> = merged.iter().map(|p| p.opp.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["Role C", "Role B!"],
            "same title once, higher score wins, best first"
        );
        assert_eq!(merged[0].opp.score, 100);
        assert_eq!(merge(merged, 1).len(), 1);
    }

    #[test]
    fn request_lists_candidates_by_number_with_known_items() {
        let mut cands: Vec<Candidate> = (0..3).map(|i| cand(i, "visibility")).collect();
        cands[2].known = Some(0);
        let known = vec![KnownOpportunity {
            key: "site2.org/opp".into(),
            title: "Opportunity 2".into(),
            status: "saved".into(),
            category: "visibility".into(),
            deadline: Some("2027-02-01".into()),
        }];
        let (profile, intel, steering) = (
            Profile {
                headline: "Cleaning".into(),
                ..Default::default()
            },
            model::Intel::default(),
            Steering::default(),
        );
        let scope = Scope {
            profile: &profile,
            intel: &intel,
            steering: &steering,
            known: &known,
            context: &[],
        };
        let r = request("2026-11-26", &scope, &cands, &[0, 2], 5);
        let user = match &r.user[0] {
            deepseek::Part::Text(t) => t.clone(),
            _ => String::new(),
        };
        assert_eq!(r.stage, "evaluate");
        assert!(r.json && r.system.contains("json"));
        assert!(user.contains("Today is 2026-11-26"));
        assert!(user.contains("Pick at most 5 opportunities from the 2 candidates below."));
        assert!(user.contains("#1 [visibility] Opportunity 0 | Site 0 (site0.org) | 2026-11-01\nhttps://site0.org/opp\n"));
        assert!(user.contains("#3 [visibility] Opportunity 2"));
        assert!(user.contains("already on their page as K1"));
        assert!(!user.contains("#2 "), "only the batch's members are listed");
        assert!(
            user.contains(
                "K1 [saved] Opportunity 2 (visibility, closes 2027-02-01) - site2.org/opp"
            )
        );
        assert!(r.system.contains("in English"));
    }
}
