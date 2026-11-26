//! Just enough RFC 5322 and MIME to write a faithful `.eml`: headers with RFC 2047 encoded words,
//! folded lines, quoted-printable bodies, multipart/alternative, CRLF everywhere.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;

/// A process-unique random-looking token (boundaries, Message-IDs).
pub fn token() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(SEQ.fetch_add(1, Ordering::Relaxed));
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    format!("{:016x}", h.finish())
}

/// "Thu, 26 Nov 2026 09:15:30 +0000" for a Unix time.
pub fn rfc5322_date(unix: i64) -> String {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, m, d) = model::date::civil_from_days(unix.div_euclid(model::date::DAY_SECS));
    let secs = unix.rem_euclid(model::date::DAY_SECS);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} +0000",
        DAYS[model::date::weekday(unix) as usize],
        d,
        MONTHS[(m - 1) as usize],
        y,
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

fn is_plain_ascii(s: &str) -> bool {
    s.bytes().all(|b| (0x20..0x7F).contains(&b)) && !s.contains("=?")
}

/// RFC 2047 encoded words (UTF-8, base64) for text that isn't plain ASCII, never splitting a
/// character; the words are separated by folding whitespace. 39 bytes per word make a 64-character
/// word, so even `Subject: ` plus a word stays within 78 characters.
pub fn encode_words(s: &str) -> String {
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in s.chars() {
        if chunk.len() + c.len_utf8() > 39 {
            words.push(std::mem::take(&mut chunk));
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(chunk);
    }
    words
        .iter()
        .map(|w| {
            format!(
                "=?UTF-8?B?{}?=",
                base64::engine::general_purpose::STANDARD.encode(w)
            )
        })
        .collect::<Vec<_>>()
        .join("\r\n ")
}

/// One header line, folded at spaces to stay within 78 characters; unstructured text that isn't
/// plain ASCII becomes encoded words. Line breaks in the value are flattened first (no header
/// injection).
pub fn header(name: &str, value: &str) -> String {
    let value = model::text::squash(value);
    if !is_plain_ascii(&value) {
        return format!("{name}: {}\r\n", encode_words(&value));
    }
    let mut out = format!("{name}:");
    let mut line_len = out.len();
    for word in value.split(' ') {
        if line_len + 1 + word.len() > 78 && line_len > name.len() + 1 {
            out.push_str("\r\n");
            line_len = 0;
        }
        out.push(' ');
        out.push_str(word);
        line_len += 1 + word.len();
    }
    out.push_str("\r\n");
    out
}

/// A mailbox for From/To: `addr`, `Name <addr>`, `"Name, Inc." <addr>` or `=?UTF-8?B?…?= <addr>`.
pub fn mailbox(name: &str, addr: &str) -> String {
    let name = model::text::squash(name);
    if name.is_empty() {
        return addr.to_string();
    }
    if !is_plain_ascii(&name) {
        return format!("{} <{addr}>", encode_words(&name));
    }
    if name.chars().any(|c| "()<>[]:;@\\,.\"".contains(c)) {
        return format!(
            "\"{}\" <{addr}>",
            name.replace('\\', "\\\\").replace('"', "\\\"")
        );
    }
    format!("{name} <{addr}>")
}

/// Split `Name <addr>` (or a bare address) into its display name and address.
pub fn parse_mailbox(s: &str) -> (String, String) {
    let s = s.trim();
    match (s.rfind('<'), s.rfind('>')) {
        (Some(l), Some(r)) if l < r => {
            let name = s[..l].trim().trim_matches('"').trim().to_string();
            (name, s[l + 1..r].trim().to_string())
        }
        _ => (String::new(), s.to_string()),
    }
}

/// Quoted-printable (RFC 2045 §6.7) with CRLF line breaks and lines of at most 76 characters.
pub fn quoted_printable(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(normalized.len() + normalized.len() / 8);
    for (n, line) in normalized.split('\n').enumerate() {
        if n > 0 {
            out.push_str("\r\n");
        }
        let bytes = line.as_bytes();
        let mut col = 0;
        for (i, &b) in bytes.iter().enumerate() {
            let at_end = i + 1 == bytes.len();
            let literal = match b {
                b'=' => false,
                b' ' | b'\t' => !at_end,
                33..=126 => true,
                _ => false,
            };
            let piece = if literal {
                (b as char).to_string()
            } else {
                format!("={b:02X}")
            };
            // Leave room for the "=" of a soft line break.
            if col + piece.len() > 75 {
                out.push_str("=\r\n");
                col = 0;
            }
            out.push_str(&piece);
            col += piece.len();
        }
    }
    out
}

/// What goes into a `.eml`.
pub struct Message<'a> {
    pub from: String,
    pub to: String,
    pub subject: &'a str,
    pub text: &'a str,
    pub html: &'a str,
    pub date_unix: i64,
    pub message_id: String,
    /// Extra headers (name, value), already safe ASCII.
    pub extra: Vec<(String, String)>,
}

fn part(content_type: &str, body: &str) -> String {
    format!(
        "Content-Type: {content_type}; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n{}\r\n",
        quoted_printable(body)
    )
}

/// The whole message: headers, then a text/plain and a text/html alternative.
pub fn build(m: &Message) -> String {
    let mut out = String::new();
    out.push_str(&header("Date", &rfc5322_date(m.date_unix)));
    out.push_str(&format!("From: {}\r\n", m.from));
    out.push_str(&format!("To: {}\r\n", m.to));
    out.push_str(&header("Subject", m.subject));
    out.push_str(&format!("Message-ID: {}\r\n", m.message_id));
    for (k, v) in &m.extra {
        out.push_str(&header(k, v));
    }
    out.push_str("MIME-Version: 1.0\r\n");
    match (m.text.is_empty(), m.html.is_empty()) {
        (false, false) => {
            let boundary = format!("=_opp_{}", token());
            out.push_str(&format!(
                "Content-Type: multipart/alternative;\r\n boundary=\"{boundary}\"\r\n\r\n"
            ));
            out.push_str("This is a multi-part message in MIME format.\r\n");
            out.push_str(&format!("\r\n--{boundary}\r\n"));
            out.push_str(&part("text/plain", m.text));
            out.push_str(&format!("\r\n--{boundary}\r\n"));
            out.push_str(&part("text/html", m.html));
            out.push_str(&format!("\r\n--{boundary}--\r\n"));
        }
        (true, false) => out.push_str(&part("text/html", m.html)),
        _ => out.push_str(&part("text/plain", m.text)),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode quoted-printable (for checking round trips).
    fn qp_decode(s: &str) -> String {
        let s = s.replace("=\r\n", "");
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'=' {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap().replace("\r\n", "\n")
    }

    #[test]
    fn quoted_printable_round_trips_and_keeps_lines_short() {
        let samples = [
            "Hello, world",
            "Café = ünïcödé — “quotes” and emoji-free text",
            "trailing space \nand tab\t\nend",
            &"x".repeat(200),
            &"é".repeat(100),
            "a=b\r\nwindows\rold mac",
            "<a href=\"https://example.com/?a=1&b=2\">link</a>",
            "",
        ];
        for s in samples {
            let enc = quoted_printable(s);
            for line in enc.split("\r\n") {
                assert!(line.len() <= 76, "{line:?} is {} long", line.len());
                assert!(
                    !line.ends_with(' ') && !line.ends_with('\t'),
                    "trailing whitespace in {line:?}"
                );
                assert!(
                    line.bytes()
                        .all(|b| (33..=126).contains(&b) || b == b' ' || b == b'\t'),
                    "{line:?}"
                );
            }
            assert!(
                !enc.contains('\n') || enc.matches('\n').count() == enc.matches("\r\n").count(),
                "only CRLF"
            );
            let expected = s.replace("\r\n", "\n").replace('\r', "\n");
            assert_eq!(qp_decode(&enc), expected, "round trip of {s:?}");
        }
        assert_eq!(quoted_printable("a=b"), "a=3Db");
        assert_eq!(quoted_printable("end "), "end=20");
    }

    #[test]
    fn headers_encode_and_fold() {
        assert_eq!(header("Subject", "Hello there"), "Subject: Hello there\r\n");
        let enc = header("Subject", "Café für dich");
        assert!(enc.starts_with("Subject: =?UTF-8?B?"), "{enc}");
        let b64 = enc
            .trim_end()
            .trim_start_matches("Subject: =?UTF-8?B?")
            .trim_end_matches("?=");
        assert_eq!(
            String::from_utf8(
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .unwrap()
            )
            .unwrap(),
            "Café für dich"
        );
        // Long non-ASCII: several words, each ≤ 75 chars, folded.
        let long = header("Subject", &"ünïcödé ".repeat(20));
        for line in long.trim_end().split("\r\n") {
            assert!(line.len() <= 78, "{line}");
        }
        assert!(long.matches("=?UTF-8?B?").count() > 1);
        // Long ASCII is folded at spaces.
        let folded = header("Subject", &"word ".repeat(40));
        assert!(
            folded.trim_end().split("\r\n").all(|l| l.len() <= 78),
            "{folded}"
        );
        assert!(folded.contains("\r\n word"));
        // No header injection.
        assert_eq!(
            header("Subject", "Hi\r\nBcc: victim@example.com"),
            "Subject: Hi Bcc: victim@example.com\r\n"
        );
        // Text that looks like an encoded word is encoded itself.
        assert!(header("Subject", "=?UTF-8?B?x?=").contains("=?UTF-8?B?PT9VVEYtOD9CP3g/PQ==?="));
    }

    #[test]
    fn mailboxes() {
        assert_eq!(mailbox("", "a@b.co"), "a@b.co");
        assert_eq!(mailbox("Prospect", "a@b.co"), "Prospect <a@b.co>");
        assert_eq!(
            mailbox("Smith, Jane \"JJ\"", "a@b.co"),
            "\"Smith, Jane \\\"JJ\\\"\" <a@b.co>"
        );
        assert!(mailbox("Zoë", "a@b.co").starts_with("=?UTF-8?B?"));
        assert_eq!(
            parse_mailbox("Prospect <hello@mg.example.com>"),
            ("Prospect".into(), "hello@mg.example.com".into())
        );
        assert_eq!(
            parse_mailbox("\"Opp, Inc\" <x@y.z>"),
            ("Opp, Inc".into(), "x@y.z".into())
        );
        assert_eq!(
            parse_mailbox(" hello@mg.example.com "),
            ("".into(), "hello@mg.example.com".into())
        );
    }

    #[test]
    fn dates() {
        assert_eq!(rfc5322_date(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(
            rfc5322_date(1_795_682_592),
            "Thu, 26 Nov 2026 08:43:12 +0000"
        );
    }

    #[test]
    fn multipart_message_structure() {
        let m = Message {
            from: mailbox("Prospect", "hello@example.com"),
            to: "jane@example.com".into(),
            subject: "Три новые возможности",
            text: "Hi Jane,\nline two",
            html: "<p>Hi Jane</p>",
            date_unix: 1_795_682_592,
            message_id: "<1.2@example.com>".into(),
            extra: vec![("X-Mailgun-Tag".into(), "digest".into())],
        };
        let eml = build(&m);
        let (head, body) = eml.split_once("\r\n\r\n").unwrap();
        assert!(
            head.contains("\r\nFrom: Prospect <hello@example.com>\r\nTo: jane@example.com\r\n")
        );
        assert!(head.contains("Subject: =?UTF-8?B?"));
        assert!(head.contains("MIME-Version: 1.0"));
        assert!(head.contains("X-Mailgun-Tag: digest"));
        assert!(head.contains("Message-ID: <1.2@example.com>"));
        let boundary = head
            .split("boundary=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert!(boundary.starts_with("=_opp_"));
        assert_eq!(body.matches(&format!("--{boundary}\r\n")).count(), 2);
        assert!(body.trim_end().ends_with(&format!("--{boundary}--")));
        assert!(body.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(body.contains("Content-Type: text/html; charset=utf-8"));
        assert!(
            !eml.replace("\r\n", "").contains('\n'),
            "CRLF line endings only"
        );
        assert!(eml.split("\r\n").all(|l| l.len() <= 998));
        // Single-part when there is only text.
        let plain = build(&Message { html: "", ..m });
        assert!(
            plain.contains("Content-Type: text/plain; charset=utf-8")
                && !plain.contains("multipart")
        );
    }

    #[test]
    fn tokens_are_unique() {
        let mut seen: Vec<String> = (0..1000).map(|_| token()).collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 1000);
    }
}
