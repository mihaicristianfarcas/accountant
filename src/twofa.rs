//! Second factors: TOTP codes and pulling one-time codes out of emails.

use anyhow::{Result, bail};
use hmac::{Hmac, Mac};
use regex::Regex;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

macro_rules! hmac_with {
    ($digest:ty, $key:expr, $msg:expr) => {{
        let mut mac = Hmac::<$digest>::new_from_slice($key).expect("HMAC accepts any key length");
        mac.update($msg);
        mac.finalize().into_bytes().to_vec()
    }};
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Sha1,
    Sha256,
    Sha512,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TotpSecret {
    pub key: Vec<u8>,
    pub digits: u32,
    pub period: u64,
    pub algorithm: Algorithm,
}

impl TotpSecret {
    /// Accepts an `otpauth://totp/...` URI or a bare base32 secret (spaces
    /// and dashes allowed, as authenticator apps display them).
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.to_ascii_lowercase().starts_with("otpauth://") {
            return Self::parse_uri(input);
        }
        Ok(TotpSecret { key: base32_decode(input)?, digits: 6, period: 30, algorithm: Algorithm::Sha1 })
    }

    fn parse_uri(uri: &str) -> Result<Self> {
        let Some((_, query)) = uri.split_once('?') else {
            bail!("otpauth URI has no parameters");
        };
        if !uri[10..].to_ascii_lowercase().starts_with("totp") {
            bail!("only time-based (totp) codes are supported");
        }
        let mut secret = None;
        let mut out = TotpSecret { key: vec![], digits: 6, period: 30, algorithm: Algorithm::Sha1 };
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = percent_decode(v);
            match k.to_ascii_lowercase().as_str() {
                "secret" => secret = Some(v),
                "digits" => out.digits = v.parse().unwrap_or(6),
                "period" => out.period = v.parse().unwrap_or(30),
                "algorithm" => {
                    out.algorithm = match v.to_ascii_uppercase().as_str() {
                        "SHA256" => Algorithm::Sha256,
                        "SHA512" => Algorithm::Sha512,
                        _ => Algorithm::Sha1,
                    }
                }
                _ => {}
            }
        }
        let Some(secret) = secret else { bail!("otpauth URI has no secret") };
        if !(6..=10).contains(&out.digits) || out.period == 0 {
            bail!("unsupported TOTP parameters");
        }
        out.key = base32_decode(&secret)?;
        Ok(out)
    }

    pub fn to_uri(&self) -> String {
        let algo = match self.algorithm {
            Algorithm::Sha1 => "SHA1",
            Algorithm::Sha256 => "SHA256",
            Algorithm::Sha512 => "SHA512",
        };
        format!(
            "otpauth://totp/accountant?secret={}&digits={}&period={}&algorithm={algo}",
            base32_encode(&self.key),
            self.digits,
            self.period
        )
    }

    pub fn code_at(&self, unix_secs: u64) -> String {
        let counter = (unix_secs / self.period).to_be_bytes();
        let digest = match self.algorithm {
            Algorithm::Sha1 => hmac_with!(sha1::Sha1, &self.key, &counter),
            Algorithm::Sha256 => hmac_with!(sha2::Sha256, &self.key, &counter),
            Algorithm::Sha512 => hmac_with!(sha2::Sha512, &self.key, &counter),
        };
        let offset = (digest[digest.len() - 1] & 0x0f) as usize;
        let bin = u32::from_be_bytes([
            digest[offset] & 0x7f,
            digest[offset + 1],
            digest[offset + 2],
            digest[offset + 3],
        ]);
        let modulo = 10u64.pow(self.digits);
        format!("{:0width$}", bin as u64 % modulo, width = self.digits as usize)
    }

    /// The current code and how many seconds it stays valid.
    pub fn now(&self) -> (String, u64) {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        (self.code_at(t), self.period - t % self.period)
    }
}

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32_decode(s: &str) -> Result<Vec<u8>> {
    let mut bits: u64 = 0;
    let mut nbits = 0;
    let mut out = Vec::new();
    for c in s.chars() {
        if c.is_whitespace() || c == '-' || c == '=' {
            continue;
        }
        let up = c.to_ascii_uppercase() as u8;
        let Some(v) = B32.iter().position(|&b| b == up) else {
            bail!("'{c}' is not valid in a base32 secret");
        };
        bits = (bits << 5) | v as u64;
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
            bits &= (1 << nbits) - 1;
        }
    }
    if out.len() < 10 {
        bail!("secret is too short — paste the full setup key");
    }
    Ok(out)
}

pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let mut bits: u64 = 0;
    let mut nbits = 0;
    for &b in data {
        bits = (bits << 8) | b as u64;
        nbits += 8;
        while nbits >= 5 {
            nbits -= 5;
            out.push(B32[((bits >> nbits) & 31) as usize] as char);
        }
        bits &= (1 << nbits) - 1;
    }
    if nbits > 0 {
        out.push(B32[((bits << (5 - nbits)) & 31) as usize] as char);
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(b) => {
                    out.push(b);
                    i += 3;
                    continue;
                }
                Err(_) => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// One-time codes in emails
// ---------------------------------------------------------------------------

static STRIP_BLOCKS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<(style|script|head)[^>]*>.*?</(style|script|head)>").unwrap());
static BREAKS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>|</(p|div|tr|td|h[1-6]|li|table)>").unwrap());
static TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());
static NUM_ENTITY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&#(x?[0-9a-fA-F]+);").unwrap());
static CANDIDATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^0-9A-Za-z#])(\d{3}[ -]\d{3}|\d{6,8})(?:$|[^0-9A-Za-z])").unwrap());
static KEYWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)code|verif|one[- ]time|passcode|otp|sign[- ]?in|log[- ]?in|confirm").unwrap()
});
static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"https://[^\s"'<>]+"#).unwrap());

pub fn html_to_text(html: &str) -> String {
    let s = STRIP_BLOCKS.replace_all(html, " ");
    let s = BREAKS.replace_all(&s, "\n");
    let s = TAGS.replace_all(&s, " ");
    let s = NUM_ENTITY.replace_all(&s, |c: &regex::Captures| {
        let raw = &c[1];
        let n = match raw.strip_prefix(['x', 'X']) {
            Some(h) => u32::from_str_radix(h, 16).ok(),
            None => raw.parse().ok(),
        };
        n.and_then(char::from_u32).map(String::from).unwrap_or_default()
    });
    let s = s
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    s.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Find the most likely one-time code in an email.
pub fn extract_code(subject: &str, text: &str) -> Option<String> {
    let mut best: Option<(i32, String)> = None;
    let mut consider = |hay: &str, bonus: i32| {
        for cap in CANDIDATE.captures_iter(hay) {
            let m = cap.get(1).unwrap();
            let code: String = m.as_str().chars().filter(char::is_ascii_digit).collect();
            let mut score = bonus;
            // Near a keyword like "code" or "verification".
            let window_start = hay[..m.start()].char_indices().rev().nth(80).map(|(i, _)| i).unwrap_or(0);
            if KEYWORD.is_match(&hay[window_start..m.start()]) {
                score += 5;
            }
            // Alone on its line — how codes are usually displayed.
            let line_start = hay[..m.start()].rfind('\n').map_or(0, |i| i + 1);
            let line_end = hay[m.end()..].find('\n').map_or(hay.len(), |i| m.end() + i);
            if hay[line_start..line_end].trim() == m.as_str() {
                score += 3;
            }
            if code.len() == 6 {
                score += 1;
            }
            // Looks like a year range, price, or phone fragment.
            let after = &hay[m.end()..];
            if after.starts_with("px") || after.starts_with('%') {
                score -= 10;
            }
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                best = Some((score, code));
            }
        }
    };
    consider(subject, 4);
    consider(text, 0);
    best.filter(|(s, _)| *s >= 4).map(|(_, c)| c)
}

/// A sign-in ("magic") link from the provider, if the email has one.
pub fn extract_link(raw: &str, domains: &[&str]) -> Option<String> {
    for m in URL.find_iter(raw) {
        let url = m.as_str().trim_end_matches(['.', ')', ',']).replace("&amp;", "&");
        let host = url[8..].split(['/', '?', '#']).next().unwrap_or("").to_lowercase();
        if !domains.iter().any(|d| host == *d || host.ends_with(&format!(".{d}"))) {
            continue;
        }
        let lower = url.to_lowercase();
        if ["magic", "verify", "login", "signin", "sign-in", "auth"].iter().any(|k| lower.contains(k)) {
            return Some(url);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_vectors() {
        // RFC 6238 appendix B, 8 digits.
        let sha1 = TotpSecret {
            key: b"12345678901234567890".to_vec(),
            digits: 8,
            period: 30,
            algorithm: Algorithm::Sha1,
        };
        assert_eq!(sha1.code_at(59), "94287082");
        assert_eq!(sha1.code_at(1111111109), "07081804");
        assert_eq!(sha1.code_at(20000000000), "65353130");
        let sha256 = TotpSecret {
            key: b"12345678901234567890123456789012".to_vec(),
            algorithm: Algorithm::Sha256,
            ..sha1.clone()
        };
        assert_eq!(sha256.code_at(59), "46119246");
        let sha512 = TotpSecret {
            key: b"1234567890123456789012345678901234567890123456789012345678901234".to_vec(),
            algorithm: Algorithm::Sha512,
            ..sha1
        };
        assert_eq!(sha512.code_at(1234567890), "93441116");
    }

    #[test]
    fn parses_setup_keys_and_uris() {
        let a = TotpSecret::parse("jbsw y3dp ehpk 3pxp").unwrap();
        assert_eq!(a.key, b"Hello!\xde\xad\xbe\xef");
        let b = TotpSecret::parse(
            "otpauth://totp/OpenAI:me%40x.com?secret=JBSWY3DPEHPK3PXP&issuer=OpenAI&digits=6&period=30",
        )
        .unwrap();
        assert_eq!(a, b);
        // URI form survives a roundtrip through the vault.
        assert_eq!(TotpSecret::parse(&b.to_uri()).unwrap(), b);
        assert!(TotpSecret::parse("not a secret!").is_err());
        assert!(TotpSecret::parse("otpauth://hotp/x?secret=JBSWY3DPEHPK3PXP").is_err());
    }

    #[test]
    fn base32_roundtrip() {
        for data in [&b"12345678901234567890"[..], b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\xff"] {
            assert_eq!(base32_decode(&base32_encode(data)).unwrap(), data);
        }
    }

    #[test]
    fn finds_codes_in_typical_emails() {
        assert_eq!(extract_code("Your ChatGPT code is 482917", "").as_deref(), Some("482917"));
        let body = html_to_text(
            r#"<html><head><style>.x{color:#123456}</style></head><body>
            <p>Hi there,</p><p>Enter this verification code to sign in:</p>
            <div style="font-size:24px">731 904</div>
            <p>This code expires in 10 minutes. Order #99812345.</p></body></html>"#,
        );
        assert_eq!(extract_code("Sign in to Claude", &body).as_deref(), Some("731904"));
        // Numbers without context are not guessed at.
        assert_eq!(extract_code("Receipt", "Invoice 20261006 total 120000"), None);
    }

    #[test]
    fn finds_magic_links_on_provider_domains_only() {
        let raw = r#"<a href="https://evil.example/login">x</a>
                     <a href="https://claude.ai/magic-link#abc123">Sign in</a>"#;
        assert_eq!(
            extract_link(raw, &["claude.ai", "anthropic.com"]).as_deref(),
            Some("https://claude.ai/magic-link#abc123")
        );
        assert_eq!(extract_link("https://claude.ai/settings", &["claude.ai"]), None);
    }
}
