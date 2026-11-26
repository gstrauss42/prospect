use super::*;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};

/// A directory under the system temp dir, removed when dropped. Not created up front, so tests
/// can check that the vault creates it.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "opportunity-vault-{tag}-{}-{}-{nanos}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        TempDir(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

/// The names in a directory, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn vault() -> Vault {
    Vault::from_key([7u8; 32])
}

fn decoded(sealed: &str) -> Vec<u8> {
    STANDARD
        .decode(sealed.strip_prefix("v1:").unwrap())
        .unwrap()
}

fn encoded(raw: &[u8]) -> String {
    format!("v1:{}", STANDARD.encode(raw))
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A key file as an operator might leave it: given bytes, mode 0600.
fn write_key_file(dir: &Path, bytes: &[u8]) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("secret.key");
    let _ = fs::remove_file(&path);
    fs::write(&path, bytes).unwrap();
    #[cfg(unix)]
    chmod(&path, 0o600);
    path
}

// --- sealing ------------------------------------------------------------------------------------

#[test]
fn seal_and_unseal_round_trip() {
    let v = vault();
    let big = "x".repeat(100_000);
    for (secret, context) in [
        ("", "user:1:bedrock"),
        ("ABSKQmVkcm9ja0FQSUtleQ==", "user:42:bedrock"),
        ("pässwörd 🔑 秘密\n\t\"'", "user:7:slack"),
        (big.as_str(), ""),
    ] {
        let sealed = v.seal(secret, context);
        assert!(sealed.starts_with("v1:") && is_sealed(&sealed));
        assert_eq!(
            decoded(&sealed).len(),
            NONCE_LEN + secret.len() + TAG_LEN,
            "nonce ‖ ciphertext ‖ tag"
        );
        if secret.len() >= 8 {
            assert!(
                !sealed.contains(&secret[..8]),
                "no plaintext in the sealed form"
            );
        }
        assert_eq!(v.unseal(&sealed, context).unwrap(), secret);
        assert_eq!(
            v.unseal(&format!("  {sealed}\n"), context).unwrap(),
            secret,
            "surrounding whitespace is ignored"
        );
    }
}

/// The format is plain RFC 8439 ChaCha20-Poly1305 (nonce ‖ ciphertext ‖ tag, context as AAD):
/// this value was sealed by another implementation, Python's `cryptography` 43, with key
/// 00 01 … 1f and nonce 00 01 … 0b.
#[test]
fn opens_a_value_sealed_by_another_implementation() {
    let key: [u8; 32] = std::array::from_fn(|i| i as u8);
    let sealed = "v1:AAECAwQFBgcICQoL8ZRwYgQml3ODrgrFryUjEKwTwIIlVE8ldZqhPWC45r4pd8SQ5g810A4=";
    assert_eq!(
        Vault::from_key(key)
            .unseal(sealed, "user:42:slack")
            .unwrap(),
        "xoxb-1234-5678-secret \u{2713}"
    );
    assert!(
        Vault::from_key(key)
            .unseal(sealed, "user:42:bedrock")
            .is_err()
    );
}

#[test]
fn every_seal_uses_a_fresh_nonce() {
    let v = vault();
    let a = v.seal("same secret", "same:context");
    let b = v.seal("same secret", "same:context");
    assert_ne!(a, b);
    assert_ne!(decoded(&a)[..NONCE_LEN], decoded(&b)[..NONCE_LEN]);
}

#[test]
fn the_context_is_bound() {
    let v = vault();
    let sealed = v.seal("xoxb-slack-token", "user:42:slack");
    for other in [
        "user:43:slack",
        "user:42:bedrock",
        "",
        "user:42:slack ",
        "USER:42:SLACK",
        "user:42:slac",
    ] {
        let e = v.unseal(&sealed, other).unwrap_err();
        assert!(
            matches!(&e, Error::Open(m) if m.contains("another record")),
            "{other:?}: {e:?}"
        );
    }
    assert_eq!(
        v.unseal(&sealed, "user:42:slack").unwrap(),
        "xoxb-slack-token"
    );
}

#[test]
fn another_key_cant_open_it() {
    let sealed = vault().seal("token", "ctx");
    for flip in [0usize, 15, 31] {
        let mut key = [7u8; 32];
        key[flip] ^= 1;
        assert!(
            matches!(Vault::from_key(key).unseal(&sealed, "ctx"), Err(Error::Open(m)) if m.contains("another key"))
        );
    }
}

#[test]
fn any_changed_bit_is_caught() {
    let v = vault();
    let sealed = v.seal("a secret worth keeping", "ctx");
    let raw = decoded(&sealed);
    for i in 0..raw.len() {
        for bit in [0x01u8, 0x80] {
            let mut tampered = raw.clone();
            tampered[i] ^= bit;
            assert!(
                matches!(v.unseal(&encoded(&tampered), "ctx"), Err(Error::Open(_))),
                "byte {i} bit {bit:#x}"
            );
        }
    }
    // Another value's nonce, or two values spliced together, don't open either.
    let other = decoded(&v.seal("a secret worth keeping", "ctx"));
    let mut spliced = other[..NONCE_LEN].to_vec();
    spliced.extend_from_slice(&raw[NONCE_LEN..]);
    assert!(v.unseal(&encoded(&spliced), "ctx").is_err());
}

#[test]
fn truncated_values_fail_cleanly() {
    let v = vault();
    let sealed = v.seal("a secret", "ctx");
    for n in 0..sealed.len() {
        assert!(v.unseal(&sealed[..n], "ctx").is_err(), "cut at {n}");
    }
    let raw = decoded(&sealed);
    for n in 0..raw.len() {
        assert!(
            matches!(v.unseal(&encoded(&raw[..n]), "ctx"), Err(Error::Open(_))),
            "{n} bytes"
        );
    }
}

#[test]
fn hostile_input_is_refused_without_panicking() {
    let v = vault();
    let huge = format!("v1:{}", "A".repeat(MAX_SEALED_LEN));
    let cases: Vec<String> = vec![
        "".into(),
        " ".into(),
        "v1:".into(),
        "v1:=".into(),
        "v1:====".into(),
        "v1:!!!!".into(),
        "v1:AAAA".into(),
        "v1:\0\0\0\0".into(),
        "v1:QUJD\u{feff}".into(),
        "\u{feff}v1:AAAA".into(),
        "V1:AAAA".into(),
        "stub:secret".into(),
        "plain secret".into(),
        "🔑🔑🔑".into(),
        encoded(&[0u8; NONCE_LEN + TAG_LEN - 1]),
        encoded(&[0u8; NONCE_LEN + TAG_LEN]),
        encoded(&[0xFFu8; 200]),
        format!("v1:{}", STANDARD_NO_PAD.encode([1u8; 40])),
        format!("v1:{}", URL_SAFE.encode([0xFBu8; 40])),
        huge,
    ];
    for case in &cases {
        let result = v.unseal(case, "ctx");
        assert!(
            matches!(result, Err(Error::Open(_))),
            "{:?}: {result:?}",
            short(case)
        );
    }
    // Pseudo-random strings, with and without the prefix (xorshift, so the run is repeatable).
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=:v1 -_\n";
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _ in 0..3000 {
        let len = (next() % 160) as usize;
        let mut s = String::from(if next() & 1 == 0 { "v1:" } else { "" });
        for _ in 0..len {
            s.push(alphabet[(next() % alphabet.len() as u64) as usize] as char);
        }
        assert!(v.unseal(&s, "ctx").is_err(), "{s:?}");
        let raw: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        assert!(v.unseal(&encoded(&raw), "ctx").is_err());
    }
}

fn short(s: &str) -> String {
    s.chars().take(40).collect()
}

#[test]
fn newer_formats_are_named() {
    let e = vault().unseal("v2:AAAA", "ctx").unwrap_err();
    assert!(
        matches!(&e, Error::Open(m) if m.contains("format v2")),
        "{e:?}"
    );
    assert!(
        matches!(vault().unseal("stub:x", "ctx"), Err(Error::Open(m)) if m.contains("isn't a sealed value"))
    );
    assert!(is_sealed(" v1:abc") && !is_sealed("V1:abc") && !is_sealed("abc") && !is_sealed(""));
}

// --- the key ------------------------------------------------------------------------------------

#[test]
fn the_key_file_is_created_once_with_mode_0600() {
    let dir = TempDir::new("create");
    let data = dir.path().join("data");
    let first = open_with(&data, None).unwrap();
    let file = data.join("secret.key");
    assert_eq!(fs::metadata(&file).unwrap().len(), 32);
    #[cfg(unix)]
    assert_eq!(mode(&file), 0o600);
    assert_ne!(first.key, [0u8; 32]);
    let second = open_with(&data, None).unwrap();
    assert_eq!(first.key, second.key);
    assert_eq!(second.unseal(&first.seal("s", "c"), "c").unwrap(), "s");
    assert_eq!(fs::read(&file).unwrap(), first.key);
    assert_eq!(
        names(&data),
        vec!["secret.key"],
        "no temporary files left behind"
    );
}

#[test]
fn an_existing_key_file_is_used_as_is() {
    let dir = TempDir::new("existing");
    let key: [u8; 32] = std::array::from_fn(|i| i as u8);
    write_key_file(dir.path(), &key);
    assert_eq!(open_with(dir.path(), None).unwrap().key, key);
}

#[test]
fn many_processes_starting_at_once_agree_on_one_key() {
    let dir = TempDir::new("race");
    let n = 16;
    let barrier = Arc::new(Barrier::new(n));
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let barrier = barrier.clone();
            let data = dir.path().to_path_buf();
            std::thread::spawn(move || {
                barrier.wait();
                open_with(&data, None).unwrap().key
            })
        })
        .collect();
    let keys: Vec<[u8; 32]> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(
        keys.windows(2).all(|w| w[0] == w[1]),
        "everyone uses the same key"
    );
    assert_eq!(fs::read(dir.path().join("secret.key")).unwrap(), keys[0]);
    assert_eq!(names(dir.path()), vec!["secret.key"]);
}

#[cfg(unix)]
#[test]
fn a_key_file_other_users_can_read_is_refused() {
    let dir = TempDir::new("perms");
    let original = open_with(dir.path(), None).unwrap().key;
    let file = dir.path().join("secret.key");
    for bad in [
        0o644,
        0o640,
        0o604,
        0o620,
        0o602,
        0o660,
        0o666,
        0o700 | 0o044,
    ] {
        chmod(&file, bad);
        let e = open_with(dir.path(), None).unwrap_err();
        assert!(
            matches!(&e, Error::Key(m) if m.contains("other users") && m.contains("chmod 600")),
            "{bad:o}: {e:?}"
        );
    }
    chmod(&file, 0o400);
    assert_eq!(
        open_with(dir.path(), None).unwrap().key,
        original,
        "read-only for the owner is fine"
    );
    chmod(&file, 0o600);
    assert_eq!(
        open_with(dir.path(), None).unwrap().key,
        original,
        "refusing never replaced it"
    );
}

#[test]
fn a_key_file_of_the_wrong_size_is_refused_and_left_alone() {
    let dir = TempDir::new("size");
    for len in [0usize, 1, 31, 33, 44, 64] {
        let file = write_key_file(dir.path(), &vec![5u8; len]);
        let e = open_with(dir.path(), None).unwrap_err();
        assert!(
            matches!(&e, Error::Key(m) if m.contains(&format!("is {len} bytes"))),
            "{len}: {e:?}"
        );
        assert_eq!(
            fs::read(&file).unwrap(),
            vec![5u8; len],
            "never overwritten"
        );
    }
}

#[test]
fn a_key_path_that_isnt_a_file_is_refused() {
    let dir = TempDir::new("notfile");
    fs::create_dir_all(dir.path().join("secret.key")).unwrap();
    assert!(
        matches!(open_with(dir.path(), None), Err(Error::Key(m)) if m.contains("regular file"))
    );
}

#[cfg(unix)]
#[test]
fn an_unwritable_data_dir_is_a_key_error() {
    let dir = TempDir::new("readonly");
    fs::create_dir_all(dir.path()).unwrap();
    chmod(dir.path(), 0o500);
    let result = open_with(dir.path(), None);
    chmod(dir.path(), 0o700);
    // (Root can write anywhere; then the key is simply created.)
    if let Err(e) = result {
        assert!(
            matches!(&e, Error::Key(m) if m.contains("can't create")),
            "{e:?}"
        );
    }
}

#[test]
fn secrets_key_takes_precedence_and_no_file_is_made() {
    let dir = TempDir::new("env");
    let key = [42u8; 32];
    let b64 = STANDARD.encode(key);
    assert_eq!(open_with(dir.path(), Some(&b64)).unwrap().key, key);
    assert!(
        !dir.path().exists(),
        "nothing written when SECRETS_KEY is set"
    );
    // A key file exists too: SECRETS_KEY still wins.
    let from_file = open_with(dir.path(), None).unwrap().key;
    assert_ne!(from_file, key);
    assert_eq!(open_with(dir.path(), Some(&b64)).unwrap().key, key);
    // Unpadded and URL-safe base64 work; blank means unset.
    assert_eq!(
        open_with(
            dir.path(),
            Some(&format!("  {}\n", STANDARD_NO_PAD.encode(key)))
        )
        .unwrap()
        .key,
        key
    );
    let url_key = [0xFBu8; 32];
    assert_eq!(
        open_with(dir.path(), Some(&URL_SAFE_NO_PAD.encode(url_key)))
            .unwrap()
            .key,
        url_key
    );
    assert_eq!(open_with(dir.path(), Some("   ")).unwrap().key, from_file);
}

#[test]
fn bad_secrets_key_values_are_explained() {
    let dir = TempDir::new("envbad");
    let cases = [
        (STANDARD.encode([1u8; 16]), "decodes to 16 bytes"),
        (STANDARD.encode([1u8; 33]), "decodes to 33 bytes"),
        (STANDARD.encode([1u8; 64]), "decodes to 64 bytes"),
        ("not base64!".to_string(), "isn't valid base64"),
        ("ab".repeat(32), "looks like hex"),
    ];
    for (value, words) in cases {
        let e = open_with(dir.path(), Some(&value)).unwrap_err();
        assert!(
            matches!(&e, Error::Key(m) if m.contains(words) && m.contains("openssl rand -base64 32")),
            "{value}: {e:?}"
        );
    }
    assert!(!dir.path().exists());
}

const CHILD_DIR: &str = "OPPORTUNITY_VAULT_TEST_DIR";

/// `Vault::open` itself reads SECRETS_KEY. Checked in a child process, so no test ever changes
/// this process's environment.
#[test]
fn open_reads_secrets_key_from_the_environment() {
    let dir = TempDir::new("envproc");
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::child_opens_with_the_env_key",
            "--ignored",
            "--test-threads=1",
        ])
        .env(CHILD_DIR, dir.path())
        .env("SECRETS_KEY", STANDARD.encode([9u8; 32]))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!dir.path().exists(), "the env key was used, not a file");
}

#[test]
#[ignore = "run in a child process by open_reads_secrets_key_from_the_environment"]
fn child_opens_with_the_env_key() {
    let dir = std::env::var(CHILD_DIR).expect("started by the parent test");
    assert_eq!(Vault::open(Path::new(&dir)).unwrap().key, [9u8; 32]);
}

#[test]
fn open_uses_the_key_file_without_secrets_key() {
    if std::env::var("SECRETS_KEY").is_ok_and(|v| !v.trim().is_empty()) {
        return; // this environment sets one; the precedence tests cover that case
    }
    let dir = TempDir::new("open");
    let v = Vault::open(dir.path()).unwrap();
    assert_eq!(fs::read(dir.path().join("secret.key")).unwrap(), v.key);
}

// --- the rest -------------------------------------------------------------------------------------

#[test]
fn masking() {
    assert_eq!(mask(""), "••••");
    assert_eq!(mask("short"), "••••");
    assert_eq!(
        mask("12345678901"),
        "••••",
        "under 12 characters: nothing shown"
    );
    assert_eq!(mask("abcdefghijkl"), "••••ijkl");
    assert_eq!(mask("ключ-секрет-1234"), "••••1234");
    assert_eq!(mask("密密密密密密密密密密密尾"), "••••密密密尾");
}

#[test]
fn the_key_never_shows() {
    let shown = format!("{:?}", vault());
    assert!(
        shown.contains("redacted") && !shown.contains('7'),
        "{shown}"
    );
    assert_eq!(Error::Key("x".into()).to_string(), "secrets key: x");
    assert_eq!(Error::Open("y".into()).to_string(), "can't open secret: y");
}
