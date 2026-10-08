//! GitHub Copilot CLI.
//!
//! Copilot keeps a token per signed-in GitHub user (in its own Keychain items)
//! and the list of those users in `~/.copilot/config.json`, starting each new
//! session as `lastLoggedInUser`. Switching only picks that user: the tokens
//! stay exactly where Copilot put them, so a snapshot holds no secret at all.
//! Older versions spell the fields `last_logged_in_user` and
//! `logged_in_users`; whichever the file uses is kept.

use super::*;

const LAST: [&str; 2] = ["lastLoggedInUser", "last_logged_in_user"];
const USERS: [&str; 2] = ["loggedInUsers", "logged_in_users"];

fn config(paths: &Paths) -> Result<Option<serde_json::Map<String, Value>>> {
    let path = paths.copilot_config();
    let Some(text) = fsutil::read_optional(&path)? else {
        return Ok(None);
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))?;
    Ok(v.as_object().cloned())
}

/// (host, login) of a user entry.
fn user_key(u: &Value) -> Option<(String, String)> {
    let host = str_at(u, &["host"]).unwrap_or("https://github.com").trim_end_matches('/').to_string();
    Some((host, str_at(u, &["login"])?.to_string()))
}

pub fn read_live(paths: &Paths) -> Result<Option<Snapshot>> {
    let Some(cfg) = config(paths)? else {
        return Ok(None);
    };
    let user = LAST.iter().find_map(|k| cfg.get(*k)).filter(|u| user_key(u).is_some());
    Ok(user.map(|u| json!({ "user": u })))
}

pub fn write_live(paths: &Paths, snap: &Snapshot) -> Result<()> {
    let user = snap
        .get("user")
        .filter(|u| user_key(u).is_some())
        .context("saved Copilot account names no GitHub user")?;
    let target = user_key(user);
    let mut cfg = config(paths)?.unwrap_or_default();
    let last = LAST.into_iter().find(|k| cfg.contains_key(*k)).unwrap_or(LAST[0]);
    let users = USERS.into_iter().find(|k| cfg.contains_key(*k)).unwrap_or(USERS[0]);
    if let Some(list) = cfg.entry(users).or_insert_with(|| json!([])).as_array_mut()
        && !list.iter().any(|u| user_key(u) == target)
    {
        list.push(user.clone());
    }
    cfg.insert(last.into(), user.clone());
    let mut text = serde_json::to_string_pretty(&Value::Object(cfg))?;
    text.push('\n');
    fsutil::write_atomic(&paths.copilot_config(), text.as_bytes(), 0o600)
}

pub fn identity(snap: &Snapshot) -> Option<Identity> {
    let (host, login) = user_key(snap.get("user")?)?;
    let host = host.trim_start_matches("https://").trim_start_matches("http://").to_string();
    Some(Identity {
        key: format!("copilot:{host}:{login}"),
        org: (host != "github.com").then_some(host),
        handle: Some(login),
        ..Default::default()
    })
}

/// The GitHub users Copilot has a sign-in for.
pub fn signed_in_users(paths: &Paths) -> Vec<String> {
    let Ok(Some(cfg)) = config(paths) else {
        return vec![];
    };
    USERS
        .iter()
        .find_map(|k| cfg.get(*k))
        .and_then(Value::as_array)
        .map(|l| l.iter().filter_map(user_key).map(|(_, login)| login).collect())
        .unwrap_or_default()
}
