//! Small filesystem helpers: atomic, permission-preserving writes.

use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Write `contents` to `path` atomically (temp file + rename).
///
/// Keeps the existing file's permission bits; new files get `default_mode`.
pub fn write_atomic(path: &Path, contents: &[u8], default_mode: u32) -> Result<()> {
    let mode = fs::metadata(path).map(|m| m.permissions().mode() & 0o7777).unwrap_or(default_mode);
    write_with_mode(path, contents, mode)
}

/// Write a credential file: atomic, and always owner-only (0600) from the
/// first byte, even if the file it replaces was more permissive.
pub fn write_secret(path: &Path, contents: &[u8]) -> Result<()> {
    write_with_mode(path, contents, 0o600)
}

fn write_with_mode(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{file_name}.accountant-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    // `mode()` is filtered by the umask; set the exact bits explicitly.
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Create a directory (and parents) readable only by the current user.
pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_tightened_and_others_keep_their_mode() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("auth.json");
        fs::write(&p, "old").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        write_secret(&p, b"new").unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&p).unwrap(), "new");

        let q = dir.path().join("config.json");
        fs::write(&q, "x").unwrap();
        fs::set_permissions(&q, fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&q, b"y", 0o600).unwrap();
        assert_eq!(fs::metadata(&q).unwrap().permissions().mode() & 0o777, 0o644);
    }
}
