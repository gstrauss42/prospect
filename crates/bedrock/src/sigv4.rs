//! AWS Signature Version 4, header form: just what Bedrock needs, without an AWS SDK.
//!
//! <https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html>
//!
//! Checked against the official SigV4 test suite (awslabs/aws-c-auth,
//! `tests/aws-signing-test-suite/v4`) and against botocore's own signature for a Bedrock
//! Converse call whose model id is an inference profile ARN.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// IAM credentials.
pub(crate) struct Credentials<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    /// Set for temporary (STS) credentials; signed as `x-amz-security-token`.
    pub session_token: Option<&'a str>,
}

/// The parts of a request that are signed.
pub(crate) struct Request<'a> {
    pub method: &'a str,
    /// The `Host` header exactly as sent (with `:port` when the port isn't the scheme's default).
    pub host: &'a str,
    /// The path as sent on the wire, already percent-encoded where it needs to be.
    pub path: &'a str,
    /// Query parameters, not encoded.
    pub query: &'a [(String, String)],
    /// Other headers to sign, any case. `host`, `x-amz-date` and `x-amz-security-token` are
    /// added by [`sign`].
    pub headers: &'a [(&'a str, &'a str)],
    pub payload: &'a [u8],
}

/// The headers that sign one request: `x-amz-date`, `x-amz-security-token` (with temporary
/// credentials) and `authorization`. A signature is valid for a few minutes, so sign every
/// attempt afresh.
pub(crate) fn sign(
    req: &Request,
    creds: &Credentials,
    region: &str,
    service: &str,
    amz_date: &str,
) -> Vec<(&'static str, String)> {
    let date = amz_date.get(..8).unwrap_or(amz_date);
    let token = creds.session_token.filter(|t| !t.is_empty());
    let (canonical, signed_headers) = canonical_request(req, amz_date, token);
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = string_to_sign(amz_date, &scope, &canonical);
    let key = signing_key(creds.secret_access_key, date, region, service);
    let signature = hex(&hmac_sha256(&key, to_sign.as_bytes()));
    let mut out = vec![("x-amz-date", amz_date.to_string())];
    if let Some(t) = token {
        out.push(("x-amz-security-token", t.to_string()));
    }
    out.push((
        "authorization",
        format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}", creds.access_key_id),
    ));
    out
}

/// The canonical request and its signed-header list.
pub(crate) fn canonical_request(
    req: &Request,
    amz_date: &str,
    token: Option<&str>,
) -> (String, String) {
    let mut headers: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), canonical_header_value(v)))
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "host" | "x-amz-date" | "x-amz-security-token" | "authorization"
            )
        })
        .collect();
    headers.push(("host".into(), canonical_header_value(req.host)));
    headers.push(("x-amz-date".into(), amz_date.to_string()));
    if let Some(t) = token {
        headers.push(("x-amz-security-token".into(), canonical_header_value(t)));
    }
    // Stable sort by name; repeated headers become one, their values joined by commas in order.
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let mut merged: Vec<(String, String)> = Vec::with_capacity(headers.len());
    for (k, v) in headers {
        match merged.last_mut() {
            Some((last, values)) if *last == k => {
                values.push(',');
                values.push_str(&v);
            }
            _ => merged.push((k, v)),
        }
    }
    let canonical_headers: String = merged.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = merged
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method.to_ascii_uppercase(),
        canonical_uri(req.path),
        canonical_query(req.query),
        canonical_headers,
        signed_headers,
        hex(&Sha256::digest(req.payload)),
    );
    (canonical, signed_headers)
}

pub(crate) fn string_to_sign(amz_date: &str, scope: &str, canonical_request: &str) -> String {
    format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    )
}

pub(crate) fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// The path normalised as for every AWS service but S3 (empty and `.` segments dropped, `..`
/// applied), each segment URI-encoded once more. A path as sent on the wire is already encoded,
/// so `%3A` there becomes `%253A` here.
pub(crate) fn canonical_uri(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    let mut out = String::from("/");
    out.push_str(
        &segments
            .iter()
            .map(|s| uri_encode(s))
            .collect::<Vec<_>>()
            .join("/"),
    );
    if !segments.is_empty() && path.ends_with('/') {
        out.push('/');
    }
    out
}

/// Parameters URI-encoded, sorted by name and then value.
pub(crate) fn canonical_query(params: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (uri_encode(k), uri_encode(v)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// The query string to send: the same encoding as the canonical form (so `+`, `/` and `=` in a
/// pagination token survive), in the given order.
pub(crate) fn query_string(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", uri_encode(k), uri_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// RFC 3986 encoding: everything except `A–Z a–z 0–9 - _ . ~` becomes `%XX` (uppercase hex).
pub(crate) fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Trimmed, with runs of whitespace collapsed to one space.
fn canonical_header_value(v: &str) -> String {
    v.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `YYYYMMDD'T'HHMMSS'Z'` for a Unix time.
pub(crate) fn amz_date(unix: i64) -> String {
    let (y, m, d) = model::date::civil_from_days(unix.div_euclid(model::date::DAY_SECS));
    let s = unix.rem_euclid(model::date::DAY_SECS);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        s / 3600,
        s % 3600 / 60,
        s % 60
    )
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    // HMAC takes keys of any length, so this never fails; a zero MAC would only make AWS refuse
    // the signature, never crash.
    let Ok(mut mac) = <Hmac<Sha256> as KeyInit>::new_from_slice(key) else {
        return [0; 32];
    };
    mac.update(data);
    mac.finalize().into_bytes().into()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // The AWS SigV4 test suite's credentials, region, service and time.
    const AKID: &str = "AKIDEXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const DATE: &str = "20150830T123600Z";
    const HOST: &str = "example.amazonaws.com";

    struct Case<'a> {
        method: &'a str,
        path: &'a str,
        query: &'a [(&'a str, &'a str)],
        headers: &'a [(&'a str, &'a str)],
        payload: &'a str,
        token: Option<&'a str>,
        canonical: &'a str,
        string_to_sign: Option<&'a str>,
        signature: &'a str,
    }

    /// Signs a case as the suite does and checks every intermediate step.
    fn check(name: &str, c: &Case) {
        let query: Vec<(String, String)> = c
            .query
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let req = Request {
            method: c.method,
            host: HOST,
            path: c.path,
            query: &query,
            headers: c.headers,
            payload: c.payload.as_bytes(),
        };
        let (canonical, signed) = canonical_request(&req, DATE, c.token);
        assert_eq!(canonical, c.canonical, "{name}: canonical request");
        let sts = string_to_sign(DATE, "20150830/us-east-1/service/aws4_request", &canonical);
        if let Some(expected) = c.string_to_sign {
            assert_eq!(sts, expected, "{name}: string to sign");
        }
        let creds = Credentials {
            access_key_id: AKID,
            secret_access_key: SECRET,
            session_token: c.token,
        };
        let headers = sign(&req, &creds, "us-east-1", "service", DATE);
        let auth = &headers
            .iter()
            .find(|(k, _)| *k == "authorization")
            .unwrap()
            .1;
        assert_eq!(
            auth,
            &format!(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders={signed}, Signature={}",
                c.signature
            ),
            "{name}: authorization"
        );
        assert_eq!(headers[0], ("x-amz-date", DATE.to_string()));
        assert_eq!(
            headers.iter().any(|(k, _)| *k == "x-amz-security-token"),
            c.token.is_some(),
            "{name}: token header"
        );
    }

    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn get_vanilla() {
        check(
            "get-vanilla",
            &Case {
                method: "GET",
                path: "/",
                query: &[],
                headers: &[],
                payload: "",
                token: None,
                canonical: &format!(
                    "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\n{EMPTY_SHA256}"
                ),
                string_to_sign: Some(
                    "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\nbb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63",
                ),
                signature: "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
            },
        );
    }

    #[test]
    fn post_vanilla() {
        check(
            "post-vanilla",
            &Case {
                method: "POST",
                path: "/",
                query: &[],
                headers: &[],
                payload: "",
                token: None,
                canonical: &format!(
                    "POST\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\n{EMPTY_SHA256}"
                ),
                string_to_sign: Some(
                    "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n553f88c9e4d10fc9e109e2aeb65f030801b70c2f6468faca261d401ae622fc87",
                ),
                signature: "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b",
            },
        );
    }

    #[test]
    fn get_vanilla_query_order_key_case() {
        check(
            "get-vanilla-query-order-key-case",
            &Case {
                method: "GET",
                path: "/",
                query: &[("Param2", "value2"), ("Param1", "value1")],
                headers: &[],
                payload: "",
                token: None,
                canonical: &format!(
                    "GET\n/\nParam1=value1&Param2=value2\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\n{EMPTY_SHA256}"
                ),
                string_to_sign: Some(
                    "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n816cd5b414d056048ba4f7c5386d6e0533120fb1fcfa93762cf0fc39e2cf19e0",
                ),
                signature: "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500",
            },
        );
    }

    #[test]
    fn post_x_www_form_urlencoded() {
        // The aws-c-auth version of this case signs the body hash header and content-length too.
        let body_sha = "9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e";
        check(
            "post-x-www-form-urlencoded",
            &Case {
                method: "POST",
                path: "/",
                query: &[],
                headers: &[
                    ("Content-Type", "application/x-www-form-urlencoded"),
                    ("Content-Length", "13"),
                    ("x-amz-content-sha256", body_sha),
                ],
                payload: "Param1=value1",
                token: None,
                canonical: &format!(
                    "POST\n/\n\ncontent-length:13\ncontent-type:application/x-www-form-urlencoded\nhost:example.amazonaws.com\nx-amz-content-sha256:{body_sha}\nx-amz-date:20150830T123600Z\n\ncontent-length;content-type;host;x-amz-content-sha256;x-amz-date\n{body_sha}"
                ),
                string_to_sign: Some(
                    "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\nb1edd1d03544c25390e32085d55b57acc9a3961bb59415ff86c45c3d89d16cfb",
                ),
                signature: "d3875051da38690788ef43de4db0d8f280229d82040bfac253562e56c3f20e0b",
            },
        );
    }

    #[test]
    fn more_suite_cases() {
        let empty = EMPTY_SHA256;
        let plain = |path: &str, query: &str| {
            format!(
                "GET\n{path}\n{query}\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\n{empty}"
            )
        };
        let cases = [
            (
                "get-vanilla-with-session-token",
                Case {
                    method: "GET",
                    path: "/",
                    query: &[],
                    headers: &[],
                    payload: "",
                    token: Some("6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267"),
                    canonical: &format!(
                        "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\nx-amz-security-token:6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267\n\nhost;x-amz-date;x-amz-security-token\n{empty}"
                    ),
                    string_to_sign: None,
                    signature: "07ec1639c89043aa0e3e2de82b96708f198cceab042d4a97044c66dd9f74e7f8",
                },
            ),
            (
                "post-header-key-sort",
                Case {
                    method: "POST",
                    path: "/",
                    query: &[],
                    headers: &[("My-Header1", "value1")],
                    payload: "",
                    token: None,
                    canonical: &format!(
                        "POST\n/\n\nhost:example.amazonaws.com\nmy-header1:value1\nx-amz-date:20150830T123600Z\n\nhost;my-header1;x-amz-date\n{empty}"
                    ),
                    string_to_sign: None,
                    signature: "c5410059b04c1ee005303aed430f6e6645f61f4dc9e1461ec8f8916fdf18852c",
                },
            ),
            (
                "get-header-value-trim",
                Case {
                    method: "GET",
                    path: "/",
                    query: &[],
                    headers: &[("My-Header1", " value1"), ("My-Header2", " \"a   b   c\"")],
                    payload: "",
                    token: None,
                    canonical: &format!(
                        "GET\n/\n\nhost:example.amazonaws.com\nmy-header1:value1\nmy-header2:\"a b c\"\nx-amz-date:20150830T123600Z\n\nhost;my-header1;my-header2;x-amz-date\n{empty}"
                    ),
                    string_to_sign: None,
                    signature: "acc3ed3afb60bb290fc8d2dd0098b9911fcaa05412b367055dee359757a9c736",
                },
            ),
            (
                "get-space-normalized",
                Case {
                    method: "GET",
                    path: "/example space/",
                    query: &[],
                    headers: &[],
                    payload: "",
                    token: None,
                    canonical: &plain("/example%20space/", ""),
                    string_to_sign: None,
                    signature: "652487583200325589f1fba4c7e578f72c47cb61beeca81406b39ddec1366741",
                },
            ),
            (
                "get-utf8",
                Case {
                    method: "GET",
                    path: "/\u{1234}",
                    query: &[],
                    headers: &[],
                    payload: "",
                    token: None,
                    canonical: &plain("/%E1%88%B4", ""),
                    string_to_sign: None,
                    signature: "8318018e0b0f223aa2bbf98705b62bb787dc9c0e678f255a891fd03141be5d85",
                },
            ),
            (
                "get-unreserved",
                Case {
                    method: "GET",
                    path: "/-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
                    query: &[],
                    headers: &[],
                    payload: "",
                    token: None,
                    canonical: &plain(
                        "/-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
                        "",
                    ),
                    string_to_sign: None,
                    signature: "07ef7494c76fa4850883e2b006601f940f8a34d404d0cfa977f52a65bbf5f24f",
                },
            ),
            (
                "get-relative-relative-normalized",
                Case {
                    method: "GET",
                    path: "/example1/example2/../..",
                    query: &[],
                    headers: &[],
                    payload: "",
                    token: None,
                    canonical: &plain("/", ""),
                    string_to_sign: None,
                    signature: "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
                },
            ),
            (
                "get-slash-dot-slash-normalized",
                Case {
                    method: "GET",
                    path: "/./",
                    query: &[],
                    headers: &[],
                    payload: "",
                    token: None,
                    canonical: &plain("/", ""),
                    string_to_sign: None,
                    signature: "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
                },
            ),
            (
                "get-slashes-normalized",
                Case {
                    method: "GET",
                    path: "//example//",
                    query: &[],
                    headers: &[],
                    payload: "",
                    token: None,
                    canonical: &plain("/example/", ""),
                    string_to_sign: None,
                    signature: "9a624bd73a37c9a373b5312afbebe7a714a789de108f0bdfe846570885f57e84",
                },
            ),
            (
                "get-vanilla-query-order-encoded",
                Case {
                    method: "GET",
                    path: "/",
                    query: &[
                        ("Param-3", "Value3"),
                        ("Param", "Value2"),
                        ("\u{1234}", "Value1"),
                    ],
                    headers: &[],
                    payload: "",
                    token: None,
                    canonical: &plain("/", "%E1%88%B4=Value1&Param=Value2&Param-3=Value3"),
                    string_to_sign: None,
                    signature: "371d3713e185cc334048618a97f809c9ffe339c62934c032af5a0e595648fcac",
                },
            ),
            (
                "get-vanilla-utf8-query",
                Case {
                    method: "GET",
                    path: "/",
                    query: &[("\u{1234}", "bar")],
                    headers: &[],
                    payload: "",
                    token: None,
                    canonical: &plain("/", "%E1%88%B4=bar"),
                    string_to_sign: None,
                    signature: "2cdec8eed098649ff3a119c94853b13c643bcf08f8b0a1d91e12c9027818dd04",
                },
            ),
            (
                "get-vanilla-query-unreserved",
                Case {
                    method: "GET",
                    path: "/",
                    query: &[(
                        "-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
                        "-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
                    )],
                    headers: &[],
                    payload: "",
                    token: None,
                    canonical: &plain(
                        "/",
                        "-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz=-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
                    ),
                    string_to_sign: None,
                    signature: "9c3e54bfcdf0b19771a7f523ee5669cdf59bc7cc0884027167c21bb143a40197",
                },
            ),
        ];
        for (name, case) in &cases {
            check(name, case);
        }
    }

    // Bedrock itself: botocore 1.42's bedrock-runtime client, Converse, clock pinned to
    // 2026-11-26T12:00:00Z, the suite's credentials. These are the bytes botocore sent.
    const BOTO_BODY: &str = r#"{"messages": [{"role": "user", "content": [{"text": "Reply with OK"}]}], "inferenceConfig": {"maxTokens": 16}}"#;
    const BOTO_HOST: &str = "bedrock-runtime.us-east-1.amazonaws.com";
    const PROFILE_ARN: &str = "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-sonnet-4-5-20250929-v1:0";

    #[test]
    fn arn_model_path_is_encoded_once_on_the_wire_and_twice_when_signed() {
        let wire = crate::converse_path(PROFILE_ARN);
        assert_eq!(
            wire,
            "/model/arn%3Aaws%3Abedrock%3Aus-east-1%3A123456789012%3Ainference-profile%2Fus.anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse"
        );
        assert_eq!(
            canonical_uri(&wire),
            "/model/arn%253Aaws%253Abedrock%253Aus-east-1%253A123456789012%253Ainference-profile%252Fus.anthropic.claude-sonnet-4-5-20250929-v1%253A0/converse"
        );
        // Behind a path prefix (the mock's /bedrock-runtime) the same rules apply.
        assert_eq!(
            canonical_uri("/bedrock-runtime/model/a%3Ab%2Fc/converse"),
            "/bedrock-runtime/model/a%253Ab%252Fc/converse"
        );
        assert_eq!(
            crate::converse_path("anthropic.claude-3-haiku-20240307-v1:0"),
            "/model/anthropic.claude-3-haiku-20240307-v1%3A0/converse"
        );
    }

    #[test]
    fn matches_botocore_for_an_inference_profile_arn() {
        let path = crate::converse_path(PROFILE_ARN);
        let req = Request {
            method: "POST",
            host: BOTO_HOST,
            path: &path,
            query: &[],
            headers: &[("Content-Type", "application/json")],
            payload: BOTO_BODY.as_bytes(),
        };
        let date = "20261126T120000Z";
        let (canonical, signed) = canonical_request(&req, date, None);
        assert_eq!(
            canonical,
            "POST\n/model/arn%253Aaws%253Abedrock%253Aus-east-1%253A123456789012%253Ainference-profile%252Fus.anthropic.claude-sonnet-4-5-20250929-v1%253A0/converse\n\ncontent-type:application/json\nhost:bedrock-runtime.us-east-1.amazonaws.com\nx-amz-date:20261126T120000Z\n\ncontent-type;host;x-amz-date\nbd5cbd4339d48d400c5cbf75a283c35e83a4db0d25e9ef527cf6eb4a3b9851bc"
        );
        assert_eq!(signed, "content-type;host;x-amz-date");
        let creds = Credentials {
            access_key_id: AKID,
            secret_access_key: SECRET,
            session_token: None,
        };
        let headers = sign(&req, &creds, "us-east-1", "bedrock", date);
        assert_eq!(
            headers.last().unwrap().1,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261126/us-east-1/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=f152f905fdb2836ec3fecfa6855f34a9bc24f5c52862b65607ae855f8a2c6a24"
        );
    }

    #[test]
    fn matches_botocore_with_a_session_token() {
        let path = crate::converse_path("anthropic.claude-3-haiku-20240307-v1:0");
        let req = Request {
            method: "POST",
            host: BOTO_HOST,
            path: &path,
            query: &[],
            headers: &[("Content-Type", "application/json")],
            payload: BOTO_BODY.as_bytes(),
        };
        let token = "FQoGZXIvYXdzEXAMPLEtoken/with+slash==";
        let creds = Credentials {
            access_key_id: AKID,
            secret_access_key: SECRET,
            session_token: Some(token),
        };
        let headers = sign(&req, &creds, "us-east-1", "bedrock", "20261126T120000Z");
        assert_eq!(headers[1], ("x-amz-security-token", token.to_string()));
        assert_eq!(
            headers[2].1,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261126/us-east-1/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token, Signature=76cfdf71c8cb7e1f702c626cb025b3d3b92f05c57f11071f6a81cba120e86ac2"
        );
    }

    #[test]
    fn empty_session_token_is_not_sent() {
        let req = Request {
            method: "GET",
            host: HOST,
            path: "/",
            query: &[],
            headers: &[],
            payload: b"",
        };
        let creds = Credentials {
            access_key_id: AKID,
            secret_access_key: SECRET,
            session_token: Some(""),
        };
        let headers = sign(&req, &creds, "us-east-1", "service", DATE);
        assert_eq!(headers.len(), 2);
        assert!(headers[1].1.ends_with(
            "Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        ));
    }

    #[test]
    fn query_encoding_keeps_tokens_intact() {
        let q = vec![
            (
                "nextToken".to_string(),
                "tok+2/SYSTEM_DEFINED==".to_string(),
            ),
            ("maxResults".to_string(), "1000".to_string()),
        ];
        assert_eq!(
            query_string(&q),
            "nextToken=tok%2B2%2FSYSTEM_DEFINED%3D%3D&maxResults=1000"
        );
        assert_eq!(
            canonical_query(&q),
            "maxResults=1000&nextToken=tok%2B2%2FSYSTEM_DEFINED%3D%3D"
        );
        assert_eq!(uri_encode("a b*c'"), "a%20b%2Ac%27");
    }

    #[test]
    fn repeated_headers_merge_in_order() {
        let req = Request {
            method: "GET",
            host: HOST,
            path: "/",
            query: &[],
            headers: &[("My-Header1", "value2"), ("my-header1", "value1")],
            payload: b"",
        };
        let (canonical, signed) = canonical_request(&req, DATE, None);
        assert!(
            canonical.contains("\nmy-header1:value2,value1\n"),
            "{canonical}"
        );
        assert_eq!(signed, "host;my-header1;x-amz-date");
    }

    #[test]
    fn amz_dates() {
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        assert_eq!(amz_date(0), "19700101T000000Z");
        let pinned = model::date::days_from_civil(2026, 11, 26) * 86_400 + 12 * 3600;
        assert_eq!(amz_date(pinned), "20261126T120000Z");
    }
}
