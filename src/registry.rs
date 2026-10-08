//! Non-secret profile metadata (`profiles.json`). Secrets live in the vault.

use crate::fsutil;
use crate::providers::{Identity, Provider};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub provider: Provider,
    pub name: String,
    /// Identity key (see [`Identity::key`]); matches a live login to a profile.
    pub identity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_at: Option<DateTime<Utc>>,
    /// Last time we switched *to* this account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_at: Option<DateTime<Utc>>,
    /// Last time we switched *away* from this account (its limits started
    /// cooling down then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_at: Option<DateTime<Utc>>,
    /// A TOTP secret is stored in the vault for this account.
    #[serde(default)]
    pub totp: bool,
    /// The saved session was rejected; a browser sign-in is needed.
    #[serde(default)]
    pub needs_login: bool,
    /// Truncated hash of the saved refresh token. Lets us recognise the live
    /// login by its credentials even if the account metadata next to it was
    /// rewritten by a stale session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

impl Profile {
    pub fn vault_key(&self) -> String {
        format!("profile:{}", self.id)
    }

    pub fn totp_key(&self) -> String {
        format!("totp:{}", self.id)
    }

    pub fn apply_identity(&mut self, id: &Identity) {
        self.identity = id.key.clone();
        if id.email.is_some() {
            self.email = id.email.clone();
        }
        if id.org.is_some() {
            self.org = id.org.clone();
        }
        if id.plan.is_some() {
            self.plan = id.plan.clone();
        }
    }

    /// The name as displayed. In hide-emails mode a name that was derived
    /// from the email (the default, e.g. "mihai" for mihai@…) is masked too,
    /// since it would give the address away.
    pub fn shown_name(&self) -> std::borrow::Cow<'_, str> {
        if crate::privacy::enabled() && self.name_from_email() {
            std::borrow::Cow::Owned(crate::privacy::mask(&self.name))
        } else {
            std::borrow::Cow::Borrowed(&self.name)
        }
    }

    fn name_from_email(&self) -> bool {
        let Some(local) = self.email.as_deref().and_then(|e| e.split('@').next()) else {
            return false;
        };
        let (local, name) = (local.to_lowercase(), self.name.to_lowercase());
        !local.is_empty() && (name.starts_with(&local) || (name.len() >= 3 && local.starts_with(&name)))
    }

    /// `name · provider`, for messages.
    pub fn display(&self) -> String {
        format!("{} · {}", self.shown_name(), self.provider.label())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

impl Registry {
    pub fn load(path: &Path) -> Result<Self> {
        match fsutil::read_optional(path)? {
            None => Ok(Registry::default()),
            Some(text) => {
                serde_json::from_str(&text).with_context(|| format!("{} is corrupt", path.display()))
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        fsutil::write_atomic(path, text.as_bytes(), 0o600)
    }

    /// Profiles in display order: grouped by provider, insertion order inside.
    pub fn ordered(&self) -> Vec<&Profile> {
        let mut out = Vec::with_capacity(self.profiles.len());
        for p in Provider::ALL {
            out.extend(self.profiles.iter().filter(|x| x.provider == p));
        }
        out
    }

    pub fn of(&self, provider: Provider) -> Vec<&Profile> {
        self.profiles.iter().filter(|p| p.provider == provider).collect()
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.id == id)
    }

    /// The profile an identity key belongs to. A weak key (see
    /// [`Identity::weak`]) can match several; the most recently live wins,
    /// since a login whose tokens rotated is the one that was live.
    pub fn by_identity(&self, provider: Provider, key: &str) -> Option<&Profile> {
        self.profiles
            .iter()
            .filter(|p| p.provider == provider && p.identity == key)
            .max_by_key(|p| [p.used_at, p.saved_at, Some(p.created_at)].into_iter().flatten().max())
    }

    /// Resolve a user query: 1-based index, id, name, or email (case-insensitive).
    pub fn resolve(&self, query: &str, provider: Option<Provider>) -> Vec<&Profile> {
        let q = query.trim().to_lowercase();
        let pool: Vec<&Profile> =
            self.ordered().into_iter().filter(|p| provider.is_none_or(|pr| p.provider == pr)).collect();
        if let Ok(n) = q.parse::<usize>() {
            // Indices are global (as shown in `ls` and the TUI).
            let all = self.ordered();
            return all
                .get(n.wrapping_sub(1))
                .filter(|p| provider.is_none_or(|pr| p.provider == pr))
                .map(|p| vec![*p])
                .unwrap_or_default();
        }
        let exact: Vec<&Profile> = pool
            .iter()
            .copied()
            .filter(|p| {
                p.id == q
                    || p.name.to_lowercase() == q
                    || p.email.as_deref().is_some_and(|e| e.to_lowercase() == q)
            })
            .collect();
        if !exact.is_empty() {
            return exact;
        }
        pool.into_iter()
            .filter(|p| {
                p.name.to_lowercase().starts_with(&q)
                    || p.email.as_deref().is_some_and(|e| e.to_lowercase().starts_with(&q))
            })
            .collect()
    }

    pub fn name_taken(&self, provider: Provider, name: &str, except: Option<&str>) -> bool {
        self.profiles.iter().any(|p| {
            p.provider == provider && p.name.eq_ignore_ascii_case(name) && except.is_none_or(|id| p.id != id)
        })
    }

    /// A friendly unique name: the email's local part, the username, or
    /// "account".
    pub fn suggest_name(&self, provider: Provider, id: &Identity) -> String {
        let base = id
            .email
            .as_deref()
            .and_then(|e| e.split('@').next())
            .or(id.handle.as_deref())
            .filter(|s| !s.is_empty())
            .map(sanitize_name)
            .unwrap_or_else(|| "account".into());
        let mut name = base.clone();
        let mut n = 2;
        while self.name_taken(provider, &name, None) {
            name = format!("{base}{n}");
            n += 1;
        }
        name
    }

    pub fn new_id(&self, provider: Provider) -> String {
        loop {
            let mut b = [0u8; 3];
            getrandom::fill(&mut b).expect("system RNG");
            let id = format!("{}-{}", &provider.slug()[..2], hex::encode(b));
            if self.get(&id).is_none() {
                return id;
            }
        }
    }
}

pub fn sanitize_name(s: &str) -> String {
    let cleaned: String = s
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .take(24)
        .collect();
    if cleaned.is_empty() { "account".into() } else { cleaned }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(reg: &Registry, provider: Provider, name: &str, email: &str) -> Profile {
        Profile {
            id: reg.new_id(provider),
            provider,
            name: name.into(),
            identity: format!("{}:{email}", provider.slug()),
            email: Some(email.into()),
            org: None,
            plan: None,
            created_at: Utc::now(),
            saved_at: None,
            used_at: None,
            left_at: None,
            totp: false,
            needs_login: false,
            fingerprint: None,
        }
    }

    #[test]
    fn resolve_by_index_name_email_prefix() {
        let mut reg = Registry::default();
        // Inserted out of provider order on purpose.
        let c1 = profile(&reg, Provider::Codex, "main", "me@open.ai");
        reg.profiles.push(c1);
        let a1 = profile(&reg, Provider::Claude, "work", "w@corp.com");
        reg.profiles.push(a1);
        let a2 = profile(&reg, Provider::Claude, "personal", "me@icloud.com");
        reg.profiles.push(a2);

        // Display order puts Claude first.
        assert_eq!(reg.resolve("1", None)[0].name, "work");
        assert_eq!(reg.resolve("3", None)[0].name, "main");
        assert!(reg.resolve("3", Some(Provider::Claude)).is_empty());
        assert_eq!(reg.resolve("PERS", None)[0].name, "personal");
        assert_eq!(reg.resolve("w@corp.com", None)[0].name, "work");
        assert_eq!(reg.resolve("me", None).len(), 2, "prefix of two emails");
        assert!(reg.resolve("nope", None).is_empty());
    }

    #[test]
    fn derived_names_are_masked_only_in_hide_mode() {
        let reg = Registry::default();
        let mut p = profile(&reg, Provider::Claude, "mihai2", "mihai@icloud.com");
        crate::privacy::set(true);
        assert_eq!(p.shown_name(), "mi•••");
        p.name = "work".into();
        assert_eq!(p.shown_name(), "work", "a chosen name stays readable");
        crate::privacy::set(false);
        p.name = "mihai".into();
        assert_eq!(p.shown_name(), "mihai");
    }

    #[test]
    fn suggested_names_are_unique() {
        let mut reg = Registry::default();
        let id = Identity { key: "k".into(), email: Some("sam@a.com".into()), ..Default::default() };
        let first = reg.suggest_name(Provider::Claude, &id);
        assert_eq!(first, "sam");
        let p = profile(&reg, Provider::Claude, &first, "sam@a.com");
        reg.profiles.push(p);
        assert_eq!(reg.suggest_name(Provider::Claude, &id), "sam2");
        // Names are per provider.
        assert_eq!(reg.suggest_name(Provider::Codex, &id), "sam");
    }
}
