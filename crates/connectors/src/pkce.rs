//! OAuth helpers: PKCE (RFC 7636, S256) and random `state` values.

use base64::Engine as _;
use sha2::{Digest, Sha256};

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    // getrandom only fails if the OS has no randomness source; then nothing secure is possible.
    if getrandom::fill(&mut b).is_err() {
        panic!("the operating system's random number generator is unavailable");
    }
    b
}

/// A PKCE verifier and its S256 challenge.
#[derive(Clone, Debug, PartialEq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// A fresh verifier (32 random bytes, 43 base64url characters) and its challenge.
    pub fn new() -> Pkce {
        Pkce::from_verifier(&b64url(&random_bytes::<32>()))
    }

    pub fn from_verifier(verifier: &str) -> Pkce {
        let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
        Pkce {
            verifier: verifier.to_string(),
            challenge,
        }
    }
}

impl Default for Pkce {
    fn default() -> Self {
        Pkce::new()
    }
}

/// A random, unguessable OAuth `state` (32 bytes, base64url).
pub fn random_state() -> String {
    b64url(&random_bytes::<32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_appendix_b_vector() {
        let p = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(p.challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn fresh_values_are_long_and_distinct() {
        let a = Pkce::new();
        let b = Pkce::new();
        assert_eq!(a.verifier.len(), 43);
        assert_ne!(a.verifier, b.verifier);
        assert_eq!(random_state().len(), 43);
        assert_ne!(random_state(), random_state());
    }
}
