//! The discovery pipeline: plan → search → shortlist → evaluate → verify → rank.
//! See docs/ARCHITECTURE.md ("Discovery") and the module docs of each step.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use deepseek::SearchRequest;
use model::{KnownOpportunity, Opportunity};

use crate::evaluate::{self, Open, Pick};
use crate::plan::{self, Query};
use crate::shortlist::{self, Hit};
use crate::util::{Reporter, parallel_map, public_ip, title_key};
use crate::{
    Ctx, DiscoverInput, DiscoverOutput, DiscoverStats, Error, Limits, context, prompt, verify,
};

/// Searches run at once.
const SEARCH_PARALLEL: usize = 8;

/// Picks asked of the review beyond `max_results`, to allow for those dropped on verification.
const SPARE_PICKS: usize = 6;

/// A second, broader search wave runs when the first pass picks fewer than this.
const SECOND_WAVE_BELOW: usize = 6;
/// Queries in the second wave.
const SECOND_WAVE_QUERIES: usize = 8;
/// New candidates the second wave may add for review.
const SECOND_WAVE_CANDIDATES: usize = 40;

/// Limits clamped to what the APIs and the budget allow.
fn clamp_limits(l: &Limits) -> Limits {
    Limits {
        max_queries: l.max_queries.clamp(1, 30),
        results_per_query: l.results_per_query.clamp(1, 50),
        max_candidates: l.max_candidates.clamp(1, 150),
        max_results: l.max_results.clamp(1, 60),
        verify_top: l.verify_top.min(20),
    }
}

pub(crate) fn run(ctx: &Ctx, input: &DiscoverInput) -> Result<DiscoverOutput, Error> {
    let started = Instant::now();
    ctx.check_cancel()?;
    let profile = input.profile.clone().sanitized();
    if profile.is_empty() {
        return Err(Error::EmptyProfile);
    }
    let limits = clamp_limits(&input.limits);
    let rep = Reporter::new(ctx);
    let mut stats = DiscoverStats::default();

    // 1. Plan.
    rep.report(
        "planning",
        "Reading your profile and planning where to look",
        3,
    );
    let queries = match ctx.ai.chat_json_value(&plan::request(
        &ctx.today,
        &profile,
        &input.intel,
        &input.steering,
        limits.max_queries,
    )) {
        Ok(v) => {
            let mut q = plan::parse(&v, &profile, &ctx.today, limits.max_queries);
            if q.len() < limits.max_queries {
                // A short plan (queries dropped by validation) is topped up from the
                // deterministic one.
                for extra in plan::fallback(&profile, &ctx.today, limits.max_queries) {
                    if q.len() >= limits.max_queries {
                        break;
                    }
                    if !q.iter().any(|x| x.q.eq_ignore_ascii_case(&extra.q)) {
                        q.push(extra);
                    }
                }
            }
            q
        }
        Err(e) if e.is_account_problem() => return Err(Error::Ai(e)),
        Err(_) => {
            stats.plan_fallback = true;
            plan::fallback(&profile, &ctx.today, limits.max_queries)
        }
    };
    ctx.check_cancel()?;
    stats.queries = queries.len();
    let cats: Vec<String> = queries.iter().map(|q| q.category.clone()).collect();
    rep.report(
        "planning",
        format!(
            "Planning {} searches across {}",
            queries.len(),
            prompt::category_phrase(&cats)
        ),
        10,
    );

    // 2. Search.
    let (hits, failed) = search_all(ctx, &rep, &queries, &limits, (12, 45))?;
    stats.failed_searches = failed;
    ctx.check_cancel()?;
    stats.results = hits.len();

    // 3. Shortlist.
    let known: Vec<KnownOpportunity> = input
        .known
        .iter()
        .filter(|k| k.status == "active" || k.status == "saved")
        .take(60)
        .cloned()
        .collect();
    let dismissed: HashSet<String> = input
        .known
        .iter()
        .filter(|k| k.status == "dismissed")
        .map(|k| k.key.clone())
        .collect();
    let short = shortlist::shortlist(
        hits,
        &queries,
        &known,
        &dismissed,
        &ctx.today,
        limits.max_candidates,
    );
    stats.candidates = short.candidates.len();
    if short.candidates.is_empty() {
        rep.report("reviewing", "No promising leads turned up this time", 95);
        stats.elapsed_ms = started.elapsed().as_millis() as u64;
        return Ok(DiscoverOutput {
            opportunities: Vec::new(),
            stale: Vec::new(),
            stats,
        });
    }
    let found = if short.unique > short.candidates.len() {
        format!(
            "Found {} leads, reviewing the best {}",
            short.unique,
            short.candidates.len()
        )
    } else {
        format!("Found {} leads, reviewing them all", short.unique)
    };
    rep.report("reviewing", found, 48);

    // 4. Evaluate.
    let max_picks = limits.max_results + SPARE_PICKS;
    let scope = evaluate::Scope {
        profile: &profile,
        intel: &input.intel,
        steering: &input.steering,
        known: &known,
        context: &input.context,
    };
    let mut cands = short.candidates;
    let all: Vec<usize> = (0..cands.len()).collect();
    let eval = evaluate::evaluate(ctx, &rep, &scope, &cands, &all, max_picks)?;
    stats.failed_reviews = eval.failed_batches;
    let mut picks = eval.picks;
    let mut stale = eval.stale;
    rep.report("reviewing", picked_message(picks.len()), 72);
    ctx.check_cancel()?;

    // 4b. One broader, bounded second wave when the first pass is thin.
    if picks.len() < SECOND_WAVE_BELOW {
        let more = second_wave(
            ctx, &rep, &profile, input, &queries, &cands, &known, &dismissed, &limits, &mut stats,
        )?;
        if !more.is_empty() {
            let start = cands.len();
            cands.extend(more);
            let members: Vec<usize> = (start..cands.len()).collect();
            stats.candidates += members.len();
            rep.report(
                "reviewing",
                format!("Reviewing {} more leads", members.len()),
                79,
            );
            match evaluate::evaluate(ctx, &rep, &scope, &cands, &members, max_picks) {
                Ok(e2) => {
                    picks.extend(e2.picks);
                    picks = evaluate::merge(picks, max_picks);
                    for s in e2.stale {
                        if !stale.contains(&s) {
                            stale.push(s);
                        }
                    }
                }
                Err(Error::Ai(e)) if !e.is_account_problem() => stats.failed_reviews += 1,
                Err(e) => return Err(e),
            }
            rep.report("reviewing", picked_message(picks.len()), 80);
            ctx.check_cancel()?;
        }
    }
    stats.selected = picks.len();

    // 5. Verify.
    verify::verify(
        ctx,
        &rep,
        &profile,
        &mut picks,
        limits.verify_top,
        &mut stats,
    )?;

    // 6. Internal opportunities from the team's own tools.
    let mut internal: Vec<Opportunity> = Vec::new();
    if !input.context.is_empty() {
        rep.report(
            "verifying",
            format!(
                "Reading {} recent items from your team's tools",
                input.context.len().min(60)
            ),
            95,
        );
        internal = context::extract(ctx.ai, &ctx.today, &profile, &input.intel, &input.context)?;
        stats.internal = internal.len();
        ctx.check_cancel()?;
    }

    // 7. Rank and return.
    let mut opportunities = finalize(picks, &ctx.today, &dismissed, limits.max_results);
    if !internal.is_empty() {
        let keys: HashSet<String> = opportunities.iter().filter_map(Opportunity::key).collect();
        internal.retain(|o| {
            o.key()
                .is_some_and(|k| !keys.contains(&k) && !dismissed.contains(&k))
        });
        opportunities.extend(internal);
        opportunities.sort_by_key(|o| std::cmp::Reverse(o.score));
        opportunities.truncate(limits.max_results);
    }
    let ready = match opportunities.len() {
        0 => "Nothing good enough this time; we'll keep looking".to_string(),
        1 => "1 opportunity ready".to_string(),
        n => format!("{n} opportunities ready"),
    };
    rep.report("verifying", ready, 96);
    stats.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(DiscoverOutput {
        opportunities,
        stale,
        stats,
    })
}

fn picked_message(n: usize) -> String {
    match n {
        0 => "None of the leads were open and right for you".into(),
        1 => "Picked 1 opportunity worth your time".into(),
        n => format!("Picked {n} opportunities worth your time"),
    }
}

/// The second wave: a few broader queries from new angles, searched and shortlisted against
/// what the first pass already saw. Bounded (about US$0.01–0.02); its failures never fail the run,
/// except account problems.
#[allow(clippy::too_many_arguments)]
fn second_wave(
    ctx: &Ctx,
    rep: &Reporter,
    profile: &model::Profile,
    input: &DiscoverInput,
    tried: &[Query],
    seen: &[shortlist::Candidate],
    known: &[KnownOpportunity],
    dismissed: &HashSet<String>,
    limits: &Limits,
    stats: &mut DiscoverStats,
) -> Result<Vec<shortlist::Candidate>, Error> {
    rep.report(
        "searching",
        "Too few strong leads yet, so searching wider",
        73,
    );
    let req = plan::request_more(
        &ctx.today,
        profile,
        &input.intel,
        &input.steering,
        tried,
        SECOND_WAVE_QUERIES,
    );
    let queries: Vec<Query> = match ctx.ai.chat_json_value(&req) {
        Ok(v) => plan::parse(&v, profile, &ctx.today, SECOND_WAVE_QUERIES)
            .into_iter()
            .filter(|q| !tried.iter().any(|t| t.q.eq_ignore_ascii_case(&q.q)))
            .collect(),
        Err(e) if e.is_account_problem() => return Err(Error::Ai(e)),
        Err(_) => Vec::new(),
    };
    ctx.check_cancel()?;
    if queries.is_empty() {
        return Ok(Vec::new());
    }
    stats.second_wave_queries = queries.len();
    let hits = match search_all(ctx, rep, &queries, limits, (74, 78)) {
        Ok((hits, failed)) => {
            stats.failed_searches += failed;
            hits
        }
        Err(Error::Ai(e)) if !e.is_account_problem() => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    stats.results += hits.len();
    let mut exclude: HashSet<String> = dismissed.clone();
    exclude.extend(seen.iter().map(|c| c.key.clone()));
    let short = shortlist::shortlist(
        hits,
        &queries,
        known,
        &exclude,
        &ctx.today,
        SECOND_WAVE_CANDIDATES,
    );
    Ok(short.candidates)
}

/// Run every query (≤ 8 at once), reporting progress within `range`. Failed queries are skipped
/// (their count is returned); an account problem stops everything; if every query fails, so does
/// the run.
fn search_all(
    ctx: &Ctx,
    rep: &Reporter,
    queries: &[Query],
    limits: &Limits,
    range: (u8, u8),
) -> Result<(Vec<Hit>, usize), Error> {
    let abort = AtomicBool::new(false);
    let started = AtomicUsize::new(0);
    let total = queries.len().max(1);
    let localization = ctx.localization.as_deref().and_then(public_ip);
    let outcomes = parallel_map(queries, SEARCH_PARALLEL, &abort, |_, q| {
        if ctx.cancel.load(Ordering::Relaxed) {
            return None;
        }
        let k = started.fetch_add(1, Ordering::Relaxed);
        rep.report(
            "searching",
            format!("Searching: {}", q.about),
            range.0 + ((range.1 - range.0) as usize * k / total) as u8,
        );
        let mut req = SearchRequest::new(q.q.clone(), limits.results_per_query);
        req.ori_question = Some(q.about.clone()).filter(|a| !a.is_empty());
        req.localization = localization.clone();
        let out = ctx.ai.search(&req);
        if let Err(e) = &out
            && e.is_account_problem()
        {
            abort.store(true, Ordering::Relaxed);
        }
        Some(out)
    });
    ctx.check_cancel()?;
    let mut hits: Vec<Hit> = Vec::new();
    let mut failed = 0usize;
    let mut last_error: Option<deepseek::Error> = None;
    for (i, out) in outcomes.into_iter().enumerate() {
        match out.flatten() {
            Some(Ok(results)) => {
                hits.extend(results.into_iter().map(|result| Hit { query: i, result }))
            }
            Some(Err(e)) => {
                if e.is_account_problem() {
                    return Err(Error::Ai(e));
                }
                failed += 1;
                last_error = Some(e);
            }
            None => failed += 1,
        }
    }
    if failed == queries.len() && !queries.is_empty() {
        return Err(Error::Ai(last_error.unwrap_or_else(|| {
            deepseek::Error::Network("every search failed".into())
        })));
    }
    let done = queries.len() - failed;
    rep.report(
        "searching",
        if done == 1 {
            "Finished 1 search".to_string()
        } else {
            format!("Finished {done} searches")
        },
        46,
    );
    Ok((hits, failed))
}

/// Final ranking: verified picks get a small boost, ones the review wasn't sure were open a small
/// penalty; then sanitised, unique by URL key and title, never dismissed, best first, capped.
pub(crate) fn finalize(
    picks: Vec<Pick>,
    today: &str,
    dismissed: &HashSet<String>,
    max: usize,
) -> Vec<Opportunity> {
    let mut ranked: Vec<Opportunity> = Vec::new();
    for p in picks {
        let mut o = p.opp;
        let adjust: i16 = match (o.verified, p.open) {
            (true, _) => 2,
            (false, Open::Yes) => 0,
            (false, Open::Likely) => -3,
            (false, Open::Unknown) => -5,
        };
        o.score = (i16::from(o.score) + adjust).clamp(0, 100) as u8;
        if let Some(o) = o.sanitized(today) {
            ranked.push(o);
        }
    }
    ranked.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(b.verified.cmp(&a.verified))
            .then(a.deadline.cmp(&b.deadline))
    });
    let mut keys: HashSet<String> = HashSet::new();
    let mut titles: HashSet<String> = HashSet::new();
    ranked.retain(|o| match o.key() {
        Some(k) => !dismissed.contains(&k) && keys.insert(k) && titles.insert(title_key(&o.title)),
        None => false,
    });
    ranked.truncate(max);
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(i: usize, score: u8, verified: bool, open: Open, title: &str) -> Pick {
        Pick {
            cand: i,
            opp: Opportunity {
                url: format!("https://o{i}.org/x"),
                title: title.into(),
                category: "jobs".into(),
                score,
                verified,
                ..Default::default()
            },
            open,
        }
    }

    #[test]
    fn finalize_ranks_dedupes_and_caps() {
        let mut expired = pick(9, 99, true, Open::Yes, "Expired");
        expired.opp.deadline = Some("2026-01-01".into());
        let mut same_url = pick(1, 50, false, Open::Yes, "Other title");
        same_url.opp.url = "https://www.o1.org/x/".into();
        let picks = vec![
            pick(1, 80, false, Open::Yes, "A"),
            pick(2, 80, true, Open::Yes, "B"),
            pick(3, 84, false, Open::Unknown, "C"),
            pick(4, 70, false, Open::Yes, "a"),
            pick(5, 60, false, Open::Likely, "Dismissed one"),
            expired,
            same_url,
        ];
        let dismissed: HashSet<String> = ["o5.org/x".to_string()].into();
        let out = finalize(picks, "2026-11-26", &dismissed, 10);
        let titles: Vec<(&str, u8)> = out.iter().map(|o| (o.title.as_str(), o.score)).collect();
        assert_eq!(titles, vec![("B", 82), ("A", 80), ("C", 79)], "{titles:?}");
        assert_eq!(
            finalize(
                vec![
                    pick(1, 80, false, Open::Yes, "A"),
                    pick(2, 70, false, Open::Yes, "B")
                ],
                "2026-11-26",
                &HashSet::new(),
                1
            )
            .len(),
            1
        );
    }

    #[test]
    fn limits_are_clamped() {
        let l = clamp_limits(&Limits {
            max_queries: 0,
            results_per_query: 500,
            max_candidates: 1000,
            max_results: 0,
            verify_top: 99,
        });
        assert_eq!(
            (
                l.max_queries,
                l.results_per_query,
                l.max_candidates,
                l.max_results,
                l.verify_top
            ),
            (1, 50, 150, 1, 20)
        );
        assert_eq!(clamp_limits(&Limits::default()), Limits::default());
    }
}
