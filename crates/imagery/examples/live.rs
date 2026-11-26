//! Live check against the real Openverse, Wikimedia Commons and DeepSeek APIs (one small vision
//! call per theme, well under a cent each). Saves each winner, the previews that were judged and
//! a JSON note to a folder outside the repo, so a person can look at what was chosen and why.
//!
//! ```text
//! set -a; source /path/to/.env; set +a
//! cargo run -p imagery --example live -- [OUT_DIR] ["Label|query one;query two|mood" ...]
//! ```
//!
//! Without theme arguments it runs three: Cooking, Climate tech and Jazz.

use std::path::PathBuf;

use imagery::{Config, find_background_report};
use model::ThemeHint;

fn ext(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/webp" => "webp",
        _ => "jpg",
    }
}

fn slug(s: &str) -> String {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn parse_hint(arg: &str) -> ThemeHint {
    let mut parts = arg.split('|');
    let label = parts.next().unwrap_or("").trim().to_string();
    let image_queries = parts
        .next()
        .map(|q| {
            q.split(';')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let mood = parts.next().unwrap_or("").trim().to_string();
    ThemeHint {
        label,
        image_queries,
        mood,
        accent: String::new(),
    }
}

fn default_hints() -> Vec<ThemeHint> {
    vec![
        ThemeHint {
            label: "Cooking".into(),
            image_queries: vec![
                "chef plating food".into(),
                "fresh ingredients kitchen".into(),
                "rustic kitchen table food".into(),
            ],
            mood: "Warm, appetising light on fresh food; inviting and crafted.".into(),
            accent: "#c2410c".into(),
        },
        ThemeHint {
            label: "Climate tech".into(),
            image_queries: vec![
                "wind turbines landscape".into(),
                "solar panels field".into(),
                "wind farm sunset".into(),
            ],
            mood: "Hopeful and expansive: clean energy in open landscapes at golden hour.".into(),
            accent: "#047857".into(),
        },
        ThemeHint {
            label: "Jazz".into(),
            image_queries: vec![
                "saxophone".into(),
                "jazz club stage lights".into(),
                "piano keys".into(),
            ],
            mood: "Moody, warm low light of a late-night jazz club.".into(),
            accent: "#7c3aed".into(),
        },
    ]
}

fn main() {
    let Some(ds) = deepseek::Config::from_env() else {
        eprintln!("Set DEEPSEEK_API_KEY (this example calls the real API).");
        std::process::exit(2);
    };
    if ds.model != "deepseek-flash" {
        eprintln!(
            "Refusing to run with model {}: keep everything on deepseek-flash.",
            ds.model
        );
        std::process::exit(2);
    }
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "/tmp/claude-1000/prospect-imagery".into()),
    );
    let hints: Vec<ThemeHint> = {
        let given: Vec<ThemeHint> = args
            .map(|a| parse_hint(&a))
            .filter(|h| !h.label.is_empty())
            .collect();
        if given.is_empty() {
            default_hints()
        } else {
            given
        }
    };
    std::fs::create_dir_all(&out).expect("create the output folder");
    let ai = deepseek::Client::new(ds);
    match ai.balance_usd() {
        Ok(b) => println!("balance before: ${b:.2}"),
        Err(e) => println!("balance before: unavailable ({e})"),
    }
    let cfg = Config::from_env();
    for hint in &hints {
        println!("\n== {} ({})", hint.label, hint.image_queries.join(" / "));
        let (result, report) = find_background_report(&cfg, &ai, hint, true);
        println!("{}", report.summary());
        let name = slug(&hint.label);
        for (i, p) in report.previews.iter().enumerate() {
            let score = report
                .verdicts
                .iter()
                .flat_map(|v| v.scores.iter())
                .nth(i)
                .copied()
                .flatten();
            let path = out.join(format!("{name}-preview-{:02}.{}", i + 1, ext(p.mime)));
            let _ = std::fs::write(&path, &p.bytes);
            println!(
                "  {:>2}. score {:>4} {:?}/{} {}×{} {} — {}",
                i + 1,
                score
                    .map(|s| format!("{s:.0}"))
                    .unwrap_or_else(|| "-".into()),
                p.candidate.provider,
                p.candidate.source,
                p.width,
                p.height,
                p.candidate.license,
                p.candidate.title
            );
        }
        for v in &report.verdicts {
            println!("  vision: best {:?}, why: {}", v.best.map(|b| b + 1), v.why);
        }
        if !report.source_errors.is_empty() {
            println!("  source errors: {}", report.source_errors.join("; "));
        }
        for f in report.failed_downloads.iter().take(6) {
            println!("  failed download: {f}");
        }
        match result {
            Ok(bg) => {
                let path = out.join(format!("{name}.{}", ext(&bg.image.mime)));
                std::fs::write(&path, &bg.bytes).expect("write the winner");
                let note = serde_json::json!({
                    "theme": hint,
                    "image": bg.image,
                    "bytes": bg.bytes.len(),
                    "decision": report.decision,
                    "used_full_size": report.used_full_size,
                    "summary": report.summary(),
                    "verdicts": report.verdicts.iter().map(|v| serde_json::json!({ "scores": v.scores, "best": v.best.map(|b| b + 1), "why": v.why })).collect::<Vec<_>>(),
                });
                let _ = std::fs::write(
                    out.join(format!("{name}.json")),
                    serde_json::to_string_pretty(&note).unwrap_or_default(),
                );
                println!(
                    "  winner: {} ({}×{}, {} KB)\n  credit: {}\n  source: {}",
                    path.display(),
                    bg.image.width,
                    bg.image.height,
                    bg.bytes.len() / 1024,
                    bg.image.credit,
                    bg.image.source_url
                );
            }
            Err(e) => println!("  no background: {e}"),
        }
    }
    let u = ai.usage();
    println!(
        "\nusage: {} chat calls, {} input + {} cached + {} output tokens, estimated ${:.5}",
        u.chat_calls, u.input_tokens, u.cached_tokens, u.output_tokens, u.cost_usd
    );
    match ai.balance_usd() {
        Ok(b) => println!("balance after: ${b:.2}"),
        Err(e) => println!("balance after: unavailable ({e})"),
    }
}
