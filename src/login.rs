//! Browser sign-in for a new (or expired) account.
//!
//! * Claude Code: we drive the official `claude auth login`, pointing
//!   `$BROWSER` at a tiny shim that hands the authorize URL to us instead of
//!   the system browser. We then open it in the account's own browser
//!   session. Claude Code receives the callback and stores the login itself,
//!   so token format and scopes are always exactly what it expects.
//! * Codex ignores `$BROWSER`, so we run its (open-source) OAuth PKCE flow
//!   ourselves, on the same localhost callback it registers, and write an
//!   `auth.json` identical to what `codex login` produces.

use crate::browser;
use crate::config::BrowserConfig;
use crate::providers::{Provider, Snapshot, codex};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum LoginEvent {
    /// The authorize URL is known.
    Url(String),
    /// The page was opened (where).
    Opened(String),
    /// Progress worth showing.
    Status(String),
    /// Signed in. Codex hands back the new login; Claude Code stored its own.
    Success(Option<Snapshot>),
    Failed(String),
}

pub struct BrowserPlan {
    pub config: BrowserConfig,
    pub root: PathBuf,
    /// Isolated-profile key, usually the account email.
    pub session_key: String,
}

impl BrowserPlan {
    pub fn open(&self, url: &str) -> Result<String> {
        browser::open(url, &self.config, &self.root, &self.session_key)
    }
}

pub struct LoginTask {
    pub provider: Provider,
    pub rx: Receiver<LoginEvent>,
    cancel: Arc<AtomicBool>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
}

impl LoginTask {
    pub fn start(provider: Provider, email: Option<String>, plan: BrowserPlan) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let stdin = Arc::new(Mutex::new(None));
        match provider {
            Provider::Claude => {
                let child = spawn_claude(email.as_deref())?;
                let (c, s) = (cancel.clone(), stdin.clone());
                std::thread::spawn(move || run_claude(child, plan, tx, c, s));
            }
            Provider::Codex => {
                let listener = TcpListener::bind(("127.0.0.1", CODEX_PORT))
                    .map_err(|_| anyhow!("port {CODEX_PORT} is busy — is another `codex login` running?"))?;
                let c = cancel.clone();
                std::thread::spawn(move || run_codex(listener, email, plan, tx, c));
            }
        }
        Ok(LoginTask { provider, rx, cancel, stdin })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Claude Code's fallback flow: paste the code shown on the callback page.
    pub fn submit_code(&self, code: &str) -> Result<()> {
        let mut guard = self.stdin.lock().unwrap();
        let stdin = guard.as_mut().ok_or_else(|| anyhow!("this sign-in does not take a pasted code"))?;
        stdin.write_all(format!("{}\n", code.trim()).as_bytes())?;
        stdin.flush()?;
        Ok(())
    }

    pub fn accepts_code(&self) -> bool {
        self.provider == Provider::Claude
    }
}

impl Drop for LoginTask {
    fn drop(&mut self) {
        self.cancel();
    }
}

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

struct ClaudeChild {
    child: Child,
    url_file: PathBuf,
    shim_dir: PathBuf,
}

fn spawn_claude(email: Option<&str>) -> Result<ClaudeChild> {
    let mut rnd = [0u8; 6];
    getrandom::fill(&mut rnd)?;
    let shim_dir = std::env::temp_dir().join(format!("accountant-{}", hex::encode(rnd)));
    crate::fsutil::private_dir(&shim_dir)?;
    let url_file = shim_dir.join("url");
    let shim = shim_dir.join("open-url");
    std::fs::write(&shim, "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$ACCOUNTANT_URL_FILE\"\n")?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700))?;

    let mut cmd = Command::new(claude_bin());
    cmd.args(["auth", "login", "--claudeai"]);
    if let Some(e) = email.filter(|e| !e.is_empty()) {
        cmd.args(["--email", e]);
    }
    cmd.env("BROWSER", &shim)
        .env("ACCOUNTANT_URL_FILE", &url_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().context("could not start `claude` — is Claude Code installed?")?;
    Ok(ClaudeChild { child, url_file, shim_dir })
}

fn claude_bin() -> String {
    std::env::var("ACCOUNTANT_CLAUDE_BIN").unwrap_or_else(|_| "claude".into())
}

fn pipe_lines<R: Read + Send + 'static>(r: R, out: Sender<String>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(r);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf).trim().to_string();
                    if !line.is_empty() && out.send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

fn strip_ansi(s: &str) -> String {
    let re = regex::Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").unwrap();
    re.replace_all(s, "").into_owned()
}

fn run_claude(
    mut c: ClaudeChild,
    plan: BrowserPlan,
    tx: Sender<LoginEvent>,
    cancel: Arc<AtomicBool>,
    stdin_slot: Arc<Mutex<Option<ChildStdin>>>,
) {
    *stdin_slot.lock().unwrap() = c.child.stdin.take();
    let (ltx, lrx) = mpsc::channel::<String>();
    if let Some(out) = c.child.stdout.take() {
        pipe_lines(out, ltx.clone());
    }
    if let Some(err) = c.child.stderr.take() {
        pipe_lines(err, ltx);
    }

    let started = Instant::now();
    let mut opened = false;
    let mut printed_url: Option<String> = None;
    let mut last_lines: Vec<String> = Vec::new();

    let result = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = c.child.kill();
            let _ = c.child.wait();
            break None;
        }
        while let Ok(line) = lrx.try_recv() {
            let line = strip_ansi(&line);
            if let Some(url) = find_url(&line) {
                printed_url.get_or_insert(url);
            } else if !line.starts_with("Opening browser") {
                // Surface anything unexpected (a question, a warning).
                let _ = tx.send(LoginEvent::Status(line.clone()));
            }
            last_lines.push(line);
            if last_lines.len() > 6 {
                last_lines.remove(0);
            }
        }
        if !opened {
            // Prefer the URL handed to $BROWSER (localhost callback, no
            // copy-paste); fall back to the printed one.
            let shim_url = std::fs::read_to_string(&c.url_file)
                .ok()
                .and_then(|s| s.lines().find(|l| l.starts_with("https://")).map(String::from));
            let url = shim_url.or_else(|| {
                (started.elapsed() > Duration::from_secs(4)).then(|| printed_url.clone()).flatten()
            });
            if let Some(url) = url {
                opened = true;
                let _ = tx.send(LoginEvent::Url(url.clone()));
                match plan.open(&url) {
                    Ok(where_) => {
                        let _ = tx.send(LoginEvent::Opened(where_));
                    }
                    Err(e) => {
                        let _ = tx.send(LoginEvent::Status(format!("could not open a browser: {e}")));
                    }
                }
            }
        }
        match c.child.try_wait() {
            Ok(Some(status)) => {
                // Drain what is left so the error message is complete.
                std::thread::sleep(Duration::from_millis(50));
                while let Ok(line) = lrx.try_recv() {
                    last_lines.push(strip_ansi(&line));
                }
                break Some(if status.success() {
                    LoginEvent::Success(None)
                } else {
                    let detail = last_lines
                        .iter()
                        .rev()
                        .find(|l| !l.starts_with("http") && !l.contains("Paste code"))
                        .cloned()
                        .unwrap_or_else(|| format!("claude exited with {status}"));
                    LoginEvent::Failed(detail)
                });
            }
            Ok(None) => {}
            Err(e) => break Some(LoginEvent::Failed(e.to_string())),
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = std::fs::remove_dir_all(&c.shim_dir);
    if let Some(ev) = result {
        let _ = tx.send(ev);
    }
}

fn find_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    Some(line[start..].split_whitespace().next()?.to_string())
}

// ---------------------------------------------------------------------------
// Codex (OpenAI OAuth, PKCE)
// ---------------------------------------------------------------------------

const CODEX_ISSUER: &str = "https://auth.openai.com";

/// The OAuth issuer; overridable so the flow can be exercised against a local
/// fake in tests.
fn codex_issuer() -> String {
    std::env::var("ACCOUNTANT_CODEX_ISSUER").unwrap_or_else(|_| CODEX_ISSUER.to_string())
}
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_PORT: u16 = 1455;
const CODEX_SCOPES: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";

struct Pkce {
    verifier: String,
    challenge: String,
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random_b64(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system RNG");
    b64url(&b)
}

fn pkce() -> Pkce {
    let verifier = random_b64(64);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    Pkce { verifier, challenge }
}

fn redirect_uri() -> String {
    format!("http://127.0.0.1:{CODEX_PORT}/auth/callback")
}

pub fn codex_authorize_url(challenge: &str, state: &str, email: Option<&str>) -> String {
    let mut params = vec![
        ("response_type", "code".to_string()),
        ("client_id", CODEX_CLIENT_ID.into()),
        ("redirect_uri", redirect_uri()),
        ("code_challenge", challenge.into()),
        ("code_challenge_method", "S256".into()),
        ("state", state.into()),
        ("scope", CODEX_SCOPES.into()),
        ("id_token_add_organizations", "true".into()),
        ("codex_cli_simplified_flow", "true".into()),
        ("originator", "codex_cli_rs".into()),
    ];
    if let Some(e) = email.filter(|e| !e.is_empty()) {
        params.push(("login_hint", e.into()));
    }
    let query: Vec<String> = params.iter().map(|(k, v)| format!("{k}={}", form_encode(v))).collect();
    format!("{}/oauth/authorize?{}", codex_issuer(), query.join("&"))
}

pub fn form_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn form_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| form_decode(v))
    })
}

fn run_codex(
    listener: TcpListener,
    email: Option<String>,
    plan: BrowserPlan,
    tx: Sender<LoginEvent>,
    cancel: Arc<AtomicBool>,
) {
    let pkce = pkce();
    let state = random_b64(32);
    let url = codex_authorize_url(&pkce.challenge, &state, email.as_deref());
    let _ = tx.send(LoginEvent::Url(url.clone()));
    match plan.open(&url) {
        Ok(where_) => {
            let _ = tx.send(LoginEvent::Opened(where_));
        }
        Err(e) => {
            let _ = tx.send(LoginEvent::Status(format!("could not open a browser: {e}")));
        }
    }

    if listener.set_nonblocking(true).is_err() {
        let _ = tx.send(LoginEvent::Failed("could not start the callback listener".into()));
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(15 * 60);
    while !cancel.load(Ordering::Relaxed) && Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(ev) = handle_codex_callback(stream, &state, &pkce, &tx) {
                    let _ = tx.send(ev);
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                let _ = tx.send(LoginEvent::Failed(e.to_string()));
                return;
            }
        }
    }
    if !cancel.load(Ordering::Relaxed) {
        let _ = tx.send(LoginEvent::Failed("timed out waiting for the browser".into()));
    }
}

/// Handle one HTTP request on the callback port. Returns the final event once
/// the OAuth callback arrived.
fn handle_codex_callback(
    mut stream: TcpStream,
    state: &str,
    pkce: &Pkce,
    tx: &Sender<LoginEvent>,
) -> Option<LoginEvent> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let target = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));

    if path != "/auth/callback" {
        respond(&mut stream, 404, "text/plain", "not found");
        return None;
    }
    if let Some(err) = query_param(query, "error") {
        let desc = query_param(query, "error_description").unwrap_or_default();
        respond(&mut stream, 400, "text/html", &page(false, &format!("{err}: {desc}")));
        return Some(LoginEvent::Failed(format!("sign-in was refused: {err} {desc}")));
    }
    if query_param(query, "state").as_deref() != Some(state) {
        respond(&mut stream, 400, "text/html", &page(false, "state mismatch — please retry"));
        return None;
    }
    let Some(code) = query_param(query, "code") else {
        respond(&mut stream, 400, "text/html", &page(false, "missing authorization code"));
        return None;
    };
    let _ = tx.send(LoginEvent::Status("exchanging tokens…".into()));
    match exchange_codex_code(&code, pkce) {
        Ok(snapshot) => {
            respond(&mut stream, 200, "text/html", &page(true, "You can close this window."));
            Some(LoginEvent::Success(Some(snapshot)))
        }
        Err(e) => {
            respond(&mut stream, 500, "text/html", &page(false, &e.to_string()));
            Some(LoginEvent::Failed(format!("token exchange failed: {e}")))
        }
    }
}

fn exchange_codex_code(code: &str, pkce: &Pkce) -> Result<Snapshot> {
    let body = [
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri()),
        ("client_id", CODEX_CLIENT_ID.to_string()),
        ("code_verifier", pkce.verifier.clone()),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", form_encode(v)))
    .collect::<Vec<_>>()
    .join("&");

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .post(&format!("{}/oauth/token", codex_issuer()))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body)?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string()?;
    if status != 200 {
        let v: Value = serde_json::from_str(&text).unwrap_or(json!({}));
        let msg = v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("unexpected response");
        bail!("HTTP {status}: {msg}");
    }
    let v: Value = serde_json::from_str(&text).context("token response is not JSON")?;
    let get = |k: &str| v[k].as_str().map(String::from).ok_or_else(|| anyhow!("token response has no {k}"));
    let auth = codex::auth_json(&get("id_token")?, &get("access_token")?, &get("refresh_token")?);
    Ok(json!({ "auth": auth }))
}

fn respond(stream: &mut TcpStream, status: u16, ctype: &str, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

fn page(ok: bool, detail: &str) -> String {
    let (mark, title, color) =
        if ok { ("✓", "Signed in", "#7ed387") } else { ("✕", "Sign-in failed", "#f06464") };
    let detail = detail.replace('&', "&amp;").replace('<', "&lt;");
    format!(
        r#"<!doctype html><meta charset="utf-8"><title>accountant</title>
<style>
body{{margin:0;height:100vh;display:grid;place-items:center;background:#0e0f13;color:#e6e6eb;font:16px ui-monospace,Menlo,monospace}}
.c{{text-align:center}} .m{{font-size:56px;color:{color};animation:p 1.6s ease-in-out infinite}}
h1{{font-weight:500;font-size:20px;margin:.6em 0 .3em}} p{{color:#8a8c99;margin:0}}
@keyframes p{{50%{{opacity:.45}}}}
</style><div class="c"><div class="m">{mark}</div><h1>{title}</h1><p>{detail}</p><p style="margin-top:1.4em">return to accountant</p></div>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_url_matches_codex_cli_parameters() {
        let url = codex_authorize_url("CHAL", "STATE", Some("me@x.com"));
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback&code_challenge=CHAL&code_challenge_method=S256&state=STATE&scope=openid+profile+email+offline_access+api.connectors.read+api.connectors.invoke&id_token_add_organizations=true&codex_cli_simplified_flow=true&originator=codex_cli_rs"));
        assert!(url.ends_with("&login_hint=me%40x.com"));
    }

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let p = pkce();
        assert!(p.verifier.len() >= 43);
        assert_eq!(p.challenge, b64url(&Sha256::digest(p.verifier.as_bytes())));
    }

    #[test]
    fn query_parsing() {
        let q = "code=ab%2Fc&state=x+y&scope=a";
        assert_eq!(query_param(q, "code").as_deref(), Some("ab/c"));
        assert_eq!(query_param(q, "state").as_deref(), Some("x y"));
        assert_eq!(query_param(q, "nope"), None);
        assert_eq!(form_decode(&form_encode("a b/c@d~")), "a b/c@d~");
    }

    #[test]
    fn finds_urls_in_cli_output() {
        assert_eq!(
            find_url("If the browser didn't open, visit: https://claude.com/cai/oauth/authorize?x=1")
                .as_deref(),
            Some("https://claude.com/cai/oauth/authorize?x=1")
        );
        assert_eq!(strip_ansi("\x1b[2mhi\x1b[0m"), "hi");
    }

    #[test]
    fn callback_rejects_wrong_state() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(b"GET /auth/callback?code=c&state=WRONG HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        });
        let (stream, _) = listener.accept().unwrap();
        let (tx, _rx) = mpsc::channel();
        let ev = handle_codex_callback(stream, "RIGHT", &pkce(), &tx);
        assert!(ev.is_none());
        assert!(client.join().unwrap().starts_with("HTTP/1.1 400"));
    }
}
