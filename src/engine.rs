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

    /// The profile holding exactly these credentials.
    fn fingerprint_owner(&self, provider: Provider, snap: &Snapshot) -> Option<String> {
        let fp = providers::fingerprint(provider, snap)?;
        self.registry
            .profiles
            .iter()
            .find(|p| p.provider == provider && p.fingerprint.as_ref() == Some(&fp))
            .map(|p| p.id.clone())
    }

    /// The profile a login is saved under. A weak identity is matched by its
    /// exact credentials first, since its key alone can name several.
    fn existing(&self, provider: Provider, snap: &Snapshot, ident: &Identity) -> Option<String> {
        ident
            .weak
            .then(|| self.fingerprint_owner(provider, snap))
            .flatten()
            .or_else(|| self.registry.by_identity(provider, &ident.key).map(|p| p.id.clone()))
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
        let existing = self.existing(provider, snap, ident);
        self.store_as(provider, snap, ident, existing, name)
    }

    /// Store `snap` under `existing`, or a new profile when `None`.
    fn store_as(
        &mut self,
        provider: Provider,
        snap: &Snapshot,
        ident: &Identity,
        existing: Option<String>,
        name: Option<&str>,
    ) -> Result<(String, bool)> {
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
        let existing = match self.existing(provider, &snap, &ident) {
            // A weak key cannot tell accounts apart: a new name means a new one.
            Some(id)
                if ident.weak
                    && self.fingerprint_owner(provider, &snap).is_none()
                    && name.is_some_and(|n| {
                        self.registry.get(&id).is_some_and(|p| !p.name.eq_ignore_ascii_case(n))
                    }) =>
            {
                None
            }
            other => other,
        };
        let (id, created) = self.store_as(provider, &snap, &ident, existing, name)?;
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

    /// After a CLI's own sign-in command (OpenCode, Cursor, Copilot): save
    /// the login it left live. `target` is the account being signed in again,
    /// `previous` the one that was live before. Returns (profile id, created).
    pub fn finish_external_login(
        &mut self,
        provider: Provider,
        target: Option<&str>,
        previous: Option<&str>,
    ) -> Result<(String, bool)> {
        let _lock = self.lock()?;
        let (snap, ident) = self
            .live(provider)?
            .ok_or_else(|| anyhow!("no {} login was found after signing in", provider.label()))?;
        let existing = match target {
            // Signing in again, unless the login plainly names someone else.
            Some(t) if ident.weak || self.profile(t)?.identity == ident.key => Some(t.to_string()),
            // A weak key cannot tell this sign-in from an earlier account.
            None if ident.weak => self.fingerprint_owner(provider, &snap),
            _ => self.existing(provider, &snap, &ident),
        };
        let (id, created) = self.store_as(provider, &snap, &ident, existing, None)?;
        let now = Utc::now();
        if let Some(prev) = previous.filter(|p| *p != id)
            && let Some(p) = self.registry.get_mut(prev)
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

/// How many interactive sessions of the provider's CLI still hold the old
/// login and need a restart to pick up a switch. Background helpers without a
/// terminal are not counted.
pub fn sessions_to_restart(provider: Provider) -> usize {
    if !provider.sessions_keep_old_login() {
        return 0;
    }
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
        let paths = Paths::sandbox(root);
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

    // -- the newer CLIs -----------------------------------------------------

    use crate::providers::tests::fake_jwt;
    use crate::sqlite::tests::{exec, opencode_db, put, state_db};

    fn openai_cred(email: &str, user: &str, refresh: &str) -> String {
        let access = fake_jwt(json!({
            "https://api.openai.com/profile": { "email": email },
            "https://api.openai.com/auth": { "chatgpt_user_id": user },
        }));
        json!({ "type": "oauth", "methodID": "chatgpt-browser", "refresh": refresh, "access": access,
                "expires": 1, "metadata": { "accountID": format!("acct-{user}") } })
        .to_string()
    }

    fn anthropic_cred(refresh: &str) -> String {
        json!({ "type": "oauth", "refresh": refresh, "access": format!("sk-ant-oat-{refresh}"), "expires": 1 })
            .to_string()
    }

    #[test]
    fn opencode_switches_credential_rows() {
        let (_d, mut e) = sandbox();
        let db = e.paths.opencode_db();
        std::fs::create_dir_all(&e.paths.opencode_dir).unwrap();
        opencode_db(&db, &[("c1", "openai", &openai_cred("a@x.com", "ua", "rt-a"))]);
        let (a, _) = e.save_current(Provider::OpenCode, None).unwrap().unwrap();
        assert_eq!(e.profile(&a).unwrap().email.as_deref(), Some("a@x.com"));
        assert_eq!(e.profile(&a).unwrap().plan.as_deref(), Some("openai"));

        opencode_db(&db, &[("c2", "openai", &openai_cred("b@x.com", "ub", "rt-b"))]);
        let (b, _) = e.save_current(Provider::OpenCode, None).unwrap().unwrap();
        assert_ne!(a, b);
        // Some unrelated table OpenCode keeps next to the credentials.
        exec(&db, "CREATE TABLE session (id text); INSERT INTO session VALUES ('keep me');");

        e.switch(&a).unwrap();
        assert_eq!(e.active_id(Provider::OpenCode).as_deref(), Some(a.as_str()));
        let rows = crate::sqlite::rows(&db, "credential", "1").unwrap();
        assert_eq!(rows.len(), 1);
        let value = crate::sqlite::kv::entry(&{
            let mut r = rows[0].clone();
            r.insert("key".into(), r["id"].clone());
            r
        })
        .unwrap()
        .1;
        assert!(value.contains("rt-a"));
        assert_eq!(crate::sqlite::rows(&db, "session", "1").unwrap().len(), 1);
        e.switch(&b).unwrap();
        assert_eq!(e.active_id(Provider::OpenCode).as_deref(), Some(b.as_str()));
    }

    #[test]
    fn opencode_tells_apart_anthropic_logins_it_cannot_name() {
        let (_d, mut e) = sandbox();
        let db = e.paths.opencode_db();
        std::fs::create_dir_all(&e.paths.opencode_dir).unwrap();
        // Two sign-ins through accountant, each to an account the token can't name.
        opencode_db(&db, &[("c1", "anthropic", &anthropic_cred("rt-a"))]);
        let (a, created) = e.finish_external_login(Provider::OpenCode, None, None).unwrap();
        assert!(created);
        opencode_db(&db, &[("c2", "anthropic", &anthropic_cred("rt-b"))]);
        let (b, created) = e.finish_external_login(Provider::OpenCode, None, Some(&a)).unwrap();
        assert!(created);
        assert_ne!(a, b);

        // b is live and OpenCode rotates its refresh token: still b, not a new account.
        opencode_db(&db, &[("c2", "anthropic", &anthropic_cred("rt-b2"))]);
        assert!(matches!(e.sync_back(Provider::OpenCode).unwrap(), Synced::Saved(ref id) if *id == b));
        assert_eq!(e.registry.of(Provider::OpenCode).len(), 2);

        e.switch(&a).unwrap();
        assert!(
            crate::sqlite::rows(&db, "credential", "1").unwrap()[0]["value"]["h"]
                .as_str()
                .unwrap()
                .contains(&hex::encode_upper("rt-a"))
        );
        // Back to b: the rotated token, saved before the swap, comes back.
        e.switch(&b).unwrap();
        assert!(e.snapshot(&b).unwrap().unwrap().to_string().contains(&hex::encode_upper("rt-b2")));
        assert_eq!(e.active_id(Provider::OpenCode).as_deref(), Some(b.as_str()));
    }

    #[test]
    fn opencode_one_still_uses_auth_json() {
        let (_d, mut e) = sandbox();
        std::fs::create_dir_all(&e.paths.opencode_dir).unwrap();
        let auth = |rt: &str| {
            json!({ "anthropic": serde_json::from_str::<serde_json::Value>(&anthropic_cred(rt)).unwrap() })
                .to_string()
        };
        std::fs::write(e.paths.opencode_auth(), auth("rt-a")).unwrap();
        let (a, _) = e.save_current(Provider::OpenCode, Some("a")).unwrap().unwrap();
        std::fs::write(e.paths.opencode_auth(), auth("rt-b")).unwrap();
        let (b, created) = e.save_current(Provider::OpenCode, Some("b")).unwrap().unwrap();
        assert!(created, "a new name for a login the key can't name is a new account");
        e.switch(&a).unwrap();
        assert!(std::fs::read_to_string(e.paths.opencode_auth()).unwrap().contains("rt-a"));
        assert_ne!(a, b);
    }

    fn cursor_token(sub: &str) -> String {
        fake_jwt(json!({ "sub": sub, "iss": "https://authentication.cursor.sh", "type": "session" }))
    }

    #[test]
    fn cursor_switches_keychain_agent_file_and_app_rows() {
        let (_d, mut e) = sandbox();
        let paths = e.paths.clone();
        let app_dir = paths.vscode_state_db(crate::providers::cursor::APP);
        std::fs::create_dir_all(app_dir.parent().unwrap()).unwrap();
        let db = state_db(app_dir.parent().unwrap());
        put(&db, "workbench.colorTheme", "dark");
        let sign_in = |sub: &str, email: &str, plan: &str| {
            let (acc, refr) = (crate::providers::cursor::ACCESS, crate::providers::cursor::REFRESH);
            acc.set(&paths, &cursor_token(sub)).unwrap();
            refr.set(&paths, &format!("refresh-{sub}")).unwrap();
            put(&db, "cursorAuth/cachedEmail", email);
            put(&db, "cursorAuth/stripeMembershipType", plan);
        };
        sign_in("auth0|user_a", "a@x.com", "pro");
        let (a, _) = e.save_current(Provider::Cursor, None).unwrap().unwrap();
        let pa = e.profile(&a).unwrap();
        assert_eq!((pa.email.as_deref(), pa.plan.as_deref()), (Some("a@x.com"), Some("pro")));
        assert_eq!(pa.identity, "cursor:auth0|user_a");

        sign_in("auth0|user_b", "b@x.com", "free");
        let (b, _) = e.save_current(Provider::Cursor, None).unwrap().unwrap();
        std::fs::create_dir_all(&paths.cursor_agent_dir).unwrap();

        e.switch(&a).unwrap();
        assert_eq!(e.active_id(Provider::Cursor).as_deref(), Some(a.as_str()));
        assert_eq!(
            crate::providers::cursor::REFRESH.get(&paths).unwrap().as_deref(),
            Some("refresh-auth0|user_a")
        );
        let rows = crate::sqlite::kv::get_prefix(&db, "cursorAuth/").unwrap();
        let email =
            rows.iter().filter_map(crate::sqlite::kv::entry).find(|(k, _)| k == "cursorAuth/cachedEmail");
        assert_eq!(email.map(|(_, v)| v).as_deref(), Some("a@x.com"));
        // The app's other settings are untouched.
        assert_eq!(crate::sqlite::kv::get(&db, &["workbench.colorTheme"]).unwrap().len(), 1);
        e.switch(&b).unwrap();
        assert_eq!(e.active_id(Provider::Cursor).as_deref(), Some(b.as_str()));
    }

    #[test]
    fn copilot_switches_the_active_github_user_and_nothing_else() {
        let (_d, mut e) = sandbox();
        let cfg = e.paths.copilot_config();
        std::fs::create_dir_all(&e.paths.copilot_home).unwrap();
        let user = |login: &str| json!({ "host": "https://github.com", "login": login });
        let write = |last: &str| {
            let doc = json!({
                "banner": "never",
                "loggedInUsers": [user("octo-work"), user("octo-home")],
                "lastLoggedInUser": user(last),
                "trusted_folders": ["/src"],
            });
            std::fs::write(&cfg, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        };
        write("octo-work");
        let (work, _) = e.save_current(Provider::Copilot, None).unwrap().unwrap();
        assert_eq!(e.profile(&work).unwrap().name, "octo-work");
        write("octo-home");
        let (home, _) = e.save_current(Provider::Copilot, None).unwrap().unwrap();

        e.switch(&work).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(doc["lastLoggedInUser"]["login"], "octo-work");
        assert_eq!(doc["loggedInUsers"].as_array().unwrap().len(), 2);
        assert_eq!(doc["banner"], "never");
        assert_eq!(doc["trusted_folders"][0], "/src");
        assert_eq!(e.active_id(Provider::Copilot).as_deref(), Some(work.as_str()));
        e.switch(&home).unwrap();
        assert_eq!(e.active_id(Provider::Copilot).as_deref(), Some(home.as_str()));
        // No secret was ever stored for Copilot.
        assert!(!e.snapshot(&home).unwrap().unwrap().to_string().contains("gho_"));
    }

    #[test]
    fn copilot_keeps_the_older_field_names() {
        let (_d, mut e) = sandbox();
        std::fs::create_dir_all(&e.paths.copilot_home).unwrap();
        let cfg = e.paths.copilot_config();
        std::fs::write(
            &cfg,
            r#"{"last_logged_in_user":{"host":"https://github.com","login":"a"},"logged_in_users":[{"host":"https://github.com","login":"a"},{"host":"https://github.com","login":"b"}]}"#,
        )
        .unwrap();
        let (a, _) = e.save_current(Provider::Copilot, None).unwrap().unwrap();
        std::fs::write(
            &cfg,
            std::fs::read_to_string(&cfg).unwrap().replacen(r#""login":"a"}"#, r#""login":"b"}"#, 1),
        )
        .unwrap();
        e.save_current(Provider::Copilot, None).unwrap();
        e.switch(&a).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(doc["last_logged_in_user"]["login"], "a");
        assert!(doc.get("lastLoggedInUser").is_none());
    }

    /// The IDE's value: base64 of a protobuf that holds base64 of another.
    fn antigravity_blob(email: &str) -> String {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let inner = [b"\x12\x10".as_slice(), email.as_bytes(), b"\x1a\x04name"].concat();
        let middle = [b"\x0a\x30".as_slice(), b64.encode(&inner).as_bytes()].concat();
        b64.encode(middle)
    }

    #[test]
    fn antigravity_switches_keychain_and_ide_rows() {
        use base64::Engine as _;
        let (_d, mut e) = sandbox();
        let paths = e.paths.clone();
        let app_db = paths.vscode_state_db("Antigravity");
        std::fs::create_dir_all(app_db.parent().unwrap()).unwrap();
        let db = state_db(app_db.parent().unwrap());
        let sign_in = |email: &str, rt: &str| {
            let item = json!({ "token": { "access_token": "ya29", "token_type": "Bearer", "refresh_token": rt,
                               "expiry": "2026-01-01T00:00:00Z" }, "auth_method": "consumer" });
            let value = format!(
                "go-keyring-base64:{}",
                base64::engine::general_purpose::STANDARD.encode(item.to_string())
            );
            crate::providers::antigravity::KEYCHAIN.set(&paths, &value).unwrap();
            put(&db, "antigravityUnifiedStateSync.oauthToken", &antigravity_blob(email));
            put(&db, "antigravityUnifiedStateSync.userStatus", &antigravity_blob(email));
            put(&db, "jetskiStateSync.agentManagerInitState", "user-of-the-moment");
        };
        sign_in("a@gmail.com", "1//rt-a");
        let (a, _) = e.save_current(Provider::Antigravity, None).unwrap().unwrap();
        let pa = e.profile(&a).unwrap();
        assert_eq!(pa.email.as_deref(), Some("a@gmail.com"));
        assert_eq!(pa.plan.as_deref(), Some("consumer"));
        sign_in("b@gmail.com", "1//rt-b");
        let (b, _) = e.save_current(Provider::Antigravity, None).unwrap().unwrap();
        assert_ne!(a, b);

        e.switch(&a).unwrap();
        assert_eq!(e.active_id(Provider::Antigravity).as_deref(), Some(a.as_str()));
        assert!(crate::sqlite::kv::get(&db, &["jetskiStateSync.agentManagerInitState"]).unwrap().is_empty());
        let item = crate::providers::antigravity::KEYCHAIN.get(&paths).unwrap().unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(item.trim_start_matches("go-keyring-base64:"))
            .unwrap();
        assert!(String::from_utf8(decoded).unwrap().contains("1//rt-a"));
        e.switch(&b).unwrap();
        assert_eq!(e.active_id(Provider::Antigravity).as_deref(), Some(b.as_str()));
    }

    #[test]
    fn missing_tools_read_as_signed_out() {
        let (_d, e) = sandbox();
        for p in [Provider::OpenCode, Provider::Antigravity, Provider::Cursor, Provider::Copilot] {
            assert!(e.live(p).unwrap().is_none(), "{}", p.label());
        }
    }
}
