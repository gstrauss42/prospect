//! The opportunity intelligence. Turns a profile into great, real, current opportunities using
//! DeepSeek's Search API, Fetch API and `deepseek-flash` chat completions.
//!
//! Three pipelines, all blocking (the server runs them on worker threads):
//!
//! * [`discover`]: profile + steering + what the person already has → ranked opportunities and
//!   the known ones that are no longer relevant.
//! * [`research`]: name + email + optional hints → a drafted [`Profile`] (auto-onboarding).
//! * [`theme_hint`]: profile → label, accent colour and photo searches for the page theme.
//!
//! See docs/ARCHITECTURE.md for the pipeline design. Owner: the engine builder.
//!
//! Every model call uses `deepseek-flash` in JSON mode with today's date in the prompt, and every
//! model answer is validated before use: candidates are referenced by number (so each URL is one
//! the search engine really returned), categories and ids must be known, dates real, lengths
//! capped. Partial failures degrade gracefully (a failed search is skipped, a failed plan falls
//! back to a deterministic one, an unreadable page keeps its pick unverified); account problems
//! (bad key, no credit) stop the run with [`Error::Ai`].
//!
//! Modules: `plan`, `shortlist`, `evaluate`, `verify` (the discovery steps, orchestrated by
//! `discover`), `research`, `theme`, plus `prompt` (how a profile is written for the model) and
//! `util` (progress, parallelism, lenient JSON readers).

mod business;
mod context;
mod discover;
mod evaluate;
mod plan;
mod prompt;
mod research;
mod shortlist;
mod theme;
mod util;
mod verify;

use std::sync::atomic::{AtomicBool, Ordering};

use model::{KnownOpportunity, Opportunity, Profile, Progress, Steering, ThemeHint};
use serde::{Deserialize, Serialize};

/// What a pipeline needs from its caller.
pub struct Ctx<'a> {
    /// A DeepSeek client for this run (the server forks one per run, so its usage is the run's).
    pub ai: &'a deepseek::Client,
    /// Called at each step for the live loading screen. Cheap, never panics. Messages are shown
    /// to the person: short, friendly, specific ("Searching for culinary grants in Cape Town").
    pub progress: &'a (dyn Fn(Progress) + Sync),
    /// Today (UTC) as YYYY-MM-DD.
    pub today: String,
    /// The person's last public IP, to localise web search; None if unknown.
    pub localization: Option<String>,
    /// Becomes true when the server is shutting down: stop at the next step and return
    /// [`Error::Cancelled`].
    pub cancel: &'a AtomicBool,
}

impl Ctx<'_> {
    pub fn report(&self, stage: &str, message: impl Into<String>, percent: u8) {
        (self.progress)(Progress::new(stage, message, percent));
    }

    pub fn check_cancel(&self) -> Result<(), Error> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// How much work a discovery run may do. Defaults keep a run around US$0.02–0.05.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Search API queries per run.
    pub max_queries: usize,
    /// Results asked for per query (1–50).
    pub results_per_query: u32,
    /// Unique search results passed to the model for review.
    pub max_candidates: usize,
    /// Opportunities returned at most.
    pub max_results: usize,
    /// Top picks whose pages are read with the Fetch API to confirm they're open.
    pub verify_top: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_queries: 14,
            results_per_query: 10,
            max_candidates: 70,
            max_results: 24,
            verify_top: 8,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct DiscoverInput {
    pub profile: Profile,
    pub steering: Steering,
    /// Everything the person already has (active, saved and dismissed). Dismissed URLs must never
    /// come back; active and saved ones may be returned again (that refreshes them).
    pub known: Vec<KnownOpportunity>,
    pub limits: Limits,
    /// The business's sourced dossier from research; opportunities are found from it.
    pub intel: model::Intel,
    /// Recent items from the team's connected tools (read-only, never stored). Internal
    /// opportunities are extracted from them; web finds the team is already on are flagged.
    pub context: Vec<model::ContextItem>,
}

/// What business research starts from: the business name, its website and optional hints.
#[derive(Clone, Debug, Default)]
pub struct BusinessInput {
    pub business_name: String,
    pub website: String,
    /// Anything the account added: country, what it does.
    pub hints: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoverStats {
    pub queries: usize,
    pub results: usize,
    pub candidates: usize,
    pub selected: usize,
    pub verified: usize,
    /// Picks dropped after reading their page (closed, irrelevant, not an opportunity).
    pub rejected_on_verify: usize,
    /// Top picks kept without confirmation: their page couldn't be read or didn't say enough.
    pub unverified: usize,
    /// Searches that failed and were skipped.
    pub failed_searches: usize,
    /// Review batches that failed (their candidates went unreviewed).
    pub failed_reviews: usize,
    /// True when the model's plan failed and the deterministic plan was used.
    pub plan_fallback: bool,
    /// Queries in the broader second wave (0 when the first pass was enough).
    pub second_wave_queries: usize,
    /// Opportunities found in the team's own tools (connector context).
    pub internal: usize,
    /// Wall time of the whole run.
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiscoverOutput {
    /// Best first, each passed through [`Opportunity::sanitized`], unique by URL key, none of them
    /// dismissed before.
    pub opportunities: Vec<Opportunity>,
    /// Keys of known active (not saved) opportunities that no longer fit the person.
    pub stale: Vec<String>,
    pub stats: DiscoverStats,
}

/// What auto-onboarding starts from.
#[derive(Clone, Debug, Default)]
pub struct ResearchInput {
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    /// Anything the person added to help find them: city, company, a link, what they do.
    pub hints: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// DeepSeek failed in a way the pipeline couldn't work around.
    Ai(deepseek::Error),
    /// The profile is too empty to search with.
    EmptyProfile,
    /// The server is shutting down.
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Ai(e) => write!(f, "{e}"),
            Error::EmptyProfile => write!(f, "The profile is too empty to search with"),
            Error::Cancelled => write!(f, "Stopped because the server is shutting down"),
        }
    }
}

impl std::error::Error for Error {}

impl From<deepseek::Error> for Error {
    fn from(e: deepseek::Error) -> Self {
        Error::Ai(e)
    }
}

/// Find the best current opportunities for a person.
///
/// Plan (model, thinking off) → search (≤ 8 in parallel) → shortlist (no model) → evaluate
/// (model, thinking low, candidates by number, in parallel batches) → verify the top picks
/// (Fetch API + model, ≤ 6 in parallel) → rank. Progress is reported at every step with the
/// percent only ever rising, ending at 96 (the server reports saving and done).
pub fn discover(ctx: &Ctx, input: &DiscoverInput) -> Result<DiscoverOutput, Error> {
    discover::run(ctx, input)
}

/// Research a person from public web results and draft their profile. The returned profile has
/// `research` set (confidence, a short summary for the person, sources) and is sanitized.
///
/// With nothing reliable to go on, the draft comes from the hints alone, with low confidence and
/// a summary that says so. Fails only when the AI or every search is unavailable.
pub fn research(ctx: &Ctx, input: &ResearchInput) -> Result<Profile, Error> {
    research::run(ctx, input)
}

/// The page theme for a profile. Never fails: falls back to a sensible theme without the model.
/// Reports no progress (it usually runs alongside a discovery run that does).
pub fn theme_hint(ctx: &Ctx, profile: &Profile) -> ThemeHint {
    theme::hint(ctx, profile)
}

pub use theme::fallback_theme;

/// Research a business from its website and the public web: the drafted [`Profile`] (with
/// `research` set) and the sourced [`model::Intel`] dossier. Every intel item cites a page that was
/// actually read or returned by search; anything else is dropped. Unknown stays empty.
pub fn research_business(
    ctx: &Ctx,
    input: &BusinessInput,
) -> Result<(Profile, model::Intel), Error> {
    business::run(ctx, input)
}
