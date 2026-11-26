//! Internal opportunities from the team's connected tools (Slack, Microsoft 365, Confluence).
//!
//! One `context` call reads the recent items, numbered, and returns the concrete openings they
//! contain, each referring to its item **by number**, so the opportunity links back to that exact
//! message, email or page (`url`) and records its connector (`source`). Chatter, opinions and
//! rumours are never treated as fact: the card says what was said and by whom.

use deepseek::{ChatRequest, Thinking};
use model::{ContextItem, Intel, Opportunity, Profile};
use serde_json::Value;

use crate::util::{array, clip, excerpt, number, parse_number, text};
use crate::{Error, prompt};

const SYSTEM: &str = r#"You read recent items from a business's own work tools (chat messages, emails, calendar invites, wiki pages) and pick out the concrete opportunities in them for the business. Reply in json.

An opportunity is something the business could act on to win or grow: a customer or prospect asking for a quote, proposal or demo; an RFP, tender or expression of interest received or mentioned with details; a partner or reseller proposal; an introduction offered; an event, award or speaking invitation; a grant or programme someone flagged; a renewal, expansion or upsell signal from a customer.
Not opportunities: routine tasks, internal admin, social chatter, complaints with nothing to win, newsletters and automated notifications.

Hard rules:
- Never treat chatter, opinions, guesses or rumours as fact. Say what was said and by whom ("Thandi said in #sales that Shoprite may re-tender its cleaning contract in March"), and keep the score low when it's only talk.
- Use only what the item says. Leave unknown fields "".
- Each pick refers to exactly one item by its number (C3).

For each pick:
- item: the item number, exactly as listed.
- title: specific, at most 90 characters ("Quote request: deep clean for Growthpoint's Century City offices").
- category: one focus area id from the list below.
- summary: one or two sentences: what was said or asked, by whom, where.
- why: one sentence on why it matters for this business, using what we know about it.
- action: the concrete next step ("Reply to Thandi's thread with a quote by Friday").
- deadline: YYYY-MM-DD if a date is stated, else "".
- value: money mentioned, else "".
- score: 0 to 100 (firm requests with details score high; hearsay low).

Reply with json only, in exactly this shape:
{"opportunities": [{"item": "C3", "title": "", "category": "sales", "summary": "", "why": "", "action": "", "deadline": "", "value": "", "score": 70}]}
If there are none, reply {"opportunities": []}.

Focus areas (id: label (what it covers)):
"#;

fn item_block(i: usize, c: &ContextItem) -> String {
    let mut head = format!(
        "[C{}] {} {}",
        i + 1,
        c.source,
        if c.kind.is_empty() { "item" } else { &c.kind }
    );
    if let Some(d) = &c.date {
        head.push_str(&format!(" | {d}"));
    }
    if !c.container.is_empty() {
        head.push_str(&format!(" | in {}", c.container));
    }
    if !c.author.is_empty() {
        head.push_str(&format!(" | by {}", c.author));
    }
    let title = if c.title.is_empty() {
        String::new()
    } else {
        format!("\nTitle: {}", c.title)
    };
    format!("{head}{title}\n{}", excerpt(&c.text, 700, &[]))
}

pub(crate) fn request(
    today: &str,
    profile: &Profile,
    intel: &Intel,
    items: &[ContextItem],
) -> ChatRequest {
    let mut user = format!(
        "Today is {today}.\n\nThe business:\n{}",
        prompt::profile_block(profile)
    );
    let dossier = prompt::intel_block(intel);
    if !dossier.is_empty() {
        user.push_str("\n\nWhat research established about it:\n");
        user.push_str(&dossier);
    }
    user.push_str("\n\nItems from its tools:\n");
    for (i, c) in items.iter().enumerate() {
        user.push('\n');
        user.push_str(&item_block(i, c));
        user.push('\n');
    }
    ChatRequest::new(
        "context",
        format!("{SYSTEM}{}", prompt::categories_block()),
        user,
    )
    .json()
    .thinking(Thinking::Low)
    .max_tokens(8000)
}

fn site_label(source: &str) -> &'static str {
    match source {
        "slack" => "Slack",
        "microsoft" => "Microsoft 365",
        "confluence" => "Confluence",
        _ => "Your tools",
    }
}

/// Validated internal opportunities: each refers to a real item, links to it and records its
/// connector.
pub(crate) fn parse(v: &Value, items: &[ContextItem], today: &str) -> Vec<Opportunity> {
    let mut out: Vec<Opportunity> = Vec::new();
    let mut used: Vec<usize> = Vec::new();
    for o in array(v, &["opportunities", "picks"]) {
        let n = match o.get("item") {
            Some(Value::String(s)) => parse_number(s.trim().trim_start_matches(['C', 'c'])),
            Some(Value::Number(n)) => n.as_f64(),
            _ => None,
        };
        let Some(n) = n.filter(|n| *n >= 1.0 && n.fract() == 0.0) else {
            continue;
        };
        let idx = n as usize - 1;
        let Some(item) = items.get(idx) else { continue };
        if used.contains(&idx) {
            continue;
        }
        let score = number(o, "score")
            .map(|s| s.round().clamp(0.0, 100.0) as u8)
            .unwrap_or(50);
        let category = {
            let c = text(o, "category").to_ascii_lowercase();
            if model::is_opportunity_type(&c) {
                c
            } else {
                "other".into()
            }
        };
        let title = Some(clip(&text(o, "title"), 140))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| clip(&item.title, 140));
        let opp = Opportunity {
            url: item.url.clone(),
            title,
            organization: String::new(),
            category,
            summary: clip(&text(o, "summary"), 420),
            why: clip(&text(o, "why"), 300),
            action: clip(&text(o, "action"), 200),
            location: String::new(),
            deadline: model::date::ymd_prefix(&text(o, "deadline")),
            value: clip(&text(o, "value"), 80),
            score,
            published: item.date.clone(),
            site_name: site_label(&item.source).into(),
            site_icon: String::new(),
            verified: false,
            source: item.source.clone(),
        };
        if let Some(opp) = opp.sanitized(today) {
            used.push(idx);
            out.push(opp);
        }
    }
    out
}

/// Internal opportunities in the team's context; none when there is no context.
pub(crate) fn extract(
    ai: &deepseek::Client,
    today: &str,
    profile: &Profile,
    intel: &Intel,
    items: &[ContextItem],
) -> Result<Vec<Opportunity>, Error> {
    let items: Vec<ContextItem> = items
        .iter()
        .cloned()
        .filter_map(ContextItem::sanitized)
        .take(60)
        .collect();
    if items.is_empty() {
        return Ok(Vec::new());
    }
    match ai.chat_json_value(&request(today, profile, intel, &items)) {
        Ok(v) => Ok(parse(&v, &items, today)),
        Err(e) if e.is_account_problem() => Err(Error::Ai(e)),
        Err(_) => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(source: &str, url: &str, title: &str) -> ContextItem {
        ContextItem {
            source: source.into(),
            kind: "message".into(),
            title: title.into(),
            text: "Can you quote for a deep clean?".into(),
            author: "Thandi".into(),
            container: "#sales".into(),
            url: url.into(),
            date: Some("2026-11-20".into()),
        }
    }

    #[test]
    fn parse_links_back_to_items() {
        let items = vec![
            item(
                "slack",
                "https://acme.slack.com/archives/C1/p1",
                "Quote request",
            ),
            item("microsoft", "https://outlook.office.com/mail/id/2", "RFP"),
        ];
        let v = json!({"opportunities": [
            {"item": "C2", "title": "RFP for cleaning", "category": "sales", "summary": "An RFP arrived.", "why": "Fits.", "action": "Reply", "deadline": "2027-01-10", "score": 80},
            {"item": "C2", "title": "Duplicate", "score": 90},
            {"item": "C9", "title": "Out of range", "score": 90},
            {"item": 1, "title": "Quote request: deep clean", "category": "bogus", "deadline": "2025-01-01", "score": 70},
            {"title": "No item", "score": 70}
        ]});
        let out = parse(&v, &items, "2026-11-26");
        assert_eq!(out.len(), 1, "expired and invalid picks dropped: {out:?}");
        assert_eq!(out[0].url, "https://outlook.office.com/mail/id/2");
        assert_eq!(out[0].source, "microsoft");
        assert_eq!(out[0].site_name, "Microsoft 365");
        assert_eq!(out[0].published.as_deref(), Some("2026-11-20"));
        assert!(!out[0].verified);
    }

    #[test]
    fn request_numbers_items() {
        let r = request(
            "2026-11-26",
            &Profile {
                organization: "Acme".into(),
                ..Default::default()
            },
            &Intel::default(),
            &[item("slack", "https://acme.slack.com/x", "Quote")],
        );
        let user = match &r.user[0] {
            deepseek::Part::Text(t) => t.clone(),
            _ => String::new(),
        };
        assert_eq!(r.stage, "context");
        assert!(
            user.contains("[C1] slack message | 2026-11-20 | in #sales | by Thandi\nTitle: Quote")
        );
        assert!(r.system.contains("Never treat chatter"));
    }
}
