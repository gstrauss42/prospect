//! Test harness: starts the real `opportunity` binary against `tests/mock/server.py` (DeepSeek,
//! image sources and Mailgun mocks) in a temporary data directory, with a tiny HTTP client.
//!
//! Ports are chosen by the OS (`:0`) and read back from each child's own log, so no two
//! servers — in this process or any other test run on the machine — can ever share one. The
//! server runs with a cleared environment in a fresh working directory, so a developer's real
//! `.env` (or exported keys) can never reach it. Everything started is killed when the `Env` is
//! dropped, even if the test panics.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

pub struct Env {
    pub mock_port: u16,
    pub port: u16,
    pub dir: PathBuf,
    mock: Option<Child>,
    server: Option<Child>,
    starts: usize,
    extra: Vec<(String, String)>,
}

impl Env {
    /// Mock + server with default settings plus `extra` environment variables.
    pub fn start(extra: &[(&str, &str)]) -> Env {
        Env::start_with_dotenv(extra, None)
    }

    /// Like [`Env::start`], with a `.env` file in the server's working directory.
    pub fn start_with_dotenv(extra: &[(&str, &str)], dotenv: Option<&str>) -> Env {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("srv-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(text) = dotenv {
            std::fs::write(dir.join(".env"), text).unwrap();
        }
        let mut env = Env {
            mock_port: 0,
            port: 0,
            dir,
            mock: None,
            server: None,
            starts: 0,
            extra: extra
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let mock_log = env.dir.join("mock.log");
        let mut mock = Command::new("python3")
            .arg(repo_root().join("tests/mock/server.py"))
            .args(["--port", "0"])
            .stdout(std::fs::File::create(&mock_log).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .expect("python3 runs the mock");
        env.mock_port = read_port(&mut mock, &mock_log, "listening on http://127.0.0.1:");
        env.mock = Some(mock);
        wait_ready(env.mock_port, "/_health");
        env.start_server();
        env
    }

    /// Start (or restart) the server on a fresh OS-chosen port; `self.port` follows it.
    pub fn start_server(&mut self) {
        self.starts += 1;
        let mock = format!("http://127.0.0.1:{}", self.mock_port);
        let log_path = self.dir.join(format!("server-{}.log", self.starts));
        let log = std::fs::File::create(&log_path).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_prospect"));
        cmd.env_clear()
            .current_dir(&self.dir)
            .env("ADDR", "127.0.0.1:0")
            .env("PUBLIC_URL", "http://127.0.0.1")
            .env("DATA_DIR", self.dir.join("data"))
            .env("DEEPSEEK_API_KEY", "test-key")
            .env("DEEPSEEK_BASE_URL", &mock)
            .env("OPENVERSE_URL", format!("{mock}/openverse/v1"))
            .env("COMMONS_API_URL", format!("{mock}/commons/w/api.php"))
            .env("MAILGUN_BASE_URL", format!("{mock}/mailgun"))
            .env("MAILGUN_API_KEY", "")
            .env("BEDROCK_RUNTIME_ENDPOINT", format!("{mock}/bedrock"))
            .env("BEDROCK_CONTROL_ENDPOINT", format!("{mock}/bedrock"))
            .env(
                "SECRETS_KEY",
                "c2VjcmV0c2tleXNlY3JldHNrZXlzZWNyZXRza2V5MzI=",
            )
            .env("TRUST_PROXY", "true")
            .env("SCHEDULER_TICK_SECS", "1")
            .stdout(Stdio::null())
            .stderr(log);
        for (k, v) in &self.extra {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("the server binary starts");
        self.port = read_port(&mut child, &log_path, "listening on http://127.0.0.1:");
        self.server = Some(child);
        wait_ready(self.port, "/healthz");
    }

    /// SIGKILL the server (a crash), leaving the data directory as it is.
    pub fn kill_server(&mut self) {
        if let Some(mut s) = self.server.take() {
            let _ = s.kill();
            let _ = s.wait();
        }
    }

    /// SIGTERM the server and wait for it to exit (graceful shutdown).
    pub fn stop_server(&mut self) -> bool {
        let Some(mut s) = self.server.take() else {
            return true;
        };
        let _ = Command::new("kill")
            .args(["-TERM", &s.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = s.try_wait() {
                return status.success();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = s.kill();
        let _ = s.wait();
        false
    }

    pub fn client(&self) -> Client {
        Client {
            port: self.port,
            cookie: None,
            ip: None,
        }
    }

    /// A client that appears to come from `ip` (TRUST_PROXY is on in tests).
    pub fn client_from(&self, ip: &str) -> Client {
        Client {
            port: self.port,
            cookie: None,
            ip: Some(ip.to_string()),
        }
    }

    pub fn db(&self) -> rusqlite::Connection {
        let c = rusqlite::Connection::open(self.dir.join("data/prospect.db")).unwrap();
        c.busy_timeout(Duration::from_secs(5)).unwrap();
        c
    }

    /// Everything every server start of this Env wrote to stderr.
    pub fn log(&self) -> String {
        (1..=self.starts)
            .filter_map(|i| std::fs::read_to_string(self.dir.join(format!("server-{i}.log"))).ok())
            .collect::<Vec<_>>()
            .join("")
    }

    /// Every file in the outbox, as text.
    pub fn outbox(&self) -> Vec<String> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.dir.join("data/outbox"))
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        files.sort();
        files
            .iter()
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .collect()
    }

    pub fn mock_get(&self, path: &str) -> Value {
        Client {
            port: self.mock_port,
            cookie: None,
            ip: None,
        }
        .get(path)
        .json()
    }

    pub fn mock_post(&self, path: &str, body: &Value) -> Value {
        Client {
            port: self.mock_port,
            cookie: None,
            ip: None,
        }
        .post(path, body)
        .json()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.kill_server();
        if let Some(mut m) = self.mock.take() {
            let _ = m.kill();
            let _ = m.wait();
        }
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        } else {
            eprintln!("server logs kept in {}", self.dir.display());
        }
    }
}

/// The port a child announced in its log (`…<marker><port>…`), waiting up to 20 s; panics with
/// the log if the child exits first (e.g. it couldn't start).
fn read_port(child: &mut Child, log: &Path, marker: &str) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        if let Some(rest) = text.split(marker).nth(1) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(port) = digits.parse::<u16>() {
                return port;
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!(
                "{} exited ({status}) before listening:\n{text}",
                log.display()
            );
        }
        assert!(
            Instant::now() < deadline,
            "no port announced in {} within 20 s:\n{text}",
            log.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_ready(port: u16, path: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(r) = (Client {
            port,
            cookie: None,
            ip: None,
        })
        .try_request("GET", path, &[], None)
            && r.status == 200
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("nothing answered on port {port} within 20 s");
}

/// Poll `f` until it returns Some, for up to `secs` seconds.
pub fn wait_for<T>(secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            Instant::now() < deadline,
            "condition not met within {secs} s"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------------------------
// A small HTTP/1.1 client
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
pub struct Res {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Res {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| {
            panic!(
                "not JSON ({}): {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Clone)]
pub struct Client {
    pub port: u16,
    pub cookie: Option<String>,
    pub ip: Option<String>,
}

impl Client {
    pub fn get(&mut self, path: &str) -> Res {
        self.request("GET", path, &[], None)
    }

    pub fn post(&mut self, path: &str, body: &Value) -> Res {
        self.request(
            "POST",
            path,
            &[("Content-Type", "application/json")],
            Some(body.to_string().as_bytes()),
        )
    }

    pub fn put(&mut self, path: &str, body: &Value) -> Res {
        self.request(
            "PUT",
            path,
            &[("Content-Type", "application/json")],
            Some(body.to_string().as_bytes()),
        )
    }

    pub fn delete(&mut self, path: &str, body: &Value) -> Res {
        self.request(
            "DELETE",
            path,
            &[("Content-Type", "application/json")],
            Some(body.to_string().as_bytes()),
        )
    }

    pub fn request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Res {
        let r = self
            .try_request(method, path, headers, body)
            .unwrap_or_else(|e| panic!("{method} {path}: {e}"));
        for (k, v) in &r.headers {
            if k.eq_ignore_ascii_case("set-cookie") {
                let pair = v.split(';').next().unwrap_or("").trim();
                if let Some(value) = pair.strip_prefix("opp_session=") {
                    self.cookie = if value.is_empty() {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
            }
        }
        r
    }

    pub fn try_request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> std::io::Result<Res> {
        let mut s = TcpStream::connect(("127.0.0.1", self.port))?;
        s.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
            self.port
        );
        if let Some(c) = &self.cookie {
            req.push_str(&format!("Cookie: opp_session={c}\r\n"));
        }
        if let Some(ip) = &self.ip {
            req.push_str(&format!("X-Forwarded-For: {ip}\r\n"));
        }
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        let body = body.unwrap_or(b"");
        if !body.is_empty() || matches!(method, "POST" | "PUT" | "DELETE") {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes())?;
        s.write_all(body)?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw)?;
        parse(&raw)
    }
}

fn parse(raw: &[u8]) -> std::io::Result<Res> {
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed response");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(bad)?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(bad)?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    let chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked")
    });
    if chunked {
        let mut out = Vec::new();
        let mut rest = &body[..];
        loop {
            let nl = rest.windows(2).position(|w| w == b"\r\n").ok_or_else(bad)?;
            let size = usize::from_str_radix(String::from_utf8_lossy(&rest[..nl]).trim(), 16)
                .map_err(|_| bad())?;
            rest = &rest[nl + 2..];
            if size == 0 {
                break;
            }
            out.extend_from_slice(&rest[..size]);
            rest = &rest[size + 2..];
        }
        body = out;
    }
    Ok(Res {
        status,
        headers,
        body,
    })
}

// ---------------------------------------------------------------------------------------------
// Flows
// ---------------------------------------------------------------------------------------------

pub fn signup(c: &mut Client, business: &str, email: &str) -> Value {
    let r = c.post(
        "/api/auth/signup",
        &serde_json::json!({ "business_name": business, "website": "acme.example", "email": email, "password": "correct horse battery", "hints": "Cleaning services in Cape Town" }),
    );
    assert_eq!(r.status, 201, "{}", r.text());
    assert!(c.cookie.is_some(), "signup sets the session cookie");
    r.json()
}

/// Wait until the account has no active run; returns /api/me.
pub fn settle(c: &mut Client) -> Value {
    wait_for(60, || {
        let me = c.get("/api/me").json();
        me["run"].is_null().then_some(me)
    })
}

/// Sign up, let research finish, confirm the profile: a ready account with opportunities.
pub fn ready_account(c: &mut Client, business: &str, email: &str) -> Value {
    signup(c, business, email);
    let me = settle(c);
    assert_eq!(me["user"]["onboarding"], "review", "{me}");
    let mut profile = me["profile"].clone();
    profile["headline"] = "Commercial cleaning for offices and clinics".into();
    profile["industry"] = "Cleaning services".into();
    profile["seeking"] = serde_json::json!(["sales", "partners"]);
    profile["interests"] = serde_json::json!(["green cleaning", "facility management"]);
    let r = c.put("/api/profile", &serde_json::json!({ "profile": profile }));
    assert_eq!(r.status, 200, "{}", r.text());
    settle(c)
}
