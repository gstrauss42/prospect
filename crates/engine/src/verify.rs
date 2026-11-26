//! Step 5 of discovery: read the top picks' pages with the Fetch API and confirm each is real,
//! open as of today and as described; correct the deadline, value and details from the page.
//!
//! Each page is checked in its own small call (fetch, then check, on one thread; up to six at
//! once), so one slow or odd page never holds up or spoils the others. Pages that can't be read
//! keep their pick, unverified. Closed or wrong ones are dropped.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use deepseek::{ChatRequest, Thinking};
use model::{Opportunity, Profile};
use serde_json::Value;

use crate::evaluate::{DATE_WORDS, Pick};
use crate::util::{Reporter, clip, excerpt, parallel_map, short_date, text};
use crate::{Ctx, DiscoverStats, Error};

/// Characters of each page given to the model.
const PAGE_CHARS: usize = 9000;

/// Fewer readable characters than this is a cookie wall or an empty shell, not a page.
const MIN_PAGE_CHARS: usize = 250;

/// Pages read at once.
const PARALLEL: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Open,
    Closed,
    Different,
    Unclear,
}

/// What the page said.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Check {
    pub verdict: Verdict,
    pub deadline: Option<String>,
    pub value: String,
    pub location: String,
    pub title: String,
    pub summary: String,
    pub action: String,
}

#[derive(Debug)]
enum Outcome {
    Checked(Check),
    Unreadable,
    Skipped,
    Account(deepseek::Error),
}

const SYSTEM: &str = r#"You check one opportunity against the text of its web page, for someone who wants to act on it. Reply in json.

Decide the verdict:
- "open": the page is this opportunity (or its official page) and it is open now, or opens later with its dates still ahead.
- "closed": its deadline or event date has passed, the page says applications are closed or the position is filled, or it is a past edition with no current round.
- "different": the page is not an actionable opportunity (news, advice, a directory with nothing specific) or is about something else entirely.
- "unclear": the text doesn't show enough to tell (a cookie wall, a login page, a cut-off page) and nothing suggests it has closed.
Compare every date with today's date. Recurring programmes whose current round isn't announced yet are "closed" unless the page gives dates ahead.

Then correct the details from the page. Write "" to keep what we already have, and never invent:
- deadline: the closing date for applications as YYYY-MM-DD (for an event without one, the event date), only when the page states it.
- value: money or other value stated on the page ("R35 000 bursary", "$10,000 prize", "€70–85k", "Fully funded", "Free").
- location: where it happens or who may apply ("Cape Town, South Africa", "Remote (EU)", "Online").
- title: a better title when the page gives specifics: at most 90 characters, the opportunity's real name and its key hook.
- summary: one or two sentences, only if ours is wrong or vague.
- action: the concrete next step with its date ("Apply on the portal by 15 January 2027").

Reply with json only, in exactly this shape, evidence first:
{"evidence": "the words on the page about dates or status, at most 25 words", "verdict": "open", "deadline": "", "value": "", "location": "", "title": "", "summary": "", "action": ""}"#;

/// The check request for one pick and its page text.
pub(crate) fn request(
    today: &str,
    profile: &Profile,
    opp: &Opportunity,
    page_title: &str,
    page_text: &str,
) -> ChatRequest {
    let who = [profile.headline.trim(), profile.location.trim()]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let user = format!(
        "Today is {today}.{who_line}\n\nThe opportunity as we described it:\nTitle: {title}\nOrganisation: {org}\nCategory: {cat}\nDeadline: {deadline}\nValue: {value}\nLocation: {location}\nSummary: {summary}\n\nIts page, {url}, titled \"{page_title}\":\n\"\"\"\n{page_text}\n\"\"\"",
        who_line = if who.is_empty() {
            String::new()
        } else {
            format!(" It's for: {who}.")
        },
        title = opp.title,
        org = opp.organization,
        cat = model::opportunity_type_label(&opp.category),
        deadline = opp.deadline.as_deref().unwrap_or("unknown"),
        value = if opp.value.is_empty() {
            "unknown"
        } else {
            &opp.value
        },
        location = if opp.location.is_empty() {
            "unknown"
        } else {
            &opp.location
        },
        summary = opp.summary,
        url = opp.url,
        page_title = model::text::truncate(&model::text::squash(page_title), 160),
    );
    ChatRequest::new("verify", SYSTEM, user)
        .json()
        .thinking(Thinking::Off)
        .temperature(0.1)
        .max_tokens(1200)
}

/// The check from the model's reply. A deadline before today closes it whatever the verdict.
pub(crate) fn parse(v: &Value, today: &str) -> Check {
    let mut verdict = match text(v, "verdict").to_ascii_lowercase().as_str() {
        "open" | "opens" | "upcoming" => Verdict::Open,
        "closed" | "expired" | "past" | "filled" => Verdict::Closed,
        "different" | "not_an_opportunity" | "not an opportunity" | "wrong" | "irrelevant" => {
            Verdict::Different
        }
        _ => Verdict::Unclear,
    };
    let deadline = model::date::ymd_prefix(&text(v, "deadline"));
    if let (Some(d), Some(t)) = (
        deadline.as_deref().and_then(model::date::parse_ymd),
        model::date::parse_ymd(today),
    ) && d < t
    {
        verdict = Verdict::Closed;
    }
    Check {
        verdict,
        deadline,
        value: clip(&text(v, "value"), 80),
        location: clip(&text(v, "location"), 100),
        title: clip(&text(v, "title"), 140),
        summary: clip(&text(v, "summary"), 420),
        action: clip(&text(v, "action"), 200),
    }
}

/// Apply the page's corrections to an open pick.
pub(crate) fn apply(opp: &mut Opportunity, check: &Check) {
    if check.deadline.is_some() {
        opp.deadline = check.deadline.clone();
    }
    for (field, new) in [
        (&mut opp.value, &check.value),
        (&mut opp.location, &check.location),
        (&mut opp.title, &check.title),
        (&mut opp.summary, &check.summary),
        (&mut opp.action, &check.action),
    ] {
        if !new.is_empty() {
            *field = new.clone();
        }
    }
}

fn check_one(ctx: &Ctx, profile: &Profile, opp: &Opportunity) -> Outcome {
    if ctx.cancel.load(Ordering::Relaxed) {
        return Outcome::Skipped;
    }
    let page = match ctx.ai.fetch(&opp.url) {
        Ok(p) => p,
        Err(e) if e.is_account_problem() => return Outcome::Account(e),
        Err(_) => return Outcome::Unreadable,
    };
    let body = excerpt(&page.content, PAGE_CHARS, DATE_WORDS);
    if body.chars().count() < MIN_PAGE_CHARS {
        return Outcome::Unreadable;
    }
    if ctx.cancel.load(Ordering::Relaxed) {
        return Outcome::Skipped;
    }
    match ctx
        .ai
        .chat_json_value(&request(&ctx.today, profile, opp, &page.title, &body))
    {
        Ok(v) => Outcome::Checked(parse(&v, &ctx.today)),
        Err(e) if e.is_account_problem() => Outcome::Account(e),
        Err(_) => Outcome::Unreadable,
    }
}

fn short_title(t: &str) -> String {
    model::text::truncate(t, 56)
}

/// Verify the `top` best picks (they must be sorted best first). Closed or wrong ones are removed;
/// open ones are corrected and marked verified; unreadable ones stay unverified.
pub(crate) fn verify(
    ctx: &Ctx,
    rep: &Reporter,
    profile: &Profile,
    picks: &mut Vec<Pick>,
    top: usize,
    stats: &mut DiscoverStats,
) -> Result<(), Error> {
    let n = top.min(picks.len());
    if n == 0 {
        return Ok(());
    }
    let (from, to) = (rep.percent().clamp(76, 90), 95u8);
    rep.report(
        "verifying",
        format!("Reading {n} top picks to check they're still open"),
        from,
    );
    let abort = AtomicBool::new(false);
    let done = AtomicUsize::new(0);
    let targets: Vec<Opportunity> = picks[..n].iter().map(|p| p.opp.clone()).collect();
    let outcomes = parallel_map(&targets, PARALLEL, &abort, |_, opp| {
        let out = check_one(ctx, profile, opp);
        if matches!(out, Outcome::Account(_)) {
            abort.store(true, Ordering::Relaxed);
        }
        let k = done.fetch_add(1, Ordering::Relaxed) + 1;
        let pct = from + ((to - from) as usize * k / n) as u8;
        let t = short_title(&opp.title);
        let msg = match &out {
            Outcome::Checked(c) if c.verdict == Verdict::Open => {
                match c.deadline.as_deref().or(opp.deadline.as_deref()) {
                    Some(d) => format!("Checked {t}: open until {}", short_date(d)),
                    None => format!("Checked {t}: open"),
                }
            }
            Outcome::Checked(c) if c.verdict == Verdict::Closed => {
                format!("Set aside {t}: it has closed")
            }
            Outcome::Checked(c) if c.verdict == Verdict::Different => {
                format!("Set aside {t}: not what it seemed")
            }
            Outcome::Checked(_) => format!("Checked {t}: no dates on the page, keeping it"),
            _ => format!("Couldn't open {t}; keeping it unverified"),
        };
        rep.report("verifying", msg, pct);
        out
    });
    let mut drop = vec![false; picks.len()];
    for (i, outcome) in outcomes.into_iter().enumerate() {
        match outcome {
            Some(Outcome::Account(e)) => return Err(Error::Ai(e)),
            Some(Outcome::Checked(c)) => match c.verdict {
                Verdict::Open => {
                    apply(&mut picks[i].opp, &c);
                    picks[i].opp.verified = true;
                    stats.verified += 1;
                }
                Verdict::Closed | Verdict::Different => {
                    drop[i] = true;
                    stats.rejected_on_verify += 1;
                }
                Verdict::Unclear => {
                    picks[i].opp.score = picks[i].opp.score.saturating_sub(4);
                    stats.unverified += 1;
                }
            },
            Some(Outcome::Unreadable) => stats.unverified += 1,
            Some(Outcome::Skipped) | None => {}
        }
    }
    ctx.check_cancel()?;
    let mut i = 0;
    picks.retain(|_| {
        let keep = !drop[i];
        i += 1;
        keep
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_reads_verdicts_and_guards_dates() {
        let c = parse(
            &json!({"evidence": "Closing date 18 December 2026", "verdict": "open", "deadline": "2026-12-18", "value": "R20 000", "location": "", "title": "", "summary": "", "action": "Email your CV by 18 December"}),
            "2026-11-26",
        );
        assert_eq!(c.verdict, Verdict::Open);
        assert_eq!(c.deadline.as_deref(), Some("2026-12-18"));
        assert_eq!(c.value, "R20 000");
        let past = parse(
            &json!({"verdict": "open", "deadline": "2026-10-15"}),
            "2026-11-26",
        );
        assert_eq!(
            past.verdict,
            Verdict::Closed,
            "a past deadline closes it whatever the model says"
        );
        assert_eq!(
            parse(&json!({"verdict": "EXPIRED"}), "2026-11-26").verdict,
            Verdict::Closed
        );
        assert_eq!(
            parse(&json!({"verdict": "not_an_opportunity"}), "2026-11-26").verdict,
            Verdict::Different
        );
        assert_eq!(
            parse(&json!({"verdict": "maybe"}), "2026-11-26").verdict,
            Verdict::Unclear
        );
        assert_eq!(parse(&json!({}), "2026-11-26").verdict, Verdict::Unclear);
    }

    #[test]
    fn apply_only_overwrites_with_real_values() {
        let mut o = Opportunity {
            title: "Old".into(),
            value: "R1".into(),
            deadline: Some("2027-01-01".into()),
            ..Default::default()
        };
        let c = parse(
            &json!({"verdict": "open", "deadline": "2027-02-02", "value": "unknown", "title": "New title"}),
            "2026-11-26",
        );
        apply(&mut o, &c);
        assert_eq!(o.title, "New title");
        assert_eq!(o.value, "R1", "a placeholder never overwrites");
        assert_eq!(o.deadline.as_deref(), Some("2027-02-02"));
    }

    #[test]
    fn request_carries_today_and_page() {
        let o = Opportunity {
            url: "https://x.org/a".into(),
            title: "T".into(),
            ..Default::default()
        };
        let r = request(
            "2026-11-26",
            &Profile {
                headline: "Chef".into(),
                location: "Cape Town".into(),
                ..Default::default()
            },
            &o,
            "Page",
            "Body text",
        );
        let user = match &r.user[0] {
            deepseek::Part::Text(t) => t.clone(),
            _ => String::new(),
        };
        assert_eq!(r.stage, "verify");
        assert!(r.system.contains("json"));
        assert!(user.starts_with("Today is 2026-11-26. It's for: Chef, Cape Town."));
        assert!(user.contains("Deadline: unknown"));
        assert!(user.contains("\"\"\"\nBody text\n\"\"\""));
    }
}
