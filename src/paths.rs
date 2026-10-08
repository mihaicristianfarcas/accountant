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
    /// OpenCode's data directory (`$XDG_DATA_HOME/opencode`, else
    /// `~/.local/share/opencode`).
    pub opencode_dir: PathBuf,
    /// cursor-agent's own directory (`~/.cursor` on macOS).
    pub cursor_agent_dir: PathBuf,
    /// Copilot CLI's home (`$COPILOT_HOME` or `~/.copilot`).
    pub copilot_home: PathBuf,
    /// Where desktop apps keep their data (`~/Library/Application Support`
    /// on macOS, `~/.config` elsewhere).
    pub app_support: PathBuf,
    /// Set in tests and sandboxes: the Keychain items of the newer providers
    /// become files in this directory instead.
    pub keychain_dir: Option<PathBuf>,
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

        let opencode_dir = env::var_os("XDG_DATA_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("share"))
            .join("opencode");

        let config_home = env::var_os("XDG_CONFIG_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let cursor_agent_dir =
            if cfg!(target_os = "macos") { home.join(".cursor") } else { config_home.join("cursor") };

        let copilot_home = env::var_os("COPILOT_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".copilot"));

        let app_support = env::var_os("ACCOUNTANT_APP_SUPPORT").map(PathBuf::from).unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                home.join("Library").join("Application Support")
            } else {
                config_home
            }
        });

        let keychain_dir =
            env::var_os("ACCOUNTANT_KEYCHAIN_DIR").filter(|d| !d.is_empty()).map(PathBuf::from);

        Paths {
            data,
            claude_dir,
            claude_json,
            claude_store,
            codex_home,
            opencode_dir,
            cursor_agent_dir,
            copilot_home,
            app_support,
            keychain_dir,
        }
    }

    /// A paths set rooted in one directory, for tests.
    #[cfg(test)]
    pub fn sandbox(root: &std::path::Path) -> Self {
        Paths {
            data: root.join("data"),
            claude_dir: root.join(".claude"),
            claude_json: root.join(".claude.json"),
            claude_store: None,
            codex_home: root.join(".codex"),
            opencode_dir: root.join("opencode"),
            cursor_agent_dir: root.join(".cursor"),
            copilot_home: root.join(".copilot"),
            app_support: root.join("Application Support"),
            keychain_dir: Some(root.join("keychain")),
        }
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

    /// OpenCode 2 keeps its logins in the `credential` table here.
    pub fn opencode_db(&self) -> PathBuf {
        self.opencode_dir.join("opencode.db")
    }

    /// OpenCode 1's login file (still read when there is no database).
    pub fn opencode_auth(&self) -> PathBuf {
        self.opencode_dir.join("auth.json")
    }

    /// The `state.vscdb` of a VS Code–based desktop app.
    pub fn vscode_state_db(&self, app: &str) -> PathBuf {
        self.app_support.join(app).join("User").join("globalStorage").join("state.vscdb")
    }

    pub fn copilot_config(&self) -> PathBuf {
        self.copilot_home.join("config.json")
    }

    pub fn claude_credentials_file(&self) -> PathBuf {
        self.claude_store
            .as_ref()
            .map_or_else(|| self.claude_dir.clone(), PathBuf::from)
            .join(".credentials.json")
    }
}
