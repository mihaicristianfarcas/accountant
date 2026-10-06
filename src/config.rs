//! User settings (`config.toml`). Every field has a sensible default, so the
//! file is optional.

use crate::fsutil;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub browser: BrowserConfig,
    pub mail: MailConfig,
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BrowserMode {
    /// A persistent, separate browser profile per account: cookies survive,
    /// so re-logins are usually a single click.
    #[default]
    Isolated,
    /// A throwaway private window with a fresh session every time.
    Private,
    /// Whatever the system default browser is.
    Default,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    pub mode: BrowserMode,
    /// "auto" or an app name such as "Google Chrome", "Helium", "Firefox".
    pub app: String,
    /// Custom command to open sign-in pages instead, e.g.
    /// `open -a Arc {url}`. `{url}` is replaced (appended when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        BrowserConfig { mode: BrowserMode::Isolated, app: "auto".into(), command: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MailSource {
    #[default]
    Off,
    /// Read codes from Apple Mail (no password needed; asks for Automation
    /// permission once).
    AppleMail,
    /// Poll an IMAP inbox (password stored in the vault).
    Imap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MailConfig {
    pub source: MailSource,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_user: String,
    pub imap_folder: String,
}

impl Default for MailConfig {
    fn default() -> Self {
        MailConfig {
            source: MailSource::Off,
            imap_host: String::new(),
            imap_port: 993,
            imap_user: String::new(),
            imap_folder: "INBOX".into(),
        }
    }
}

pub const IMAP_PASSWORD_KEY: &str = "mail:imap";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Quit right after a successful switch — you're ready to go.
    pub auto_exit: bool,
    /// Fetch live usage meters for accounts with a fresh token.
    pub usage: bool,
    /// Skip the intro animation.
    pub reduced_motion: bool,
    /// Partially mask email addresses (for screenshots and recordings).
    pub hide_emails: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig { auto_exit: true, usage: true, reduced_motion: false, hide_emails: false }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match fsutil::read_optional(path)? {
            None => Ok(Config::default()),
            Some(text) => toml::from_str(&text).with_context(|| format!("{} is invalid", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        fsutil::write_atomic(path, text.as_bytes(), 0o600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_uses_defaults() {
        let cfg: Config = toml::from_str("[mail]\nsource = \"apple-mail\"\n").unwrap();
        assert_eq!(cfg.mail.source, MailSource::AppleMail);
        assert_eq!(cfg.mail.imap_port, 993);
        assert_eq!(cfg.browser.mode, BrowserMode::Isolated);
        assert!(cfg.ui.auto_exit);
    }

    #[test]
    fn roundtrip() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.browser.app, "auto");
    }
}
