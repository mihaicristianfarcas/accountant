//! Watch the inbox for a login code while a sign-in is in progress.
//!
//! Two sources:
//! * Apple Mail — scripted through `osascript`; no password, macOS asks once
//!   for Automation permission.
//! * IMAP — any provider (iCloud, Gmail, Fastmail…) with an app password kept
//!   in the vault. A deliberately tiny client: LOGIN, EXAMINE, SEARCH, FETCH.

use crate::config::{MailConfig, MailSource};
use crate::providers::Provider;
use crate::twofa;
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Message {
    pub from: String,
    pub subject: String,
    pub date: Option<DateTime<Utc>>,
    /// Plain-text rendering of the body.
    pub text: String,
    /// Raw body (HTML or text) — links live in hrefs.
    pub raw: String,
}

#[derive(Debug, Clone)]
pub enum MailEvent {
    Code { code: String, from: String },
    Link { url: String },
    Error(String),
}

pub fn provider_domains(p: Provider) -> &'static [&'static str] {
    match p {
        Provider::Claude => &["claude.ai", "claude.com", "anthropic.com"],
        Provider::Codex => &["openai.com", "chatgpt.com"],
        Provider::OpenCode => &["claude.ai", "anthropic.com", "openai.com", "chatgpt.com", "github.com"],
        Provider::Antigravity => &["google.com"],
        Provider::Cursor => &["cursor.com", "cursor.sh"],
        Provider::Copilot => &["github.com"],
    }
}

/// Fetch messages from `senders` that arrived after `since`.
pub fn fetch(
    cfg: &MailConfig,
    password: Option<&str>,
    senders: &[&str],
    since: DateTime<Utc>,
) -> Result<Vec<Message>> {
    match cfg.source {
        MailSource::Off => Ok(vec![]),
        MailSource::AppleMail => apple_mail(senders, since),
        MailSource::Imap => {
            let pw = password.ok_or_else(|| anyhow!("no IMAP password saved — run `accountant mail`"))?;
            imap_fetch(cfg, pw, senders, since)
        }
    }
}

/// Poll the inbox in the background until `stop` is set or 10 minutes pass.
pub fn watch(
    cfg: MailConfig,
    password: Option<String>,
    provider: Provider,
    since: DateTime<Utc>,
    stop: Arc<AtomicBool>,
) -> Receiver<MailEvent> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut seen: HashSet<String> = HashSet::new();
        let mut last_error = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        // Allow for clock skew between us and the mail server.
        let since = since - chrono::Duration::seconds(90);
        while !stop.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
            match fetch(&cfg, password.as_deref(), provider.mail_senders(), since) {
                Ok(mut msgs) => {
                    msgs.sort_by_key(|m| m.date);
                    for m in msgs {
                        let id = format!("{}|{}|{:?}", m.from, m.subject, m.date);
                        if !seen.insert(id) {
                            continue;
                        }
                        if let Some(code) = twofa::extract_code(&m.subject, &m.text) {
                            let _ = tx.send(MailEvent::Code { code, from: m.from.clone() });
                        } else if let Some(url) = twofa::extract_link(&m.raw, provider_domains(provider)) {
                            let _ = tx.send(MailEvent::Link { url });
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    if msg != last_error {
                        let _ = tx.send(MailEvent::Error(msg.clone()));
                        last_error = msg;
                    }
                }
            }
            for _ in 0..40 {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
    rx
}

// ---------------------------------------------------------------------------
// Apple Mail
// ---------------------------------------------------------------------------

const APPLE_SCRIPT: &str = r#"
on run argv
    set secs to (item 1 of argv) as integer
    set needles to rest of argv
    set cutoff to (current date) - secs
    set RS to character id 30
    set US to character id 31
    set out to ""
    tell application "Mail"
        try
            check for new mail
        end try
        set msgs to (messages of inbox whose date received > cutoff)
        repeat with m in msgs
            set s to sender of m
            set hit to false
            repeat with n in needles
                if s contains (n as text) then set hit to true
            end repeat
            if hit then
                set d to (date received of m) as «class isot» as string
                set out to out & RS & s & US & (subject of m) & US & d & US & (content of m)
            end if
        end repeat
    end tell
    return out
end run
"#;

fn apple_mail(senders: &[&str], since: DateTime<Utc>) -> Result<Vec<Message>> {
    let secs = (Utc::now() - since).num_seconds().max(60);
    let out = Command::new("osascript")
        .arg("-e")
        .arg(APPLE_SCRIPT)
        .arg(secs.to_string())
        .args(senders)
        .output()
        .context("running osascript")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("-1743") || err.contains("Not authorized") {
            bail!("allow your terminal to control Mail in System Settings › Privacy › Automation");
        }
        bail!("Apple Mail: {}", err.trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(parse_apple_mail(&text))
}

fn parse_apple_mail(text: &str) -> Vec<Message> {
    text.split('\u{1e}')
        .filter_map(|rec| {
            let mut f = rec.splitn(4, '\u{1f}');
            let from = f.next()?.trim().to_string();
            let subject = f.next()?.to_string();
            let date = f
                .next()
                .and_then(|d| NaiveDateTime::parse_from_str(d.trim(), "%Y-%m-%dT%H:%M:%S").ok())
                .and_then(|n| Local.from_local_datetime(&n).single())
                .map(|d| d.with_timezone(&Utc));
            let body = f.next().unwrap_or("").to_string();
            Some(Message { from, subject, date, text: body.clone(), raw: body })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// IMAP
// ---------------------------------------------------------------------------

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

struct Imap {
    io: BufReader<TlsStream>,
    tag: u32,
}

/// One untagged response line plus any literals embedded in it.
#[derive(Debug, Default)]
struct Untagged {
    line: String,
    literals: Vec<Vec<u8>>,
}

impl Imap {
    fn connect(host: &str, port: u16) -> Result<Self> {
        let addr = (host, port)
            .to_socket_addrs()
            .with_context(|| format!("resolving {host}"))?
            .next()
            .ok_or_else(|| anyhow!("no address for {host}"))?;
        let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
            .with_context(|| format!("connecting to {host}:{port}"))?;
        tcp.set_read_timeout(Some(Duration::from_secs(20)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(20)))?;

        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.into() };
        let config =
            rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(host.to_string())?;
        let conn = rustls::ClientConnection::new(Arc::new(config), name)?;
        let mut imap = Imap { io: BufReader::new(rustls::StreamOwned::new(conn, tcp)), tag: 0 };
        let greeting = imap.read_line()?;
        if !greeting.starts_with("* OK") && !greeting.starts_with("* PREAUTH") {
            bail!("unexpected IMAP greeting: {}", greeting.trim());
        }
        Ok(imap)
    }

    fn read_line(&mut self) -> Result<String> {
        let mut buf = Vec::new();
        let n = self.io.read_until(b'\n', &mut buf)?;
        if n == 0 {
            bail!("IMAP server closed the connection");
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    fn command(&mut self, cmd: &str) -> Result<Vec<Untagged>> {
        self.tag += 1;
        let tag = format!("a{}", self.tag);
        self.io.get_mut().write_all(format!("{tag} {cmd}\r\n").as_bytes())?;
        self.io.get_mut().flush()?;
        read_response(&mut self.io, &tag)
    }

    fn logout(mut self) {
        let _ = self.command("LOGOUT");
    }
}

fn read_response<R: BufRead>(io: &mut R, tag: &str) -> Result<Vec<Untagged>> {
    let mut out = Vec::new();
    let mut current: Option<Untagged> = None;
    loop {
        let mut buf = Vec::new();
        if io.read_until(b'\n', &mut buf)? == 0 {
            bail!("IMAP server closed the connection");
        }
        let line = String::from_utf8_lossy(&buf).into_owned();
        let trimmed = line.trim_end();
        if current.is_none()
            && let Some(rest) = trimmed.strip_prefix(tag).and_then(|r| r.strip_prefix(' '))
        {
            if rest.starts_with("OK") {
                return Ok(out);
            }
            bail!("IMAP: {rest}");
        }
        let entry = current.get_or_insert_with(Untagged::default);
        entry.line.push_str(trimmed);
        // A line ending in {N} is followed by exactly N bytes of literal data.
        if let Some(n) = literal_len(trimmed) {
            let mut lit = vec![0u8; n];
            io.read_exact(&mut lit)?;
            entry.literals.push(lit);
            continue;
        }
        out.push(current.take().unwrap());
    }
}

fn literal_len(line: &str) -> Option<usize> {
    let inner = line.strip_suffix('}')?;
    let open = inner.rfind('{')?;
    inner[open + 1..].trim_end_matches('+').parse().ok()
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn search_query(senders: &[&str], since: DateTime<Utc>) -> String {
    fn or_tree(s: &[&str]) -> String {
        match s {
            [] => String::new(),
            [one] => format!("FROM {}", quote(one)),
            [first, rest @ ..] => format!("OR FROM {} ({})", quote(first), or_tree(rest)),
        }
    }
    // SINCE has day granularity; the exact cutoff is applied after fetching.
    let day = (since - chrono::Duration::days(1)).format("%-d-%b-%Y");
    format!("UID SEARCH SINCE {day} {}", or_tree(senders))
}

fn imap_fetch(
    cfg: &MailConfig,
    password: &str,
    senders: &[&str],
    since: DateTime<Utc>,
) -> Result<Vec<Message>> {
    if cfg.imap_host.is_empty() || cfg.imap_user.is_empty() {
        bail!("IMAP is not configured — run `accountant mail`");
    }
    let mut imap = Imap::connect(&cfg.imap_host, cfg.imap_port)?;
    imap.command(&format!("LOGIN {} {}", quote(&cfg.imap_user), quote(password)))
        .map_err(|e| anyhow!("IMAP login failed ({e}) — use an app-specific password"))?;
    imap.command(&format!("EXAMINE {}", quote(&cfg.imap_folder)))?;

    let found = imap.command(&search_query(senders, since))?;
    let mut uids: Vec<u64> = found
        .iter()
        .filter_map(|u| u.line.strip_prefix("* SEARCH"))
        .flat_map(|rest| rest.split_whitespace().filter_map(|n| n.parse().ok()))
        .collect();
    uids.sort_unstable();
    let recent: Vec<String> = uids.iter().rev().take(6).map(u64::to_string).collect();
    let mut messages = Vec::new();
    if !recent.is_empty() {
        let fetched = imap.command(&format!("UID FETCH {} (BODY.PEEK[])", recent.join(",")))?;
        for u in fetched {
            if let Some(raw) = u.literals.first()
                && let Some(m) = parse_rfc822(raw)
                && m.date.is_none_or(|d| d >= since)
            {
                messages.push(m);
            }
        }
    }
    imap.logout();
    Ok(messages)
}

fn parse_rfc822(raw: &[u8]) -> Option<Message> {
    use mailparse::MailHeaderMap;
    let mail = mailparse::parse_mail(raw).ok()?;
    let from = mail.headers.get_first_value("From").unwrap_or_default();
    let subject = mail.headers.get_first_value("Subject").unwrap_or_default();
    let date = mail
        .headers
        .get_first_value("Date")
        .and_then(|d| mailparse::dateparse(&d).ok())
        .and_then(|t| Utc.timestamp_opt(t, 0).single());

    let mut plain = None;
    let mut html = None;
    collect_bodies(&mail, &mut plain, &mut html);
    let raw_body = html.clone().or_else(|| plain.clone()).unwrap_or_default();
    let text = match (plain, html) {
        (Some(p), _) => p,
        (None, Some(h)) => twofa::html_to_text(&h),
        (None, None) => String::new(),
    };
    Some(Message { from, subject, date, text, raw: raw_body })
}

fn collect_bodies(part: &mailparse::ParsedMail, plain: &mut Option<String>, html: &mut Option<String>) {
    if part.subparts.is_empty() {
        let ty = part.ctype.mimetype.to_ascii_lowercase();
        if let Ok(body) = part.get_body() {
            if ty == "text/plain" && plain.is_none() {
                *plain = Some(body);
            } else if ty == "text/html" && html.is_none() {
                *html = Some(body);
            }
        }
    }
    for sub in &part.subparts {
        collect_bodies(sub, plain, html);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tagged_responses_with_literals() {
        let wire = b"* 1 FETCH (UID 7 BODY[] {11}\r\nhello\r\nworl)\r\n* 2 FETCH (UID 9 BODY[] {2}\r\nok)\r\na4 OK done\r\n";
        let mut io = &wire[..];
        let resp = read_response(&mut io, "a4").unwrap();
        assert_eq!(resp.len(), 2);
        assert_eq!(resp[0].literals[0], b"hello\r\nworl");
        assert!(resp[0].line.ends_with(')'));
        assert_eq!(resp[1].literals[0], b"ok");

        let mut io = &b"* NO nope\r\na1 NO [AUTHENTICATIONFAILED] bad\r\n"[..];
        assert!(read_response(&mut io, "a1").is_err());
    }

    #[test]
    fn search_query_ors_all_senders() {
        let since = Utc.with_ymd_and_hms(2026, 10, 6, 12, 0, 0).unwrap();
        assert_eq!(
            search_query(&["anthropic", "claude"], since),
            r#"UID SEARCH SINCE 5-Oct-2026 OR FROM "anthropic" (FROM "claude")"#
        );
    }

    #[test]
    fn parses_multipart_mail() {
        let raw = b"From: Anthropic <no-reply@mail.anthropic.com>\r\n\
Subject: Your verification code\r\n\
Date: Tue, 6 Oct 2026 12:00:00 +0000\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/alternative; boundary=XX\r\n\r\n\
--XX\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n\
<p>Your code is:</p><p style=3D\"font-size:20px\">123456</p>\r\n\
--XX--\r\n";
        let m = parse_rfc822(raw).unwrap();
        assert!(m.from.contains("anthropic"));
        assert_eq!(m.date.unwrap().timestamp(), 1791288000);
        assert_eq!(twofa::extract_code(&m.subject, &m.text).as_deref(), Some("123456"));
    }

    #[test]
    fn parses_apple_mail_records() {
        let out = "\u{1e}OpenAI <noreply@tm.openai.com>\u{1f}Your ChatGPT code is 555123\u{1f}2026-10-06T14:00:00\u{1f}body";
        let msgs = parse_apple_mail(out);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].date.is_some());
        assert_eq!(twofa::extract_code(&msgs[0].subject, &msgs[0].text).as_deref(), Some("555123"));
    }
}
