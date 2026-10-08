//! Google Antigravity.
//!
//! Antigravity 2.0 and its `agy` CLI keep the Google login in one Keychain
//! item (service `gemini`, account `antigravity`, written by go-keyring). The
//! Antigravity IDE keeps it as base64 protobuf rows in its `state.vscdb`
//! (`antigravityUnifiedStateSync.*`). A snapshot holds both, as stored.

use super::*;
use crate::sqlite;

pub const KEYCHAIN: Slot = Slot::new("gemini", "antigravity");
/// The desktop apps whose `state.vscdb` holds a login.
pub const APPS: [&str; 2] = ["Antigravity", "Antigravity IDE"];
const KEYS: [&str; 3] = [
    "antigravityUnifiedStateSync.oauthToken",
    "antigravityUnifiedStateSync.userStatus",
    "antigravityUnifiedStateSync.enterprisePreferences",
];
/// The IDE caches the previous user's id here; left stale, it hides the new
/// account's chat history. Cleared on every switch.
const STALE: &str = "jetskiStateSync.agentManagerInitState";
const GO_KEYRING: &str = "go-keyring-base64:";

pub fn read_live(paths: &Paths) -> Result<Option<Snapshot>> {
    let keychain = KEYCHAIN.get(paths)?;
    let mut ide = serde_json::Map::new();
    for app in APPS {
        let rows = sqlite::kv::get(&paths.vscode_state_db(app), &KEYS)?;
        if !rows.is_empty() {
            ide.insert(app.into(), json!(rows));
        }
    }
    let signed_in = keychain.is_some() || ide_value(&Value::Object(ide.clone()), KEYS[0]).is_some();
    Ok(signed_in.then(|| json!({ "keychain": keychain, "ide": ide })))
}

pub fn write_live(paths: &Paths, snap: &Snapshot) -> Result<()> {
    let ide = snap.get("ide").cloned().unwrap_or_else(|| json!({}));
    let mut filter_keys = KEYS.to_vec();
    filter_keys.push(STALE);
    for app in APPS {
        let db = paths.vscode_state_db(app);
        if !db.exists() {
            continue;
        }
        let rows: Vec<sqlite::Row> =
            serde_json::from_value(ide.get(app).cloned().unwrap_or_else(|| json!([])))
                .context("saved Antigravity login is corrupt")?;
        sqlite::replace(&db, "ItemTable", &sqlite::kv::keys_filter(&filter_keys), &rows)?;
    }
    match snap.get("keychain").and_then(Value::as_str) {
        Some(v) => KEYCHAIN.set(paths, v),
        None => KEYCHAIN.delete(paths),
    }
}

/// A value from the IDE rows of any app.
fn ide_value(ide: &Value, key: &str) -> Option<String> {
    ide.as_object()?
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|r| sqlite::kv::entry(r.as_object()?))
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
}

/// The Keychain item's JSON (`{"token": {…}, "auth_method": …}`).
fn keychain_json(snap: &Snapshot) -> Option<Value> {
    let raw = snap.get("keychain")?.as_str()?.trim();
    let text = match raw.strip_prefix(GO_KEYRING) {
        Some(b64) => String::from_utf8(base64::engine::general_purpose::STANDARD.decode(b64).ok()?).ok()?,
        None => raw.to_string(),
    };
    serde_json::from_str(&text).ok()
}

fn email(snap: &Snapshot) -> Option<String> {
    let ide = snap.get("ide")?;
    [KEYS[1], KEYS[0]]
        .iter()
        .filter_map(|k| ide_value(ide, k))
        .find_map(|v| emails_in(v.as_bytes(), 2).into_iter().next())
}

pub fn identity(snap: &Snapshot) -> Option<Identity> {
    let email = email(snap);
    let key = match (&email, secret(snap)) {
        (Some(e), _) => format!("antigravity:{}", e.to_lowercase()),
        // Google refresh tokens do not rotate, so this one names the account.
        (None, Some(s)) => format!("antigravity:token:{}", short_hash(&s)),
        (None, None) => return None,
    };
    Some(Identity {
        key,
        email,
        plan: keychain_json(snap).and_then(|j| str_at(&j, &["auth_method"]).map(str::to_string)),
        ..Default::default()
    })
}

pub fn secret(snap: &Snapshot) -> Option<String> {
    keychain_json(snap)
        .and_then(|j| str_at(&j, &["token", "refresh_token"]).map(str::to_string))
        .or_else(|| snap.get("keychain").and_then(Value::as_str).map(str::to_string))
        .or_else(|| ide_value(snap.get("ide")?, KEYS[0]))
}
