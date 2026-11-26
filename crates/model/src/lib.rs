//! Shared types and small utilities for Opportunity.
//!
//! Everything that crosses a crate boundary (the profile, a found opportunity, a theme, run
//! progress) is defined here so the engine, the server, the mailer and the web client agree on
//! one shape. The serde form of these types *is* the JSON the HTTP API speaks (see docs/API.md).
//!
//! No dependencies beyond serde: dates, URL normalisation and text helpers are hand-rolled.

pub mod date;
pub mod links;
pub mod text;

use serde::{Deserialize, Serialize};

use crate::text::{clean_list, squash, truncate};

// ---------------------------------------------------------------------------------------------
// Taxonomy
// ---------------------------------------------------------------------------------------------

/// One kind of opportunity. Ids are stable: they are stored in the database and used by the web
/// client for filters.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct OpportunityType {
    pub id: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

/// The focus areas a business finds opportunities in. Ids are stable: stored in the database and
/// used by the web client for filters.
pub const OPPORTUNITY_TYPES: &[OpportunityType] = &[
    OpportunityType {
        id: "sales",
        label: "Sales & customers",
        hint: "Leads, tenders, RFPs and buying signals",
    },
    OpportunityType {
        id: "partners",
        label: "Partnerships & channels",
        hint: "Partners, resellers, integrations and distribution",
    },
    OpportunityType {
        id: "funding",
        label: "Funding & incentives",
        hint: "Grants, tax incentives, public programmes and investors",
    },
    OpportunityType {
        id: "regulation",
        label: "Regulation & compliance",
        hint: "New rules, deadlines and the demand they create",
    },
    OpportunityType {
        id: "market",
        label: "Market & expansion",
        hint: "New segments, regions, products and competitor gaps",
    },
    OpportunityType {
        id: "visibility",
        label: "Events, awards & media",
        hint: "Trade shows, awards, speaking slots and press",
    },
    OpportunityType {
        id: "talent",
        label: "Talent & skills",
        hint: "Hiring pools, training subsidies and skills programmes",
    },
    OpportunityType {
        id: "operations",
        label: "Operations & suppliers",
        hint: "Suppliers, cost savings and technology",
    },
    OpportunityType {
        id: "other",
        label: "Other",
        hint: "Anything else worth acting on",
    },
];

/// Where a business is.
pub const STAGES: &[(&str, &str)] = &[
    ("startup", "Startup"),
    ("growth", "Scale-up"),
    ("established", "Established business"),
    ("enterprise", "Enterprise"),
    ("nonprofit", "Non-profit"),
    ("public", "Public sector"),
];

/// The markets a business serves.
pub const REACH: &[(&str, &str)] = &[
    ("local", "Local"),
    ("national", "National"),
    ("international", "International"),
];

pub fn is_opportunity_type(id: &str) -> bool {
    OPPORTUNITY_TYPES.iter().any(|t| t.id == id)
}

pub fn opportunity_type_label(id: &str) -> &'static str {
    OPPORTUNITY_TYPES
        .iter()
        .find(|t| t.id == id)
        .map(|t| t.label)
        .unwrap_or("Opportunity")
}

pub fn is_stage(id: &str) -> bool {
    STAGES.iter().any(|(s, _)| *s == id)
}

pub fn is_reach(id: &str) -> bool {
    REACH.iter().any(|(r, _)| *r == id)
}

// ---------------------------------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------------------------------

/// The business: who it is and what it wants. One per account. Research drafts it from the name
/// and website; the account confirms it. (The detailed, sourced picture lives in [`Intel`].)
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// What the business does, in one line: "Commercial cleaning for offices in Cape Town".
    pub headline: String,
    /// A [`STAGES`] id, or empty.
    pub stage: String,
    /// The business's name.
    pub organization: String,
    /// The business's website.
    pub website: String,
    /// The industry in plain words: "Commercial cleaning", "B2B payments software".
    pub industry: String,
    /// Headquarters: "City, Country".
    pub location: String,
    /// A [`REACH`] id: the markets it serves.
    pub reach: String,
    /// Focus areas ([`OPPORTUNITY_TYPES`] ids) to look in. Empty means all of them.
    pub seeking: Vec<String>,
    /// Markets, sectors and topics the business cares about.
    pub interests: Vec<String>,
    /// Its products, services and capabilities.
    pub skills: Vec<String>,
    /// Its goals and priorities, in its own words.
    pub goals: String,
    /// What to avoid: markets, deal types, anything off-strategy.
    pub avoid: String,
    /// Other public links (LinkedIn page, social).
    pub links: Vec<String>,
    /// Languages it works in. Empty means English.
    pub languages: Vec<String>,
    /// Set by research; None when filled in by hand.
    pub research: Option<Research>,
}

/// What auto-onboarding found, kept with the profile so the person can see where it came from.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Research {
    /// 0.0 to 1.0: how sure the research is that it found the right person.
    pub confidence: f32,
    /// A short plain-English note for the person ("We found you at HeyHi in Cape Town…").
    pub summary: String,
    /// Pages the research relied on.
    pub sources: Vec<String>,
    /// Unix seconds.
    pub researched_at: i64,
}

impl Profile {
    /// The same profile with every field trimmed, length-capped and limited to known ids.
    /// Apply to anything that comes from a person or a model before storing it.
    pub fn sanitized(mut self) -> Profile {
        self.headline = truncate(&squash(&self.headline), 200);
        self.stage = if is_stage(self.stage.trim()) {
            self.stage.trim().to_string()
        } else {
            String::new()
        };
        self.organization = truncate(&squash(&self.organization), 120);
        self.website = if links::is_http_url(self.website.trim()) {
            truncate(self.website.trim(), 300)
        } else {
            String::new()
        };
        self.industry = truncate(&squash(&self.industry), 80);
        self.location = truncate(&squash(&self.location), 120);
        self.reach = if is_reach(self.reach.trim()) {
            self.reach.trim().to_string()
        } else {
            String::new()
        };
        let mut seeking: Vec<String> = Vec::new();
        for id in self.seeking.iter().map(|s| s.trim().to_lowercase()) {
            if is_opportunity_type(&id) && !seeking.contains(&id) {
                seeking.push(id);
            }
        }
        self.seeking = seeking;
        self.interests = clean_list(self.interests, 20, 60);
        self.skills = clean_list(self.skills, 20, 60);
        self.goals = truncate(self.goals.trim(), 1000);
        self.avoid = truncate(self.avoid.trim(), 600);
        self.links = clean_list(self.links, 10, 300)
            .into_iter()
            .filter(|l| links::is_http_url(l))
            .collect();
        self.languages = clean_list(self.languages, 8, 40);
        if let Some(r) = self.research.as_mut() {
            r.confidence = if r.confidence.is_finite() {
                r.confidence.clamp(0.0, 1.0)
            } else {
                0.0
            };
            r.summary = truncate(r.summary.trim(), 600);
            r.sources = clean_list(std::mem::take(&mut r.sources), 12, 500)
                .into_iter()
                .filter(|l| links::is_http_url(l))
                .collect();
        }
        self
    }

    /// True when there is too little to search with.
    pub fn is_empty(&self) -> bool {
        self.headline.trim().is_empty()
            && self.industry.trim().is_empty()
            && self.interests.is_empty()
            && self.skills.is_empty()
            && self.goals.trim().is_empty()
            && self.seeking.is_empty()
    }
}

// ---------------------------------------------------------------------------------------------
// Opportunities
// ---------------------------------------------------------------------------------------------

/// One opportunity found for someone, as the engine produces it. The server stores it and adds
/// its own bookkeeping (id, status, first seen…) in the API.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Opportunity {
    pub url: String,
    /// Specific and scannable: "Bryan Montgomery Bursary: R35 000 towards culinary tuition".
    pub title: String,
    pub organization: String,
    /// An [`OPPORTUNITY_TYPES`] id.
    pub category: String,
    /// What it is, in one or two sentences.
    pub summary: String,
    /// Why it fits this person, one sentence addressed to them.
    pub why: String,
    /// The next step: "Apply online by 15 January".
    pub action: String,
    /// "Cape Town, South Africa", "Remote", "Global".
    pub location: String,
    /// Closing date as YYYY-MM-DD, if known.
    pub deadline: Option<String>,
    /// Money or value involved ("R35 000", "$10k prize", "Paid role"), empty if unknown.
    pub value: String,
    /// 0 to 100: how well it matches the person.
    pub score: u8,
    /// Publication date of the source page as YYYY-MM-DD, if known.
    pub published: Option<String>,
    pub site_name: String,
    pub site_icon: String,
    /// True when the page itself was read and confirmed open and relevant.
    pub verified: bool,
    /// Where it was found: empty or "web" for the public web, otherwise the connector it came
    /// from ("slack", "microsoft", "confluence") — then `url` opens it in that tool.
    pub source: String,
}

impl Opportunity {
    /// The same opportunity cleaned up, or None when it can't be shown: no usable URL or title,
    /// or a deadline before `today` (YYYY-MM-DD).
    pub fn sanitized(mut self, today: &str) -> Option<Opportunity> {
        self.url = self.url.trim().to_string();
        if !links::is_http_url(&self.url) || self.url.len() > 2000 {
            return None;
        }
        self.title = truncate(&squash(&self.title), 160);
        if self.title.is_empty() {
            return None;
        }
        self.organization = truncate(&squash(&self.organization), 120);
        let cat = self.category.trim().to_lowercase();
        self.category = if is_opportunity_type(&cat) {
            cat
        } else {
            "other".to_string()
        };
        self.summary = truncate(&squash(&self.summary), 420);
        self.why = truncate(&squash(&self.why), 300);
        self.action = truncate(&squash(&self.action), 200);
        self.location = truncate(&squash(&self.location), 100);
        self.value = truncate(&squash(&self.value), 80);
        self.deadline = self.deadline.as_deref().and_then(date::ymd_prefix);
        if let (Some(d), Some(t)) = (
            self.deadline.as_deref().and_then(date::parse_ymd),
            date::parse_ymd(today),
        ) && d < t
        {
            return None;
        }
        self.published = self.published.as_deref().and_then(date::ymd_prefix);
        self.score = self.score.min(100);
        self.site_name = truncate(&squash(&self.site_name), 80);
        self.site_icon = if links::is_http_url(self.site_icon.trim()) {
            self.site_icon.trim().to_string()
        } else {
            String::new()
        };
        let source = self.source.trim().to_ascii_lowercase();
        self.source = if CONTEXT_SOURCES.contains(&source.as_str()) {
            source
        } else {
            String::new()
        };
        Some(self)
    }

    /// The dedupe key: the normalised URL.
    pub fn key(&self) -> Option<String> {
        links::normalize_url(&self.url)
    }
}

/// An opportunity the person already has, given to the engine so it can avoid repeats and judge
/// what is no longer relevant.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KnownOpportunity {
    /// [`links::normalize_url`] of its URL.
    pub key: String,
    pub title: String,
    pub category: String,
    /// "active", "saved" or "dismissed".
    pub status: String,
    pub deadline: Option<String>,
}

/// The connectors a [`ContextItem`] or an [`Opportunity`] can come from.
pub const CONTEXT_SOURCES: &[&str] = &["slack", "microsoft", "confluence"];

/// Something from a person's connected work tools — a Slack message, an email or calendar
/// invite, a Confluence page — reduced to what the engine needs. Fetched read-only at run time
/// and never stored; only opportunities extracted from it are.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextItem {
    /// One of [`CONTEXT_SOURCES`].
    pub source: String,
    /// "message", "email", "event" or "page".
    pub kind: String,
    /// Subject, page title or a short description.
    pub title: String,
    /// The text, trimmed to what matters.
    pub text: String,
    /// Who wrote or sent it (display name), may be empty.
    pub author: String,
    /// Where it lives: channel, mail folder, space. May be empty.
    pub container: String,
    /// A link that opens it in the tool (Slack permalink, Outlook webLink, page URL).
    pub url: String,
    /// When it was written or received, YYYY-MM-DD.
    pub date: Option<String>,
}

impl ContextItem {
    /// The same item trimmed and capped, or None without a known source, a usable link or text.
    pub fn sanitized(mut self) -> Option<ContextItem> {
        self.source = self.source.trim().to_ascii_lowercase();
        if !CONTEXT_SOURCES.contains(&self.source.as_str()) {
            return None;
        }
        self.url = self.url.trim().to_string();
        if !links::is_http_url(&self.url) || self.url.len() > 2000 {
            return None;
        }
        self.kind = truncate(&squash(&self.kind), 20);
        self.title = truncate(&squash(&self.title), 200);
        self.text = truncate(self.text.trim(), 2000);
        if self.title.is_empty() && self.text.is_empty() {
            return None;
        }
        self.author = truncate(&squash(&self.author), 80);
        self.container = truncate(&squash(&self.container), 80);
        self.date = self.date.as_deref().and_then(date::ymd_prefix);
        Some(self)
    }
}

/// A chat model people can choose. Search always runs on DeepSeek's Search API; this is the model
/// that plans, judges and writes.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct AiModel {
    /// "deepseek" or "bedrock".
    pub provider: &'static str,
    /// The model id to send; empty for Bedrock, where the person gives their own model id or ARN.
    pub id: &'static str,
    pub label: &'static str,
    pub note: &'static str,
}

/// The choices in the model selector. The first is the default.
pub const AI_MODELS: &[AiModel] = &[
    AiModel {
        provider: "deepseek",
        id: "deepseek-flash",
        label: "DeepSeek Flash",
        note: "Fast and inexpensive. The default.",
    },
    AiModel {
        provider: "deepseek",
        id: "deepseek-v4-pro",
        label: "DeepSeek V4 Pro",
        note: "Deeper reasoning, at about four times the cost.",
    },
    AiModel {
        provider: "bedrock",
        id: "",
        label: "Claude on Amazon Bedrock",
        note: "Runs in your own AWS account, with your model id or inference profile ARN.",
    },
];

/// What the person has told us, used to steer the next search.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Steering {
    /// Their own words, most recent first ("more remote roles", "nothing academic").
    pub notes: Vec<String>,
    /// Titles they saved or gave a thumbs-up.
    pub liked: Vec<String>,
    /// Titles they dismissed or gave a thumbs-down.
    pub disliked: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------------------------

/// The look of someone's page, chosen by the engine from their profile.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeHint {
    /// What the page is about in one to three words: "Cooking", "Climate tech", "Jazz".
    pub label: String,
    /// Two to four short photo searches for a background: "chef plating food".
    pub image_queries: Vec<String>,
    /// One sentence of art direction for choosing between photos.
    pub mood: String,
    /// Accent colour as #rrggbb that suits the theme and reads on white.
    pub accent: String,
}

/// A chosen background photo. The bytes are stored by the server; this is its description.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeImage {
    /// "image/jpeg", "image/png" or "image/webp".
    pub mime: String,
    pub width: u32,
    pub height: u32,
    /// Attribution: "Photo by Jane Doe · CC BY 2.0 · via Openverse".
    pub credit: String,
    /// The page the photo came from, for the credit link.
    pub source_url: String,
}

/// Validate a #rrggbb colour, falling back to `fallback`.
pub fn sanitize_hex_color(c: &str, fallback: &str) -> String {
    let c = c.trim();
    let ok = c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|ch| ch.is_ascii_hexdigit());
    if ok {
        c.to_ascii_lowercase()
    } else {
        fallback.to_string()
    }
}

// ---------------------------------------------------------------------------------------------
// Business intelligence (the tabs)
// ---------------------------------------------------------------------------------------------

/// Where a piece of intelligence came from. Every item in [`Intel`] carries one: no source, no item.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    pub url: String,
    pub title: String,
    /// When it was published (or read), YYYY-MM-DD.
    pub date: Option<String>,
}

impl Source {
    fn clean(mut self) -> Option<Source> {
        self.url = self.url.trim().to_string();
        if !links::is_http_url(&self.url) || self.url.len() > 2000 {
            return None;
        }
        self.title = truncate(&squash(&self.title), 160);
        self.date = self.date.as_deref().and_then(date::ymd_prefix);
        Some(self)
    }
}

/// The business intelligence dossier: what research established about a business, section by
/// section, every item sourced. It is the ground truth opportunities are found from, and what the
/// dashboard's tabs show. Only what public sources support goes in (the business's own website
/// first); a section with nothing reliable stays empty and its tab isn't shown.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Intel {
    pub overview: Overview,
    /// Products and services, from the business's own site.
    pub offerings: Vec<Offering>,
    /// Who it serves, as segments (never customer lists).
    pub segments: Vec<Segment>,
    pub competitors: Vec<Competitor>,
    /// Market trends and drivers.
    pub trends: Vec<Trend>,
    /// Rules and changes in the rules that matter to it.
    pub regulation: Vec<Regulation>,
    /// Recent developments about the business itself.
    pub news: Vec<NewsItem>,
    /// 0.0 to 1.0: how sure research is overall (right business, facts that hold).
    pub confidence: f32,
    /// Unix seconds.
    pub updated_at: i64,
}

/// The company snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Overview {
    /// Two or three sentences: what the business does, for whom, where.
    pub summary: String,
    pub industry: String,
    pub headquarters: String,
    /// Year founded, empty if unknown.
    pub founded: String,
    /// Employee band such as "11-50 employees", empty if unknown.
    pub size: String,
    /// How it makes money: "B2B SaaS subscription", "Retail stores and online shop".
    pub business_model: String,
    /// Countries or regions it serves.
    pub markets: Vec<String>,
    pub sources: Vec<Source>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Offering {
    pub name: String,
    pub description: String,
    /// Who it's for.
    pub audience: String,
    /// Published pricing, empty if none.
    pub pricing: String,
    pub source: Source,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Segment {
    pub name: String,
    pub description: String,
    /// Customers the business itself names publicly (case studies, logos). May be empty.
    pub examples: Vec<String>,
    pub source: Source,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Competitor {
    pub name: String,
    pub website: String,
    /// What they do, in a sentence.
    pub summary: String,
    /// How they compare to the business (positioning, price, focus).
    pub difference: String,
    /// "high", "medium" or "low" overlap with the business.
    pub overlap: String,
    pub source: Source,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Trend {
    pub title: String,
    pub summary: String,
    /// "tailwind", "headwind" or "mixed" for the business.
    pub impact: String,
    pub source: Source,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Regulation {
    pub title: String,
    pub summary: String,
    /// "South Africa", "EU", "California"…
    pub jurisdiction: String,
    /// "proposed", "consultation", "passed" or "in force".
    pub status: String,
    /// When it takes effect, YYYY-MM-DD, if known.
    pub effective: Option<String>,
    /// "risk", "opportunity" or "both" for the business.
    pub impact: String,
    pub source: Source,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NewsItem {
    pub title: String,
    pub summary: String,
    pub source: Source,
}

fn one_of(v: &str, allowed: &[&str], fallback: &str) -> String {
    let v = v.trim().to_ascii_lowercase();
    if allowed.contains(&v.as_str()) {
        v
    } else {
        fallback.to_string()
    }
}

impl Intel {
    /// The same dossier with every item checked: unsourced items dropped, text trimmed and capped,
    /// enums normalised, lists capped. Apply to everything research produces before storing it.
    pub fn sanitized(mut self) -> Intel {
        let o = &mut self.overview;
        o.summary = truncate(&squash(&o.summary), 700);
        o.industry = truncate(&squash(&o.industry), 80);
        o.headquarters = truncate(&squash(&o.headquarters), 120);
        o.founded = truncate(&squash(&o.founded), 10);
        o.size = truncate(&squash(&o.size), 40);
        o.business_model = truncate(&squash(&o.business_model), 120);
        o.markets = clean_list(std::mem::take(&mut o.markets), 12, 60);
        o.sources = std::mem::take(&mut o.sources)
            .into_iter()
            .filter_map(Source::clean)
            .take(8)
            .collect();
        if o.sources.is_empty() {
            *o = Overview::default();
        }
        self.offerings = std::mem::take(&mut self.offerings)
            .into_iter()
            .filter_map(|mut x| {
                x.source = x.source.clean()?;
                x.name = truncate(&squash(&x.name), 100);
                x.description = truncate(&squash(&x.description), 400);
                x.audience = truncate(&squash(&x.audience), 160);
                x.pricing = truncate(&squash(&x.pricing), 120);
                (!x.name.is_empty()).then_some(x)
            })
            .take(12)
            .collect();
        self.segments = std::mem::take(&mut self.segments)
            .into_iter()
            .filter_map(|mut x| {
                x.source = x.source.clean()?;
                x.name = truncate(&squash(&x.name), 100);
                x.description = truncate(&squash(&x.description), 400);
                x.examples = clean_list(std::mem::take(&mut x.examples), 8, 60);
                (!x.name.is_empty()).then_some(x)
            })
            .take(8)
            .collect();
        self.competitors = std::mem::take(&mut self.competitors)
            .into_iter()
            .filter_map(|mut x| {
                x.source = x.source.clean()?;
                x.name = truncate(&squash(&x.name), 100);
                x.website = if links::is_http_url(x.website.trim()) {
                    x.website.trim().to_string()
                } else {
                    String::new()
                };
                x.summary = truncate(&squash(&x.summary), 300);
                x.difference = truncate(&squash(&x.difference), 300);
                x.overlap = one_of(&x.overlap, &["high", "medium", "low"], "medium");
                (!x.name.is_empty()).then_some(x)
            })
            .take(8)
            .collect();
        self.trends = std::mem::take(&mut self.trends)
            .into_iter()
            .filter_map(|mut x| {
                x.source = x.source.clean()?;
                x.title = truncate(&squash(&x.title), 140);
                x.summary = truncate(&squash(&x.summary), 400);
                x.impact = one_of(&x.impact, &["tailwind", "headwind", "mixed"], "mixed");
                (!x.title.is_empty()).then_some(x)
            })
            .take(8)
            .collect();
        self.regulation = std::mem::take(&mut self.regulation)
            .into_iter()
            .filter_map(|mut x| {
                x.source = x.source.clean()?;
                x.title = truncate(&squash(&x.title), 160);
                x.summary = truncate(&squash(&x.summary), 400);
                x.jurisdiction = truncate(&squash(&x.jurisdiction), 60);
                x.status = one_of(
                    &x.status,
                    &["proposed", "consultation", "passed", "in force"],
                    "in force",
                );
                x.effective = x.effective.as_deref().and_then(date::ymd_prefix);
                x.impact = one_of(&x.impact, &["risk", "opportunity", "both"], "both");
                (!x.title.is_empty()).then_some(x)
            })
            .take(8)
            .collect();
        self.news = std::mem::take(&mut self.news)
            .into_iter()
            .filter_map(|mut x| {
                x.source = x.source.clean()?;
                x.title = truncate(&squash(&x.title), 160);
                x.summary = truncate(&squash(&x.summary), 300);
                (!x.title.is_empty()).then_some(x)
            })
            .take(10)
            .collect();
        self.confidence = if self.confidence.is_finite() {
            self.confidence.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self
    }

    /// True once research has produced at least a sourced overview.
    pub fn is_ready(&self) -> bool {
        !self.overview.summary.is_empty() && !self.overview.sources.is_empty()
    }
}

// ---------------------------------------------------------------------------------------------
// Runs and schedule
// ---------------------------------------------------------------------------------------------

/// A step of a background run, shown live on the loading screen.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Progress {
    /// Machine-readable stage: "planning", "searching", "reviewing", "verifying", "theming",
    /// "saving", "done" or "failed".
    pub stage: String,
    /// One short human line: "Searching for culinary grants in Cape Town".
    pub message: String,
    /// 0 to 100, best effort.
    pub percent: u8,
}

impl Progress {
    pub fn new(stage: &str, message: impl Into<String>, percent: u8) -> Progress {
        Progress {
            stage: stage.to_string(),
            message: message.into(),
            percent: percent.min(100),
        }
    }
}

/// How often someone's opportunities are refreshed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Frequency {
    Daily,
    #[default]
    Weekly,
    Fortnightly,
    Monthly,
    Off,
}

impl Frequency {
    pub const ALL: [Frequency; 5] = [
        Frequency::Daily,
        Frequency::Weekly,
        Frequency::Fortnightly,
        Frequency::Monthly,
        Frequency::Off,
    ];

    /// Seconds between refreshes, None for Off.
    pub fn interval_secs(self) -> Option<i64> {
        const DAY: i64 = 86_400;
        match self {
            Frequency::Daily => Some(DAY),
            Frequency::Weekly => Some(7 * DAY),
            Frequency::Fortnightly => Some(14 * DAY),
            Frequency::Monthly => Some(30 * DAY),
            Frequency::Off => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Frequency::Daily => "daily",
            Frequency::Weekly => "weekly",
            Frequency::Fortnightly => "fortnightly",
            Frequency::Monthly => "monthly",
            Frequency::Off => "off",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Frequency::Daily => "Every day",
            Frequency::Weekly => "Every week",
            Frequency::Fortnightly => "Every two weeks",
            Frequency::Monthly => "Every month",
            Frequency::Off => "Only when I ask",
        }
    }

    pub fn parse(s: &str) -> Option<Frequency> {
        Frequency::ALL
            .into_iter()
            .find(|f| f.as_str() == s.trim().to_ascii_lowercase())
    }
}

/// The taxonomy as JSON, served at GET /api/meta so the web client never hard-codes it.
pub fn meta_json() -> serde_json::Value {
    let pairs = |list: &[(&str, &str)]| -> Vec<serde_json::Value> {
        list.iter()
            .map(|(id, label)| serde_json::json!({ "id": id, "label": label }))
            .collect()
    };
    serde_json::json!({
        "opportunity_types": OPPORTUNITY_TYPES,
        "stages": pairs(STAGES),
        "reach": pairs(REACH),
        "frequencies": Frequency::ALL.iter().map(|f| serde_json::json!({ "id": f.as_str(), "label": f.label() })).collect::<Vec<_>>(),
        "ai_models": AI_MODELS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_sanitize_limits_and_filters() {
        let p = Profile {
            headline: "  Chef   and\n caterer  ".into(),
            stage: "growth".into(),
            reach: "nowhere".into(),
            seeking: vec![
                "Funding".into(),
                "funding".into(),
                "bogus".into(),
                "sales".into(),
            ],
            interests: vec![
                " cooking ".into(),
                "Cooking".into(),
                "".into(),
                "x".repeat(100),
            ],
            links: vec!["https://example.com".into(), "javascript:alert(1)".into()],
            research: Some(Research {
                confidence: 7.0,
                ..Default::default()
            }),
            ..Default::default()
        }
        .sanitized();
        assert_eq!(p.headline, "Chef and caterer");
        assert_eq!(p.stage, "growth");
        assert_eq!(p.reach, "");
        assert_eq!(p.seeking, vec!["funding", "sales"]);
        assert_eq!(p.interests.len(), 2);
        assert_eq!(p.interests[0], "cooking");
        assert!(p.interests[1].chars().count() <= 60);
        assert_eq!(p.links, vec!["https://example.com"]);
        assert_eq!(p.research.unwrap().confidence, 1.0);
    }

    #[test]
    fn opportunity_sanitize_drops_bad_and_expired() {
        let ok = Opportunity {
            url: "https://x.org/a".into(),
            title: "A".into(),
            category: "FUNDING".into(),
            deadline: Some("2026-12-01".into()),
            score: 140,
            ..Default::default()
        };
        let s = ok.clone().sanitized("2026-11-26").unwrap();
        assert_eq!(s.category, "funding");
        assert_eq!(s.score, 100);
        assert!(
            ok.clone().sanitized("2026-12-02").is_none(),
            "past deadline is dropped"
        );
        let no_url = Opportunity {
            url: "ftp://x".into(),
            title: "A".into(),
            ..Default::default()
        };
        assert!(no_url.sanitized("2026-11-26").is_none());
        let weird = Opportunity {
            url: "https://x.org".into(),
            title: "T".into(),
            category: "unknown".into(),
            deadline: Some("soon".into()),
            ..Default::default()
        };
        let w = weird.sanitized("2026-11-26").unwrap();
        assert_eq!(w.category, "other");
        assert_eq!(w.deadline, None);
    }

    #[test]
    fn frequency_roundtrip() {
        for f in Frequency::ALL {
            assert_eq!(Frequency::parse(f.as_str()), Some(f));
            let j = serde_json::to_string(&f).unwrap();
            assert_eq!(j, format!("\"{}\"", f.as_str()));
        }
        assert_eq!(Frequency::default(), Frequency::Weekly);
        assert_eq!(Frequency::parse("WEEKLY"), Some(Frequency::Weekly));
        assert_eq!(Frequency::parse("hourly"), None);
    }

    #[test]
    fn colors() {
        assert_eq!(sanitize_hex_color("#AABBCC", "#000000"), "#aabbcc");
        assert_eq!(sanitize_hex_color("red", "#123456"), "#123456");
        assert_eq!(sanitize_hex_color("#abc", "#123456"), "#123456");
    }

    #[test]
    fn meta_has_everything() {
        let m = meta_json();
        assert_eq!(
            m["opportunity_types"].as_array().unwrap().len(),
            OPPORTUNITY_TYPES.len()
        );
        assert_eq!(m["frequencies"][1]["id"], "weekly");
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;

    #[test]
    fn context_items_are_checked() {
        let ok = ContextItem {
            source: "Slack".into(),
            kind: "message".into(),
            title: "Hackathon".into(),
            text: "Sign up by Friday".into(),
            url: "https://acme.slack.com/archives/C1/p1".into(),
            date: Some("2026-11-20T10:00:00Z".into()),
            ..Default::default()
        };
        let s = ok.clone().sanitized().unwrap();
        assert_eq!(s.source, "slack");
        assert_eq!(s.date.as_deref(), Some("2026-11-20"));
        assert!(
            ContextItem {
                source: "gmail".into(),
                ..ok.clone()
            }
            .sanitized()
            .is_none()
        );
        assert!(
            ContextItem {
                url: "slack://channel".into(),
                ..ok.clone()
            }
            .sanitized()
            .is_none()
        );
        assert!(
            ContextItem {
                title: String::new(),
                text: "  ".into(),
                ..ok
            }
            .sanitized()
            .is_none()
        );
    }

    #[test]
    fn opportunity_source_is_known_or_empty() {
        let o = Opportunity {
            url: "https://x.org".into(),
            title: "T".into(),
            source: "Confluence".into(),
            ..Default::default()
        };
        assert_eq!(
            o.clone().sanitized("2026-11-26").unwrap().source,
            "confluence"
        );
        assert_eq!(
            Opportunity {
                source: "carrier pigeon".into(),
                ..o
            }
            .sanitized("2026-11-26")
            .unwrap()
            .source,
            ""
        );
    }

    #[test]
    fn ai_models_default_is_flash() {
        assert_eq!(AI_MODELS[0].id, "deepseek-flash");
        assert!(AI_MODELS.iter().any(|m| m.provider == "bedrock"));
        assert_eq!(
            meta_json()["ai_models"].as_array().unwrap().len(),
            AI_MODELS.len()
        );
    }
}

#[cfg(test)]
mod intel_tests {
    use super::*;

    fn src(u: &str) -> Source {
        Source {
            url: u.into(),
            title: "t".into(),
            date: Some("2026-11-02T10:00:00Z".into()),
        }
    }

    #[test]
    fn unsourced_items_never_survive() {
        let intel = Intel {
            overview: Overview {
                summary: "We clean offices.".into(),
                sources: vec![src("https://acme.example/about")],
                ..Default::default()
            },
            offerings: vec![
                Offering {
                    name: "Office cleaning".into(),
                    source: src("https://acme.example/services"),
                    ..Default::default()
                },
                Offering {
                    name: "Imaginary product from Slack".into(),
                    source: Source::default(),
                    ..Default::default()
                },
            ],
            competitors: vec![Competitor {
                name: "Rival".into(),
                overlap: "HIGH".into(),
                website: "javascript:x".into(),
                source: src("https://news.example/a"),
                ..Default::default()
            }],
            regulation: vec![Regulation {
                title: "New act".into(),
                status: "maybe".into(),
                effective: Some("2027-03-01".into()),
                source: src("https://gov.example/act"),
                ..Default::default()
            }],
            confidence: 3.0,
            ..Default::default()
        }
        .sanitized();
        assert!(intel.is_ready());
        assert_eq!(
            intel.offerings.len(),
            1,
            "the unsourced offering is dropped"
        );
        assert_eq!(intel.competitors[0].overlap, "high");
        assert_eq!(intel.competitors[0].website, "");
        assert_eq!(intel.regulation[0].status, "in force");
        assert_eq!(
            intel.overview.sources[0].date.as_deref(),
            Some("2026-11-02")
        );
        assert_eq!(intel.confidence, 1.0);
        let empty = Intel {
            overview: Overview {
                summary: "No source".into(),
                ..Default::default()
            },
            ..Default::default()
        }
        .sanitized();
        assert!(!empty.is_ready(), "an unsourced overview is wiped");
    }

    #[test]
    fn business_taxonomy() {
        assert!(is_opportunity_type("sales") && is_opportunity_type("regulation"));
        assert!(!is_opportunity_type("jobs"));
        assert!(is_stage("enterprise") && !is_stage("student"));
        assert!(is_reach("international"));
        let p = Profile {
            website: "https://acme.example".into(),
            industry: " Cleaning ".into(),
            ..Default::default()
        }
        .sanitized();
        assert_eq!(p.industry, "Cleaning");
        assert!(!p.is_empty());
    }
}
