//! Small helpers: logging, time, randomness, hashing and IP classification.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use sha2::{Digest, Sha256};

static DEBUG: AtomicBool = AtomicBool::new(false);

pub fn set_debug(on: bool) {
    DEBUG.store(on, Ordering::Relaxed);
}

pub fn debug_enabled() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

/// One log line on stderr: `2026-11-26T12:00:00Z LEVEL message`.
pub fn log(level: &str, msg: &str) {
    eprintln!("{} {level} {msg}", timestamp(now()));
}

macro_rules! info {
    ($($arg:tt)*) => { $crate::util::log("INFO", &format!($($arg)*)) };
}

macro_rules! warn {
    ($($arg:tt)*) => { $crate::util::log("WARN", &format!($($arg)*)) };
}

macro_rules! error {
    ($($arg:tt)*) => { $crate::util::log("ERROR", &format!($($arg)*)) };
}

macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::util::debug_enabled() {
            $crate::util::log("DEBUG", &format!($($arg)*))
        }
    };
}

/// Unix seconds.
pub fn now() -> i64 {
    model::date::now_unix()
}

/// RFC 3339 UTC timestamp for Unix seconds.
pub fn timestamp(unix: i64) -> String {
    let secs = unix.rem_euclid(86_400);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        model::date::ymd(unix),
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// Lock a mutex, recovering the data if another thread panicked while holding it.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

/// `bytes` random bytes from the OS, hex-encoded. None only if the OS RNG fails.
pub fn random_hex(bytes: usize) -> Option<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).ok()?;
    Some(hex(&buf))
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// True for an address on the public internet (worth using to localise web search), false for
/// loopback, private, link-local, CGNAT, documentation, multicast and other special ranges.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let s = v6.segments();
            let global_unicast = (s[0] & 0xe000) == 0x2000; // 2000::/3
            let documentation = s[0] == 0x2001 && s[1] == 0x0db8; // 2001:db8::/32
            let special = s[0] == 0x2001 && s[1] < 0x0200; // 2001::/23 (Teredo, benchmarking…)
            global_unicast && !documentation && !special
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let special = a == 0
        || a == 10
        || a == 127
        || (a == 100 && (b & 0xc0) == 64) // 100.64.0.0/10 carrier-grade NAT
        || (a == 169 && b == 254)
        || (a == 172 && (b & 0xf0) == 16)
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 168)
        || (a == 198 && (b & 0xfe) == 18) // benchmarking
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224; // multicast, reserved, broadcast
    !special
}

/// Parse one entry of `X-Forwarded-For`: `1.2.3.4`, `2001:db8::1`, `[2001:db8::1]:443`, `1.2.3.4:80`.
pub fn parse_forwarded_ip(s: &str) -> Option<IpAddr> {
    let s = s.trim().trim_matches('"');
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Some(ip);
    }
    if let Some(rest) = s.strip_prefix('[') {
        return rest.split(']').next()?.parse().ok();
    }
    let (host, port) = s.rsplit_once(':')?;
    if port.chars().all(|c| c.is_ascii_digit()) {
        host.parse::<Ipv4Addr>().ok().map(IpAddr::V4)
    } else {
        None
    }
}

/// Decode `%XX` escapes and `+` (as a space) in a query-string value.
pub fn percent_decode(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (
            b[i],
            b.get(i + 1).copied().and_then(hex),
            b.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(hi), Some(lo)) => {
                out.push(hi * 16 + lo);
                i += 3;
            }
            (b'+', _, _) => {
                out.push(b' ');
                i += 1;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode everything but unreserved characters, for a query-string value.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_ips() {
        for ip in [
            "8.8.8.8",
            "81.2.69.160",
            "2a00:1450:4001:80b::200e",
            "::ffff:8.8.4.4",
        ] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} is public");
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "203.0.113.5",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "2001:db8::1",
            "::ffff:192.168.0.1",
            "ff02::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} is not public");
        }
        assert!(is_public_ip("172.32.0.1".parse().unwrap()));
    }

    #[test]
    fn forwarded_entries() {
        assert_eq!(
            parse_forwarded_ip(" 8.8.8.8 "),
            Some("8.8.8.8".parse().unwrap())
        );
        assert_eq!(
            parse_forwarded_ip("8.8.8.8:1234"),
            Some("8.8.8.8".parse().unwrap())
        );
        assert_eq!(
            parse_forwarded_ip("[2001:db8::1]:443"),
            Some("2001:db8::1".parse().unwrap())
        );
        assert_eq!(
            parse_forwarded_ip("2001:db8::1"),
            Some("2001:db8::1".parse().unwrap())
        );
        assert_eq!(parse_forwarded_ip("unknown"), None);
        assert_eq!(parse_forwarded_ip(""), None);
    }

    #[test]
    fn percent_coding() {
        assert_eq!(percent_decode("a%20b+c%2Fd"), "a b c/d");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_encode("It's 5/5 ok"), "It%27s%205%2F5%20ok");
        assert_eq!(percent_decode(&percent_encode("ünï & co")), "ünï & co");
    }

    #[test]
    fn hashing_and_time() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(timestamp(1_795_682_592), "2026-11-26T08:43:12Z");
        let t = random_hex(32).unwrap();
        assert_eq!(t.len(), 64);
        assert_ne!(t, random_hex(32).unwrap());
    }
}
