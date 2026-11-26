//! How a business profile, its intelligence, steering and the taxonomy are written into prompts.
//! Shared by every stage so the model always sees the business the same way.

use model::text::truncate;
use model::{ContextItem, Intel, OPPORTUNITY_TYPES, Profile, REACH, STAGES, Steering};

pub(crate) fn stage_label(id: &str) -> &'static str {
    STAGES
        .iter()
        .find(|(s, _)| *s == id)
        .map(|(_, l)| *l)
        .unwrap_or("")
}

pub(crate) fn reach_label(id: &str) -> &'static str {
    REACH
        .iter()
        .find(|(r, _)| *r == id)
        .map(|(_, l)| *l)
        .unwrap_or("National")
}

/// The reach id with a sensible default: empty means national.
pub(crate) fn reach_id(p: &Profile) -> &str {
    if model::is_reach(&p.reach) {
        &p.reach
    } else {
        "national"
    }
}

/// A short plural noun for a focus area, for progress messages ("tenders and leads", "grants").
pub(crate) fn category_noun(id: &str) -> &'static str {
    match id {
        "sales" => "sales leads",
        "partners" => "partners",
        "funding" => "grants and incentives",
        "regulation" => "regulation",
        "market" => "market openings",
        "visibility" => "events and awards",
        "talent" => "talent programmes",
        "operations" => "suppliers",
        _ => "other leads",
    }
}

/// The focus areas, one per line, for the model.
pub(crate) fn categories_block() -> String {
    OPPORTUNITY_TYPES
        .iter()
        .map(|t| format!("- {}: {} ({})", t.id, t.label, t.hint))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The language the cards should be written in.
pub(crate) fn output_language(p: &Profile) -> String {
    let english = |l: &str| l.to_ascii_lowercase().starts_with("en");
    match p.languages.first() {
        None => "English".into(),
        Some(_) if p.languages.iter().any(|l| english(l)) => "English".into(),
        Some(first) => first.clone(),
    }
}

/// The business profile as plain lines. The website and links are left out on purpose: they
/// don't help find opportunities and must never end up in a search.
pub(crate) fn profile_block(p: &Profile) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut line = |label: &str, value: &str| {
        let v = value.trim();
        if !v.is_empty() {
            out.push(format!("{label}: {v}"));
        }
    };
    line("Business", &p.organization);
    line("What it does", &p.headline);
    line("Industry", &p.industry);
    line("Stage", stage_label(&p.stage));
    line(
        "Headquarters",
        if p.location.trim().is_empty() {
            "not given"
        } else {
            &p.location
        },
    );
    let reach = reach_id(p);
    line(
        "Markets served",
        &format!("{reach} ({})", reach_label(reach)),
    );
    let wants = if p.seeking.is_empty() {
        "all focus areas: choose what suits the business best".to_string()
    } else {
        p.seeking
            .iter()
            .map(|id| format!("{id} ({})", model::opportunity_type_label(id)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    line("Focus areas", &wants);
    line("Markets, sectors and topics", &p.interests.join(", "));
    line("Products and services", &p.skills.join(", "));
    line("Goals", &truncate(&p.goals, 700));
    line("Avoid", &truncate(&p.avoid, 400));
    let langs = if p.languages.is_empty() {
        "English".to_string()
    } else {
        p.languages.join(", ")
    };
    line("Languages", &langs);
    out.join("\n")
}

/// Who a business sells to, which decides the search mix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Model {
    Consumer,
    Business,
    Unknown,
}

impl Model {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Model::Consumer => {
                "sells mainly to consumers or runs a marketplace (B2C): use the consumer mix"
            }
            Model::Business => {
                "sells mainly to businesses or government (B2B): use the business mix"
            }
            Model::Unknown => "not stated: judge from the profile and dossier",
        }
    }
}

/// The business model from the dossier's business model and segments (and the profile).
pub(crate) fn business_model(p: &Profile, i: &Intel) -> Model {
    let mut text = format!(
        "{} {} {}",
        i.overview.business_model, p.headline, i.overview.summary
    );
    for s in &i.segments {
        text.push(' ');
        text.push_str(&s.name);
        text.push(' ');
        text.push_str(&s.description);
    }
    let t = format!(" {} ", text.to_lowercase());
    const CONSUMER: &[&str] = &[
        " b2c",
        "consumer",
        "household",
        "homeowner",
        "families",
        "individuals",
        "shoppers",
        "patients",
        "marketplace",
        " app ",
        "on-demand",
        "retail customers",
        "home services",
        "diners",
        "guests",
    ];
    const BUSINESS: &[&str] = &[
        " b2b",
        "businesses",
        "enterprise",
        "companies",
        "saas",
        "government",
        "public sector",
        "municipal",
        "procurement",
        "contracts",
        "corporate clients",
        "industrial",
        "mines",
        "employers",
        "hr teams",
    ];
    let c = CONSUMER.iter().filter(|w| t.contains(*w)).count();
    let b = BUSINESS.iter().filter(|w| t.contains(*w)).count();
    match (c, b) {
        (0, 0) => Model::Unknown,
        (c, b) if c > b => Model::Consumer,
        (c, b) if b > c => Model::Business,
        _ => Model::Unknown,
    }
}

/// The research dossier as a compact digest (names and one-liners, no URLs), or "".
pub(crate) fn intel_block(i: &Intel) -> String {
    let mut out: Vec<String> = Vec::new();
    if !i.overview.summary.is_empty() {
        out.push(format!("Overview: {}", truncate(&i.overview.summary, 500)));
    }
    if !i.overview.markets.is_empty() {
        out.push(format!("Markets: {}", i.overview.markets.join(", ")));
    }
    if !i.overview.business_model.is_empty() {
        out.push(format!("Business model: {}", i.overview.business_model));
    }
    let mut section = |label: &str, items: Vec<String>| {
        let items: Vec<String> = items
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .take(8)
            .collect();
        if !items.is_empty() {
            out.push(format!("{label}:"));
            out.extend(
                items
                    .into_iter()
                    .map(|s| format!("- {}", truncate(&s, 220))),
            );
        }
    };
    section(
        "Offerings",
        i.offerings
            .iter()
            .map(|o| {
                if o.description.is_empty() {
                    o.name.clone()
                } else {
                    format!("{}: {}", o.name, o.description)
                }
            })
            .collect(),
    );
    section(
        "Customer segments",
        i.segments
            .iter()
            .map(|s| {
                if s.description.is_empty() {
                    s.name.clone()
                } else {
                    format!("{}: {}", s.name, s.description)
                }
            })
            .collect(),
    );
    section(
        "Competitors",
        i.competitors
            .iter()
            .map(|c| {
                if c.summary.is_empty() {
                    c.name.clone()
                } else {
                    format!("{}: {}", c.name, c.summary)
                }
            })
            .collect(),
    );
    section(
        "Market trends",
        i.trends
            .iter()
            .map(|t| format!("{} ({})", t.title, t.impact))
            .collect(),
    );
    section(
        "Regulation",
        i.regulation
            .iter()
            .map(|r| format!("{} [{}; {}]", r.title, r.jurisdiction, r.status))
            .collect(),
    );
    section(
        "Recent news",
        i.news.iter().map(|n| n.title.clone()).collect(),
    );
    out.join("\n")
}

/// The team's internal context (titles only), for spotting what they're already on, or "".
pub(crate) fn radar_block(context: &[ContextItem]) -> String {
    let items: Vec<String> = context
        .iter()
        .take(40)
        .map(|c| {
            format!(
                "- [{}] {}",
                c.source,
                truncate(
                    if c.title.is_empty() {
                        &c.text
                    } else {
                        &c.title
                    },
                    140
                )
            )
        })
        .collect();
    if items.is_empty() {
        String::new()
    } else {
        format!(
            "Already in the team's own tools (they are on these):\n{}",
            items.join("\n")
        )
    }
}

/// Steering as plain lines, or "" when there is none.
pub(crate) fn steering_block(s: &Steering) -> String {
    let mut out: Vec<String> = Vec::new();
    let notes: Vec<String> = s
        .notes
        .iter()
        .map(|n| truncate(n.trim(), 240))
        .filter(|n| !n.is_empty())
        .take(8)
        .collect();
    if !notes.is_empty() {
        out.push(
            "Their steering notes, most recent first (direct instructions; follow them):".into(),
        );
        out.extend(notes.iter().map(|n| format!("- {n}")));
    }
    let titles = |items: &[String]| -> String {
        items
            .iter()
            .map(|t| truncate(t.trim(), 120))
            .filter(|t| !t.is_empty())
            .take(12)
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join("; ")
    };
    let liked = titles(&s.liked);
    if !liked.is_empty() {
        out.push(format!(
            "They liked or saved (want more like these): {liked}"
        ));
    }
    let disliked = titles(&s.disliked);
    if !disliked.is_empty() {
        out.push(format!(
            "They dismissed or disliked (avoid things like these): {disliked}"
        ));
    }
    out.join("\n")
}

/// "grants, sales leads and partners" from the categories with the most queries.
pub(crate) fn category_phrase(categories: &[String]) -> String {
    let mut counts: Vec<(&str, usize, usize)> = Vec::new();
    for (i, c) in categories.iter().enumerate() {
        match counts.iter_mut().find(|(id, _, _)| *id == c.as_str()) {
            Some(entry) => entry.1 += 1,
            None => counts.push((c.as_str(), 1, i)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    let nouns: Vec<&str> = counts.iter().map(|(id, _, _)| category_noun(id)).collect();
    match nouns.len() {
        0 => "the web".into(),
        1 => nouns[0].into(),
        2 => format!("{} and {}", nouns[0], nouns[1]),
        3 => format!("{}, {} and {}", nouns[0], nouns[1], nouns[2]),
        _ => format!("{}, {}, {} and more", nouns[0], nouns[1], nouns[2]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_block_reads_well() {
        let p = Profile {
            organization: "Acme Cleaning".into(),
            headline: "Commercial cleaning for offices".into(),
            industry: "Commercial cleaning".into(),
            stage: "established".into(),
            website: "https://acme.example".into(),
            location: "Cape Town, South Africa".into(),
            seeking: vec!["sales".into(), "funding".into()],
            ..Default::default()
        };
        let b = profile_block(&p);
        assert!(b.contains("Business: Acme Cleaning"));
        assert!(b.contains("Industry: Commercial cleaning"));
        assert!(b.contains("Stage: Established business"));
        assert!(b.contains("Markets served: national (National)"));
        assert!(
            b.contains("Focus areas: sales (Sales & customers), funding (Funding & incentives)")
        );
        assert!(
            !b.contains("acme.example"),
            "the website never reaches discovery prompts"
        );
    }

    #[test]
    fn intel_and_radar_blocks() {
        let mut i = Intel::default();
        assert_eq!(intel_block(&i), "");
        i.overview.summary = "Acme cleans offices.".into();
        i.competitors.push(model::Competitor {
            name: "Spotless".into(),
            summary: "Office cleaning".into(),
            ..Default::default()
        });
        let b = intel_block(&i);
        assert!(b.contains("Overview: Acme cleans offices."));
        assert!(b.contains("Competitors:\n- Spotless: Office cleaning"));
        assert_eq!(radar_block(&[]), "");
        let c = ContextItem {
            source: "slack".into(),
            title: "Tender for City offices".into(),
            ..Default::default()
        };
        assert!(radar_block(&[c]).contains("- [slack] Tender for City offices"));
    }

    #[test]
    fn steering_and_phrases() {
        assert_eq!(steering_block(&Steering::default()), "");
        let s = Steering {
            notes: vec!["more public tenders".into()],
            liked: vec!["A".into()],
            disliked: vec!["B".into()],
        };
        assert!(steering_block(&s).contains("- more public tenders"));
        let c = |v: &[&str]| category_phrase(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            c(&["funding", "sales", "funding", "partners"]),
            "grants and incentives, sales leads and partners"
        );
        assert_eq!(c(&[]), "the web");
        assert_eq!(
            output_language(&Profile {
                languages: vec!["Español".into()],
                ..Default::default()
            }),
            "Español"
        );
    }
}
