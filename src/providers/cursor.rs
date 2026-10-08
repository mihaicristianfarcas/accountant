//! Cursor: the desktop app and `cursor-agent`.
//!
//! On macOS both keep the session in two Keychain items, `cursor-access-token`
//! and `cursor-refresh-token` (account `cursor-user`). The app also keeps
//! `cursorAuth/*` rows in its `state.vscdb` (the cached email and plan, and in
//! older versions the tokens themselves). `cursor-agent` falls back to
//! `~/.cursor/auth.json` where there is no Keychain. A snapshot holds all
//! three as stored.

use super::*;
use crate::sqlite;

const ACCOUNT: &str = "cursor-user";
pub const ACCESS: Slot = Slot::new("cursor-access-token", ACCOUNT);
pub const REFRESH: Slot = Slot::new("cursor-refresh-token", ACCOUNT);
pub const APP: &str = "Cursor";
const IDE_PREFIX: &str = "cursorAuth/";

pub fn agent_file(paths: &Paths) -> PathBuf {
    paths.cursor_agent_dir.join("auth.json")
}

pub fn read_live(paths: &Paths) -> Result<Option<Snapshot>> {
    let access = ACCESS.get(paths)?;
    let refresh = REFRESH.get(paths)?;
    let agent = fsutil::read_optional(&agent_file(paths))?.filter(|s| !s.trim().is_empty());
    let ide = sqlite::kv::get_prefix(&paths.vscode_state_db(APP), IDE_PREFIX)?;
    let snap = json!({
        "keychain": { "access": access, "refresh": refresh },
        "agent": agent,
        "ide": ide,
        // Not restored: who cursor-agent says is signed in, for naming.
        "email": agent_email(paths),
    });
    Ok(token(&snap).is_some().then_some(snap))
}

/// `cursor-agent` notes the signed-in user in its CLI config.
fn agent_email(paths: &Paths) -> Option<String> {
    let text = fsutil::read_optional(&paths.cursor_agent_dir.join("cli-config.json")).ok()??;
    let v: Value = serde_json::from_str(&text).ok()?;
    str_at(&v, &["authInfo", "email"]).map(str::to_string)
}

pub fn write_live(paths: &Paths, snap: &Snapshot) -> Result<()> {
    if token(snap).is_none() {
        bail!("saved Cursor login has no token");
    }
    // The app's database first: it is the one that can be busy.
    let db = paths.vscode_state_db(APP);
    if db.exists() {
        let rows: Vec<sqlite::Row> =
            serde_json::from_value(snap.get("ide").cloned().unwrap_or_else(|| json!([])))
                .context("saved Cursor login is corrupt")?;
        sqlite::replace(&db, "ItemTable", &sqlite::kv::prefix_filter(IDE_PREFIX), &rows)?;
    }
    for (slot, field) in [(ACCESS, "access"), (REFRESH, "refresh")] {
        match snap.get("keychain").and_then(|k| k.get(field)).and_then(Value::as_str) {
            Some(v) => slot.set(paths, v)?,
            None => slot.delete(paths)?,
        }
    }
    let file = agent_file(paths);
    match snap.get("agent").and_then(Value::as_str) {
        Some(text) => fsutil::write_secret(&file, text.as_bytes())?,
        None => match std::fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        },
    }
    Ok(())
}

fn ide_value(snap: &Snapshot, key: &str) -> Option<String> {
    snap.get("ide")?
        .as_array()?
        .iter()
        .filter_map(|r| sqlite::kv::entry(r.as_object()?))
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
}

fn agent_value(snap: &Snapshot, field: &str) -> Option<String> {
    let v: Value = serde_json::from_str(snap.get("agent")?.as_str()?).ok()?;
    str_at(&v, &[field]).map(str::to_string)
}

fn keychain_value(snap: &Snapshot, field: &str) -> Option<String> {
    str_at(snap, &["keychain", field]).map(str::to_string)
}

/// The access token, wherever this login keeps it.
fn token(snap: &Snapshot) -> Option<String> {
    keychain_value(snap, "access")
        .or_else(|| agent_value(snap, "accessToken"))
        .or_else(|| ide_value(snap, "cursorAuth/accessToken"))
}

pub fn identity(snap: &Snapshot) -> Option<Identity> {
    let token = token(snap)?;
    // Cursor's tokens are JWTs whose subject is the user ("auth0|user_…").
    let key = match jwt_claims(&token).as_ref().and_then(|c| str_at(c, &["sub"])) {
        Some(sub) => format!("cursor:{sub}"),
        None => format!("cursor:token:{}", short_hash(&secret(snap).unwrap_or(token))),
    };
    Some(Identity {
        key,
        email: ide_value(snap, "cursorAuth/cachedEmail")
            .or_else(|| str_at(snap, &["email"]).map(str::to_string)),
        plan: ide_value(snap, "cursorAuth/stripeMembershipType"),
        ..Default::default()
    })
}

pub fn secret(snap: &Snapshot) -> Option<String> {
    keychain_value(snap, "refresh")
        .or_else(|| agent_value(snap, "refreshToken"))
        .or_else(|| ide_value(snap, "cursorAuth/refreshToken"))
        .or_else(|| token(snap))
}
