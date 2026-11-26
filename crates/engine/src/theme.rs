//! The look of someone's page: a label, an accent colour that reads on white, photo searches for
//! an atmospheric background, and a line of art direction. Never fails: without the model (or
//! when its answer is unusable) a keyword palette gives a fitting theme deterministically.

use std::sync::atomic::Ordering;

use deepseek::{ChatRequest, Thinking};
use model::{Profile, ThemeHint};
use serde_json::Value;

use crate::util::{clip, list, text, title_case};
use crate::{Ctx, prompt};

/// WCAG AA for normal text.
const MIN_CONTRAST: f64 = 4.5;

struct Palette {
    /// Words that point at this theme. A trailing '*' matches any word starting with it.
    words: &'static [&'static str],
    label: &'static str,
    accent: &'static str,
    queries: [&'static str; 3],
    mood: &'static str,
}

const PALETTE: &[Palette] = &[
    Palette {
        words: &[
            "clean*",
            "hygiene",
            "janitor*",
            "facilit*",
            "pest",
            "laundry",
            "sanitation",
        ],
        label: "Cleaning & Facilities",
        accent: "#0e7490",
        queries: [
            "sunlit modern office atrium",
            "clean glass building at blue hour",
            "calm minimal interior morning light",
        ],
        mood: "Fresh, bright light on clean lines; orderly and calm",
    },
    Palette {
        words: &[
            "steel*",
            "metal*",
            "fabricat*",
            "machining",
            "manufactur*",
            "foundry",
            "welding",
            "industrial",
            "factory",
            "engineering",
        ],
        label: "Manufacturing",
        accent: "#9a3412",
        queries: [
            "steel mill sparks at dusk",
            "industrial port cranes at blue hour",
            "factory floor in warm light",
        ],
        mood: "Strong industrial light, sparks and steel textures; dependable and skilled",
    },
    Palette {
        words: &[
            "bakery",
            "baker",
            "bakers",
            "baking",
            "bread",
            "pastry",
            "patisserie",
            "cake*",
            "confectioner*",
            "sourdough",
        ],
        label: "Baking",
        accent: "#b45309",
        queries: [
            "rustic bakery bread loaves warm light",
            "flour dusted wooden table morning light",
            "wheat field at golden hour",
        ],
        mood: "Warm golden light, flour and wood textures; homely and inviting",
    },
    Palette {
        words: &[
            "chef*",
            "cook*",
            "culinary",
            "food*",
            "cater*",
            "restaurant*",
            "kitchen*",
            "cuisine",
            "hospitality",
            "gastronom*",
            "street food",
            "menu*",
        ],
        label: "Cooking",
        accent: "#c2410c",
        queries: [
            "rustic kitchen fresh herbs warm light",
            "restaurant kitchen at dusk",
            "colourful market produce morning light",
        ],
        mood: "Warm, earthy light with rustic textures; generous and inviting",
    },
    Palette {
        words: &[
            "wine*",
            "vineyard*",
            "winery",
            "sommelier",
            "brew*",
            "coffee",
            "barista",
        ],
        label: "Wine & Coffee",
        accent: "#9f1239",
        queries: [
            "vineyard rows in morning mist",
            "coffee plantation hills at sunrise",
            "oak barrels in a cellar warm light",
        ],
        mood: "Rich, deep tones and soft light; crafted and unhurried",
    },
    Palette {
        words: &[
            "farm*",
            "agricultur*",
            "agri*",
            "crop*",
            "garden*",
            "permaculture",
            "horticultur*",
            "livestock",
            "agronom*",
        ],
        label: "Farming",
        accent: "#15803d",
        queries: [
            "rolling green farmland at sunrise",
            "terraced fields in morning mist",
            "golden wheat field at dusk",
        ],
        mood: "Fresh green and gold, open skies; grounded and hopeful",
    },
    Palette {
        words: &[
            "climate",
            "sustainab*",
            "environment*",
            "renewable*",
            "solar",
            "energy",
            "conservation",
            "ecolog*",
            "wildlife",
            "ocean*",
            "marine",
            "carbon",
            "circular",
        ],
        label: "Climate & Nature",
        accent: "#047857",
        queries: [
            "misty forest at sunrise",
            "wind turbines on hills at golden hour",
            "calm ocean coastline at dawn",
        ],
        mood: "Clear natural light, deep greens and blues; calm and purposeful",
    },
    Palette {
        words: &[
            "ai",
            "ml",
            "machine learning",
            "data",
            "analytics",
            "llm*",
            "artificial intelligence",
            "deep learning",
        ],
        label: "Data & AI",
        accent: "#6d28d9",
        queries: [
            "aurora over mountains at night",
            "city lights at night long exposure",
            "starry sky over a desert",
        ],
        mood: "Deep night blues and violets with points of light; precise and expansive",
    },
    Palette {
        words: &[
            "software",
            "developer*",
            "programm*",
            "code",
            "coding",
            "engineer*",
            "rust",
            "python",
            "javascript",
            "typescript",
            "backend",
            "frontend",
            "devops",
            "cloud",
            "web",
            "tech",
            "technology",
            "saas",
            "cyber*",
            "security",
        ],
        label: "Software",
        accent: "#4338ca",
        queries: [
            "city lights at night long exposure",
            "aerial city at blue hour",
            "mountain ridge under a starry sky",
        ],
        mood: "Cool blue-hour light, clean lines and depth; focused and modern",
    },
    Palette {
        words: &[
            "music*",
            "jazz",
            "piano*",
            "guitar*",
            "singer*",
            "song*",
            "compos*",
            "band",
            "dj",
            "orchestra*",
            "producer",
        ],
        label: "Music",
        accent: "#7c3aed",
        queries: [
            "empty concert hall stage lights",
            "vinyl records warm light",
            "festival field at sunset",
        ],
        mood: "Moody stage light and rich shadows; soulful and alive",
    },
    Palette {
        words: &[
            "film*",
            "video*",
            "cinema*",
            "photograph*",
            "camera",
            "documentar*",
            "animation",
            "animator",
        ],
        label: "Film & Photography",
        accent: "#b91c1c",
        queries: [
            "golden hour landscape through a lens",
            "film set lights at night",
            "dramatic clouds over hills",
        ],
        mood: "Cinematic contrast and golden light; dramatic and story-rich",
    },
    Palette {
        words: &[
            "art",
            "arts",
            "artist*",
            "paint*",
            "illustrat*",
            "design*",
            "graphic*",
            "craft*",
            "sculpt*",
            "ceramic*",
            "pottery",
            "creative*",
        ],
        label: "Art & Design",
        accent: "#be185d",
        queries: [
            "artist studio with natural light",
            "colourful paint texture close up",
            "ceramics studio warm light",
        ],
        mood: "Bright studio light and bold colour; expressive and playful",
    },
    Palette {
        words: &[
            "writ*",
            "author*",
            "journalis*",
            "poet*",
            "editor*",
            "publish*",
            "book*",
            "novel*",
            "storytell*",
        ],
        label: "Writing",
        accent: "#9a3412",
        queries: [
            "quiet library with warm light",
            "desk by a rainy window",
            "old bookshop shelves soft light",
        ],
        mood: "Soft lamplight and paper textures; quiet and thoughtful",
    },
    Palette {
        words: &[
            "fashion*", "clothing", "textile*", "apparel", "beauty", "jewel*", "stylist",
            "tailor*", "sewing",
        ],
        label: "Fashion",
        accent: "#9d174d",
        queries: [
            "draped fabric textures soft light",
            "atelier with sewing patterns",
            "city street at golden hour",
        ],
        mood: "Soft light on rich fabrics; elegant and confident",
    },
    Palette {
        words: &[
            "sport*", "athlet*", "fitness", "football", "soccer", "rugby", "cricket", "running",
            "runner", "coach*", "yoga", "cycling", "gym",
        ],
        label: "Sport",
        accent: "#b91c1c",
        queries: [
            "empty stadium at dusk",
            "running track at sunrise",
            "mountain trail in morning light",
        ],
        mood: "Energetic light and open space; driven and bold",
    },
    Palette {
        words: &[
            "health*",
            "medic*",
            "doctor*",
            "nurs*",
            "clinic*",
            "pharma*",
            "biotech*",
            "wellbeing",
            "wellness",
            "care",
            "therap*",
            "dentist*",
        ],
        label: "Health",
        accent: "#0f766e",
        queries: [
            "calm turquoise sea at dawn",
            "soft morning light over green hills",
            "quiet lake with mist at sunrise",
        ],
        mood: "Soft, clean light and calm water; caring and reassuring",
    },
    Palette {
        words: &[
            "student*",
            "university",
            "scholarship*",
            "graduate*",
            "study",
            "studies",
            "degree",
            "phd",
            "academ*",
        ],
        label: "Study",
        accent: "#1d4ed8",
        queries: [
            "university campus at golden hour",
            "library reading room warm light",
            "sunrise over a city skyline",
        ],
        mood: "Bright, optimistic light; curious and ambitious",
    },
    Palette {
        words: &[
            "teach*",
            "education*",
            "school*",
            "tutor*",
            "lectur*",
            "learning",
        ],
        label: "Education",
        accent: "#1d4ed8",
        queries: [
            "sunlit library shelves",
            "chalk and books on a wooden desk",
            "morning light over a quiet campus",
        ],
        mood: "Warm daylight and open spaces; encouraging and clear",
    },
    Palette {
        words: &[
            "research*",
            "scien*",
            "physic*",
            "chemist*",
            "biolog*",
            "lab",
            "laboratory",
            "astronom*",
            "math*",
        ],
        label: "Science",
        accent: "#1e40af",
        queries: [
            "milky way over mountains",
            "starry night sky over a desert",
            "microscope light abstract blue",
        ],
        mood: "Deep blues and clear night skies; curious and precise",
    },
    Palette {
        words: &[
            "travel*",
            "touris*",
            "hotel*",
            "adventure*",
            "guide",
            "safari",
            "hiking",
        ],
        label: "Travel",
        accent: "#0369a1",
        queries: [
            "winding coastal road at sunset",
            "mountain lake at sunrise",
            "old town rooftops at golden hour",
        ],
        mood: "Golden-hour light and wide horizons; adventurous and free",
    },
    Palette {
        words: &[
            "architect*",
            "construction",
            "building*",
            "interior*",
            "real estate",
            "property",
            "urban*",
            "civil",
        ],
        label: "Architecture",
        accent: "#57534e",
        queries: [
            "modern architecture lines in sunlight",
            "city skyline at dawn",
            "concrete and glass building at dusk",
        ],
        mood: "Clean lines, strong light and shadow; structured and calm",
    },
    Palette {
        words: &[
            "financ*",
            "fintech",
            "account*",
            "invest*",
            "bank*",
            "insurance",
            "economic*",
        ],
        label: "Finance",
        accent: "#166534",
        queries: [
            "city skyline at blue hour",
            "financial district at dawn",
            "calm harbour at sunrise",
        ],
        mood: "Crisp morning light and steady lines; trustworthy and clear",
    },
    Palette {
        words: &[
            "law",
            "legal",
            "lawyer*",
            "attorney*",
            "policy",
            "government",
            "public sector",
            "justice",
        ],
        label: "Law & Policy",
        accent: "#1e3a8a",
        queries: [
            "grand library with columns",
            "historic building at dusk",
            "city hall steps in morning light",
        ],
        mood: "Stately light and classic architecture; principled and steady",
    },
    Palette {
        words: &[
            "nonprofit",
            "non-profit",
            "charity",
            "ngo",
            "community",
            "volunteer*",
            "social impact",
            "activis*",
        ],
        label: "Community",
        accent: "#be123c",
        queries: [
            "warm sunrise over small town rooftops",
            "community garden in morning light",
            "hills and village at golden hour",
        ],
        mood: "Warm, human light; hopeful and together",
    },
    Palette {
        words: &["game*", "gaming", "esport*"],
        label: "Games",
        accent: "#6d28d9",
        queries: [
            "neon city street at night in rain",
            "glowing city skyline at night",
            "aurora over snowy mountains",
        ],
        mood: "Neon glow against deep night; playful and immersive",
    },
    Palette {
        words: &[
            "startup*",
            "founder*",
            "entrepreneur*",
            "business*",
            "venture*",
            "company",
            "retail*",
            "shop*",
            "store",
            "ecommerce",
            "e-commerce",
            "sales",
            "marketing",
            "brand*",
            "smb",
            "sme",
        ],
        label: "Business",
        accent: "#0e7490",
        queries: [
            "city skyline at sunrise",
            "open road through hills at dawn",
            "harbour at golden hour",
        ],
        mood: "Fresh morning light and open horizons; ambitious and clear",
    },
];

const DEFAULT_ACCENT: &str = "#3b5bdb";
const DEFAULT_QUERIES: [&str; 3] = [
    "sunrise over mountains",
    "open road through rolling hills at golden hour",
    "calm sea at dawn",
];
const DEFAULT_MOOD: &str = "Bright, hopeful and spacious; soft morning light";

fn words_of(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn matches(word_list: &[String], joined: &str, kw: &str) -> bool {
    if kw.contains(' ') {
        return joined.contains(kw);
    }
    match kw.strip_suffix('*') {
        Some(prefix) => word_list.iter().any(|w| w.starts_with(prefix)),
        None => word_list.iter().any(|w| w == kw),
    }
}

/// The best palette entry for a profile: headline and interests count double.
fn palette_for(profile: &Profile) -> Option<&'static Palette> {
    let strong = format!(
        "{} {} {}",
        profile.industry,
        profile.headline,
        profile.skills.join(" ")
    );
    let weak = format!(
        "{} {} {}",
        profile.interests.join(" "),
        profile.goals,
        profile.organization
    );
    let (sw, ww) = (words_of(&strong), words_of(&weak));
    let (sj, wj) = (format!(" {} ", sw.join(" ")), format!(" {} ", ww.join(" ")));
    let mut best: Option<(usize, &Palette)> = None;
    for p in PALETTE {
        let score: usize = p
            .words
            .iter()
            .map(|kw| 2 * usize::from(matches(&sw, &sj, kw)) + usize::from(matches(&ww, &wj, kw)))
            .sum();
        if score > 0 && best.is_none_or(|(s, _)| score > s) {
            best = Some((score, p));
        }
    }
    best.map(|(_, p)| p)
}

/// A fitting theme without the model.
pub fn fallback_theme(profile: &Profile) -> ThemeHint {
    match palette_for(profile) {
        Some(p) => ThemeHint {
            label: p.label.into(),
            image_queries: p.queries.iter().map(|q| q.to_string()).collect(),
            mood: p.mood.into(),
            accent: p.accent.into(),
        },
        None => {
            let label = profile
                .interests
                .first()
                .map(|i| short_label(i))
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| "Business".into());
            ThemeHint {
                label,
                image_queries: DEFAULT_QUERIES.iter().map(|q| q.to_string()).collect(),
                mood: DEFAULT_MOOD.into(),
                accent: DEFAULT_ACCENT.into(),
            }
        }
    }
}

/// At most three words and 30 characters, title case.
fn short_label(s: &str) -> String {
    let words: Vec<&str> = s.split_whitespace().take(3).collect();
    let label = title_case(&words.join(" "));
    model::text::truncate(
        label.trim_matches(|c: char| !c.is_alphanumeric() && c != '&'),
        30,
    )
}

fn channel(c: u8) -> f64 {
    let s = f64::from(c) / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

fn rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let p = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    Some((p(0)?, p(2)?, p(4)?))
}

/// Contrast ratio of a colour against white.
pub(crate) fn contrast_on_white(hex: &str) -> f64 {
    match rgb(hex) {
        Some((r, g, b)) => {
            1.05 / (0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b) + 0.05)
        }
        None => 1.0,
    }
}

/// The colour darkened (keeping its hue) until text in it reads on white.
pub(crate) fn readable(hex: &str) -> String {
    let Some((mut r, mut g, mut b)) = rgb(hex) else {
        return DEFAULT_ACCENT.into();
    };
    let to_hex = |r: u8, g: u8, b: u8| format!("#{r:02x}{g:02x}{b:02x}");
    for _ in 0..40 {
        if contrast_on_white(&to_hex(r, g, b)) >= MIN_CONTRAST {
            break;
        }
        let f = |c: u8| (f64::from(c) * 0.93).floor() as u8;
        (r, g, b) = (f(r), f(g), f(b));
    }
    to_hex(r, g, b)
}

const SYSTEM: &str = r##"You art-direct the dashboard of a business-intelligence service for one business. From the business's profile (its industry and what it offers), choose the theme of its page. Reply in json.

- label: the business's world in 1 to 3 words, title case: "Commercial Cleaning", "Steel Fabrication", "Retail Analytics", "Hospitality".
- accent: one rich colour as #rrggbb that suits the subject and reads well as text on white (contrast at least 4.5:1, so a mid-to-deep tone; no neon, pastel, grey or near-black).
- image_queries: 2 to 4 short photo searches (2 to 5 words each) for a stunning, atmospheric, wide background with no text or logos. Describe objects, scenes and places from its industry, never people: "stainless steel commercial kitchen", "wind turbines at dusk", "warehouse shelving aisles", "vintage stage microphone", "city lights at night long exposure". No people, workers, faces, hands or portraits: searches about people return portraits, which make poor backgrounds.
- mood: one sentence of art direction for choosing between photos ("Warm, earthy light with rustic textures; calm and inviting").

Reply with json only, in exactly this shape:
{"label": "Cooking", "accent": "#c2410c", "image_queries": ["rustic kitchen herbs warm light", "restaurant kitchen at dusk"], "mood": "Warm, earthy light with rustic textures; calm and inviting"}"##;

pub(crate) fn request(today: &str, profile: &Profile) -> ChatRequest {
    let user = format!(
        "Today is {today}.\n\nThe business:\n{}",
        prompt::profile_block(profile)
    );
    ChatRequest::new("theme", SYSTEM, user)
        .json()
        .thinking(Thinking::Off)
        .temperature(0.8)
        .max_tokens(600)
}

/// True when a photo search is about people (those return portraits, poor backgrounds).
pub(crate) fn mentions_people(q: &str) -> bool {
    const PEOPLE: &[&str] = &[
        "person",
        "people",
        "man",
        "men",
        "woman",
        "women",
        "child",
        "children",
        "kid",
        "kids",
        "face",
        "faces",
        "portrait",
        "portraits",
        "hands",
        "worker",
        "workers",
        "team",
        "staff",
        "employee",
        "employees",
        "chef",
        "chefs",
        "engineer",
        "engineers",
        "farmer",
        "farmers",
        "student",
        "students",
        "crowd",
        "family",
        "customer",
        "customers",
        "businessman",
        "businesswoman",
        "doctor",
        "nurse",
        "artist",
        "musician",
        "athlete",
    ];
    q.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| PEOPLE.contains(&w))
}

/// The model's theme, validated and completed from the fallback.
pub(crate) fn validate(v: &Value, profile: &Profile) -> ThemeHint {
    let fb = fallback_theme(profile);
    let label = Some(short_label(&clip(&text(v, "label"), 60)))
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| fb.label.clone());
    let accent = readable(&model::sanitize_hex_color(&text(v, "accent"), &fb.accent));
    let mut queries: Vec<String> = Vec::new();
    for q in list(v, "image_queries")
        .into_iter()
        .chain(list(v, "queries"))
    {
        let q = clip(&q, 60).trim_end_matches('…').trim().to_string();
        if !mentions_people(&q)
            && !queries.iter().any(|x| x.eq_ignore_ascii_case(&q))
            && !q.is_empty()
        {
            queries.push(q);
        }
        if queries.len() >= 4 {
            break;
        }
    }
    for q in &fb.image_queries {
        if queries.len() >= 2 {
            break;
        }
        if !queries.iter().any(|x| x.eq_ignore_ascii_case(q)) {
            queries.push(q.clone());
        }
    }
    let mood = Some(clip(&text(v, "mood"), 200))
        .filter(|m| !m.is_empty())
        .unwrap_or(fb.mood);
    ThemeHint {
        label,
        image_queries: queries,
        mood,
        accent,
    }
}

/// The page theme for a profile. Never fails.
pub(crate) fn hint(ctx: &Ctx, profile: &Profile) -> ThemeHint {
    if ctx.cancel.load(Ordering::Relaxed) {
        return fallback_theme(profile);
    }
    match ctx.ai.chat_json_value(&request(&ctx.today, profile)) {
        Ok(v) => validate(&v, profile),
        Err(_) => fallback_theme(profile),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(headline: &str, interests: &[&str]) -> Profile {
        Profile {
            headline: headline.into(),
            interests: interests.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn palette_accents_read_on_white() {
        for e in PALETTE {
            assert!(
                contrast_on_white(e.accent) >= MIN_CONTRAST,
                "{} {} = {:.2}",
                e.label,
                e.accent,
                contrast_on_white(e.accent)
            );
            assert_eq!(model::sanitize_hex_color(e.accent, "x"), e.accent);
        }
        assert!(contrast_on_white(DEFAULT_ACCENT) >= MIN_CONTRAST);
    }

    #[test]
    fn fallback_picks_by_keywords() {
        assert_eq!(
            fallback_theme(&p(
                "Chef and caterer starting a food business",
                &["street food"]
            ))
            .label,
            "Cooking"
        );
        assert_eq!(
            fallback_theme(&p("Family bakery with 12 staff", &["sourdough"])).label,
            "Baking"
        );
        assert_eq!(
            fallback_theme(&p(
                "Senior software engineer",
                &["Rust", "distributed systems"]
            ))
            .label,
            "Software"
        );
        assert_eq!(
            fallback_theme(&p("Final-year student", &["scholarships"])).label,
            "Study"
        );
        assert_eq!(
            fallback_theme(&p("", &["machine learning"])).label,
            "Data & AI"
        );
        assert_eq!(fallback_theme(&p("Startup founder", &[])).label, "Business");
        // "art" must not match inside "start" or "smart".
        assert_ne!(fallback_theme(&p("Smart start", &[])).label, "Art & Design");
        let unknown = fallback_theme(&p("", &["origami boats"]));
        assert_eq!(unknown.label, "Origami Boats");
        assert_eq!(unknown.accent, DEFAULT_ACCENT);
        assert_eq!(fallback_theme(&Profile::default()).label, "Business");
        for t in [
            fallback_theme(&Profile::default()),
            fallback_theme(&p("Chef", &[])),
        ] {
            assert!((2..=4).contains(&t.image_queries.len()));
            assert!(!t.mood.is_empty());
        }
    }

    #[test]
    fn readable_darkens_light_colours() {
        let c = readable("#ffd43b");
        assert!(contrast_on_white(&c) >= MIN_CONTRAST, "{c}");
        assert_eq!(
            readable("#1d4ed8"),
            "#1d4ed8",
            "already readable colours are kept"
        );
        assert_eq!(readable("nonsense"), DEFAULT_ACCENT);
    }

    #[test]
    fn validate_cleans_model_output() {
        let chef = p("Chef", &["street food"]);
        let t = validate(
            &json!({"label": "cooking with fire and smoke", "accent": "#FFB000", "image_queries": ["a", "A", "smoky grill at dusk", "", "x".repeat(100), "five", "six"], "mood": ""}),
            &chef,
        );
        assert_eq!(t.label, "Cooking With Fire");
        assert!(contrast_on_white(&t.accent) >= MIN_CONTRAST);
        assert!(t.accent.starts_with('#') && t.accent.len() == 7);
        assert_eq!(t.image_queries.len(), 4);
        assert_eq!(t.image_queries[0], "a");
        assert_eq!(t.image_queries[1], "smoky grill at dusk");
        assert!(t.image_queries[2].chars().count() <= 60);
        assert!(!t.mood.is_empty(), "mood falls back");
        let bad = validate(&json!({"accent": "red", "image_queries": []}), &chef);
        assert_eq!(bad.label, "Cooking");
        assert_eq!(bad.accent, "#c2410c");
        assert_eq!(
            bad.image_queries.len(),
            2,
            "topped up to two from the fallback"
        );
    }
}
