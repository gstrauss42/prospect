//! URL checks and normalisation without a URL library. Good enough for deduplicating links that
//! search results and models produce; not a full RFC 3986 parser.

/// Query parameters that only track clicks and never change the page.
const TRACKING: &[&str] = &[
    "gclid",
    "fbclid",
    "msclkid",
    "dclid",
    "yclid",
    "mc_cid",
    "mc_eid",
    "igshid",
    "ref",
    "ref_src",
    "referrer",
    "source",
    "src",
    "_hsenc",
    "_hsmi",
    "spm",
    "si",
    "share",
    "cmpid",
    "trk",
    "trackingid",
];

/// True for an absolute http(s) URL with a host and no spaces or credentials.
pub fn is_http_url(s: &str) -> bool {
    let s = s.trim();
    let rest = match strip_scheme(s) {
        Some(r) => r,
        None => return false,
    };
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let host = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => authority,
    };
    (!host.is_empty() && host.contains('.') && !host.starts_with('.') && !host.ends_with('.'))
        || host.eq_ignore_ascii_case("localhost")
        || (host.starts_with('[') && host.ends_with(']'))
}

fn strip_scheme(s: &str) -> Option<&str> {
    let lower = s
        .get(0..8)
        .map(|p| p.to_ascii_lowercase())
        .unwrap_or_default();
    if lower.starts_with("https://") {
        Some(&s[8..])
    } else if lower.starts_with("http://") {
        Some(&s[7..])
    } else {
        None
    }
}

/// The host of an http(s) URL, lowercased and without "www." or a port.
pub fn host(s: &str) -> Option<String> {
    if !is_http_url(s) {
        return None;
    }
    let rest = strip_scheme(s.trim())?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => authority,
    };
    let host = host.to_ascii_lowercase();
    Some(
        host.strip_prefix("www.")
            .map(str::to_string)
            .unwrap_or(host),
    )
}

/// A stable key for "the same page": no scheme, lowercase host without "www." or default port,
/// no fragment, no tracking parameters, sorted query, no trailing slash.
///
/// `https://www.Example.com/jobs/?utm_source=x&b=2&a=1#top` → `example.com/jobs?a=1&b=2`
pub fn normalize_url(s: &str) -> Option<String> {
    let s = s.trim();
    if !is_http_url(s) {
        return None;
    }
    let rest = strip_scheme(s)?;
    let rest = rest.split('#').next().unwrap_or("");
    let (before_query, query) = match rest.split_once('?') {
        Some((a, q)) => (a, q),
        None => (rest, ""),
    };
    let (authority, path) = match before_query.find('/') {
        Some(i) => (&before_query[..i], &before_query[i..]),
        None => (before_query, ""),
    };
    let mut host = authority.to_ascii_lowercase();
    for port in [":80", ":443"] {
        if let Some(h) = host.strip_suffix(port) {
            host = h.to_string();
        }
    }
    if let Some(h) = host.strip_prefix("www.") {
        host = h.to_string();
    }
    let path = path.trim_end_matches('/');
    let mut params: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| {
            let k = p.split('=').next().unwrap_or("").to_ascii_lowercase();
            !k.starts_with("utm_") && !TRACKING.contains(&k.as_str())
        })
        .collect();
    params.sort_unstable();
    params.dedup();
    let mut key = format!("{host}{path}");
    if !params.is_empty() {
        key.push('?');
        key.push_str(&params.join("&"));
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validity() {
        assert!(is_http_url("https://example.com"));
        assert!(is_http_url("HTTP://Example.com/a?b=c"));
        assert!(is_http_url("http://localhost:8080/x"));
        assert!(!is_http_url("example.com"));
        assert!(!is_http_url("javascript:alert(1)"));
        assert!(!is_http_url("https://user:pw@example.com"));
        assert!(!is_http_url("https://exa mple.com"));
        assert!(!is_http_url("https://"));
        assert!(!is_http_url("ftp://example.com"));
    }

    #[test]
    fn normalization() {
        assert_eq!(
            normalize_url("https://www.Example.com/jobs/?utm_source=x&b=2&a=1#top").as_deref(),
            Some("example.com/jobs?a=1&b=2")
        );
        assert_eq!(
            normalize_url("http://example.com:80/").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            normalize_url("https://example.com:443").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            normalize_url("https://example.com:8443/a").as_deref(),
            Some("example.com:8443/a")
        );
        assert_eq!(
            normalize_url("https://example.com/A?fbclid=1").as_deref(),
            Some("example.com/A")
        );
        assert_eq!(normalize_url("not a url"), None);
        assert_eq!(
            normalize_url("https://x.org/a"),
            normalize_url("http://www.x.org/a/")
        );
    }

    #[test]
    fn hosts() {
        assert_eq!(
            host("https://www.Bursaries.co.za/x").as_deref(),
            Some("bursaries.co.za")
        );
        assert_eq!(host("http://localhost:9000").as_deref(), Some("localhost"));
        assert_eq!(host("nope"), None);
    }
}
