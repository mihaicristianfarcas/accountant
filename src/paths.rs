//! Where everything lives. Every location can be overridden through the
//! environment, which is also how the integration tests sandbox the tool.

use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Paths {
    /// accountant's own state (profiles, config, isolated browser profiles).
    pub data: PathBuf,
    /// Claude Code config directory (`$CLAUDE_CONFIG_DIR` or `~/.claude`).
    pub claude_dir: PathBuf,
    /// Claude Code global config file holding `oauthAccount`.
    pub claude_json: PathBuf,
    /// The directory Claude Code keys its credential store by, exactly as
    /// given (`$CLAUDE_SECURESTORAGE_CONFIG_DIR`, else `$CLAUDE_CONFIG_DIR`);
    /// `None` for the default store.
    pub claude_store: Option<String>,
    /// Codex home (`$CODEX_HOME` or `~/.codex`).
    pub codex_home: PathBuf,
}

impl Paths {
    pub fn detect() -> Self {
        let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));

        let data = env::var_os("ACCOUNTANT_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config").join("accountant"));

        let (claude_dir, claude_json) = match env::var_os("CLAUDE_CONFIG_DIR") {
            Some(dir) => {
                let dir = PathBuf::from(dir);
                let json = dir.join(".claude.json");
                (dir, json)
            }
            None => (home.join(".claude"), home.join(".claude.json")),
        };

        // Set but empty pins the default store, as in Claude Code.
        let claude_store = match env::var("CLAUDE_SECURESTORAGE_CONFIG_DIR") {
            Ok(dir) => Some(dir).filter(|d| !d.is_empty()),
            Err(_) => env::var("CLAUDE_CONFIG_DIR").ok().filter(|d| !d.is_empty()),
        };

        let codex_home = env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex"));

        Paths { data, claude_dir, claude_json, claude_store, codex_home }
    }

    pub fn registry(&self) -> PathBuf {
        self.data.join("profiles.json")
    }

    pub fn config(&self) -> PathBuf {
        self.data.join("config.toml")
    }

    pub fn usage_cache(&self) -> PathBuf {
        self.data.join("usage.json")
    }

    pub fn browsers(&self) -> PathBuf {
        self.data.join("browsers")
    }

    pub fn lock(&self) -> PathBuf {
        self.data.join(".lock")
    }

    pub fn codex_auth(&self) -> PathBuf {
        self.codex_home.join("auth.json")
    }

    pub fn claude_credentials_file(&self) -> PathBuf {
        self.claude_store
            .as_ref()
            .map_or_else(|| self.claude_dir.clone(), PathBuf::from)
            .join(".credentials.json")
    }
}
