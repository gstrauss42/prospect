//! The art director: one `deepseek-flash` vision call that looks at the candidate previews and
//! picks the most beautiful on-theme background, or none.

use model::ThemeHint;
use model::text::{squash, truncate};
use serde_json::Value;

/// The model's judgement of one batch of photos.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Verdict {
    /// 0–10 per photo, in the order sent (None where the model gave no usable number).
    pub scores: Vec<Option<f64>>,
    /// The chosen photo (0-based), or None when none is good enough.
    pub best: Option<usize>,
    /// One sentence from the model about its choice.
    pub why: String,
}

/// The lowest score a photo needs to be used.
pub const MIN_SCORE: f64 = 6.0;

pub const STAGE: &str = "curate";

fn label(hint: &ThemeHint) -> String {
    let l = squash(&hint.label);
    let l = if l.is_empty() {
        hint.image_queries
            .first()
            .map(|q| squash(q))
            .unwrap_or_default()
    } else {
        l
    };
    truncate(&l, 60)
}

pub fn system_prompt(hint: &ThemeHint) -> String {
    let label = label(hint);
    format!(
        "You are the art director choosing the background photo for someone's personal page about \"{label}\". \
The photo fills the whole screen behind white text, under a dark overlay.

Score every photo from 0 to 10:
- 9-10: stunning and atmospheric, unmistakably about {label}; beautiful light, depth and colour; sharp and professional; still lovely when darkened, with calm areas where text can sit.
- 6-8: good: on topic and attractive, with small flaws.
- 3-5: mediocre: dull, cluttered, awkwardly cropped or only loosely on topic.
- 0-2: unusable.
Score 0-2 for any of these: visible text, captions, watermarks, logos, signs or screenshots; a person's face or a portrait as the main subject; disasters, damage, floods, fire, injury, gore, dead animals or nudity; diagrams, maps, illustrations, documents or scans; blur, noise, low resolution or heavy filters; anything sad, ugly, unsafe or off topic.
This is a hard rule: a photo with any watermark, logo or visible text scores 2 at most, however beautiful it is otherwise.

Reply with JSON only, exactly in this shape:
{{\"scores\": [one integer per photo, in order], \"best\": <number of the best photo, or 0 if none scores {min} or more>, \"why\": \"<one short sentence>\"}}",
        min = MIN_SCORE as u32
    )
}

pub fn user_intro(hint: &ThemeHint, count: usize) -> String {
    let mut s = format!("Theme: {}", label(hint));
    let mood = truncate(&squash(&hint.mood), 240);
    if !mood.is_empty() {
        s.push_str(&format!("\nMood: {mood}"));
    }
    s.push_str(&format!(
        "\nThere are {count} photos, numbered 1 to {count}."
    ));
    s
}

/// A chat request with the previews inline (low detail), labelled "Photo 1", "Photo 2"…
pub fn request(hint: &ThemeHint, images: &[(&'static str, &[u8])]) -> deepseek::ChatRequest {
    let mut req =
        deepseek::ChatRequest::new(STAGE, system_prompt(hint), user_intro(hint, images.len()))
            .json()
            .max_tokens(400)
            .temperature(0.0);
    for (i, (mime, bytes)) in images.iter().enumerate() {
        req = req
            .text(format!("Photo {}:", i + 1))
            .image(*mime, bytes.to_vec());
    }
    req
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_end_matches("/10").trim().parse().ok(),
        _ => None,
    }
    .filter(|f: &f64| f.is_finite())
}

/// Read the model's reply for `count` photos. Tolerates numbers as strings, missing scores and a
/// "best" that contradicts the scores; fails only when the reply has neither a usable "best" nor
/// any scores.
pub fn parse_verdict(v: &Value, count: usize) -> Result<Verdict, String> {
    let scores: Vec<Option<f64>> = match v["scores"].as_array() {
        Some(a) => (0..count)
            .map(|i| a.get(i).and_then(number).map(|s| s.clamp(0.0, 10.0)))
            .collect(),
        None => vec![None; count],
    };
    let why = truncate(&squash(v["why"].as_str().unwrap_or("")), 300);
    let best_raw = number(&v["best"]).map(|b| b.round() as i64);
    let any_score = scores.iter().any(Option::is_some);
    if best_raw.is_none() && !any_score {
        return Err(format!(
            "no usable \"best\" or \"scores\" in {}",
            truncate(&v.to_string(), 200)
        ));
    }
    let argmax = scores
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.map(|s| (i, s)))
        .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)));
    let best = match best_raw {
        Some(b) if b >= 1 && (b as usize) <= count => {
            let i = b as usize - 1;
            // A pick the model itself scored as mediocre isn't used.
            match scores[i] {
                Some(s) if s < MIN_SCORE - 1.0 => None,
                _ => Some(i),
            }
        }
        // "None is good enough" — unless the scores clearly say otherwise.
        _ => argmax
            .filter(|&(_, s)| s >= MIN_SCORE + 1.0)
            .map(|(i, _)| i),
    };
    // The model sometimes names a hard-rule flaw in its pick ("…though a visible watermark is a
    // minor flaw") and keeps it anyway. Its own words win: take the next best good photo instead.
    let lower = why.to_lowercase();
    let admits_flaw = [
        "watermark",
        "logo",
        "caption",
        "visible text",
        "text overlay",
    ]
    .iter()
    .any(|w| lower.contains(w));
    let best = match best {
        Some(b) if admits_flaw => scores
            .iter()
            .enumerate()
            .filter(|&(i, s)| i != b && s.is_some_and(|s| s >= MIN_SCORE))
            .max_by(|a, b| {
                a.1.unwrap_or(0.0)
                    .total_cmp(&b.1.unwrap_or(0.0))
                    .then(b.0.cmp(&a.0))
            })
            .map(|(i, _)| i),
        other => other,
    };
    Ok(Verdict { scores, best, why })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hint() -> ThemeHint {
        ThemeHint {
            label: "Jazz".into(),
            image_queries: vec!["saxophone".into()],
            mood: "Smoky late-night club".into(),
            accent: "#7c3aed".into(),
        }
    }

    #[test]
    fn request_carries_numbered_low_detail_images_in_json_mode() {
        let req = request(
            &hint(),
            &[("image/png", &[1, 2, 3]), ("image/jpeg", &[4, 5])],
        );
        assert_eq!(req.stage, "curate");
        assert!(req.json);
        assert!(req.low_detail_images);
        assert!(req.system.contains("\"Jazz\"") && req.system.contains("JSON"));
        assert!(
            req.system.contains("watermarks")
                && req.system.contains("face")
                && req.system.contains("disasters")
        );
        let texts: Vec<String> = req
            .user
            .iter()
            .filter_map(|p| {
                if let deepseek::Part::Text(t) = p {
                    Some(t.clone())
                } else {
                    None
                }
            })
            .collect();
        assert!(texts[0].contains("Mood: Smoky late-night club") && texts[0].contains("2 photos"));
        assert_eq!(&texts[1..], ["Photo 1:", "Photo 2:"]);
        let images: Vec<&str> = req
            .user
            .iter()
            .filter_map(|p| {
                if let deepseek::Part::Image { mime, .. } = p {
                    Some(mime.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(images, ["image/png", "image/jpeg"]);
    }

    #[test]
    fn a_pick_with_an_admitted_watermark_is_replaced() {
        // Live deepseek-flash reply (Jazz, Nov 2026): it named the watermark and kept the photo.
        let why = "Photo 1 captures the moody, warm low light of a live jazz performance, though slight motion blur and a visible watermark are minor flaws.";
        let v = parse_verdict(
            &json!({ "scores": [7, 4, 5, 3, 6], "best": 1, "why": why }),
            5,
        )
        .unwrap();
        assert_eq!(v.best, Some(4), "the next photo scoring 6 or more");
        let v = parse_verdict(&json!({ "scores": [7, 4, 5], "best": 1, "why": why }), 3).unwrap();
        assert_eq!(v.best, None, "no other good photo: nothing");
        let fine = parse_verdict(
            &json!({ "scores": [7, 4], "best": 1, "why": "Calm, warm and on topic." }),
            2,
        )
        .unwrap();
        assert_eq!(fine.best, Some(0));
        assert!(system_prompt(&hint()).contains("hard rule"));
    }

    #[test]
    fn verdicts() {
        let v = parse_verdict(
            &json!({ "scores": [4, 9, 7], "best": 2, "why": "Warm light." }),
            3,
        )
        .unwrap();
        assert_eq!(v.best, Some(1));
        assert_eq!(v.scores, vec![Some(4.0), Some(9.0), Some(7.0)]);
        assert_eq!(v.why, "Warm light.");
        // Strings, a missing score, and an out-of-range number.
        let v = parse_verdict(&json!({ "scores": ["8", "x"], "best": "1" }), 3).unwrap();
        assert_eq!(v.scores, vec![Some(8.0), None, None]);
        assert_eq!(v.best, Some(0));
        // None is good enough.
        let v = parse_verdict(
            &json!({ "scores": [2, 3], "best": 0, "why": "All cluttered." }),
            2,
        )
        .unwrap();
        assert_eq!(v.best, None);
        assert_eq!(
            parse_verdict(&json!({ "scores": [2, 3], "best": -1 }), 2)
                .unwrap()
                .best,
            None
        );
        // "best: 0" contradicted by a clearly good score.
        assert_eq!(
            parse_verdict(&json!({ "scores": [2, 8.5], "best": 0 }), 2)
                .unwrap()
                .best,
            Some(1)
        );
        // A best out of range falls back to the scores.
        assert_eq!(
            parse_verdict(&json!({ "scores": [9, 3], "best": 7 }), 2)
                .unwrap()
                .best,
            Some(0)
        );
        // A pick the model scored as poor is not used.
        assert_eq!(
            parse_verdict(&json!({ "scores": [3, 2], "best": 1 }), 2)
                .unwrap()
                .best,
            None
        );
        // Only a best.
        assert_eq!(
            parse_verdict(&json!({ "best": 2 }), 2).unwrap().best,
            Some(1)
        );
        // Nothing usable.
        assert!(parse_verdict(&json!({ "answer": "photo two" }), 2).is_err());
        assert!(parse_verdict(&json!({ "scores": [], "best": null }), 2).is_err());
    }
}
