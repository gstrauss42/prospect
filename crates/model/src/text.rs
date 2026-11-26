//! Small text helpers.

/// Escape text for HTML element content and attribute values.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// At most `max_chars` characters, ending in "…" when cut. Never splits a character.
pub fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max_chars - 1).collect();
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    out.push('…');
    out
}

/// Whitespace (including newlines) collapsed to single spaces, trimmed, control characters removed.
pub fn squash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = true;
        } else if !c.is_control() {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    out
}

/// Trimmed, squashed, non-empty, case-insensitively unique items, capped in count and length.
pub fn clean_list(items: Vec<String>, max_items: usize, max_chars: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let item = truncate(&squash(&item), max_chars);
        if item.is_empty() || out.iter().any(|o| o.to_lowercase() == item.to_lowercase()) {
            continue;
        }
        out.push(item);
        if out.len() >= max_items {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping() {
        assert_eq!(
            escape_html("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("ünïcödé", 3), "ün…");
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn squashing() {
        assert_eq!(squash("  a \n\t b\u{0007}c  "), "a bc");
        assert_eq!(squash(""), "");
    }

    #[test]
    fn lists() {
        let l = clean_list(
            vec!["A".into(), " a ".into(), "".into(), "b".into(), "c".into()],
            2,
            10,
        );
        assert_eq!(l, vec!["A", "b"]);
    }
}
