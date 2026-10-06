//! Opening sign-in pages in the right browser session.
//!
//! *Isolated* mode gives every account its own persistent browser profile
//! directory (Chromium `--user-data-dir`, Firefox `-profile`). One browser
//! app, many independent cookie jars: when you come back to an account, its
//! session is usually still there, so the sign-in is a single click and the
//! email code is only needed when the provider expires the session.

use crate::config::{BrowserConfig, BrowserMode};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Chromium,
    Firefox,
    Safari,
}

#[derive(Debug, Clone)]
pub struct BrowserApp {
    pub name: String,
    /// `.app` bundle on macOS, executable elsewhere.
    pub path: PathBuf,
    pub family: Family,
}

const KNOWN: &[(&str, Family)] = &[
    ("Google Chrome", Family::Chromium),
    ("Helium", Family::Chromium),
    ("Brave Browser", Family::Chromium),
    ("Chromium", Family::Chromium),
    ("Microsoft Edge", Family::Chromium),
    ("Vivaldi", Family::Chromium),
    ("Thorium", Family::Chromium),
    ("Firefox", Family::Firefox),
    ("Zen", Family::Firefox),
    ("Zen Browser", Family::Firefox),
    ("LibreWolf", Family::Firefox),
    ("Waterfox", Family::Firefox),
    ("Floorp", Family::Firefox),
    ("Safari", Family::Safari),
];

const LINUX_BINARIES: &[(&str, &str, Family)] = &[
    ("Google Chrome", "google-chrome", Family::Chromium),
    ("Chromium", "chromium", Family::Chromium),
    ("Chromium", "chromium-browser", Family::Chromium),
    ("Brave Browser", "brave-browser", Family::Chromium),
    ("Microsoft Edge", "microsoft-edge", Family::Chromium),
    ("Firefox", "firefox", Family::Firefox),
    ("LibreWolf", "librewolf", Family::Firefox),
];

pub fn installed() -> Vec<BrowserApp> {
    let mut out = Vec::new();
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let roots = [PathBuf::from("/Applications"), home.join("Applications")];
        for (name, family) in KNOWN {
            for root in &roots {
                let path = root.join(format!("{name}.app"));
                if path.exists() {
                    out.push(BrowserApp { name: name.to_string(), path, family: *family });
                    break;
                }
            }
        }
    } else {
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        for (name, bin, family) in LINUX_BINARIES {
            if out.iter().any(|b: &BrowserApp| b.name == *name) {
                continue;
            }
            if let Some(p) = std::env::split_paths(&path_var).map(|d| d.join(bin)).find(|p| p.exists()) {
                out.push(BrowserApp { name: name.to_string(), path: p, family: *family });
            }
        }
    }
    out
}

/// The browser to use: the configured one, else the first that can isolate.
pub fn pick(cfg: &BrowserConfig) -> Option<BrowserApp> {
    let apps = installed();
    if cfg.app != "auto"
        && let Some(app) = apps.iter().find(|a| a.name.eq_ignore_ascii_case(&cfg.app))
    {
        return Some(app.clone());
    }
    apps.iter()
        .find(|a| a.family == Family::Chromium)
        .or_else(|| apps.iter().find(|a| a.family == Family::Firefox))
        .cloned()
}

/// Directory name for an account's isolated browser profile.
pub fn profile_dir(root: &Path, key: &str) -> PathBuf {
    let clean: String = key
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "@._-".contains(c) { c } else { '_' })
        .collect();
    root.join(if clean.is_empty() { "default".into() } else { clean })
}

/// Open `url`. `session_key` picks the isolated profile (usually the email).
/// Returns a short description of where it opened, for the UI.
pub fn open(url: &str, cfg: &BrowserConfig, browsers_root: &Path, session_key: &str) -> Result<String> {
    let custom = std::env::var("ACCOUNTANT_BROWSER_CMD").ok().or_else(|| cfg.command.clone());
    if let Some(template) = custom.filter(|c| !c.trim().is_empty()) {
        return open_custom(&template, url);
    }
    let app = pick(cfg);
    match (cfg.mode, app) {
        (BrowserMode::Default, _) | (_, None) => {
            open_default(url)?;
            Ok("your default browser".into())
        }
        (_, Some(app)) if app.family == Family::Safari => {
            // Safari has no command-line switch for private or separate sessions.
            launch(&app, &[url.to_string()])?;
            Ok("Safari".into())
        }
        (BrowserMode::Isolated, Some(app)) => {
            let dir = profile_dir(browsers_root, session_key);
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            launch(&app, &isolated_args(app.family, &dir, url))?;
            Ok(format!("{} · own profile", app.name))
        }
        (BrowserMode::Private, Some(app)) => {
            // A throwaway data dir makes the private session truly fresh even if
            // other private windows are open.
            let mut rnd = [0u8; 4];
            getrandom::fill(&mut rnd).ok();
            let dir = std::env::temp_dir().join(format!("accountant-private-{}", hex::encode(rnd)));
            std::fs::create_dir_all(&dir)?;
            launch(&app, &private_args(app.family, &dir, url))?;
            Ok(format!("{} · private window", app.name))
        }
    }
}

fn isolated_args(family: Family, dir: &Path, url: &str) -> Vec<String> {
    let dir = dir.display().to_string();
    match family {
        Family::Chromium => vec![
            format!("--user-data-dir={dir}"),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            url.into(),
        ],
        Family::Firefox => vec!["-profile".into(), dir, url.into()],
        Family::Safari => vec![url.into()],
    }
}

fn private_args(family: Family, dir: &Path, url: &str) -> Vec<String> {
    let dir = dir.display().to_string();
    match family {
        Family::Chromium => vec![
            format!("--user-data-dir={dir}"),
            "--incognito".into(),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            url.into(),
        ],
        Family::Firefox => vec!["-profile".into(), dir, "-private-window".into(), url.into()],
        Family::Safari => vec![url.into()],
    }
}

fn launch(app: &BrowserApp, args: &[String]) -> Result<()> {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        // -n: a new instance, so a different profile dir is honoured even when
        // the browser is already running.
        c.arg("-na").arg(&app.path).arg("--args").args(args);
        c
    } else {
        let mut c = Command::new(&app.path);
        c.args(args);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    cmd.spawn().with_context(|| format!("launching {}", app.name))?;
    Ok(())
}

/// Run a user-supplied command; `{url}` is substituted, or appended.
fn open_custom(template: &str, url: &str) -> Result<String> {
    let mut parts: Vec<String> = template.split_whitespace().map(String::from).collect();
    if parts.is_empty() {
        anyhow::bail!("browser command is empty");
    }
    if parts.iter().any(|p| p.contains("{url}")) {
        for p in &mut parts {
            *p = p.replace("{url}", url);
        }
    } else {
        parts.push(url.to_string());
    }
    Command::new(&parts[0])
        .args(&parts[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("running browser command `{}`", parts[0]))?;
    Ok(parts[0].rsplit('/').next().unwrap_or("browser").to_string())
}

pub fn open_default(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("opening the browser")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_dirs_are_safe_and_stable() {
        let root = Path::new("/r");
        assert_eq!(profile_dir(root, "Me@iCloud.com"), Path::new("/r/me@icloud.com"));
        assert_eq!(profile_dir(root, "../../etc"), Path::new("/r/.._.._etc"));
        assert_eq!(profile_dir(root, ""), Path::new("/r/default"));
    }

    #[test]
    fn chromium_isolation_flags() {
        let args = isolated_args(Family::Chromium, Path::new("/p"), "https://x");
        assert_eq!(args[0], "--user-data-dir=/p");
        assert_eq!(args.last().unwrap(), "https://x");
        let args = private_args(Family::Firefox, Path::new("/p"), "https://x");
        assert!(args.contains(&"-private-window".to_string()));
    }
}
