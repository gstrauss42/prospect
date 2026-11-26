//! The three emails: welcome, digest, password reset.
//!
//! HTML that survives real email clients: nested tables, inline styles, a 600 px column, a hidden
//! preheader, `bgcolor` for Outlook, and a small `<style>` block that only improves things where
//! it is supported (narrow screens, dark mode). Every dynamic value is HTML-escaped and only
//! http(s) URLs become links. Each email has a plain-text alternative that reads well on its own.

use model::Opportunity;
use model::text::{escape_html, squash, truncate};

use crate::{Config, Email};

/// The app's own accent, for emails that aren't styled for a person yet.
pub const DEFAULT_ACCENT: &str = "#2563eb";
/// The most opportunities listed in one digest.
pub const DIGEST_MAX_ITEMS: usize = 8;

const FONT: &str = "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
const PAGE_BG: &str = "#f3f4f6";
const CARD_BG: &str = "#ffffff";
const INK: &str = "#111827";
const BODY: &str = "#374151";
const MUTED: &str = "#6b7280";
const RULE: &str = "#e5e7eb";
const DARK_PAGE: &str = "#0f1115";
const DARK_CARD: &str = "#1a1d23";

// ---------------------------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------------------------

type Rgb = (f64, f64, f64);

fn parse_hex(c: &str) -> Option<Rgb> {
    let c = model::sanitize_hex_color(c, "");
    if c.is_empty() {
        return None;
    }
    let v = |i: usize| u8::from_str_radix(&c[i..i + 2], 16).ok().map(f64::from);
    Some((v(1)?, v(3)?, v(5)?))
}

fn to_hex((r, g, b): Rgb) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        r.round().clamp(0.0, 255.0) as u8,
        g.round().clamp(0.0, 255.0) as u8,
        b.round().clamp(0.0, 255.0) as u8
    )
}

/// WCAG relative luminance.
fn luminance((r, g, b): Rgb) -> f64 {
    let lin = |c: f64| {
        let c = c / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    (
        a.0 + (b.0 - a.0) * t,
        a.1 + (b.1 - a.1) * t,
        a.2 + (b.2 - a.2) * t,
    )
}

/// Move `color` towards `target` until it contrasts with `against` by at least `ratio`.
fn adjust(color: Rgb, target: Rgb, against: Rgb, ratio: f64) -> Rgb {
    let mut t = 0.0;
    let mut c = color;
    while contrast(c, against) < ratio && t < 1.0 {
        t += 0.05;
        c = mix(color, target, t);
    }
    c
}

/// The person's accent, made dark enough for white text on it and for links on white (4.5:1).
pub fn accent_on_light(accent: &str) -> String {
    let c = parse_hex(accent)
        .or_else(|| parse_hex(DEFAULT_ACCENT))
        .unwrap_or((37.0, 99.0, 235.0));
    to_hex(adjust(c, (0.0, 0.0, 0.0), (255.0, 255.0, 255.0), 4.5))
}

/// A lighter version for links and labels on the dark card in dark mode.
pub fn accent_on_dark(accent: &str) -> String {
    let c = parse_hex(&accent_on_light(accent)).unwrap_or((37.0, 99.0, 235.0));
    let dark = parse_hex(DARK_CARD).unwrap_or((26.0, 29.0, 35.0));
    to_hex(adjust(c, (255.0, 255.0, 255.0), dark, 4.5))
}

// ---------------------------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------------------------

/// Text for HTML: whitespace squashed, escaped.
fn esc(s: &str) -> String {
    escape_html(&squash(s))
}

/// An http(s) URL, trimmed; None for anything else (javascript:, data:, relative…).
fn http(url: &str) -> Option<String> {
    let u = url.trim();
    model::links::is_http_url(u).then(|| u.to_string())
}

/// The business name as written (squashed, at most 80 characters), or None when blank.
fn business(name: &str) -> Option<String> {
    let n = truncate(&squash(name), 80);
    (!n.is_empty()).then_some(n)
}

fn host(public_url: &str) -> String {
    model::links::host(public_url).unwrap_or_else(|| public_url.to_string())
}

fn paragraph(html: &str) -> String {
    format!(
        "<p class=\"text\" style=\"margin:0 0 16px 0;font-family:{FONT};font-size:16px;line-height:1.6;color:{BODY};\">{html}</p>"
    )
}

fn button(label: &str, url: &str, accent: &str) -> String {
    let accent = accent_on_light(accent);
    let href = escape_html(url);
    format!(
        "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"margin:8px 0 24px 0;\">\
<tr><td align=\"center\" bgcolor=\"{accent}\" style=\"border-radius:8px;background-color:{accent};\">\
<a href=\"{href}\" target=\"_blank\" rel=\"noopener\" style=\"display:inline-block;padding:13px 26px;font-family:{FONT};font-size:16px;font-weight:600;line-height:1.2;color:#ffffff;text-decoration:none;border-radius:8px;\">{label}</a>\
</td></tr></table>",
        label = esc(label)
    )
}

struct Layout<'a> {
    title: &'a str,
    preheader: &'a str,
    accent: &'a str,
    eyebrow: &'a str,
    heading: &'a str,
    body: String,
    footer: String,
}

fn layout(cfg: &Config, l: Layout) -> String {
    let accent = accent_on_light(l.accent);
    let accent_dark = accent_on_dark(l.accent);
    // Spacer characters stop clients from filling the preview line with the body text.
    let spacer = "&#847;&zwnj;&nbsp;".repeat(40);
    let home = http(&cfg.public_url);
    let brand = match &home {
        Some(u) => format!(
            "<a href=\"{}\" target=\"_blank\" rel=\"noopener\" class=\"muted\" style=\"color:{MUTED};text-decoration:none;\">{}</a>",
            escape_html(u),
            esc(&cfg.app_name)
        ),
        None => esc(&cfg.app_name),
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="en" xmlns="http://www.w3.org/1999/xhtml" xmlns:o="urn:schemas-microsoft-com:office:office">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="x-apple-disable-message-reformatting">
<meta name="format-detection" content="telephone=no, date=no, address=no, email=no, url=no">
<meta name="color-scheme" content="light dark">
<meta name="supported-color-schemes" content="light dark">
<title>{title}</title>
<!--[if mso]><noscript><xml><o:OfficeDocumentSettings><o:PixelsPerInch>96</o:PixelsPerInch></o:OfficeDocumentSettings></xml></noscript><![endif]-->
<style>
:root {{ color-scheme: light dark; supported-color-schemes: light dark; }}
body {{ margin: 0; padding: 0; width: 100% !important; -webkit-text-size-adjust: 100%; -ms-text-size-adjust: 100%; }}
table {{ border-collapse: collapse; }}
a {{ color: {accent}; }}
@media screen and (max-width: 620px) {{
  .container {{ width: 100% !important; }}
  .px {{ padding-left: 22px !important; padding-right: 22px !important; }}
  .h1 {{ font-size: 22px !important; }}
}}
@media (prefers-color-scheme: dark) {{
  .bg {{ background-color: {DARK_PAGE} !important; }}
  .card {{ background-color: {DARK_CARD} !important; }}
  .h1, .title {{ color: #f3f4f6 !important; }}
  .text {{ color: #d1d5db !important; }}
  .muted {{ color: #9ca3af !important; }}
  .rule {{ border-color: #2d323b !important; }}
  .accent, .accent a, a.accent {{ color: {accent_dark} !important; }}
}}
[data-ogsc] .h1, [data-ogsc] .title {{ color: #f3f4f6 !important; }}
[data-ogsc] .text {{ color: #d1d5db !important; }}
[data-ogsc] .muted {{ color: #9ca3af !important; }}
[data-ogsc] .accent, [data-ogsc] a.accent {{ color: {accent_dark} !important; }}
</style>
</head>
<body class="bg" style="margin:0;padding:0;background-color:{PAGE_BG};">
<div style="display:none;font-size:1px;line-height:1px;max-height:0;max-width:0;opacity:0;overflow:hidden;mso-hide:all;">{preheader}{spacer}</div>
<table role="presentation" class="bg" width="100%" cellpadding="0" cellspacing="0" border="0" bgcolor="{PAGE_BG}" style="background-color:{PAGE_BG};">
<tr><td align="center" style="padding:28px 12px 36px 12px;">
<!--[if mso]><table role="presentation" width="600" cellpadding="0" cellspacing="0" border="0"><tr><td><![endif]-->
<table role="presentation" class="container" width="600" cellpadding="0" cellspacing="0" border="0" style="width:600px;max-width:600px;">
<tr><td class="px muted" style="padding:0 8px 14px 8px;font-family:{FONT};font-size:14px;font-weight:600;letter-spacing:0.02em;color:{MUTED};">{brand}</td></tr>
<tr><td class="card" bgcolor="{CARD_BG}" style="background-color:{CARD_BG};border-radius:14px;overflow:hidden;">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0">
<tr><td height="6" bgcolor="{accent}" style="height:6px;line-height:6px;font-size:6px;background-color:{accent};">&nbsp;</td></tr>
<tr><td class="px" style="padding:32px 40px 4px 40px;">
<p class="accent" style="margin:0 0 10px 0;font-family:{FONT};font-size:12px;line-height:1.4;font-weight:700;letter-spacing:0.08em;text-transform:uppercase;color:{accent};">{eyebrow}</p>
<h1 class="h1" style="margin:0 0 20px 0;font-family:{FONT};font-size:26px;line-height:1.25;font-weight:700;color:{INK};">{heading}</h1>
</td></tr>
<tr><td class="px" style="padding:0 40px 20px 40px;">
{body}
</td></tr>
</table>
</td></tr>
<tr><td class="px muted" style="padding:22px 24px 0 24px;font-family:{FONT};font-size:12px;line-height:1.6;color:{MUTED};text-align:center;">
{footer}
</td></tr>
</table>
<!--[if mso]></td></tr></table><![endif]-->
</td></tr>
</table>
</body>
</html>
"#,
        title = esc(l.title),
        preheader = esc(l.preheader),
        eyebrow = esc(l.eyebrow),
        heading = esc(l.heading),
        body = l.body,
        footer = l.footer,
    )
}

fn footer_link(label: &str, url: &str) -> String {
    format!(
        "<a href=\"{}\" target=\"_blank\" rel=\"noopener\" class=\"muted\" style=\"color:{MUTED};text-decoration:underline;\">{}</a>",
        escape_html(url),
        esc(label)
    )
}

/// "Opportunity · example.com", linked when the public URL is usable.
fn signature(cfg: &Config) -> String {
    match http(&cfg.public_url) {
        Some(u) => format!("{} · {}", esc(&cfg.app_name), footer_link(&host(&u), &u)),
        None => esc(&cfg.app_name),
    }
}

// ---------------------------------------------------------------------------------------------
// Welcome
// ---------------------------------------------------------------------------------------------

/// Sent right after sign-up. `first_name` is the business name (one account per business).
pub fn welcome(cfg: &Config, first_name: &str, to: &str) -> Email {
    let app = squash(&cfg.app_name);
    let name = business(first_name);
    let subject = match &name {
        Some(b) => format!("Welcome to {app}, {b}"),
        None => format!("Welcome to {app}"),
    };
    let home = http(&cfg.public_url);
    let thanks = match &name {
        Some(b) => format!("Thanks for setting up {app} for {b}."),
        None => format!("Thanks for setting up {app} for your business."),
    };
    let what = "We look for current opportunities that fit your business, such as new customers and tenders, partners, funding and incentives, rule changes and new markets, and keep them on one page. Each one links to its source and says why it matters to you.";
    let steps = [
        (
            "Tell us about the business.",
            "Fill in a short profile, or let us draft one from public information for you to check. It takes a couple of minutes.",
        ),
        (
            "We do the searching.",
            "We search the web, read the pages and keep the opportunities that fit what you do and where you want to grow.",
        ),
        (
            "Your page stays current.",
            "We check again every week, or as often as you like, and email you when something new turns up.",
        ),
    ];

    let mut body = paragraph(&format!("{} {}", esc(&thanks), esc(what)));
    body.push_str("<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"margin:4px 0 20px 0;\">");
    for (i, (lead, rest)) in steps.iter().enumerate() {
        body.push_str(&format!(
            "<tr><td width=\"36\" valign=\"top\" style=\"padding:0 0 14px 0;font-family:{FONT};font-size:15px;font-weight:700;line-height:1.6;color:{accent};\" class=\"accent\">{n}.</td>\
<td valign=\"top\" class=\"text\" style=\"padding:0 0 14px 0;font-family:{FONT};font-size:15px;line-height:1.6;color:{BODY};\"><strong class=\"title\" style=\"color:{INK};\">{lead}</strong> {rest}</td></tr>",
            accent = accent_on_light(DEFAULT_ACCENT),
            n = i + 1,
            lead = esc(lead),
            rest = esc(rest)
        ));
    }
    body.push_str("</table>");
    if let Some(u) = &home {
        body.push_str(&button("Set up your page", u, DEFAULT_ACCENT));
    }
    body.push_str(&paragraph(&format!(
        "Glad to have you,<br>The {} team",
        esc(&app)
    )));
    let footer = format!(
        "You're getting this because an account was created with {} on {}. If that wasn't you, you can ignore this email.<br>{}",
        esc(to),
        esc(&app),
        signature(cfg)
    );
    let html = layout(
        cfg,
        Layout {
            title: &subject,
            preheader: "Your opportunities page is almost ready. Here's what happens next.",
            accent: DEFAULT_ACCENT,
            eyebrow: "Welcome",
            heading: &subject,
            body,
            footer,
        },
    );

    let mut text = format!("{subject}\n\n{thanks} {what}\n\nWhat happens next:\n\n");
    for (i, (lead, rest)) in steps.iter().enumerate() {
        text.push_str(&format!("{}. {lead} {rest}\n\n", i + 1));
    }
    if let Some(u) = &home {
        text.push_str(&format!("Set up your page: {u}\n\n"));
    }
    text.push_str(&format!(
        "Glad to have you,\nThe {app} team\n\n--\nYou're getting this because an account was created with {} on {app}. If that wasn't you, you can ignore this email.\n",
        squash(to)
    ));

    Email {
        to: to.trim().to_string(),
        subject,
        text,
        html,
        tag: "welcome".into(),
    }
}

// ---------------------------------------------------------------------------------------------
// Digest
// ---------------------------------------------------------------------------------------------

struct Item {
    title: String,
    url: Option<String>,
    meta: Vec<String>,
    why: String,
}

/// "Slack", "Microsoft 365", "Confluence" for an opportunity found through a connector; None
/// for the public web.
fn connector_name(source: &str) -> Option<String> {
    let s = squash(source).to_lowercase();
    match s.as_str() {
        "" | "web" => None,
        "slack" => Some("Slack".into()),
        "microsoft" | "teams" | "sharepoint" | "outlook" => Some("Microsoft 365".into()),
        "confluence" => Some("Confluence".into()),
        other => {
            let mut c = other.chars();
            c.next()
                .map(|f| truncate(&(f.to_uppercase().collect::<String>() + c.as_str()), 30))
        }
    }
}

fn item(o: &Opportunity) -> Item {
    let mut meta = Vec::new();
    meta.push(model::opportunity_type_label(o.category.trim()).to_string());
    let org = truncate(&squash(&o.organization), 80);
    if !org.is_empty() {
        meta.push(org);
    }
    if let Some(d) = o.deadline.as_deref().and_then(model::date::ymd_prefix) {
        meta.push(format!("Closes {}", model::date::human(&d)));
    }
    let value = truncate(&squash(&o.value), 60);
    if !value.is_empty() {
        meta.push(value);
    }
    if let Some(c) = connector_name(&o.source) {
        meta.push(format!("From {c}"));
    }
    let title = truncate(&squash(&o.title), 160);
    Item {
        title: if title.is_empty() {
            "An opportunity".into()
        } else {
            title
        },
        url: http(&o.url),
        meta,
        why: truncate(&squash(&o.why), 300),
    }
}

/// The digest after a refresh: the new opportunities (best first) and a link back to the page.
/// `first_name` is the business name; `theme_label` and `accent` (#rrggbb) style it like the
/// business's page.
pub fn digest(
    cfg: &Config,
    first_name: &str,
    to: &str,
    new_items: &[Opportunity],
    total_active: usize,
    theme_label: &str,
    accent: &str,
) -> Email {
    let app = squash(&cfg.app_name);
    let accent_light = accent_on_light(accent);
    let who = business(first_name).unwrap_or_else(|| "your business".to_string());
    let mut sorted: Vec<&Opportunity> = new_items.iter().collect();
    sorted.sort_by_key(|o| std::cmp::Reverse(o.score));
    let items: Vec<Item> = sorted
        .iter()
        .take(DIGEST_MAX_ITEMS)
        .map(|o| item(o))
        .collect();
    let n = new_items.len();
    let more = n.saturating_sub(items.len());
    let total = total_active.max(n);
    let label = {
        let l = truncate(&squash(theme_label), 40);
        if l.is_empty() {
            "Your opportunities".to_string()
        } else {
            l
        }
    };
    let home = http(&cfg.public_url);
    let settings = http(&cfg.settings_url());

    let subject = match n {
        0 => format!("No new opportunities for {who} this time"),
        1 => format!("A new opportunity for {who}"),
        _ => format!("{n} new opportunities for {who}"),
    };
    let heading = match n {
        0 => "Nothing new this time".to_string(),
        _ => format!("New opportunities for {who}"),
    };
    let preheader = match items.first() {
        Some(first) if n > 1 => format!("{} and {} more, picked for {who}.", first.title, n - 1),
        Some(first) => format!("{}, picked for {who}.", first.title),
        None => "We looked again and your page is current.".to_string(),
    };
    let intro = match n {
        0 => format!(
            "We looked again and found nothing new that fits {who}. Your page still has everything we found before."
        ),
        1 => "We found one new opportunity since we last looked.".to_string(),
        _ => format!(
            "We found {n} new opportunities since we last looked. Here they are, best match first."
        ),
    };

    // HTML
    let mut body = paragraph(&esc(&intro));
    if !items.is_empty() {
        body.push_str("<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" style=\"margin:4px 0 8px 0;\">");
        for it in &items {
            let title = match &it.url {
                Some(u) => format!(
                    "<a href=\"{}\" target=\"_blank\" rel=\"noopener\" class=\"accent\" style=\"color:{accent_light};text-decoration:none;\">{}</a>",
                    escape_html(u),
                    esc(&it.title)
                ),
                None => format!(
                    "<span class=\"title\" style=\"color:{INK};\">{}</span>",
                    esc(&it.title)
                ),
            };
            let meta = it
                .meta
                .iter()
                .map(|m| esc(m))
                .collect::<Vec<_>>()
                .join(" &middot; ");
            let why = if it.why.is_empty() {
                String::new()
            } else {
                format!(
                    "<p class=\"text\" style=\"margin:8px 0 0 0;font-family:{FONT};font-size:15px;line-height:1.55;color:{BODY};\">{}</p>",
                    esc(&it.why)
                )
            };
            body.push_str(&format!(
                "<tr><td class=\"rule\" style=\"padding:18px 0;border-top:1px solid {RULE};\">\
<p class=\"title\" style=\"margin:0 0 6px 0;font-family:{FONT};font-size:18px;line-height:1.35;font-weight:700;color:{INK};\">{title}</p>\
<p class=\"muted\" style=\"margin:0;font-family:{FONT};font-size:13px;line-height:1.5;color:{MUTED};\">{meta}</p>{why}</td></tr>"
            ));
        }
        body.push_str("</table>");
    }
    if more > 0 {
        body.push_str(&format!(
            "<p class=\"muted\" style=\"margin:0 0 16px 0;font-family:{FONT};font-size:15px;line-height:1.6;color:{MUTED};border-top:1px solid {RULE};padding-top:16px;\">And {more} more on your page.</p>"
        ));
    }
    if let Some(u) = &home {
        let label = if total > 0 {
            format!("See all {total} on your page")
        } else {
            "Open your page".to_string()
        };
        body.push_str(&button(&label, u, accent));
    }
    let footer = {
        let mut f = format!(
            "You're getting this because opportunity emails are on for {}.",
            esc(to)
        );
        match &settings {
            Some(s) => f.push_str(&format!(" To change how often we look, or to stop these emails, {}.", footer_link("open your settings", s))),
            None => f.push_str(" You can change how often we look, or turn these emails off, in Settings on your page."),
        }
        f.push_str(&format!("<br>{}", signature(cfg)));
        f
    };
    let html = layout(
        cfg,
        Layout {
            title: &subject,
            preheader: &preheader,
            accent,
            eyebrow: &label,
            heading: &heading,
            body,
            footer,
        },
    );

    // Plain text
    let mut text = format!("{heading}\n\n{intro}\n\n");
    for (i, it) in items.iter().enumerate() {
        text.push_str(&format!("{}. {}\n", i + 1, it.title));
        if !it.meta.is_empty() {
            text.push_str(&format!("   {}\n", it.meta.join(" · ")));
        }
        if !it.why.is_empty() {
            text.push_str(&format!("   {}\n", it.why));
        }
        if let Some(u) = &it.url {
            text.push_str(&format!("   {u}\n"));
        }
        text.push('\n');
    }
    if more > 0 {
        text.push_str(&format!("And {more} more on your page.\n\n"));
    }
    if let Some(u) = &home {
        if total > 0 {
            text.push_str(&format!("See all {total} on your page: {u}\n\n"));
        } else {
            text.push_str(&format!("Your page: {u}\n\n"));
        }
    }
    text.push_str(&format!(
        "--\nYou're getting this because opportunity emails are on for {}.\n",
        squash(to)
    ));
    match &settings {
        Some(s) => text.push_str(&format!("Change how often we look, or stop these emails: {s}\n")),
        None => text.push_str("You can change how often we look, or turn these emails off, in Settings on your page.\n"),
    }
    text.push_str(&format!("{app}\n"));

    Email {
        to: to.trim().to_string(),
        subject,
        text,
        html,
        tag: "digest".into(),
    }
}

// ---------------------------------------------------------------------------------------------
// Password reset
// ---------------------------------------------------------------------------------------------

/// A password-reset link, valid for an hour. `first_name` is the business name.
pub fn password_reset(cfg: &Config, first_name: &str, to: &str, reset_url: &str) -> Email {
    let app = squash(&cfg.app_name);
    let subject = format!("Reset your {app} password");
    let link = http(reset_url);
    let account = match business(first_name) {
        Some(b) => format!("the {b} account on {app}"),
        None => format!("your {app} account"),
    };
    let request = format!(
        "We got a request to reset the password for {account}. Choose a new one with the button below. The link works for one hour and can be used once."
    );
    let mut body = paragraph(&esc(&request));
    if let Some(u) = &link {
        body.push_str(&button("Choose a new password", u, DEFAULT_ACCENT));
        body.push_str(&format!(
            "<p class=\"muted\" style=\"margin:0 0 16px 0;font-family:{FONT};font-size:13px;line-height:1.6;color:{MUTED};\">If the button doesn't work, copy this link into your browser:<br>\
<a href=\"{href}\" target=\"_blank\" rel=\"noopener\" class=\"accent\" style=\"color:{accent};word-break:break-all;\">{shown}</a></p>",
            href = escape_html(u),
            shown = escape_html(u),
            accent = accent_on_light(DEFAULT_ACCENT)
        ));
    }
    let ignore = "If you didn't ask for this, you can ignore this email. The password stays the same, and it can't be changed without this link.";
    body.push_str(&paragraph(ignore));
    let footer = format!(
        "Sent because a password reset was requested for {} on {}.<br>{}",
        esc(to),
        esc(&app),
        signature(cfg)
    );
    let html = layout(
        cfg,
        Layout {
            title: &subject,
            preheader: "This link works for one hour.",
            accent: DEFAULT_ACCENT,
            eyebrow: "Your account",
            heading: "Reset your password",
            body,
            footer,
        },
    );

    let mut text = format!(
        "Reset your password\n\nWe got a request to reset the password for {account}. Choose a new one with this link. It works for one hour and can be used once.\n\n"
    );
    if let Some(u) = &link {
        text.push_str(&format!("{u}\n\n"));
    }
    text.push_str(&format!(
        "{ignore}\n\n--\nSent because a password reset was requested for {} on {app}.\n",
        squash(to)
    ));
    Email {
        to: to.trim().to_string(),
        subject,
        text,
        html,
        tag: "password-reset".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cfg() -> Config {
        Config {
            api_key: None,
            domain: String::new(),
            sender: String::new(),
            base_urls: vec![],
            outbox_dir: PathBuf::from("/nonexistent"),
            public_url: "https://opportunity.example.org".into(),
            app_name: "Opportunity".into(),
        }
    }

    fn opp(title: &str, score: u8) -> Opportunity {
        Opportunity {
            url: format!(
                "https://example.org/{}",
                title.to_lowercase().replace(' ', "-")
            ),
            title: title.into(),
            organization: "City of Cape Town".into(),
            category: "funding".into(),
            why: "You're expanding your cleaning crews into offices.".into(),
            deadline: Some("2027-01-15".into()),
            value: "R350 000".into(),
            score,
            ..Default::default()
        }
    }

    /// Every href in the HTML.
    fn hrefs(html: &str) -> Vec<String> {
        html.split("href=\"")
            .skip(1)
            .map(|s| s.split('"').next().unwrap().to_string())
            .collect()
    }

    #[test]
    fn colours_stay_readable() {
        // A pale accent is darkened until white text on it reads (4.5:1).
        let yellow = parse_hex(&accent_on_light("#ffeb3b")).unwrap();
        assert!(contrast(yellow, (255.0, 255.0, 255.0)) >= 4.5);
        // A good accent is left alone.
        assert_eq!(accent_on_light("#c2410c"), "#c2410c");
        // Garbage falls back to the default.
        assert_eq!(accent_on_light("red; background:url(x)"), DEFAULT_ACCENT);
        // The dark-mode version reads on the dark card.
        let dark = parse_hex(&accent_on_dark("#1e3a8a")).unwrap();
        assert!(contrast(dark, parse_hex(DARK_CARD).unwrap()) >= 4.5);
    }

    #[test]
    fn digest_lists_best_first_with_details() {
        let mut slack = opp("Second best", 70);
        slack.category = "sales".into();
        slack.source = "slack".into();
        let items = vec![slack, opp("Best tender", 95), opp("Third", 40)];
        let e = digest(
            &cfg(),
            "Acme Cleaning",
            "ops@acme.example",
            &items,
            18,
            "Commercial cleaning",
            "#c2410c",
        );
        assert_eq!(e.subject, "3 new opportunities for Acme Cleaning");
        assert_eq!(e.tag, "digest");
        assert_eq!(e.to, "ops@acme.example");
        let best = e.html.find("Best tender").unwrap();
        let second = e.html.find("Second best").unwrap();
        let third = e.html.find("Third").unwrap();
        assert!(best < second && second < third, "best first");
        assert!(
            e.html.contains("New opportunities for Acme Cleaning"),
            "addressed to the business"
        );
        assert!(e.html.contains("Commercial cleaning"), "theme label");
        assert!(e.html.contains("#c2410c"), "accent");
        assert!(e.html.contains("Closes 15 Jan 2027"));
        assert!(
            e.html.contains("Funding &amp; incentives"),
            "business taxonomy label, escaped"
        );
        assert!(e.html.contains("Sales &amp; customers"));
        assert!(e.html.contains("From Slack"), "connector source");
        assert!(e.html.contains("City of Cape Town") && e.html.contains("R350 000"));
        assert!(e.html.contains("You&#39;re expanding your cleaning crews"));
        assert!(e.html.contains("See all 18 on your page"));
        assert!(
            e.html.contains("https://opportunity.example.org/settings"),
            "settings link in the footer"
        );
        assert!(
            e.html
                .contains("Best tender and 2 more, picked for Acme Cleaning."),
            "preheader"
        );
        assert!(e.html.contains("max-width:600px"));
        assert!(e.html.contains("prefers-color-scheme: dark"));
        // Plain text mirrors it.
        assert!(
            e.text
                .starts_with("New opportunities for Acme Cleaning\n\nWe found 3 new opportunities")
        );
        let t_best = e.text.find("1. Best tender").unwrap();
        assert!(t_best < e.text.find("2. Second best").unwrap());
        assert!(
            e.text.contains(
                "Funding & incentives · City of Cape Town · Closes 15 Jan 2027 · R350 000"
            )
        );
        assert!(e.text.contains(
            "Sales & customers · City of Cape Town · Closes 15 Jan 2027 · R350 000 · From Slack"
        ));
        assert!(e.text.contains("https://example.org/best-tender"));
        assert!(
            e.text
                .contains("See all 18 on your page: https://opportunity.example.org")
        );
        assert!(
            e.text
                .contains("stop these emails: https://opportunity.example.org/settings")
        );
        assert!(!e.text.contains('<'), "no HTML in the text version");
    }

    #[test]
    fn digest_caps_at_eight_and_counts_the_rest() {
        let items: Vec<Opportunity> = (0..11)
            .map(|i| opp(&format!("Opportunity {i:02}"), 50 + i as u8))
            .collect();
        let e = digest(
            &cfg(),
            "Ana's Bakery",
            "ana@example.com",
            &items,
            30,
            "Bakery",
            "#047857",
        );
        assert_eq!(e.html.matches("class=\"rule\"").count(), 8);
        assert!(e.html.contains("And 3 more on your page."));
        assert!(e.text.contains("And 3 more on your page."));
        assert!(
            e.html.contains("Opportunity 10") && !e.html.contains("Opportunity 02"),
            "the best eight"
        );
        let one = digest(&cfg(), "", "ana@example.com", &items[..1], 1, "", "");
        assert_eq!(one.subject, "A new opportunity for your business");
        assert!(one.text.starts_with("New opportunities for your business"));
        assert!(one.html.contains("Your opportunities"), "default label");
        assert!(one.html.contains(DEFAULT_ACCENT), "default accent");
        let none = digest(
            &cfg(),
            "Ana's Bakery",
            "ana@example.com",
            &[],
            5,
            "Bakery",
            "#7c3aed",
        );
        assert_eq!(
            none.subject,
            "No new opportunities for Ana's Bakery this time"
        );
        assert!(none.html.contains("Nothing new this time"));
        assert!(
            none.html
                .contains("found nothing new that fits Ana&#39;s Bakery")
        );
    }

    #[test]
    fn connector_names() {
        assert_eq!(connector_name(""), None);
        assert_eq!(connector_name("web"), None);
        assert_eq!(connector_name("slack").as_deref(), Some("Slack"));
        assert_eq!(
            connector_name("microsoft").as_deref(),
            Some("Microsoft 365")
        );
        assert_eq!(connector_name("Confluence").as_deref(), Some("Confluence"));
        assert_eq!(connector_name("notion").as_deref(), Some("Notion"));
    }

    #[test]
    fn everything_dynamic_is_escaped_and_links_are_http_only() {
        let mut evil = opp("<script>alert('x')</script> Grant", 90);
        evil.organization = "<b>Org</b> & \"Co\"".into();
        evil.why = "<img src=x onerror=alert(1)>".into();
        evil.value = "<i>$10k</i>".into();
        evil.source = "<svg onload=alert(1)>".into();
        let mut js = opp("Click me", 80);
        js.url = "javascript:alert(document.cookie)".into();
        let mut data = opp("Data link", 70);
        data.url = "data:text/html,<script>alert(1)</script>".into();
        let e = digest(
            &cfg(),
            "<Acme>",
            "ops@acme.example",
            &[evil, js, data],
            3,
            "<Cleaning>",
            "#c2410c\" onload=\"x",
        );
        assert!(!e.html.contains("<script>"), "script tags escaped");
        assert!(
            e.html
                .contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; Grant")
        );
        assert!(!e.html.contains("<img") && !e.html.contains("<svg"));
        assert!(
            !e.html.contains("<b>Org</b>")
                && e.html
                    .contains("&lt;b&gt;Org&lt;/b&gt; &amp; &quot;Co&quot;")
        );
        assert!(
            e.html.contains("New opportunities for &lt;Acme&gt;") && !e.html.contains("<Acme>")
        );
        assert!(e.html.contains("&lt;Cleaning&gt;"));
        assert!(
            !e.html.contains("onload=\"x"),
            "a bad accent can't break out of the style attribute"
        );
        for href in hrefs(&e.html) {
            assert!(
                href.starts_with("https://") || href.starts_with("http://"),
                "non-http link: {href}"
            );
        }
        assert!(
            e.html.contains("Click me") && e.html.contains("Data link"),
            "titles stay, unlinked"
        );
        assert!(!e.text.contains("javascript:") && !e.text.contains("data:text"));
        for e in [
            welcome(&cfg(), "<b>x</b>", "a@b.co"),
            password_reset(&cfg(), "<b>x</b>", "a@b.co", "javascript:alert(1)"),
        ] {
            assert!(!e.html.contains("<b>x</b>"));
            for href in hrefs(&e.html) {
                assert!(
                    href.starts_with("https://") || href.starts_with("http://"),
                    "non-http link: {href}"
                );
            }
        }
    }

    #[test]
    fn welcome_and_reset_address_the_business() {
        let w = welcome(&cfg(), "Acme Cleaning", "ops@acme.example");
        assert_eq!(w.subject, "Welcome to Opportunity, Acme Cleaning");
        assert_eq!(w.tag, "welcome");
        assert!(
            w.html.contains("<h1")
                && w.html
                    .contains("Welcome to Opportunity, Acme Cleaning</h1>")
        );
        assert!(
            w.html
                .contains("Thanks for setting up Opportunity for Acme Cleaning.")
        );
        assert!(
            w.html.contains("Set up your page")
                && w.html.contains("href=\"https://opportunity.example.org\"")
        );
        assert!(w.text.starts_with("Welcome to Opportunity, Acme Cleaning\n\nThanks for setting up Opportunity for Acme Cleaning."));
        assert!(
            w.text
                .contains("Set up your page: https://opportunity.example.org")
        );
        assert_eq!(
            welcome(&cfg(), "  ", "ops@acme.example").subject,
            "Welcome to Opportunity"
        );
        let r = password_reset(
            &cfg(),
            "Acme Cleaning",
            "ops@acme.example",
            "https://opportunity.example.org/reset?token=abc&x=1",
        );
        assert_eq!(r.subject, "Reset your Opportunity password");
        assert_eq!(r.tag, "password-reset");
        assert!(r.html.contains("the Acme Cleaning account on Opportunity"));
        assert!(
            r.html
                .contains("href=\"https://opportunity.example.org/reset?token=abc&amp;x=1\"")
        );
        assert!(
            r.text
                .contains("https://opportunity.example.org/reset?token=abc&x=1")
        );
        assert!(r.text.contains("one hour"));
        for e in [&w, &r] {
            assert!(
                !e.subject.contains('!') && !e.text.contains('!') && !e.html.contains("!<"),
                "no exclamation hype"
            );
            assert!(
                !e.text.contains("Hi ") && !e.html.contains("Hi "),
                "no personal greeting for a business"
            );
        }
    }
}
