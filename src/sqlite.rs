//! Rows of another app's SQLite database, read and written byte-for-byte.
//!
//! Goes through the `sqlite3` shell (always present on macOS) instead of a
//! linked SQLite. SQL travels on stdin and every value as a hex literal, so no
//! secret ever reaches the process list, and nothing needs quoting.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// How long to wait for an app that is writing the database.
const BUSY_TIMEOUT_MS: u32 = 5000;

/// One column of one row, exactly as stored: its SQLite type and the hex of
/// its bytes (the text form, for numbers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    #[serde(rename = "t")]
    pub kind: String,
    #[serde(rename = "h", default, skip_serializing_if = "String::is_empty")]
    pub hex: String,
}

impl Cell {
    /// The value as UTF-8 text (numbers in their text form).
    pub fn as_text(&self) -> Option<String> {
        if self.kind == "null" {
            return None;
        }
        String::from_utf8(hex::decode(&self.hex).ok()?).ok()
    }

    fn literal(&self) -> String {
        match self.kind.as_str() {
            "null" => "NULL".into(),
            "integer" => format!("CAST(CAST(X'{}' AS TEXT) AS INTEGER)", self.hex),
            "real" => format!("CAST(CAST(X'{}' AS TEXT) AS REAL)", self.hex),
            "blob" => format!("X'{}'", self.hex),
            _ => format!("CAST(X'{}' AS TEXT)", self.hex),
        }
    }
}

/// A row as column name → cell, in the table's column order.
pub type Row = serde_json::Map<String, Value>;

fn bin() -> String {
    std::env::var("ACCOUNTANT_SQLITE3").unwrap_or_else(|_| {
        if Path::new("/usr/bin/sqlite3").exists() { "/usr/bin/sqlite3".into() } else { "sqlite3".into() }
    })
}

/// Run `sql` against `db` and return the rows `-json` prints.
fn run(db: &Path, sql: &str, readonly: bool) -> Result<Vec<serde_json::Map<String, Value>>> {
    let mut cmd = Command::new(bin());
    if readonly {
        cmd.arg("-readonly");
    }
    let mut child = cmd
        .args(["-batch", "-bail", "-json"])
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running sqlite3")?;
    {
        let mut stdin = child.stdin.take().context("sqlite3 stdin")?;
        stdin.write_all(format!(".timeout {BUSY_TIMEOUT_MS}\n{sql}\n").as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{} ({}): {}", db.display(), "sqlite3", String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = String::from_utf8(out.stdout).context("sqlite3 printed something that is not UTF-8")?;
    let mut rows = vec![];
    // One JSON array per statement that returned rows.
    for chunk in serde_json::Deserializer::from_str(&text).into_iter::<Vec<serde_json::Map<String, Value>>>()
    {
        rows.extend(chunk.context("reading sqlite3 output")?);
    }
    Ok(rows)
}

fn hex_text(s: &str) -> String {
    format!("CAST(X'{}' AS TEXT)", hex::encode(s.as_bytes()))
}

/// Whether `db` exists and has `table`.
pub fn has_table(db: &Path, table: &str) -> Result<bool> {
    if !db.exists() {
        return Ok(false);
    }
    let rows = run(
        db,
        &format!(
            "SELECT count(*) AS n FROM sqlite_master WHERE type = 'table' AND name = {};",
            hex_text(table)
        ),
        true,
    )?;
    Ok(rows.first().and_then(|r| r.get("n")).and_then(Value::as_i64).unwrap_or(0) > 0)
}

fn columns(db: &Path, table: &str) -> Result<Vec<String>> {
    let rows =
        run(db, &format!("SELECT name FROM pragma_table_info({}) ORDER BY cid;", hex_text(table)), true)?;
    let cols: Vec<String> =
        rows.iter().filter_map(|r| r.get("name").and_then(Value::as_str).map(String::from)).collect();
    if cols.is_empty() {
        bail!("{} has no table {table}", db.display());
    }
    Ok(cols)
}

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Every row of `table` matching `filter` (an SQL condition built by the
/// caller from constants), oldest first.
pub fn rows(db: &Path, table: &str, filter: &str) -> Result<Vec<Row>> {
    let cols = columns(db, table)?;
    let select = cols
        .iter()
        .enumerate()
        .map(|(i, c)| format!("typeof({0}) AS t{i}, hex({0}) AS h{i}", ident(c)))
        .collect::<Vec<_>>()
        .join(", ");
    let raw =
        run(db, &format!("SELECT {select} FROM {} WHERE {filter} ORDER BY rowid;", ident(table)), true)?;
    let mut out = vec![];
    for r in raw {
        let mut row = Row::new();
        for (i, c) in cols.iter().enumerate() {
            let cell = Cell {
                kind: r.get(&format!("t{i}")).and_then(Value::as_str).unwrap_or("null").to_string(),
                hex: r.get(&format!("h{i}")).and_then(Value::as_str).unwrap_or("").to_string(),
            };
            row.insert(c.clone(), serde_json::to_value(cell)?);
        }
        out.push(row);
    }
    Ok(out)
}

/// Replace the rows of `table` matching `filter` with `rows`, in one
/// transaction: either all of them land or none do.
pub fn replace(db: &Path, table: &str, filter: &str, rows: &[Row]) -> Result<()> {
    let mut sql = format!("BEGIN IMMEDIATE;\nDELETE FROM {} WHERE {filter};\n", ident(table));
    for row in rows {
        let mut names = vec![];
        let mut values = vec![];
        for (col, v) in row {
            let cell: Cell =
                serde_json::from_value(v.clone()).with_context(|| format!("saved column {col}"))?;
            names.push(ident(col));
            values.push(cell.literal());
        }
        sql.push_str(&format!(
            "INSERT OR REPLACE INTO {} ({}) VALUES ({});\n",
            ident(table),
            names.join(", "),
            values.join(", ")
        ));
    }
    sql.push_str("COMMIT;\n");
    run(db, &sql, false)?;
    Ok(())
}

/// A VS Code–style key/value store (`ItemTable` in `state.vscdb`).
pub mod kv {
    use super::*;

    /// The keys among `keys` that exist, with their cells.
    pub fn get(db: &Path, keys: &[&str]) -> Result<Vec<Row>> {
        if !db.exists() {
            return Ok(vec![]);
        }
        let list = keys.iter().map(|k| hex_text(k)).collect::<Vec<_>>().join(", ");
        rows(db, "ItemTable", &format!("key IN ({list})"))
    }

    /// Every key starting with `prefix`.
    pub fn get_prefix(db: &Path, prefix: &str) -> Result<Vec<Row>> {
        if !db.exists() {
            return Ok(vec![]);
        }
        rows(db, "ItemTable", &prefix_filter(prefix))
    }

    pub fn prefix_filter(prefix: &str) -> String {
        format!("substr(key, 1, {}) = {}", prefix.chars().count(), hex_text(prefix))
    }

    pub fn keys_filter(keys: &[&str]) -> String {
        format!("key IN ({})", keys.iter().map(|k| hex_text(k)).collect::<Vec<_>>().join(", "))
    }

    /// A row's key and its value as text.
    pub fn entry(row: &Row) -> Option<(String, String)> {
        let cell = |name: &str| -> Option<String> {
            serde_json::from_value::<Cell>(row.get(name)?.clone()).ok()?.as_text()
        };
        Some((cell("key")?, cell("value")?))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh database with a VS Code `ItemTable`.
    pub fn state_db(dir: &Path) -> std::path::PathBuf {
        let db = dir.join("state.vscdb");
        run(&db, "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);", false).unwrap();
        db
    }

    pub fn exec(db: &Path, sql: &str) {
        run(db, sql, false).unwrap();
    }

    /// OpenCode 2's `credential` table, as it ships.
    pub fn opencode_db(db: &Path, rows: &[(&str, &str, &str)]) {
        exec(
            db,
            "CREATE TABLE IF NOT EXISTS credential (id text PRIMARY KEY, integration_id text, label text NOT NULL, \
             value text NOT NULL, connector_id text, method_id text, active integer, time_created integer NOT NULL, \
             time_updated integer NOT NULL); DELETE FROM credential;",
        );
        for (id, integration, value) in rows {
            exec(
                db,
                &format!(
                    "INSERT INTO credential VALUES ({}, {}, 'OAuth', {}, NULL, NULL, NULL, 1, 2);",
                    hex_text(id),
                    hex_text(integration),
                    hex_text(value)
                ),
            );
        }
    }

    pub fn put(db: &Path, key: &str, value: &str) {
        run(db, &format!("INSERT INTO ItemTable VALUES ({}, {});", hex_text(key), hex_text(value)), false)
            .unwrap();
    }

    #[test]
    fn rows_round_trip_with_their_types() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        run(&db, "CREATE TABLE c (id TEXT PRIMARY KEY, n INTEGER, v TEXT, b BLOB, x REAL);", false).unwrap();
        run(&db, "INSERT INTO c VALUES ('a', 42, 'it''s \"quoted\" ✓', X'00ff', 1.5), ('b', NULL, '', NULL, NULL);", false)
            .unwrap();
        let before = rows(&db, "c", "1").unwrap();
        assert_eq!(before.len(), 2);
        replace(&db, "c", "1", &[]).unwrap();
        assert!(rows(&db, "c", "1").unwrap().is_empty());
        replace(&db, "c", "1", &before).unwrap();
        assert_eq!(rows(&db, "c", "1").unwrap(), before);
        let typed =
            run(&db, "SELECT typeof(n) AS a, typeof(b) AS b, n + 1 AS m FROM c WHERE id = 'a';", true)
                .unwrap();
        assert_eq!(typed[0]["a"], "integer");
        assert_eq!(typed[0]["b"], "blob");
        assert_eq!(typed[0]["m"], 43);
    }

    #[test]
    fn key_value_rows_by_prefix_and_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let db = state_db(dir.path());
        put(&db, "cursorAuth/accessToken", "tok");
        put(&db, "cursorAuth/cachedEmail", "me@x.com");
        put(&db, "cursorAuthX", "not mine");
        put(&db, "other", "y");
        let mine = kv::get_prefix(&db, "cursorAuth/").unwrap();
        let entries: Vec<_> = mine.iter().filter_map(kv::entry).collect();
        assert_eq!(
            entries,
            vec![
                ("cursorAuth/accessToken".to_string(), "tok".to_string()),
                ("cursorAuth/cachedEmail".to_string(), "me@x.com".to_string())
            ]
        );
        assert_eq!(kv::get(&db, &["other", "missing"]).unwrap().len(), 1);
        assert!(has_table(&db, "ItemTable").unwrap());
        assert!(!has_table(&db, "credential").unwrap());
        assert!(!has_table(&dir.path().join("nope.db"), "ItemTable").unwrap());
    }
}
