//! Secret storage.
//!
//! On macOS everything goes into the login Keychain through `/usr/bin/security`.
//! Using the system binary (instead of linking Security.framework) means items
//! that Claude Code created are readable without an extra "allow access" prompt,
//! and secrets are piped through stdin as hex — they never appear in argv.
//!
//! Elsewhere (or with `ACCOUNTANT_SECRETS=file`) secrets are 0600 files in a
//! 0700 directory.

use crate::fsutil;
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub const SERVICE: &str = "accountant";

/// A generic-password slot in the macOS Keychain.
#[derive(Debug, Clone)]
pub struct KeychainItem {
    pub service: String,
    pub account: String,
}

impl KeychainItem {
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        KeychainItem { service: service.into(), account: account.into() }
    }

    pub fn get(&self) -> Result<Option<String>> {
        let out = Command::new("security")
            .args(["find-generic-password", "-s", &self.service, "-a", &self.account, "-w"])
            .stdin(Stdio::null())
            .output()
            .context("running `security` (macOS Keychain)")?;
        match out.status.code() {
            Some(0) => {}
            // errSecItemNotFound
            Some(44) => return Ok(None),
            _ => bail!(
                "keychain read failed for '{}': {}",
                self.service,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
        let mut s = String::from_utf8(out.stdout).context("keychain item is not UTF-8")?;
        while s.ends_with('\n') || s.ends_with('\r') {
            s.pop();
        }
        Ok(Some(decode_if_hex(s)))
    }

    pub fn set(&self, value: &str) -> Result<()> {
        // `security -i` reads commands from stdin, so the secret never shows up
        // in the process list. Hex avoids every quoting problem.
        let line = format!(
            "add-generic-password -U -s {} -a {} -X {}\n",
            quote(&self.service),
            quote(&self.account),
            hex::encode(value.as_bytes())
        );
        let mut child = Command::new("security")
            .arg("-i")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("running `security` (macOS Keychain)")?;
        child.stdin.take().unwrap().write_all(line.as_bytes())?;
        let out = child.wait_with_output()?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() || stderr.contains("error") {
            bail!("keychain write failed for '{}': {}", self.service, stderr.trim());
        }
        Ok(())
    }

    pub fn delete(&self) -> Result<()> {
        let out = Command::new("security")
            .args(["delete-generic-password", "-s", &self.service, "-a", &self.account])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        match out.code() {
            Some(0) | Some(44) => Ok(()),
            _ => bail!("keychain delete failed for '{}'", self.service),
        }
    }
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `security -w` prints binary-looking data as hex. Our values are JSON or
/// base32, so decode only when the result is clearly a hex dump of text.
fn decode_if_hex(s: String) -> String {
    let looks_hex = s.len() >= 2
        && s.len().is_multiple_of(2)
        && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    if looks_hex
        && let Ok(bytes) = hex::decode(&s)
        && let Ok(text) = String::from_utf8(bytes)
    {
        let t = text.trim_start();
        if t.starts_with('{') || t.starts_with('[') {
            return text;
        }
    }
    s
}

/// accountant's own secret vault.
#[derive(Debug, Clone)]
pub enum Vault {
    Keychain,
    Files(PathBuf),
}

impl Vault {
    pub fn detect(data_dir: &std::path::Path) -> Self {
        let forced_file = std::env::var("ACCOUNTANT_SECRETS").is_ok_and(|v| v == "file");
        if cfg!(target_os = "macos") && !forced_file {
            Vault::Keychain
        } else {
            Vault::Files(data_dir.join("secrets"))
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Vault::Keychain => "macOS Keychain".into(),
            Vault::Files(dir) => format!("files in {}", dir.display()),
        }
    }

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        match self {
            Vault::Keychain => KeychainItem::new(SERVICE, key).get(),
            Vault::Files(dir) => fsutil::read_optional(&dir.join(file_name(key))),
        }
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        match self {
            Vault::Keychain => KeychainItem::new(SERVICE, key).set(value),
            Vault::Files(dir) => {
                fsutil::private_dir(dir)?;
                fsutil::write_secret(&dir.join(file_name(key)), value.as_bytes())
            }
        }
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        match self {
            Vault::Keychain => KeychainItem::new(SERVICE, key).delete(),
            Vault::Files(dir) => match std::fs::remove_file(dir.join(file_name(key))) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            },
        }
    }
}

fn file_name(key: &str) -> String {
    key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_decoding_only_for_text_payloads() {
        let json = r#"{"a":1}"#;
        assert_eq!(decode_if_hex(hex::encode(json)), json);
        // A base32 TOTP secret or a plain token must pass through untouched.
        assert_eq!(decode_if_hex("JBSWY3DPEHPK3PXP".into()), "JBSWY3DPEHPK3PXP");
        assert_eq!(decode_if_hex("deadbeef".into()), "deadbeef");
    }

    #[test]
    fn file_vault_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::Files(dir.path().join("secrets"));
        assert_eq!(vault.get("profile:x").unwrap(), None);
        vault.set("profile:x", "{\"k\":1}").unwrap();
        assert_eq!(vault.get("profile:x").unwrap().as_deref(), Some("{\"k\":1}"));
        vault.delete("profile:x").unwrap();
        assert_eq!(vault.get("profile:x").unwrap(), None);
    }
}
