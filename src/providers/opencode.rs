//! OpenCode: one login per upstream integration (OpenAI, Anthropic, GitHub
//! Copilot, API keys…), all of them together making up an "account".
//!
//! OpenCode 2 keeps them as the rows of the `credential` table in
//! `opencode.db`; OpenCode 1 kept them in `auth.json`. A snapshot holds
//! whichever the installed version uses, row for row, byte for byte.

use super::*;
use crate::sqlite;

const TABLE: &str = "credential";

pub fn read_live(paths: &Paths) -> Result<Option<Snapshot>> {
    let db = paths.opencode_db();
    if sqlite::has_table(&db, TABLE)? {
        let rows = sqlite::rows(&db, TABLE, "1")?;
        return Ok((!rows.is_empty()).then(|| json!({ "credentials": rows })));
    }
    let Some(text) = fsutil::read_optional(&paths.opencode_auth())? else {
        return Ok(None);
    };
    let v: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", paths.opencode_auth().display()))?;
    Ok(v.as_object().is_some_and(|o| !o.is_empty()).then(|| json!({ "auth": text })))
}

pub fn write_live(paths: &Paths, snap: &Snapshot) -> Result<()> {
    let db = paths.opencode_db();
    let current = sqlite::has_table(&db, TABLE)?;
    if let Some(rows) = snap.get("credentials") {
        if !current {
            bail!("this OpenCode login was saved from OpenCode 2 — update OpenCode, then switch again");
        }
        let rows: Vec<sqlite::Row> =
            serde_json::from_value(rows.clone()).context("saved OpenCode login is corrupt")?;
        return sqlite::replace(&db, TABLE, "1", &rows);
    }
    let auth = snap.get("auth").and_then(Value::as_str).context("saved OpenCode login is empty")?;
    if current {
        bail!(
            "this OpenCode login was saved before OpenCode 2 moved logins into its database — sign in to it again"
        );
    }
    serde_json::from_str::<Value>(auth).context("saved OpenCode login is corrupt")?;
    fsutil::write_secret(&paths.opencode_auth(), auth.as_bytes())
}

/// Each integration's credential: (integration id, credential JSON).
fn entries(snap: &Snapshot) -> Vec<(String, Value)> {
    if let Some(rows) = snap.get("credentials").and_then(Value::as_array) {
        return rows
            .iter()
            .filter_map(|r| {
                let cell =
                    |c: &str| serde_json::from_value::<sqlite::Cell>(r.get(c)?.clone()).ok()?.as_text();
                let id = cell("integration_id")
                    .or_else(|| cell("connector_id"))
                    .unwrap_or_else(|| "credential".into());
                Some((id, serde_json::from_str(&cell("value")?).ok()?))
            })
            .collect();
    }
    snap.get("auth")
        .and_then(Value::as_str)
        .and_then(|a| serde_json::from_str::<serde_json::Map<String, Value>>(a).ok())
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

pub fn identity(snap: &Snapshot) -> Option<Identity> {
    let mut parts = vec![];
    let mut names = vec![];
    let mut email = None;
    let mut weak = false;
    for (integration, cred) in entries(snap) {
        let part = match str_at(&cred, &["type"]).unwrap_or("") {
            "api" => format!("{integration}:key:{}", short_hash(str_at(&cred, &["key"]).unwrap_or(""))),
            "oauth" => {
                // OpenAI's access token is a JWT that names the ChatGPT user.
                let claims = str_at(&cred, &["access"]).and_then(jwt_claims);
                let user = claims.as_ref().and_then(|c| {
                    str_at(c, &["https://api.openai.com/auth", "chatgpt_user_id"])
                        .or_else(|| str_at(c, &["sub"]))
                });
                if email.is_none() {
                    email = claims
                        .as_ref()
                        .and_then(|c| {
                            str_at(c, &["https://api.openai.com/profile", "email"])
                                .or_else(|| str_at(c, &["email"]))
                        })
                        .map(str::to_string);
                }
                let account =
                    str_at(&cred, &["metadata", "accountID"]).or_else(|| str_at(&cred, &["accountId"]));
                match (user, account) {
                    (Some(u), a) => format!("{integration}:{u}:{}", a.unwrap_or("-")),
                    (None, Some(a)) => format!("{integration}:{a}"),
                    // A GitHub token does not rotate, so it identifies the account.
                    (None, None) if integration.contains("copilot") || integration.contains("github") => {
                        format!("{integration}:{}", short_hash(str_at(&cred, &["refresh"]).unwrap_or("")))
                    }
                    // Anthropic's tokens rotate and name no one.
                    (None, None) => {
                        weak = true;
                        format!("{integration}:oauth")
                    }
                }
            }
            _ => {
                weak = true;
                integration.clone()
            }
        };
        parts.push(part);
        names.push(integration);
    }
    if parts.is_empty() {
        return None;
    }
    parts.sort();
    names.sort();
    names.dedup();
    Some(Identity {
        key: format!("opencode:{}", parts.join("+")),
        email,
        plan: Some(names.join(" + ")),
        weak,
        ..Default::default()
    })
}

/// Every refresh token and key, so any rotation changes the fingerprint.
pub fn secret(snap: &Snapshot) -> Option<String> {
    let mut all: Vec<String> = entries(snap)
        .into_iter()
        .filter_map(|(i, c)| {
            str_at(&c, &["refresh"]).or_else(|| str_at(&c, &["key"])).map(|s| format!("{i}:{s}"))
        })
        .collect();
    all.sort();
    (!all.is_empty()).then(|| all.join("\n"))
}
