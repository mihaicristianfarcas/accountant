//! The switching engine.
//!
//! Invariant: the live login is never thrown away. Before anything replaces
//! it, it is written back into the profile it belongs to (or adopted as a new
//! profile). Both CLIs rotate refresh tokens, so the copy in the vault would
//! otherwise go stale the moment the CLI refreshes.

use crate::config::Config;
use crate::fsutil;
use crate::paths::Paths;
use crate::providers::{self, Identity, Provider, Snapshot};
use crate::registry::{Profile, Registry};
use crate::secrets::Vault;
use crate::twofa;
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use std::fs::File;
use std::process::{Command, Stdio};

pub struct Engine {
    pub paths: Paths,
    pub vault: Vault,
    pub config: Config,
    pub registry: Registry,
}

/// Exclusive lock on accountant's state, held while mutating.
pub struct Lock(#[allow(dead_code)] File);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Save,
    Load,
    Swap,
    Verify,
}

impl Stage {
    pub const ALL: [Stage; 4] = [Stage::Save, Stage::Load, Stage::Swap, Stage::Verify];

    pub fn label(self) -> &'static str {
        match self {
            Stage::Save => "saving current session",
            Stage::Load => "unlocking saved login",
            Stage::Swap => "swapping credentials",
            Stage::Verify => "verifying",
        }
    }
}

/// An in-flight switch, advanced one [`Stage`] at a time so the UI can
/// animate each step.
pub struct Switch {
    pub provider: Provider,
    pub target: String,
    /// The profile that was live before (if any, and not the target).
    pub from: Option<String>,
    pub already_active: bool,
    snapshot: Option<Snapshot>,
    /// The live login as the Save stage stored it.
    saved: Option<Snapshot>,
    _lock: Lock,
}

/// Result of a completed switch (the state lock is released).
#[derive(Debug, Clone)]
pub struct Switched {
    pub provider: Provider,
    pub target: String,
    pub from: Option<String>,
    pub already_active: bool,
}

impl Switch {
    pub fn finish(self) -> Switched {
        Switched {
            provider: self.provider,
            target: self.target,
            from: self.from,
            already_active: self.already_active,
        }
    }
}

#[derive(Debug)]
pub enum Synced {
    NothingLive,
    Saved(String),
    Adopted(String),
}

impl Synced {
    pub fn profile_id(&self) -> Option<&str> {
        match self {
            Synced::NothingLive => None,
            Synced::Saved(id) | Synced::Adopted(id) => Some(id),
        }
    }
}

impl Engine {
    pub fn open() -> Result<Self> {
        Self::open_at(Paths::detect())
    }

    pub fn open_at(paths: Paths) -> Result<Self> {
        fsutil::private_dir(&paths.data)?;
        let config = Config::load(&paths.config())?;
        let forced = std::env::var("ACCOUNTANT_HIDE_EMAILS").is_ok_and(|v| v == "1" || v == "true");
        crate::privacy::set(config.ui.hide_emails || forced);
        Ok(Engine {
            vault: Vault::detect(&paths.data),
            config,
            registry: Registry::load(&paths.registry())?,
            paths,
        })
    }

    pub fn lock(&mut self) -> Result<Lock> {
        let f = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.paths.lock())
            .context("opening lock file")?;
        f.lock().context("locking accountant state")?;
        // Another instance may have changed things while we waited.
        self.registry = Registry::load(&self.paths.registry())?;
        Ok(Lock(f))
    }

    fn persist(&self) -> Result<()> {
        self.registry.save(&self.paths.registry())
    }

    pub fn save_config(&self) -> Result<()> {
        self.config.save(&self.paths.config())
    }

    pub fn profile(&self, id: &str) -> Result<&Profile> {
        self.registry.get(id).ok_or_else(|| anyhow!("unknown account '{id}'"))
    }

    /// The live login for `provider` and who it belongs to.
    pub fn live(&self, provider: Provider) -> Result<Option<(Snapshot, Identity)>> {
        let Some(snap) = providers::read_live(provider, &self.paths)? else {
            return Ok(None);
        };
        let ident = providers::identity(provider, &snap).unwrap_or_else(|| Identity {
            key: format!("{}:unknown", provider.slug()),
            ..Default::default()
        });
        Ok(Some((snap, ident)))
    }

    /// Profile id of the account currently logged in for `provider`.
    pub fn active_id(&self, provider: Provider) -> Option<String> {
        let (snap, ident) = self.live(provider).ok()??;
        self.owner(provider, &snap, &ident)
    }

    /// Which saved profile a live login belongs to. Exact credentials win over
    /// the account metadata stored next to them.
    fn owner(&self, provider: Provider, snap: &Snapshot, ident: &Identity) -> Option<String> {
        let fp = providers::fingerprint(provider, snap);
        fp.as_ref()
            .and_then(|fp| {
                self.registry
                    .profiles
                    .iter()
                    .find(|p| p.provider == provider && p.fingerprint.as_ref() == Some(fp))
            })
            .or_else(|| self.registry.by_identity(provider, &ident.key))
            .map(|p| p.id.clone())
    }

    /// Store `snap` under the matching profile, or create one.
    /// Returns (profile id, created).
    fn store(
        &mut self,
        provider: Provider,
        snap: &Snapshot,
        ident: &Identity,
        name: Option<&str>,
    ) -> Result<(String, bool)> {
        let existing = self.registry.by_identity(provider, &ident.key).map(|p| p.id.clone());
        let (id, created) = match existing {
            Some(id) => (id, false),
            None => {
                let name = match name {
                    Some(n) if !self.registry.name_taken(provider, n, None) => n.to_string(),
                    _ => self.registry.suggest_name(provider, ident),
                };
                let id = self.registry.new_id(provider);
                self.registry.profiles.push(Profile {
                    id: id.clone(),
                    provider,
                    name,
                    identity: ident.key.clone(),
                    email: None,
                    org: None,
                    plan: None,
                    created_at: Utc::now(),
                    saved_at: None,
                    used_at: None,
                    left_at: None,
                    totp: false,
                    needs_login: false,
                    fingerprint: None,
                });
                (id, true)
            }
        };
        let key = format!("profile:{id}");
        self.vault.set(&key, &serde_json::to_string(snap)?)?;
        let p = self.registry.get_mut(&id).expect("just ensured");
        p.apply_identity(ident);
        p.fingerprint = providers::fingerprint(provider, snap);
        p.saved_at = Some(Utc::now());
        Ok((id, created))
    }

    fn sync_back_locked(&mut self, provider: Provider) -> Result<Synced> {
        let live = self.live(provider)?;
        self.store_live_locked(provider, live)
    }

    /// Save a live login that was just read into its profile.
    fn store_live_locked(
        &mut self,
        provider: Provider,
        live: Option<(Snapshot, Identity)>,
    ) -> Result<Synced> {
        let Some((snap, ident)) = live else {
            return Ok(Synced::NothingLive);
        };
        // The exact credentials of a saved profile whose account metadata
        // disagrees: the metadata is stale (rewritten by an old session). The
        // vault already holds these credentials with the right metadata.
        if let Some(owner) = self.owner(provider, &snap, &ident) {
            let p = self.profile(&owner)?;
            if p.identity != ident.key && p.fingerprint == providers::fingerprint(provider, &snap) {
                return Ok(Synced::Saved(owner));
            }
        }
        let (id, created) = self.store(provider, &snap, &ident, None)?;
        self.persist()?;
        Ok(if created { Synced::Adopted(id) } else { Synced::Saved(id) })
    }

    /// Save the live login into its profile (adopting unknown accounts).
    pub fn sync_back(&mut self, provider: Provider) -> Result<Synced> {
        let _lock = self.lock()?;
        self.sync_back_locked(provider)
    }

    /// Save the live login as a profile, optionally naming it.
    pub fn save_current(&mut self, provider: Provider, name: Option<&str>) -> Result<Option<(String, bool)>> {
        let _lock = self.lock()?;
        let Some((snap, ident)) = self.live(provider)? else {
            return Ok(None);
        };
        let (id, created) = self.store(provider, &snap, &ident, name)?;
        if let Some(n) = name
            && !created
            && !self.registry.name_taken(provider, n, Some(&id))
        {
            self.registry.get_mut(&id).unwrap().name = n.to_string();
        }
        self.persist()?;
        Ok(Some((id, created)))
    }

    pub fn begin_switch(&mut self, id: &str) -> Result<Switch> {
        let lock = self.lock()?;
        let provider = self.profile(id)?.provider;
        Ok(Switch {
            provider,
            target: id.to_string(),
            from: None,
            already_active: false,
            snapshot: None,
            saved: None,
            _lock: lock,
        })
    }

    pub fn run_stage(&mut self, sw: &mut Switch, stage: Stage) -> Result<()> {
        match stage {
            Stage::Save => {
                let live = self.live(sw.provider)?;
                sw.saved = live.as_ref().map(|(s, _)| s.clone());
                let synced = self.store_live_locked(sw.provider, live)?;
                match synced.profile_id() {
                    Some(id) if id == sw.target => sw.already_active = true,
                    Some(id) => sw.from = Some(id.to_string()),
                    None => {}
                }
            }
            Stage::Load => {
                let p = self.profile(&sw.target)?;
                let raw = self
                    .vault
                    .get(&p.vault_key())?
                    .ok_or_else(|| anyhow!("no saved login for '{}' — sign in again with r", p.name))?;
                sw.snapshot = Some(serde_json::from_str(&raw).context("saved login is corrupt")?);
            }
            Stage::Swap => {
                if !sw.already_active {
                    // The CLI may have refreshed (and so rotated) its tokens
                    // since Save. Keep the newer copy too, right before it is
                    // replaced, or that account's saved login is already dead.
                    let live = self.live(sw.provider)?;
                    if live.as_ref().map(|(s, _)| s) != sw.saved.as_ref() {
                        self.store_live_locked(sw.provider, live)?;
                    }
                    let snap = sw.snapshot.as_ref().context("nothing loaded")?;
                    providers::write_live(sw.provider, &self.paths, snap)?;
                }
            }
            Stage::Verify => {
                let live = self.live(sw.provider)?;
                let owner = live.as_ref().and_then(|(s, i)| self.owner(sw.provider, s, i));
                if owner.as_deref() != Some(sw.target.as_str()) {
                    bail!("verification failed: the live login does not match after the swap");
                }
                let now = Utc::now();
                if let Some(from) = &sw.from
                    && let Some(p) = self.registry.get_mut(from)
                {
                    p.left_at = Some(now);
                }
                let p = self.registry.get_mut(&sw.target).unwrap();
                p.used_at = Some(now);
                p.needs_login = false;
                self.persist()?;
            }
        }
        Ok(())
    }

    /// Switch in one go (CLI path).
    pub fn switch(&mut self, id: &str) -> Result<Switched> {
        let mut sw = self.begin_switch(id)?;
        for stage in Stage::ALL {
            self.run_stage(&mut sw, stage)?;
        }
        Ok(sw.finish())
    }

    /// After a browser sign-in: put `snap` live (if given) and save it as a
    /// profile. `previous` is the profile that was live before the sign-in
    /// started. Returns (profile id, created).
    pub fn finish_login(
        &mut self,
        provider: Provider,
        snap: Option<&Snapshot>,
        name: Option<&str>,
        previous: Option<&str>,
    ) -> Result<(String, bool)> {
        let _lock = self.lock()?;
        let previous = match snap {
            // Codex: we hold the new tokens; save the old login first.
            Some(new) => {
                let prev = self.sync_back_locked(provider)?;
                providers::write_live(provider, &self.paths, new)?;
                prev.profile_id().map(String::from)
            }
            // Claude: `claude auth login` already wrote the live login.
            None => previous.map(String::from),
        };
        let (snap, ident) = self
            .live(provider)?
            .ok_or_else(|| anyhow!("sign-in finished but no {} login was found", provider.label()))?;
        let (id, created) = self.store(provider, &snap, &ident, name)?;
        let now = Utc::now();
        if let Some(prev) = previous.filter(|p| *p != id)
            && let Some(p) = self.registry.get_mut(&prev)
        {
            p.left_at = Some(now);
        }
        let p = self.registry.get_mut(&id).unwrap();
        p.used_at = Some(now);
        p.needs_login = false;
        self.persist()?;
        Ok((id, created))
    }

    pub fn rename(&mut self, id: &str, name: &str) -> Result<()> {
        let _lock = self.lock()?;
        let name = crate::registry::sanitize_name(name);
        let provider = self.profile(id)?.provider;
        if self.registry.name_taken(provider, &name, Some(id)) {
            bail!("another {} account is already called '{name}'", provider.label());
        }
        self.registry.get_mut(id).unwrap().name = name;
        self.persist()
    }

    pub fn remove(&mut self, id: &str) -> Result<Profile> {
        let _lock = self.lock()?;
        let p = self.profile(id)?.clone();
        self.vault.delete(&p.vault_key())?;
        self.vault.delete(&p.totp_key())?;
        self.registry.profiles.retain(|x| x.id != id);
        self.persist()?;
        Ok(p)
    }

    pub fn set_needs_login(&mut self, id: &str, value: bool) -> Result<()> {
        let _lock = self.lock()?;
        if let Some(p) = self.registry.get_mut(id) {
            p.needs_login = value;
        }
        self.persist()
    }

    pub fn set_totp(&mut self, id: &str, secret: Option<&str>) -> Result<()> {
        let _lock = self.lock()?;
        let key = self.profile(id)?.totp_key();
        match secret {
            Some(s) => {
                let parsed = twofa::TotpSecret::parse(s)?;
                self.vault.set(&key, &parsed.to_uri())?;
            }
            None => self.vault.delete(&key)?,
        }
        self.registry.get_mut(id).unwrap().totp = secret.is_some();
        self.persist()
    }

    pub fn totp(&self, id: &str) -> Result<Option<twofa::TotpSecret>> {
        let p = self.profile(id)?;
        if !p.totp {
            return Ok(None);
        }
        match self.vault.get(&p.totp_key())? {
            Some(s) => Ok(Some(twofa::TotpSecret::parse(&s)?)),
            None => Ok(None),
        }
    }

    /// Saved snapshot of a profile (the live one is fresher for the active
    /// account).
    #[cfg(test)]
    pub fn snapshot(&self, id: &str) -> Result<Option<Snapshot>> {
        let p = self.profile(id)?;
        match self.vault.get(&p.vault_key())? {
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            None => Ok(None),
        }
    }
}

/// How many interactive sessions of the provider's CLI are running (they keep
/// the old login in memory until restarted). Background helpers without a
/// terminal are not counted.
pub fn running_sessions(provider: Provider) -> usize {
    let Ok(out) = Command::new("ps").args(["-axo", "tty=,comm="]).stderr(Stdio::null()).output() else {
        return 0;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim_start().split_once(char::is_whitespace))
        .filter(|(tty, comm)| {
            let name = comm.trim().rsplit('/').next().unwrap_or("");
            name == provider.process_name() && !tty.starts_with('?')
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::tests::{fake_claude, fake_codex_auth};
    use serde_json::json;

    fn sandbox() -> (tempfile::TempDir, Engine) {
        // Never let a test reach the real Keychain item.
        // SAFETY: every test sets the same value.
        unsafe { std::env::set_var("ACCOUNTANT_CLAUDE_CREDENTIALS", "file") };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = Paths {
            data: root.join("data"),
            claude_dir: root.join(".claude"),
            claude_json: root.join(".claude.json"),
            claude_store: None,
            codex_home: root.join(".codex"),
        };
        std::fs::create_dir_all(&paths.claude_dir).unwrap();
        std::fs::create_dir_all(&paths.codex_home).unwrap();
        let mut engine = Engine::open_at(paths).unwrap();
        engine.vault = Vault::Files(root.join("data/secrets"));
        (dir, engine)
    }

    fn put_codex_live(e: &Engine, auth: &str) {
        std::fs::write(e.paths.codex_auth(), auth).unwrap();
    }

    #[test]
    fn switch_syncs_back_rotated_tokens() {
        let (_d, mut e) = sandbox();
        put_codex_live(&e, &fake_codex_auth("a@x.com", "ua", "acct-a", "plus"));
        let (a, created) = e.save_current(Provider::Codex, Some("alpha")).unwrap().unwrap();
        assert!(created);

        put_codex_live(&e, &fake_codex_auth("b@x.com", "ub", "acct-b", "pro"));
        let (b, _) = e.save_current(Provider::Codex, None).unwrap().unwrap();
        assert_eq!(e.profile(&b).unwrap().name, "b");
        assert_eq!(e.active_id(Provider::Codex).as_deref(), Some(b.as_str()));

        // Codex refreshes b's tokens while it is live.
        let refreshed = fake_codex_auth("b@x.com", "ub", "acct-b", "pro").replace("rt-ub", "rt-ub-2");
        put_codex_live(&e, &refreshed);

        let sw = e.switch(&a).unwrap();
        assert_eq!(sw.from.as_deref(), Some(b.as_str()));
        assert_eq!(e.active_id(Provider::Codex).as_deref(), Some(a.as_str()));
        assert!(e.profile(&b).unwrap().left_at.is_some());

        // The rotated refresh token of b was saved before the swap.
        let saved = e.snapshot(&b).unwrap().unwrap();
        assert!(saved["auth"].as_str().unwrap().contains("rt-ub-2"));

        e.switch(&b).unwrap();
        let live = std::fs::read_to_string(e.paths.codex_auth()).unwrap();
        assert!(live.contains("rt-ub-2"));
    }

    #[test]
    fn tokens_rotated_during_a_switch_are_kept() {
        let (_d, mut e) = sandbox();
        put_codex_live(&e, &fake_codex_auth("a@x.com", "ua", "acct-a", "plus"));
        let (a, _) = e.save_current(Provider::Codex, None).unwrap().unwrap();
        put_codex_live(&e, &fake_codex_auth("b@x.com", "ub", "acct-b", "pro"));
        let (b, _) = e.save_current(Provider::Codex, None).unwrap().unwrap();

        let mut sw = e.begin_switch(&a).unwrap();
        e.run_stage(&mut sw, Stage::Save).unwrap();
        // Codex refreshes b's login while the switch is under way.
        let rotated = fake_codex_auth("b@x.com", "ub", "acct-b", "pro").replace("rt-ub", "rt-ub-late");
        put_codex_live(&e, &rotated);
        for stage in [Stage::Load, Stage::Swap, Stage::Verify] {
            e.run_stage(&mut sw, stage).unwrap();
        }
        drop(sw);
        let saved = e.snapshot(&b).unwrap().unwrap();
        assert!(saved["auth"].as_str().unwrap().contains("rt-ub-late"), "the newer tokens were saved");
        assert_eq!(e.active_id(Provider::Codex).as_deref(), Some(a.as_str()));
    }

    #[test]
    fn unknown_live_account_is_adopted_not_lost() {
        let (_d, mut e) = sandbox();
        put_codex_live(&e, &fake_codex_auth("a@x.com", "ua", "acct-a", "plus"));
        let (a, _) = e.save_current(Provider::Codex, None).unwrap().unwrap();
        // Someone logs in with `codex login` outside of accountant.
        put_codex_live(&e, &fake_codex_auth("new@x.com", "un", "acct-n", "plus"));
        let sw = e.switch(&a).unwrap();
        let adopted = sw.from.expect("adopted profile");
        assert_eq!(e.profile(&adopted).unwrap().email.as_deref(), Some("new@x.com"));
        assert_eq!(e.registry.of(Provider::Codex).len(), 2);
    }

    #[test]
    fn claude_switch_patches_global_config_and_keeps_other_keys() {
        let (_d, mut e) = sandbox();
        let write = |e: &Engine, s: &Snapshot| providers::write_live(Provider::Claude, &e.paths, s).unwrap();
        std::fs::write(&e.paths.claude_json, r#"{"numStartups": 7, "projects": {"/x": {}}}"#).unwrap();

        write(&e, &fake_claude("w@corp.com", "u1", "o1", "one"));
        let (work, _) = e.save_current(Provider::Claude, Some("work")).unwrap().unwrap();
        write(&e, &fake_claude("me@home.com", "u2", "o2", "two"));
        let (home, _) = e.save_current(Provider::Claude, Some("home")).unwrap().unwrap();

        e.switch(&work).unwrap();
        let global: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&e.paths.claude_json).unwrap()).unwrap();
        assert_eq!(global["oauthAccount"]["emailAddress"], "w@corp.com");
        assert_eq!(global["numStartups"], 7);
        assert_eq!(global["projects"], json!({"/x": {}}));
        let creds = std::fs::read_to_string(e.paths.claude_credentials_file()).unwrap();
        assert!(creds.contains("rt-one"));
        assert_eq!(e.active_id(Provider::Claude).as_deref(), Some(work.as_str()));

        let sw = e.switch(&work).unwrap();
        assert!(sw.already_active);
        e.switch(&home).unwrap();
        assert_eq!(e.active_id(Provider::Claude).as_deref(), Some(home.as_str()));
    }

    #[test]
    fn stale_account_metadata_does_not_mislabel_credentials() {
        let (_d, mut e) = sandbox();
        let write = |e: &Engine, s: &Snapshot| providers::write_live(Provider::Claude, &e.paths, s).unwrap();
        write(&e, &fake_claude("a@x.com", "ua", "o", "aaa"));
        let (a, _) = e.save_current(Provider::Claude, Some("a")).unwrap().unwrap();
        write(&e, &fake_claude("b@x.com", "ub", "o", "bbb"));
        let (b, _) = e.save_current(Provider::Claude, Some("b")).unwrap().unwrap();

        // Switch to a, then an old session of b rewrites only the account metadata.
        e.switch(&a).unwrap();
        let mut global: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&e.paths.claude_json).unwrap()).unwrap();
        global["oauthAccount"] = fake_claude("b@x.com", "ub", "o", "x")["oauthAccount"].clone();
        std::fs::write(&e.paths.claude_json, global.to_string()).unwrap();

        // The live credentials are still a's: a is active, and syncing back
        // must not store a's tokens under b.
        assert_eq!(e.active_id(Provider::Claude).as_deref(), Some(a.as_str()));
        e.sync_back(Provider::Claude).unwrap();
        let b_saved = e.snapshot(&b).unwrap().unwrap();
        assert!(b_saved["credentials"].as_str().unwrap().contains("rt-bbb"));
        // Switching to b and back still works.
        e.switch(&b).unwrap();
        e.switch(&a).unwrap();
        assert_eq!(e.active_id(Provider::Claude).as_deref(), Some(a.as_str()));
    }

    #[test]
    fn missing_secret_is_a_clear_error() {
        let (_d, mut e) = sandbox();
        put_codex_live(&e, &fake_codex_auth("a@x.com", "ua", "acct-a", "plus"));
        let (a, _) = e.save_current(Provider::Codex, None).unwrap().unwrap();
        e.vault.delete(&format!("profile:{a}")).unwrap();
        std::fs::remove_file(e.paths.codex_auth()).unwrap();
        let err = e.switch(&a).err().unwrap().to_string();
        assert!(err.contains("sign in again"), "{err}");
    }

    #[test]
    fn totp_roundtrip_and_remove_cleans_vault() {
        let (_d, mut e) = sandbox();
        put_codex_live(&e, &fake_codex_auth("a@x.com", "ua", "acct-a", "plus"));
        let (a, _) = e.save_current(Provider::Codex, None).unwrap().unwrap();
        e.set_totp(&a, Some("JBSW Y3DP EHPK 3PXP")).unwrap();
        assert!(e.totp(&a).unwrap().is_some());
        e.remove(&a).unwrap();
        assert!(e.vault.get(&format!("totp:{a}")).unwrap().is_none());
        assert!(e.vault.get(&format!("profile:{a}")).unwrap().is_none());
    }
}
