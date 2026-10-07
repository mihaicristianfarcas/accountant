//! The two CLIs we manage, and how their live login is read and replaced.
//!
//! A *snapshot* is everything needed to restore a login. Credential blobs are
//! kept byte-for-byte as the CLI wrote them; only identity fields are parsed.
//!
//! * Claude Code — OAuth credentials (macOS Keychain item
//!   `Claude Code-credentials`, or `~/.claude/.credentials.json`) plus the
//!   `oauthAccount` object in `~/.claude.json`.
//! * Codex — `~/.codex/auth.json`.

use crate::fsutil;
use crate::paths::Paths;
use crate::secrets::KeychainItem;
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];

    pub fn label(self) -> &'static str {
        match self {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "cc" | "anthropic" => Some(Provider::Claude),
            "codex" | "openai" | "chatgpt" | "cx" => Some(Provider::Codex),
            _ => None,
        }
    }

    /// The executable whose running sessions hold the old login in memory.
    pub fn process_name(self) -> &'static str {
        self.slug()
    }

    /// Substrings of sender addresses that send this provider's login codes.
    pub fn mail_senders(self) -> &'static [&'static str] {
        match self {
            Provider::Claude => &["anthropic", "claude"],
            Provider::Codex => &["openai", "chatgpt"],
        }
    }
}

/// Who a snapshot belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Identity {
    /// Stable key: survives token refreshes, distinguishes orgs/workspaces.
    pub key: String,
    pub email: Option<String>,
    pub org: Option<String>,
    pub plan: Option<String>,
}

pub type Snapshot = Value;

pub fn read_live(provider: Provider, paths: &Paths) -> Result<Option<Snapshot>> {
    match provider {
        Provider::Claude => claude::read_live(paths),
        Provider::Codex => codex::read_live(paths),
    }
}

pub fn write_live(provider: Provider, paths: &Paths, snap: &Snapshot) -> Result<()> {
    match provider {
        Provider::Claude => claude::write_live(paths, snap),
        Provider::Codex => codex::write_live(paths, snap),
    }
}

pub fn identity(provider: Provider, snap: &Snapshot) -> Option<Identity> {
    match provider {
        Provider::Claude => claude::identity(snap),
        Provider::Codex => codex::identity(snap),
    }
}

/// A short, non-reversible tag of the snapshot's refresh token. Stable while
/// the token is unchanged; changes when the CLI rotates it.
pub fn fingerprint(provider: Provider, snap: &Snapshot) -> Option<String> {
    let rt = match provider {
        Provider::Claude => {
            let raw = snap.get("credentials")?.as_str()?;
            let v: Value = serde_json::from_str(raw).ok()?;
            v.get("claudeAiOauth")?.get("refreshToken")?.as_str()?.to_string()
        }
        Provider::Codex => {
            let v: Value = serde_json::from_str(snap.get("auth")?.as_str()?).ok()?;
            v.get("tokens")?.get("refresh_token")?.as_str()?.to_string()
        }
    };
    (!rt.is_empty()).then(|| short_hash(&rt))
}

/// A short-lived bearer token from the snapshot, if it has not expired.
pub fn fresh_access_token(provider: Provider, snap: &Snapshot) -> Option<AccessToken> {
    let tok = match provider {
        Provider::Claude => claude::access_token(snap),
        Provider::Codex => codex::access_token(snap),
    }?;
    match tok.expires_at {
        Some(exp) if exp <= Utc::now() + chrono::Duration::seconds(30) => None,
        _ => Some(tok),
    }
}

#[derive(Debug, Clone)]
pub struct AccessToken {
    pub token: String,
    pub expires_at: Option<DateTime<Utc>>,
    /// Codex: ChatGPT workspace id, sent as `ChatGPT-Account-Id`.
    pub account_id: Option<String>,
}

/// Decode the claims of a JWT without verifying it (we only read our own
/// tokens to learn which account they belong to).
pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn short_hash(s: &str) -> String {
    hex::encode(&Sha256::digest(s.as_bytes())[..6])
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_str().filter(|s| !s.is_empty())
}

pub mod claude {
    use super::*;

    pub const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

    /// Where Claude Code keeps its OAuth credentials on this machine.
    #[derive(Debug, Clone)]
    pub enum CredStore {
        Keychain(KeychainItem),
        File(PathBuf),
    }

    impl CredStore {
        pub fn detect(paths: &Paths) -> Self {
            let mode = std::env::var("ACCOUNTANT_CLAUDE_CREDENTIALS").unwrap_or_default();
            if mode == "file" || !cfg!(target_os = "macos") {
                return CredStore::File(paths.claude_credentials_file());
            }
            let service = std::env::var("ACCOUNTANT_CLAUDE_KEYCHAIN_SERVICE")
                .unwrap_or_else(|_| KEYCHAIN_SERVICE.to_string());
            let account = std::env::var("USER").unwrap_or_else(|_| "claude".into());
            CredStore::Keychain(KeychainItem::new(service, account))
        }

        pub fn describe(&self) -> String {
            match self {
                CredStore::Keychain(item) => format!("Keychain item \"{}\"", item.service),
                CredStore::File(p) => p.display().to_string(),
            }
        }

        fn get(&self, paths: &Paths) -> Result<Option<String>> {
            match self {
                CredStore::Keychain(item) => match item.get()? {
                    Some(s) => Ok(Some(s)),
                    // Claude Code falls back to the plaintext file when the
                    // keychain is unavailable; mirror that.
                    None => fsutil::read_optional(&paths.claude_credentials_file()),
                },
                CredStore::File(p) => fsutil::read_optional(p),
            }
        }

        fn set(&self, value: &str) -> Result<()> {
            match self {
                CredStore::Keychain(item) => item.set(value),
                CredStore::File(p) => fsutil::write_secret(p, value.as_bytes()),
            }
        }
    }

    pub fn read_live(paths: &Paths) -> Result<Option<Snapshot>> {
        let Some(creds) = CredStore::detect(paths).get(paths)? else {
            return Ok(None);
        };
        if creds.trim().is_empty() {
            return Ok(None);
        }
        let account = read_global(paths)?.and_then(|g| g.get("oauthAccount").cloned()).unwrap_or(Value::Null);
        Ok(Some(json!({ "credentials": creds, "oauthAccount": account })))
    }

    pub fn write_live(paths: &Paths, snap: &Snapshot) -> Result<()> {
        let creds = snap
            .get("credentials")
            .and_then(Value::as_str)
            .context("saved Claude login has no credentials")?;
        let account = snap.get("oauthAccount").cloned().unwrap_or(Value::Null);

        // A running Claude Code rewrites ~/.claude.json all the time (project
        // history, tips, counters). Read-modify-write it under Claude's own
        // lock so neither side loses the other's change.
        with_config_lock(&paths.claude_json, CONFIG_LOCK_WAIT, || {
            // Patch ~/.claude.json first: if it is unreadable we stop before
            // touching the credentials, so the two never disagree.
            let mut global = read_global(paths)?.unwrap_or_else(|| json!({}));
            let obj = global.as_object_mut().context("~/.claude.json is not a JSON object")?;
            if account.is_null() {
                obj.remove("oauthAccount");
            } else {
                obj.insert("oauthAccount".into(), account);
            }
            let mut text = serde_json::to_string_pretty(&global)?;
            text.push('\n');

            CredStore::detect(paths).set(creds)?;
            fsutil::write_atomic(&paths.claude_json, text.as_bytes(), 0o600)
        })
    }

    /// How long to wait for a Claude Code that holds its config lock.
    const CONFIG_LOCK_WAIT: Duration = Duration::from_secs(6);
    /// `proper-lockfile`'s default: a lock not refreshed for this long was
    /// left behind by a process that died.
    const CONFIG_LOCK_STALE: Duration = Duration::from_secs(10);

    /// Claude Code guards `~/.claude.json` with `proper-lockfile`: a
    /// `<file>.lock` directory that exists while the lock is held and is
    /// touched every few seconds. Take it the same way.
    pub(crate) fn with_config_lock<T>(
        file: &Path,
        wait: Duration,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let mut name = file.file_name().context("config path has no file name")?.to_os_string();
        name.push(".lock");
        let lock = file.with_file_name(name);
        let started = Instant::now();
        loop {
            match std::fs::create_dir(&lock) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&lock)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .is_some_and(|age| age > CONFIG_LOCK_STALE);
                    if stale {
                        let _ = std::fs::remove_dir(&lock);
                        continue;
                    }
                    if started.elapsed() > wait {
                        bail!("Claude Code is busy updating {} — retry in a moment", file.display());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                // No directory to lock in (first run): nothing to race with.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return f(),
                Err(e) => return Err(e).with_context(|| format!("locking {}", file.display())),
            }
        }
        let result = f();
        let _ = std::fs::remove_dir(&lock);
        result
    }

    fn read_global(paths: &Paths) -> Result<Option<Value>> {
        match fsutil::read_optional(&paths.claude_json)? {
            None => Ok(None),
            Some(text) => serde_json::from_str(&text)
                .map(Some)
                .with_context(|| format!("{} is not valid JSON", paths.claude_json.display())),
        }
    }

    fn oauth(snap: &Snapshot) -> Option<Value> {
        let raw = snap.get("credentials")?.as_str()?;
        let v: Value = serde_json::from_str(raw).ok()?;
        v.get("claudeAiOauth").cloned()
    }

    pub fn identity(snap: &Snapshot) -> Option<Identity> {
        let acct = snap.get("oauthAccount").filter(|v| v.is_object());
        let oauth = oauth(snap);
        let plan = oauth.as_ref().and_then(|o| {
            let sub = str_at(o, &["subscriptionType"])?;
            let tier = str_at(o, &["rateLimitTier"]).unwrap_or("");
            let mult = ["20x", "5x"].into_iter().find(|m| tier.contains(m));
            Some(match mult {
                Some(m) => format!("{sub} {m}"),
                None => sub.to_string(),
            })
        });
        match acct {
            Some(a) => {
                let user = str_at(a, &["accountUuid"]).unwrap_or("?");
                let org = str_at(a, &["organizationUuid"]).unwrap_or("-");
                Some(Identity {
                    key: format!("claude:{user}:{org}"),
                    email: str_at(a, &["emailAddress"]).map(str::to_string),
                    org: str_at(a, &["organizationName"]).map(str::to_string),
                    plan,
                })
            }
            // Credentials without account metadata (e.g. hand-made). Still
            // switchable, just anonymous.
            None => snap.get("credentials").map(|_| Identity {
                key: "claude:unknown".into(),
                plan,
                ..Default::default()
            }),
        }
    }

    pub fn access_token(snap: &Snapshot) -> Option<AccessToken> {
        let o = oauth(snap)?;
        let token = str_at(&o, &["accessToken"])?.to_string();
        let expires_at =
            o.get("expiresAt").and_then(Value::as_i64).and_then(|ms| Utc.timestamp_millis_opt(ms).single());
        Some(AccessToken { token, expires_at, account_id: None })
    }
}

pub mod codex {
    use super::*;

    pub fn read_live(paths: &Paths) -> Result<Option<Snapshot>> {
        let Some(text) = fsutil::read_optional(&paths.codex_auth())? else {
            return Ok(None);
        };
        if text.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(json!({ "auth": text })))
    }

    pub fn write_live(paths: &Paths, snap: &Snapshot) -> Result<()> {
        let auth = snap.get("auth").and_then(Value::as_str).context("saved Codex login has no auth.json")?;
        if serde_json::from_str::<Value>(auth).is_err() {
            bail!("saved Codex login is corrupt (auth.json is not JSON)");
        }
        fsutil::write_secret(&paths.codex_auth(), auth.as_bytes())
    }

    /// Build the auth.json Codex expects from a fresh OAuth token response.
    pub fn auth_json(id_token: &str, access_token: &str, refresh_token: &str) -> String {
        let account_id = jwt_claims(id_token)
            .and_then(|c| {
                str_at(&c, &["https://api.openai.com/auth", "chatgpt_account_id"]).map(String::from)
            })
            .map(Value::String)
            .unwrap_or(Value::Null);
        let doc = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": Value::Null,
            "tokens": {
                "id_token": id_token,
                "access_token": access_token,
                "refresh_token": refresh_token,
                "account_id": account_id,
            },
            "last_refresh": Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        });
        let mut s = serde_json::to_string_pretty(&doc).expect("static JSON");
        s.push('\n');
        s
    }

    fn auth(snap: &Snapshot) -> Option<Value> {
        serde_json::from_str(snap.get("auth")?.as_str()?).ok()
    }

    pub fn identity(snap: &Snapshot) -> Option<Identity> {
        let auth = auth(snap)?;
        if let Some(id_token) = str_at(&auth, &["tokens", "id_token"]) {
            let claims = jwt_claims(id_token).unwrap_or(Value::Null);
            let oa = claims.get("https://api.openai.com/auth").cloned().unwrap_or(Value::Null);
            let user = str_at(&oa, &["chatgpt_user_id"])
                .or_else(|| str_at(&oa, &["user_id"]))
                .or_else(|| str_at(&claims, &["sub"]))
                .or_else(|| str_at(&claims, &["email"]))
                .unwrap_or("?");
            let account = str_at(&oa, &["chatgpt_account_id"])
                .or_else(|| str_at(&auth, &["tokens", "account_id"]))
                .unwrap_or("-");
            return Some(Identity {
                key: format!("codex:{user}:{account}"),
                email: str_at(&claims, &["email"]).map(str::to_string),
                org: None,
                plan: str_at(&oa, &["chatgpt_plan_type"]).map(str::to_string),
            });
        }
        let key = str_at(&auth, &["OPENAI_API_KEY"])?;
        Some(Identity {
            key: format!("codex:apikey:{}", short_hash(key)),
            email: None,
            org: None,
            plan: Some("api key".into()),
        })
    }

    pub fn access_token(snap: &Snapshot) -> Option<AccessToken> {
        let auth = auth(snap)?;
        let token = str_at(&auth, &["tokens", "access_token"])?.to_string();
        let expires_at = jwt_claims(&token)
            .and_then(|c| c.get("exp").and_then(Value::as_i64))
            .and_then(|s| Utc.timestamp_opt(s, 0).single());
        let account_id = str_at(&auth, &["tokens", "account_id"]).map(str::to_string);
        Some(AccessToken { token, expires_at, account_id })
    }

    /// Codex can be configured to keep credentials in the OS keyring, in
    /// which case auth.json is not the source of truth.
    pub fn uses_keyring(paths: &Paths) -> bool {
        let Ok(Some(text)) = fsutil::read_optional(&paths.codex_home.join("config.toml")) else {
            return false;
        };
        let Ok(cfg) = text.parse::<toml::Table>() else {
            return false;
        };
        matches!(
            cfg.get("cli_auth_credentials_store").and_then(|v| v.as_str()),
            Some("keyring") | Some("auto")
        )
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn fake_jwt(claims: Value) -> String {
        let enc = |v: &Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
        };
        format!("{}.{}.sig", enc(&json!({"alg": "none"})), enc(&claims))
    }

    pub fn fake_codex_auth(email: &str, user: &str, account: &str, plan: &str) -> String {
        let id = fake_jwt(json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_user_id": user,
                "chatgpt_account_id": account,
                "chatgpt_plan_type": plan,
            }
        }));
        let access = fake_jwt(json!({ "exp": Utc::now().timestamp() + 3600 }));
        codex::auth_json(&id, &access, &format!("rt-{user}"))
    }

    pub fn fake_claude(email: &str, uuid: &str, org: &str, token: &str) -> Snapshot {
        let creds = json!({ "claudeAiOauth": {
            "accessToken": format!("at-{token}"),
            "refreshToken": format!("rt-{token}"),
            "expiresAt": (Utc::now().timestamp() + 3600) * 1000,
            "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max",
            "rateLimitTier": "default_claude_max_20x",
        }});
        json!({
            "credentials": creds.to_string(),
            "oauthAccount": {
                "accountUuid": uuid,
                "emailAddress": email,
                "organizationUuid": org,
                "organizationName": format!("{email}'s Organization"),
            }
        })
    }

    #[test]
    fn claude_identity_is_stable_across_token_refresh() {
        let a = fake_claude("a@x.com", "u1", "o1", "first");
        let b = fake_claude("a@x.com", "u1", "o1", "refreshed");
        let ia = claude::identity(&a).unwrap();
        assert_eq!(ia, claude::identity(&b).unwrap());
        assert_eq!(ia.email.as_deref(), Some("a@x.com"));
        assert_eq!(ia.plan.as_deref(), Some("max 20x"));
        // Same user in another org is another account.
        let c = fake_claude("a@x.com", "u1", "o2", "first");
        assert_ne!(ia.key, claude::identity(&c).unwrap().key);
    }

    #[test]
    fn codex_identity_from_id_token() {
        let snap = json!({ "auth": fake_codex_auth("me@y.com", "user-1", "acct-1", "plus") });
        let id = codex::identity(&snap).unwrap();
        assert_eq!(id.key, "codex:user-1:acct-1");
        assert_eq!(id.email.as_deref(), Some("me@y.com"));
        assert_eq!(id.plan.as_deref(), Some("plus"));
        let tok = fresh_access_token(Provider::Codex, &snap).unwrap();
        assert_eq!(tok.account_id.as_deref(), Some("acct-1"));
    }

    #[test]
    fn codex_auth_json_has_codex_shape() {
        let auth: Value = serde_json::from_str(&fake_codex_auth("me@y.com", "u", "acct-9", "pro")).unwrap();
        assert_eq!(auth["auth_mode"], "chatgpt");
        assert!(auth["OPENAI_API_KEY"].is_null());
        assert_eq!(auth["tokens"]["account_id"], "acct-9");
        assert_eq!(auth["last_refresh"].as_str().unwrap().len(), 27);
    }

    #[test]
    fn codex_api_key_mode() {
        let snap = json!({ "auth": r#"{"OPENAI_API_KEY":"sk-test"}"# });
        let id = codex::identity(&snap).unwrap();
        assert!(id.key.starts_with("codex:apikey:"));
        assert!(!id.key.contains("sk-test"));
    }

    #[test]
    fn claude_config_is_patched_under_claude_codes_own_lock() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".claude.json");
        let lock = dir.path().join(".claude.json.lock");

        // Claude holds the lock; we wait for it rather than writing over it.
        std::fs::create_dir(&lock).unwrap();
        let held = lock.clone();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            std::fs::remove_dir(&held).unwrap();
        });
        let started = Instant::now();
        let saw_lock = claude::with_config_lock(&file, Duration::from_secs(5), || Ok(lock.exists())).unwrap();
        release.join().unwrap();
        assert!(saw_lock, "the closure runs while we hold the lock");
        assert!(started.elapsed() >= Duration::from_millis(200), "waited for Claude");
        assert!(!lock.exists(), "released afterwards");

        // A lock nobody releases: give up with a clear error, change nothing.
        std::fs::create_dir(&lock).unwrap();
        let err = claude::with_config_lock(&file, Duration::from_millis(150), || -> Result<()> {
            panic!("must not run without the lock")
        })
        .unwrap_err();
        assert!(err.to_string().contains("busy"), "{err}");

        // A lock left behind by a crashed process is taken over.
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::open(&lock).unwrap().set_modified(old).unwrap();
        assert!(claude::with_config_lock(&file, Duration::from_millis(150), || Ok(true)).unwrap());
        assert!(!lock.exists());
    }

    #[test]
    fn expired_tokens_are_not_offered() {
        let creds = json!({ "claudeAiOauth": { "accessToken": "x", "expiresAt": 1000 } });
        let snap = json!({ "credentials": creds.to_string(), "oauthAccount": null });
        assert!(fresh_access_token(Provider::Claude, &snap).is_none());
    }
}
