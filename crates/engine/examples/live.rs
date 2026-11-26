//! Run one engine pipeline against the REAL DeepSeek API and print the results. It costs real
//! money (a discovery run is about US$0.02–0.05; see docs/DEVELOPMENT.md), so run it on purpose.
//!
//! ```text
//! set -a; . /home/gabriel/Documents/opportunity/.env; set +a
//! cargo run -p engine --example live -- discover crates/engine/examples/personas/chef_cape_town.json
//! cargo run -p engine --example live -- research crates/engine/examples/personas/gabriel.json
//! cargo run -p engine --example live -- theme    crates/engine/examples/personas/student_lagos.json
//! ```
//!
//! Options: `--today YYYY-MM-DD`, `--queries N`, `--candidates N`, `--results N`, `--verify N`,
//! `--json OUT.json` (writes everything, for QA), `--quiet` (no live progress).
//!
//! A persona file is JSON with any of: `profile` (model::Profile), `steering` (model::Steering),
//! `known` ([model::KnownOpportunity]), `limits` (engine::Limits), `localization` (an IP address),
//! and `research` (`first_name`, `last_name`, `email`, `hints`) for the research pipeline.
//! `DEEPSEEK_BASE_URL` is honoured, so a local caching proxy can sit in front of the API.

use std::sync::atomic::AtomicBool;
use std::time::Instant;

use engine::{BusinessInput, Ctx, DiscoverInput, Limits, ResearchInput};
use model::{ContextItem, Intel, KnownOpportunity, Profile, Progress, Steering};
use serde_json::{Value, json};

fn usage_exit(msg: &str) -> ! {
    eprintln!(
        "{msg}\n\nusage: live <business|discover|research|theme> <persona.json> [--from business-run.json] [--today YYYY-MM-DD] [--queries N] [--candidates N] [--results N] [--verify N] [--json out.json] [--quiet]"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        usage_exit("missing arguments");
    }
    let pipeline = args[0].clone();
    let persona_path = args[1].clone();
    let mut today = model::date::today();
    let mut json_out: Option<String> = None;
    let mut quiet = false;
    let mut from: Option<String> = None;
    let mut overrides: Vec<(String, usize)> = Vec::new();
    let mut i = 2;
    while i < args.len() {
        let flag = args[i].as_str();
        let value = args.get(i + 1).cloned();
        match flag {
            "--today" => today = value.unwrap_or_else(|| usage_exit("--today needs a date")),
            "--from" => from = Some(value.unwrap_or_else(|| usage_exit("--from needs a path"))),
            "--json" => json_out = Some(value.unwrap_or_else(|| usage_exit("--json needs a path"))),
            "--quiet" => {
                quiet = true;
                i += 1;
                continue;
            }
            "--queries" | "--candidates" | "--results" | "--verify" => {
                let n = value
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage_exit("expected a number"));
                overrides.push((flag.trim_start_matches("--").to_string(), n));
            }
            other => usage_exit(&format!("unknown option {other}")),
        }
        i += 2;
    }

    let persona: Value = std::fs::read_to_string(&persona_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| usage_exit(&format!("can't read persona JSON from {persona_path}")));
    // A business run's output (--json) can supply the profile and intel for discovery or theme.
    let earlier: Value = from
        .as_deref()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let pick = |key: &str| {
        if earlier[key].is_null() {
            persona[key].clone()
        } else {
            earlier[key].clone()
        }
    };
    let profile: Profile = serde_json::from_value(pick("profile")).unwrap_or_default();
    let intel: Intel = serde_json::from_value(pick("intel")).unwrap_or_default();
    let context: Vec<ContextItem> =
        serde_json::from_value(persona["context"].clone()).unwrap_or_default();
    let steering: Steering =
        serde_json::from_value(persona["steering"].clone()).unwrap_or_default();
    let known: Vec<KnownOpportunity> =
        serde_json::from_value(persona["known"].clone()).unwrap_or_default();
    let mut limits: Limits = serde_json::from_value(persona["limits"].clone()).unwrap_or_default();
    for (k, n) in overrides {
        match k.as_str() {
            "queries" => limits.max_queries = n,
            "candidates" => limits.max_candidates = n,
            "results" => limits.max_results = n,
            _ => limits.verify_top = n,
        }
    }

    let Some(cfg) = deepseek::Config::from_env() else {
        usage_exit("set DEEPSEEK_API_KEY (and optionally DEEPSEEK_BASE_URL)")
    };
    eprintln!(
        "API: {} · model {} · today {today}",
        cfg.base_url, cfg.model
    );
    let ai = deepseek::Client::new(cfg);
    let started = Instant::now();
    let progress = move |p: Progress| {
        if !quiet {
            eprintln!(
                "[{:6.1}s] {:3}% {:<11} {}",
                started.elapsed().as_secs_f64(),
                p.percent,
                p.stage,
                p.message
            );
        }
    };
    let cancel = AtomicBool::new(false);
    let ctx = Ctx {
        ai: &ai,
        progress: &progress,
        today: today.clone(),
        localization: persona["localization"].as_str().map(str::to_string),
        cancel: &cancel,
    };

    let output: Value = match pipeline.as_str() {
        "discover" => {
            let input = DiscoverInput {
                profile,
                steering,
                known,
                limits,
                intel,
                context,
            };
            match engine::discover(&ctx, &input) {
                Ok(out) => {
                    println!("\n=== {} opportunities ===", out.opportunities.len());
                    for (n, o) in out.opportunities.iter().enumerate() {
                        let mut facts: Vec<String> = Vec::new();
                        if !o.organization.is_empty() {
                            facts.push(o.organization.clone());
                        }
                        if !o.location.is_empty() {
                            facts.push(o.location.clone());
                        }
                        facts.push(format!("deadline {}", o.deadline.as_deref().unwrap_or("-")));
                        if !o.value.is_empty() {
                            facts.push(o.value.clone());
                        }
                        if let Some(p) = &o.published {
                            facts.push(format!("published {p}"));
                        }
                        println!(
                            "\n{:2}. [{}] {} ({}){}",
                            n + 1,
                            o.score,
                            o.title,
                            o.category,
                            if o.verified { "  ✓ verified" } else { "" }
                        );
                        println!("    {}", o.url);
                        println!("    {}", facts.join(" · "));
                        println!("    {}", o.summary);
                        println!("    why: {}", o.why);
                        println!("    next: {}", o.action);
                    }
                    if !out.stale.is_empty() {
                        println!("\nstale: {:?}", out.stale);
                    }
                    println!(
                        "\nstats: {}",
                        serde_json::to_string(&out.stats).unwrap_or_default()
                    );
                    json!({ "opportunities": out.opportunities, "stale": out.stale, "stats": out.stats })
                }
                Err(e) => {
                    println!("\ndiscover failed: {e}");
                    json!({ "error": e.to_string() })
                }
            }
        }
        "research" => {
            let r = &persona["research"];
            let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
            let input = ResearchInput {
                first_name: s("first_name"),
                last_name: s("last_name"),
                email: s("email"),
                hints: s("hints"),
            };
            match engine::research(&ctx, &input) {
                Ok(p) => {
                    if let Some(r) = &p.research {
                        println!(
                            "\n=== research: confidence {:.2} ===\n{}\nsources:",
                            r.confidence, r.summary
                        );
                        for s in &r.sources {
                            println!("  - {s}");
                        }
                    }
                    let mut shown = p.clone();
                    shown.research = None;
                    println!(
                        "\nprofile: {}",
                        serde_json::to_string_pretty(&shown).unwrap_or_default()
                    );
                    json!({ "profile": p })
                }
                Err(e) => {
                    println!("\nresearch failed: {e}");
                    json!({ "error": e.to_string() })
                }
            }
        }
        "business" => {
            let b = &persona["business"];
            let s = |k: &str| b[k].as_str().unwrap_or("").to_string();
            let input = BusinessInput {
                business_name: s("business_name"),
                website: s("website"),
                hints: s("hints"),
            };
            match engine::research_business(&ctx, &input) {
                Ok((p, intel)) => {
                    if let Some(r) = &p.research {
                        println!(
                            "\n=== business research: confidence {:.2} ===\n{}",
                            r.confidence, r.summary
                        );
                    }
                    let mut shown = p.clone();
                    shown.research = None;
                    println!(
                        "\nprofile: {}",
                        serde_json::to_string_pretty(&shown).unwrap_or_default()
                    );
                    let o = &intel.overview;
                    println!(
                        "\noverview: {}\n  industry {} · HQ {} · founded {} · size {} · model {} · markets {:?}",
                        o.summary,
                        o.industry,
                        o.headquarters,
                        o.founded,
                        o.size,
                        o.business_model,
                        o.markets
                    );
                    for s in &o.sources {
                        println!("  source: {}", s.url);
                    }
                    for x in &intel.offerings {
                        println!(
                            "offering: {} — {} [{}]",
                            x.name, x.description, x.source.url
                        );
                    }
                    for x in &intel.segments {
                        println!(
                            "segment: {} — {} examples {:?} [{}]",
                            x.name, x.description, x.examples, x.source.url
                        );
                    }
                    for x in &intel.competitors {
                        println!(
                            "competitor: {} ({}) {} — {} [{}]",
                            x.name, x.overlap, x.website, x.summary, x.source.url
                        );
                    }
                    for x in &intel.trends {
                        println!(
                            "trend: {} ({}) — {} [{}]",
                            x.title, x.impact, x.summary, x.source.url
                        );
                    }
                    for x in &intel.regulation {
                        println!(
                            "regulation: {} [{} · {} · effective {:?} · {}] — {} [{}]",
                            x.title,
                            x.jurisdiction,
                            x.status,
                            x.effective,
                            x.impact,
                            x.summary,
                            x.source.url
                        );
                    }
                    for x in &intel.news {
                        println!(
                            "news: {} {:?} — {} [{}]",
                            x.title, x.source.date, x.summary, x.source.url
                        );
                    }
                    json!({ "profile": p, "intel": intel })
                }
                Err(e) => {
                    println!("\nbusiness research failed: {e}");
                    json!({ "error": e.to_string() })
                }
            }
        }
        "theme" => {
            let t = engine::theme_hint(&ctx, &profile);
            println!(
                "\ntheme: {}",
                serde_json::to_string_pretty(&t).unwrap_or_default()
            );
            json!({ "theme": t })
        }
        other => usage_exit(&format!("unknown pipeline {other}")),
    };

    let u = ai.usage();
    println!(
        "\nusage: {} chat calls · {} input tokens ({} cached) · {} output tokens · {} searches · {} fetches · {} failed · ${:.4} · {:.1}s",
        u.chat_calls,
        u.input_tokens + u.cached_tokens,
        u.cached_tokens,
        u.output_tokens,
        u.searches,
        u.fetches,
        u.failed_calls,
        u.cost_usd,
        started.elapsed().as_secs_f64()
    );
    for c in u.calls.iter().filter(|c| c.kind == "chat") {
        println!(
            "  chat {:<9} {:>6}ms  in {:>6}  out {:>5}  status {}  attempts {}",
            c.stage, c.ms, c.tokens_in, c.tokens_out, c.status, c.attempts
        );
    }
    let slowest = u
        .calls
        .iter()
        .filter(|c| c.kind != "chat")
        .map(|c| c.ms)
        .max()
        .unwrap_or(0);
    println!(
        "  tools: slowest search/fetch {slowest}ms; failed: {}",
        u.calls
            .iter()
            .filter(|c| c.kind != "chat" && !(200..300).contains(&c.status))
            .map(|c| format!("{} {} {}", c.kind, c.status, c.note))
            .collect::<Vec<_>>()
            .join("; ")
    );

    if let Some(path) = json_out {
        let mut all = output;
        all["usage"] = serde_json::to_value(&u).unwrap_or(Value::Null);
        all["elapsed_s"] = json!(started.elapsed().as_secs_f64());
        all["today"] = json!(today);
        match std::fs::write(
            &path,
            serde_json::to_string_pretty(&all).unwrap_or_default(),
        ) {
            Ok(()) => eprintln!("wrote {path}"),
            Err(e) => eprintln!("couldn't write {path}: {e}"),
        }
    }
}
