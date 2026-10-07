//! Secret storage.
//!
//! On macOS everything goes into the login Keychain through `/usr/bin/security`.
//! Using the system binary (instead of linking Security.framework) means items
//! that Claude Code created are readable without an extra "allow access" prompt,
//! and secrets are piped through stdin as hex — they never appear in argv.
//!
//! `security -i` reads each command into a 4 KiB line buffer and runs whatever
//! spills over as more commands, so every line has to stay short. accountant's
//! own values that would not fit (a Codex login carries several JWTs) are
//! stored in pieces; see [`write_value`].
//!
//! Elsewhere (or with `ACCOUNTANT_SECRETS=file`) secrets are 0600 files in a
//! 0700 directory.

use crate::fsutil;
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const SECURITY: &str = "/usr/bin/security";

pub const SERVICE: &str = "accountant";

/// Longest command line `security -i` takes whole.
const LINE_MAX: usize = 4000;

/// How long `security` may take. A locked keychain makes it wait for the
/// unlock dialog, so this leaves time to type a password, but a dialog nobody
/// answers no longer freezes accountant for good.
const SECURITY_TIMEOUT: Duration = Duration::from_secs(60);

struct Finished {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run `security` with `stdin` (if any) and wait for it at most
/// [`SECURITY_TIMEOUT`]. A child that overruns is killed and reaped.
fn run_security(args: &[&str], stdin: Option<&[u8]>) -> Result<Finished> {
    run_bounded(Command::new(SECURITY).args(args), stdin, SECURITY_TIMEOUT)
}

fn run_bounded(cmd: &mut Command, stdin: Option<&[u8]>, timeout: Duration) -> Result<Finished> {
    let mut child = cmd
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running `security` (macOS Keychain)")?;
    // Drain both pipes on their own threads so a chatty child never blocks on
    // a full pipe while we wait for it to exit.
    fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut out);
            }
            out
        })
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let written = pipe.write_all(input);
        drop(pipe);
        if let Err(e) = written {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e).context("writing to `security`");
        }
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("the Keychain did not answer — unlock the login keychain and retry");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    Ok(Finished {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// A generic-password slot in the macOS Keychain.
#[derive(Debug, Clone)]
pub struct KeychainItem {
    pub service: String,
    pub account: String,
}

impl KeychainItem {
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        KeychainItem { service: service.into(), account: account.into() }
    }

    pub fn get(&self) -> Result<Option<String>> {
        Ok(self.get_raw()?.map(decode_if_hex))
    }

    /// The value exactly as `security -w` prints it.
    fn get_raw(&self) -> Result<Option<String>> {
        let out =
            run_security(&["find-generic-password", "-s", &self.service, "-a", &self.account, "-w"], None)?;
        match out.status.code() {
            Some(0) => {}
            // errSecItemNotFound
            Some(44) => return Ok(None),
            _ => bail!(
                "keychain read failed for '{}': {}",
                self.service,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
        let mut s = String::from_utf8(out.stdout).context("keychain item is not UTF-8")?;
        while s.ends_with('\n') || s.ends_with('\r') {
            s.pop();
        }
        Ok(Some(s))
    }

    pub fn set(&self, value: &str) -> Result<()> {
        security_batch(&self.service, &[self.add_command(value)?])
    }

    /// `security -i` reads commands from stdin, so the secret never shows up
    /// in the process list. Hex avoids every quoting problem.
    fn add_command(&self, value: &str) -> Result<String> {
        let line = format!(
            "add-generic-password -U -s {} -a {} -X {}\n",
            quote(&self.service),
            quote(&self.account),
            hex::encode(value.as_bytes())
        );
        if line.len() > LINE_MAX {
            bail!(
                "keychain write failed for '{}': the value is too long for `security` ({} bytes)",
                self.service,
                value.len()
            );
        }
        Ok(line)
    }

    pub fn delete(&self) -> Result<()> {
        let out = run_security(&["delete-generic-password", "-s", &self.service, "-a", &self.account], None)?;
        match out.status.code() {
            Some(0) | Some(44) => Ok(()),
            _ => bail!("keychain delete failed for '{}'", self.service),
        }
    }
}

/// Run `add-generic-password` commands through one `security -i`.
fn security_batch(service: &str, lines: &[String]) -> Result<()> {
    let out = run_security(&["-i"], Some(lines.concat().as_bytes()))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || stderr.contains("error") || stderr.contains("unknown command") {
        bail!("keychain write failed for '{service}': {}", stderr.trim());
    }
    Ok(())
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `security -w` prints binary-looking data as hex. Our values are JSON or
/// base32, so decode only when the result is clearly a hex dump of text.
fn decode_if_hex(s: String) -> String {
    let looks_hex = s.len() >= 2
        && s.len().is_multiple_of(2)
        && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
    if looks_hex
        && let Ok(bytes) = hex::decode(&s)
        && let Ok(text) = String::from_utf8(bytes)
    {
        let t = text.trim_start();
        if t.starts_with('{') || t.starts_with('[') {
            return text;
        }
    }
    s
}

/// Where the vault keeps its items: the login Keychain, or a map in tests.
trait Slots {
    fn read(&self, account: &str) -> Result<Option<String>>;
    /// Writes every item, or fails.
    fn write(&self, items: &[(String, String)]) -> Result<()>;
    fn remove(&self, account: &str) -> Result<()>;
}

struct Keychain;

impl Slots for Keychain {
    fn read(&self, account: &str) -> Result<Option<String>> {
        KeychainItem::new(SERVICE, account).get_raw()
    }

    fn write(&self, items: &[(String, String)]) -> Result<()> {
        let lines = items
            .iter()
            .map(|(account, value)| KeychainItem::new(SERVICE, account).add_command(value))
            .collect::<Result<Vec<_>>>()?;
        security_batch(SERVICE, &lines)
    }

    fn remove(&self, account: &str) -> Result<()> {
        KeychainItem::new(SERVICE, account).delete()
    }
}

/// A value too long for one `security -i` line is stored as base64 pieces
/// `<key>#<gen>.<i>`; the item itself then holds `PIECES<gen>:<count>`.
const PIECES: &str = "accountant-pieces:v1:";
/// Bytes per piece: base64, then hex, makes a line ~2.7× this.
const PIECE_BYTES: usize = 1200;

fn piece_key(key: &str, generation: &str, i: usize) -> String {
    format!("{key}#{generation}.{i}")
}

fn pieces_header(raw: &str) -> Option<(String, usize)> {
    let (generation, count) = raw.strip_prefix(PIECES)?.split_once(':')?;
    Some((generation.to_string(), count.parse().ok()?))
}

fn read_value(slots: &impl Slots, key: &str) -> Result<Option<String>> {
    let Some(raw) = slots.read(key)? else { return Ok(None) };
    let Some((generation, count)) = pieces_header(&raw) else { return Ok(Some(decode_if_hex(raw))) };
    let mut bytes = vec![];
    for i in 0..count {
        let piece = slots
            .read(&piece_key(key, &generation, i))?
            .with_context(|| format!("keychain item '{key}' is missing piece {} of {count}", i + 1))?;
        bytes.extend(
            BASE64.decode(piece.trim()).with_context(|| format!("keychain item '{key}' is damaged"))?,
        );
    }
    Ok(Some(String::from_utf8(bytes).context("keychain item is not UTF-8")?))
}

/// New pieces are written before the header that names them, and the old ones
/// removed only after, so an interrupted write leaves the previous value.
fn write_value(slots: &impl Slots, key: &str, value: &str) -> Result<()> {
    let old = slots.read(key)?.as_deref().and_then(pieces_header);
    let mut generation = None;
    if KeychainItem::new(SERVICE, key).add_command(value).is_ok() {
        slots.write(&[(key.to_string(), value.to_string())])?;
    } else {
        let mut rnd = [0u8; 4];
        getrandom::fill(&mut rnd).context("system RNG")?;
        let g = hex::encode(rnd);
        let pieces: Vec<(String, String)> = value
            .as_bytes()
            .chunks(PIECE_BYTES)
            .enumerate()
            .map(|(i, chunk)| (piece_key(key, &g, i), BASE64.encode(chunk)))
            .collect();
        slots.write(&pieces)?;
        slots.write(&[(key.to_string(), format!("{PIECES}{g}:{}", pieces.len()))])?;
        generation = Some(g);
    }
    if let Some((old_gen, count)) = old
        && generation.as_ref() != Some(&old_gen)
    {
        for i in 0..count {
            // Leftovers are harmless; the new value is already in place.
            let _ = slots.remove(&piece_key(key, &old_gen, i));
        }
    }
    Ok(())
}

fn delete_value(slots: &impl Slots, key: &str) -> Result<()> {
    let old = slots.read(key)?.as_deref().and_then(pieces_header);
    slots.remove(key)?;
    if let Some((generation, count)) = old {
        for i in 0..count {
            slots.remove(&piece_key(key, &generation, i))?;
        }
    }
    Ok(())
}

/// accountant's own secret vault.
#[derive(Debug, Clone)]
pub enum Vault {
    Keychain,
    Files(PathBuf),
}

impl Vault {
    pub fn detect(data_dir: &std::path::Path) -> Self {
        let forced_file = std::env::var("ACCOUNTANT_SECRETS").is_ok_and(|v| v == "file");
        if cfg!(target_os = "macos") && !forced_file {
            Vault::Keychain
        } else {
            Vault::Files(data_dir.join("secrets"))
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Vault::Keychain => "macOS Keychain".into(),
            Vault::Files(dir) => format!("files in {}", dir.display()),
        }
    }

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        match self {
            Vault::Keychain => read_value(&Keychain, key),
            Vault::Files(dir) => fsutil::read_optional(&dir.join(file_name(key))),
        }
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        match self {
            Vault::Keychain => write_value(&Keychain, key, value),
            Vault::Files(dir) => {
                fsutil::private_dir(dir)?;
                fsutil::write_secret(&dir.join(file_name(key)), value.as_bytes())
            }
        }
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        match self {
            Vault::Keychain => delete_value(&Keychain, key),
            Vault::Files(dir) => match std::fs::remove_file(dir.join(file_name(key))) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            },
        }
    }
}

fn file_name(key: &str) -> String {
    key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// An in-memory keychain; `fail_write: Some(n)` fails the write after n more.
    #[derive(Default)]
    struct Map {
        items: RefCell<BTreeMap<String, String>>,
        fail_write: RefCell<Option<usize>>,
    }

    impl Slots for Map {
        fn read(&self, account: &str) -> Result<Option<String>> {
            Ok(self.items.borrow().get(account).cloned())
        }

        fn write(&self, items: &[(String, String)]) -> Result<()> {
            let mut fail = self.fail_write.borrow_mut();
            match *fail {
                Some(0) => bail!("write failed"),
                Some(n) => *fail = Some(n - 1),
                None => {}
            }
            for (k, v) in items {
                // Each one has to fit a single `security -i` line.
                KeychainItem::new(SERVICE, k).add_command(v).unwrap();
                self.items.borrow_mut().insert(k.clone(), v.clone());
            }
            Ok(())
        }

        fn remove(&self, account: &str) -> Result<()> {
            self.items.borrow_mut().remove(account);
            Ok(())
        }
    }

    /// About the size of a Codex `auth.json`.
    fn big_login(tag: &str) -> String {
        format!(
            r#"{{"id_token":"{}","access_token":"{}","tag":"{tag}"}}"#,
            "a".repeat(2600),
            "b".repeat(2400)
        )
    }

    #[test]
    fn long_values_are_stored_in_pieces_that_fit_a_line() {
        let slots = Map::default();
        let count = || slots.items.borrow().len();
        write_value(&slots, "profile:co-1", r#"{"small":true}"#).unwrap();
        assert_eq!(count(), 1);
        assert_eq!(read_value(&slots, "profile:co-1").unwrap().as_deref(), Some(r#"{"small":true}"#));

        let big = big_login("one");
        assert!(KeychainItem::new(SERVICE, "profile:co-1").add_command(&big).is_err());
        write_value(&slots, "profile:co-1", &big).unwrap();
        assert_eq!(count(), 1 + big.len().div_ceil(PIECE_BYTES));
        assert_eq!(read_value(&slots, "profile:co-1").unwrap(), Some(big.clone()));

        // Rewriting drops the previous pieces; so does going back to a short value.
        write_value(&slots, "profile:co-1", &big_login("two")).unwrap();
        assert_eq!(count(), 1 + big.len().div_ceil(PIECE_BYTES));
        assert_eq!(read_value(&slots, "profile:co-1").unwrap(), Some(big_login("two")));
        write_value(&slots, "profile:co-1", "{}").unwrap();
        assert_eq!(count(), 1);

        write_value(&slots, "profile:co-1", &big).unwrap();
        delete_value(&slots, "profile:co-1").unwrap();
        assert_eq!(count(), 0);
    }

    #[test]
    fn an_interrupted_write_keeps_the_previous_value() {
        let slots = Map::default();
        write_value(&slots, "profile:co-1", &big_login("old")).unwrap();
        // The new pieces land, then writing the header that names them fails.
        *slots.fail_write.borrow_mut() = Some(1);
        assert!(write_value(&slots, "profile:co-1", &big_login("new")).is_err());
        assert_eq!(read_value(&slots, "profile:co-1").unwrap(), Some(big_login("old")));
    }

    #[test]
    fn hex_decoding_only_for_text_payloads() {
        let json = r#"{"a":1}"#;
        assert_eq!(decode_if_hex(hex::encode(json)), json);
        // A base32 TOTP secret or a plain token must pass through untouched.
        assert_eq!(decode_if_hex("JBSWY3DPEHPK3PXP".into()), "JBSWY3DPEHPK3PXP");
        assert_eq!(decode_if_hex("deadbeef".into()), "deadbeef");
    }

    #[test]
    fn a_child_that_never_answers_is_killed_not_waited_for_forever() {
        let started = Instant::now();
        let err = run_bounded(Command::new("/bin/sleep").arg("30"), None, Duration::from_millis(200))
            .err()
            .expect("timed out");
        assert!(err.to_string().contains("unlock the login keychain"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));

        // Output larger than a pipe buffer still arrives whole.
        let out = run_bounded(
            Command::new("/bin/sh").args(["-c", "head -c 200000 /dev/zero; cat"]),
            Some(b"tail"),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 200_004);
        assert!(out.stdout.ends_with(b"tail"));
    }

    #[test]
    fn file_vault_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::Files(dir.path().join("secrets"));
        assert_eq!(vault.get("profile:x").unwrap(), None);
        vault.set("profile:x", "{\"k\":1}").unwrap();
        assert_eq!(vault.get("profile:x").unwrap().as_deref(), Some("{\"k\":1}"));
        vault.delete("profile:x").unwrap();
        assert_eq!(vault.get("profile:x").unwrap(), None);
    }
}
