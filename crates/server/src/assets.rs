//! The web client: `web/` embedded in the binary, or read from `WEB_DIR` on every request for
//! live editing. Files get an `ETag` and `Cache-Control: no-cache` (so a deploy shows up at once
//! and an unchanged file costs a 304). Any other path that isn't a file gets `index.html`: the
//! client routes `/`, `/reset` and the rest itself.

use std::path::{Component, Path};
use std::sync::OnceLock;

use crate::app;
use crate::http::{ApiError, Req, Resp};

struct Asset {
    name: &'static str,
    content_type: &'static str,
    bytes: &'static [u8],
}

const EMBEDDED: &[Asset] = &[
    Asset {
        name: "index.html",
        content_type: "text/html; charset=utf-8",
        bytes: include_bytes!("../../../web/index.html"),
    },
    Asset {
        name: "app.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../../web/app.js"),
    },
    Asset {
        name: "app.css",
        content_type: "text/css; charset=utf-8",
        bytes: include_bytes!("../../../web/app.css"),
    },
    Asset {
        name: "padlock.css",
        content_type: "text/css; charset=utf-8",
        bytes: include_bytes!("../../../web/padlock.css"),
    },
    Asset {
        name: "padlock.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../../web/padlock.js"),
    },
    Asset {
        name: "admin.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../../../web/admin.js"),
    },
    Asset {
        name: "favicon.svg",
        content_type: "image/svg+xml",
        bytes: include_bytes!("../../../web/favicon.svg"),
    },
];

/// ETags of the embedded files, computed once.
fn embedded_etag(i: usize) -> &'static str {
    static TAGS: OnceLock<Vec<String>> = OnceLock::new();
    &TAGS.get_or_init(|| EMBEDDED.iter().map(|a| etag(a.bytes)).collect())[i]
}

fn etag(bytes: &[u8]) -> String {
    format!("\"{}\"", &crate::util::sha256_hex(bytes)[..20])
}

pub fn content_type(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "json" | "map" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "woff2" => "font/woff2",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

pub fn serve(req: &Req) -> Resp {
    if req.method != "GET" {
        return Resp::error(&ApiError::method_not_allowed()).with_header("Allow", "GET, HEAD");
    }
    let name = req.path.trim_start_matches('/');
    let name = if name.is_empty() { "index.html" } else { name };
    if let Some(resp) = file(req, name) {
        return resp;
    }
    file(req, "index.html").unwrap_or_else(|| Resp::error(&ApiError::not_found()))
}

/// One file of the web client, from `WEB_DIR` when set (falling back to the embedded copy).
fn file(req: &Req, name: &str) -> Option<Resp> {
    if let Some(dir) = &app().cfg.web_dir
        && let Some(rel) = safe_relative(name)
        && let Ok(bytes) = std::fs::read(dir.join(rel))
    {
        let tag = etag(&bytes);
        return Some(respond(req, content_type(name), bytes, tag));
    }
    let i = EMBEDDED.iter().position(|a| a.name == name)?;
    let asset = &EMBEDDED[i];
    Some(respond(
        req,
        asset.content_type,
        asset.bytes.to_vec(),
        embedded_etag(i).to_string(),
    ))
}

fn respond(req: &Req, content_type: &'static str, bytes: Vec<u8>, tag: String) -> Resp {
    let fresh = req.header("if-none-match").is_some_and(|v| {
        v.split(',')
            .any(|t| t.trim().trim_start_matches("W/") == tag)
    });
    let resp = if fresh {
        Resp::empty(304)
    } else {
        Resp::bytes(200, content_type, bytes)
    };
    resp.with_header("ETag", tag)
        .with_header("Cache-Control", "no-cache")
}

/// A path inside WEB_DIR made only of normal components with tame characters, or None.
fn safe_relative(name: &str) -> Option<&Path> {
    let ok_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/'));
    let path = Path::new(name);
    let ok_parts = path
        .components()
        .all(|c| matches!(c, Component::Normal(p) if !p.to_string_lossy().starts_with('.')));
    (ok_chars && ok_parts && !name.is_empty()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths() {
        assert!(safe_relative("app.js").is_some());
        assert!(safe_relative("dev/tool.html").is_some());
        assert!(safe_relative("../etc/passwd").is_none());
        assert!(safe_relative("/etc/passwd").is_none());
        assert!(safe_relative(".env").is_none());
        assert!(safe_relative("a/./b").is_some());
        assert!(safe_relative("a b.js").is_none());
        assert!(safe_relative("").is_none());
    }

    #[test]
    fn types() {
        assert_eq!(content_type("app.JS"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("x.svg"), "image/svg+xml");
        assert_eq!(content_type("noext"), "application/octet-stream");
    }

    #[test]
    fn embedded_files_present() {
        assert_eq!(EMBEDDED.len(), 7);
        assert!(EMBEDDED.iter().all(|a| !a.bytes.is_empty()));
        assert_eq!(etag(b"x").len(), 22);
    }
}
