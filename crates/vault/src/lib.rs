//! Secrets at rest: model provider keys (Bedrock), connector tokens (Slack, Microsoft, Atlassian).
//!
//! ChaCha20-Poly1305 with a 256-bit master key. The master key comes from `SECRETS_KEY` (base64,
//! 32 bytes) or, failing that, `DATA_DIR/secret.key`, created on first run with mode 0600. Every
//! sealed value is bound to a *context* (e.g. `"user:42:bedrock"`) as associated data, so a
//! ciphertext copied onto another record won't open. Secrets never leave the server: the web
//! client only ever sees [`mask`]ed forms.
//!
//! Format: `v1:` + base64(12-byte random nonce ‖ ciphertext+tag).
//!
//! Keep the key safe and backed up with the database: without it, nothing sealed can be opened.
//! A key file that other users can read, or that isn't exactly 32 bytes, is refused rather than
//! used or replaced.
//!
//! Owner: the Bedrock & vault builder.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

const PREFIX: &str = "v1:";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;
const KEY_FILE: &str = "secret.key";
/// Sealed secrets are small; anything bigger than this isn't one, and isn't decoded.
const MAX_SEALED_LEN: usize = 4 * 1024 * 1024;
const KEY_HELP: &str = "Generate one with: openssl rand -base64 32";

#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// The master key is missing, malformed or unreadable.
    Key(String),
    /// A sealed value is malformed, was sealed with another key, or for another context.
    Open(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Key(m) => write!(f, "secrets key: {m}"),
            Error::Open(m) => write!(f, "can't open secret: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub struct Vault {
    key: [u8; KEY_LEN],
}

/// Never shows the key.
impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault").field("key", &"<redacted>").finish()
    }
}

impl Vault {
    /// The vault for this installation: `SECRETS_KEY` if set, else `data_dir/secret.key`
    /// (generated on first use, mode 0600).
    pub fn open(data_dir: &Path) -> Result<Vault, Error> {
        open_with(data_dir, std::env::var("SECRETS_KEY").ok().as_deref())
    }

    pub fn from_key(key: [u8; 32]) -> Vault {
        Vault { key }
    }

    /// Encrypt `plaintext` for `context`.
    pub fn seal(&self, plaintext: &str, context: &str) -> String {
        let nonce = fresh_nonce();
        let cipher = ChaCha20Poly1305::new(&Key::from(self.key));
        match cipher.encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: plaintext.as_bytes(),
                aad: context.as_bytes(),
            },
        ) {
            Ok(sealed) => {
                let mut raw = Vec::with_capacity(NONCE_LEN + sealed.len());
                raw.extend_from_slice(&nonce);
                raw.extend_from_slice(&sealed);
                format!("{PREFIX}{}", STANDARD.encode(raw))
            }
            // Only a message over 256 GiB can fail, which no secret is. The bare prefix never opens.
            Err(_) => PREFIX.to_string(),
        }
    }

    /// Decrypt a value sealed for `context`.
    pub fn unseal(&self, sealed: &str, context: &str) -> Result<String, Error> {
        let sealed = sealed.trim();
        if sealed.len() > MAX_SEALED_LEN {
            return Err(Error::Open(
                "the value is too large to be a sealed secret".into(),
            ));
        }
        let Some(encoded) = sealed.strip_prefix(PREFIX) else {
            return Err(Error::Open(match sealed.split_once(':') {
                Some((tag, _))
                    if tag.len() > 1
                        && tag.starts_with('v')
                        && tag[1..].bytes().all(|b| b.is_ascii_digit()) =>
                {
                    format!("it was sealed in format {tag}, which this version can't read")
                }
                _ => "it isn't a sealed value".into(),
            }));
        };
        let raw = STANDARD
            .decode(encoded)
            .map_err(|_| Error::Open("the sealed value is damaged (not valid base64)".into()))?;
        if raw.len() < NONCE_LEN + TAG_LEN {
            return Err(Error::Open("the sealed value is too short".into()));
        }
        let (nonce, ciphertext) = raw.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce
            .try_into()
            .map_err(|_| Error::Open("the sealed value is too short".into()))?;
        let cipher = ChaCha20Poly1305::new(&Key::from(self.key));
        let plain = cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad: context.as_bytes(),
                },
            )
            .map_err(|_| {
                Error::Open(
                    "it was sealed with another key or for another record, or it was changed"
                        .into(),
                )
            })?;
        String::from_utf8(plain).map_err(|_| Error::Open("the secret isn't text".into()))
    }
}

/// A secret as the web client may see it: `••••` plus its last 4 characters (all bullets when it
/// is shorter than 12 characters).
pub fn mask(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() < 12 {
        return "••••".into();
    }
    format!(
        "••••{}",
        chars[chars.len() - 4..].iter().collect::<String>()
    )
}

/// True when `value` looks sealed (has the format prefix); says nothing about whether it opens.
/// Handy when moving plaintext settings over to sealed ones.
pub fn is_sealed(value: &str) -> bool {
    value.trim().starts_with(PREFIX)
}

/// [`Vault::open`] with the environment's `SECRETS_KEY` passed in (blank means unset).
fn open_with(data_dir: &Path, env_key: Option<&str>) -> Result<Vault, Error> {
    if let Some(value) = env_key.map(str::trim).filter(|v| !v.is_empty()) {
        return decode_env_key(value).map(Vault::from_key);
    }
    let path = data_dir.join(KEY_FILE);
    match read_key_file(&path)? {
        Some(key) => Ok(Vault::from_key(key)),
        None => {
            create_key_file(data_dir, &path)?;
            // Read back what is on disk: if another process created the file at the same moment,
            // its key is the one everybody uses.
            read_key_file(&path)?.map(Vault::from_key).ok_or_else(|| {
                Error::Key(format!(
                    "{} disappeared right after it was created",
                    path.display()
                ))
            })
        }
    }
}

fn decode_env_key(value: &str) -> Result<[u8; KEY_LEN], Error> {
    let bytes = [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD]
        .iter()
        .find_map(|engine| engine.decode(value).ok())
        .ok_or_else(|| Error::Key(format!("SECRETS_KEY isn't valid base64. {KEY_HELP}")))?;
    <[u8; KEY_LEN]>::try_from(bytes.as_slice()).map_err(|_| {
        let hint = if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) { " It looks like hex; it must be base64." } else { "" };
        Error::Key(format!("SECRETS_KEY must be the base64 of exactly 32 bytes, but it decodes to {} bytes.{hint} {KEY_HELP}", bytes.len()))
    })
}

/// The key in `path`, None when there is no such file, an error when the file can't be trusted.
fn read_key_file(path: &Path) -> Result<Option<[u8; KEY_LEN]>, Error> {
    let shown = path.display();
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Key(format!("can't read {shown}: {e}"))),
    };
    if !meta.is_file() {
        return Err(Error::Key(format!("{shown} isn't a regular file")));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(Error::Key(format!(
                "{shown} can be read or changed by other users (mode {mode:03o}). Run: chmod 600 {shown}"
            )));
        }
    }
    if meta.len() != KEY_LEN as u64 {
        return Err(Error::Key(format!(
            "{shown} is {} bytes, but a secrets key file is exactly {KEY_LEN}. Restore the original file (without it, sealed secrets can't be opened), or set SECRETS_KEY",
            meta.len()
        )));
    }
    let mut file = File::open(path).map_err(|e| Error::Key(format!("can't read {shown}: {e}")))?;
    let mut key = [0u8; KEY_LEN];
    file.read_exact(&mut key)
        .map_err(|e| Error::Key(format!("can't read {shown}: {e}")))?;
    let mut more = [0u8; 1];
    if file
        .read(&mut more)
        .map_err(|e| Error::Key(format!("can't read {shown}: {e}")))?
        != 0
    {
        return Err(Error::Key(format!(
            "{shown} changed while it was being read"
        )));
    }
    Ok(Some(key))
}

/// A new random key in `path`, created whole and atomically, mode 0600: written to a temporary
/// file in the same directory, then linked into place. Unlike a rename, a link never replaces a
/// file that is already there, so two processes starting at once can't each keep a different
/// key. Filesystems without hard links fall back to a rename.
fn create_key_file(dir: &Path, path: &Path) -> Result<(), Error> {
    fs::create_dir_all(dir)
        .map_err(|e| Error::Key(format!("can't create {}: {e}", dir.display())))?;
    let mut key = [0u8; KEY_LEN];
    getrandom::fill(&mut key)
        .map_err(|e| Error::Key(format!("no secure randomness to make a key: {e}")))?;
    let tmp = dir.join(format!(
        ".{KEY_FILE}.{}.{:08x}.tmp",
        std::process::id(),
        unique_u32()
    ));
    let result = write_new_file(&tmp, &key)
        .map_err(|e| Error::Key(format!("can't create {}: {e}", tmp.display())))
        .and_then(|()| publish(&tmp, path));
    let _ = fs::remove_file(&tmp);
    result
}

fn write_new_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        // Exactly 0600, whatever the umask did.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.sync_all()
}

fn publish(tmp: &Path, path: &Path) -> Result<(), Error> {
    match fs::hard_link(tmp, path) {
        Ok(()) => {}
        // Someone else made the key first: theirs stands.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(()),
        Err(_) => {
            if fs::symlink_metadata(path).is_ok() {
                return Ok(());
            }
            fs::rename(tmp, path)
                .map_err(|e| Error::Key(format!("can't create {}: {e}", path.display())))?;
        }
    }
    // Make the new name durable too (best effort; not every platform can sync a directory).
    if let Some(dir) = path.parent()
        && let Ok(d) = File::open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(())
}

/// A nonce for one seal: 12 random bytes. Should the OS ever have no randomness to give, a
/// nonce that is still unique for this key: the clock in nanoseconds, a per-process counter
/// and the process id. (ChaCha20-Poly1305 needs nonces never to repeat, not to be secret.)
fn fresh_nonce() -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    if getrandom::fill(&mut nonce).is_ok() {
        return nonce;
    }
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed) ^ std::process::id().rotate_left(16);
    nonce[..8].copy_from_slice(&nanos.to_le_bytes());
    nonce[8..].copy_from_slice(&count.to_le_bytes());
    nonce
}

fn unique_u32() -> u32 {
    getrandom::u32().unwrap_or_else(|_| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    })
}

#[cfg(test)]
mod tests;
