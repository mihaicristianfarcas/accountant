//! Non-interactive commands.

use crate::config::{IMAP_PASSWORD_KEY, MailSource};
use crate::engine::{self, Engine};
use crate::providers::{self, Provider};
use crate::registry::Profile;
use crate::{browser, clipboard, mail, privacy, tui, usage};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use std::io::{BufRead, IsTerminal, Write};
use std::process::Command;

pub fn paint(code: &str, s: &str) -> String {
    if std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn provider_arg(p: Option<&str>) -> Result<Option<Provider>> {
    match p {
        None => Ok(None),
        Some(s) => {
            Provider::parse(s).map(Some).ok_or_else(|| anyhow!("unknown provider '{s}' (claude | codex)"))
        }
    }
}

fn accent(p: Provider, s: &str) -> String {
    match p {
        Provider::Claude => paint("38;2;217;119;87", s),
        Provider::Codex => paint("38;2;94;196;170", s),
    }
}

fn resolve_one(e: &Engine, who: &str, provider: Option<Provider>) -> Result<Profile> {
    let found = e.registry.resolve(who, provider);
    match found.as_slice() {
        [] => bail!("no account matches '{who}' — see `accountant ls`"),
        [one] => Ok((*one).clone()),
        many => bail!(
            "'{who}' is ambiguous: {} — be more specific or pass --provider",
            many.iter().map(|p| p.display()).collect::<Vec<_>>().join(", ")
        ),
    }
}

pub fn interactive(start: tui::Start) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        return list();
    }
    tui::run(Engine::open()?, start)
}

/// Bring the vault up to date with the live logins (quietly).
fn sync_all(e: &mut Engine) {
    for p in Provider::ALL {
        if let Err(err) = e.sync_back(p) {
            eprintln!("{} {}: {}", paint("33", "!"), p.label(), privacy::text(&format!("{err:#}")));
        }
    }
}

pub fn list() -> Result<()> {
    let mut e = Engine::open()?;
    sync_all(&mut e);
    let cache = usage::Cache::load(&e.paths.usage_cache());
    let all = e.registry.ordered().into_iter().cloned().collect::<Vec<_>>();
    let active: Vec<Option<String>> = Provider::ALL.iter().map(|p| e.active_id(*p)).collect();
    for (pi, provider) in Provider::ALL.iter().enumerate() {
        println!("{}", accent(*provider, &format!("◆ {}", provider.label().to_uppercase())));
        let mine: Vec<(usize, &Profile)> =
            all.iter().enumerate().filter(|(_, p)| p.provider == *provider).collect();
        if mine.is_empty() {
            println!("  {}", paint("2", &format!("no accounts — `accountant login {}`", provider.slug())));
        }
        for (i, p) in mine {
            let live = active[pi].as_deref() == Some(p.id.as_str());
            let dot = if live { paint("32", "●") } else { paint("2", "○") };
            let mut extra = String::new();
            if let Some(w) = cache.by_profile.get(&p.id).and_then(|u| u.binding()) {
                extra = format!("{} {:.0}%", w.label, w.used);
                if w.used >= 99.5
                    && let Some(r) = w.resets_at
                {
                    extra = format!(
                        "limited · resets in {}",
                        tui::theme::short_duration((r - Utc::now()).num_seconds())
                    );
                }
            }
            if p.needs_login {
                extra = "needs sign-in".into();
            }
            println!(
                "  {dot} {} {:<14} {:<30} {:<9} {}",
                paint("2", &format!("{}", i + 1)),
                p.shown_name(),
                p.email.as_deref().map_or("—".into(), privacy::email),
                p.plan.as_deref().unwrap_or(""),
                paint("2", &extra)
            );
        }
        if pi + 1 < Provider::ALL.len() {
            println!();
        }
    }
    Ok(())
}

fn report_switch(e: &Engine, done: &engine::Switched) {
    let Some(p) = e.registry.get(&done.target) else { return };
    let provider = done.provider;
    let email = p.email.as_deref().map(|e| privacy::email(e).into_owned());
    let detail = [email, p.plan.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    if done.already_active {
        println!(
            "{} {} is already on {}  {}",
            paint("32", "✓"),
            provider.label(),
            accent(provider, &p.shown_name()),
            paint("2", &detail)
        );
        return;
    }
    let from = done
        .from
        .as_deref()
        .and_then(|id| e.registry.get(id))
        .map(|f| format!("{} ", paint("2", &f.shown_name())))
        .unwrap_or_default();
    println!(
        "{} {} {from}→ {}  {}",
        paint("32", "✓"),
        provider.label(),
        accent(provider, &p.shown_name()),
        paint("2", &detail)
    );
    let n = engine::running_sessions(provider);
    if n > 0 {
        println!(
            "  {}",
            paint(
                "2",
                &format!(
                    "restart {n} running {} session{} to pick it up",
                    p.provider.process_name(),
                    if n == 1 { "" } else { "s" }
                )
            )
        );
    }
}

pub fn use_account(who: &str, provider: Option<&str>) -> Result<()> {
    let mut e = Engine::open()?;
    let provider = provider_arg(provider)?;
    let found: Vec<Profile> = e.registry.resolve(who, provider).into_iter().cloned().collect();
    // One name across both providers = switch both ("work" Claude + "work" Codex).
    let distinct = found.iter().map(|p| p.provider).collect::<std::collections::BTreeSet<_>>();
    let targets = if found.len() > 1 && distinct.len() == found.len() {
        found
    } else {
        vec![resolve_one(&e, who, provider)?]
    };
    for t in targets {
        let done = e.switch(&t.id).inspect_err(|err| {
            if err.to_string().contains("sign in again") {
                let _ = e.set_needs_login(&t.id, true);
            }
        })?;
        report_switch(&e, &done);
    }
    Ok(())
}

pub fn next(provider: Option<&str>) -> Result<()> {
    let mut e = Engine::open()?;
    sync_all(&mut e);
    let provider = match provider_arg(provider)? {
        Some(p) => p,
        None => {
            let multi: Vec<Provider> =
                Provider::ALL.into_iter().filter(|p| e.registry.of(*p).len() > 1).collect();
            match multi.as_slice() {
                [one] => *one,
                [] => bail!("add a second account first (`accountant login claude`)"),
                _ => bail!("which one? `accountant next claude` or `accountant next codex`"),
            }
        }
    };
    let cache = usage::Cache::load(&e.paths.usage_cache());
    let active = e.active_id(provider);
    let pick = e
        .registry
        .of(provider)
        .into_iter()
        .filter(|p| Some(&p.id) != active.as_ref() && !p.needs_login)
        .min_by(|a, b| {
            let used =
                |p: &Profile| cache.by_profile.get(&p.id).and_then(|u| u.binding()).map_or(50.0, |w| w.used);
            // Never used (None) sorts first, then the longest rested.
            used(a).total_cmp(&used(b)).then(a.left_at.cmp(&b.left_at))
        })
        .cloned()
        .ok_or_else(|| anyhow!("no other {} account to switch to", provider.label()))?;
    let done = e.switch(&pick.id)?;
    report_switch(&e, &done);
    Ok(())
}

pub fn save(name: Option<&str>, provider: Option<&str>) -> Result<()> {
    let mut e = Engine::open()?;
    let providers = match provider_arg(provider)? {
        Some(p) => vec![p],
        None => Provider::ALL.to_vec(),
    };
    for p in providers {
        match e.save_current(p, name)? {
            Some((id, created)) => {
                let pr = e.profile(&id)?;
                println!(
                    "{} {} {} {}  {}",
                    paint("32", "✓"),
                    if created { "saved" } else { "updated" },
                    p.label(),
                    accent(p, &pr.shown_name()),
                    paint("2", &privacy::email(pr.email.as_deref().unwrap_or("")))
                );
            }
            None => println!("{} nothing signed in to {}", paint("2", "·"), p.label()),
        }
    }
    Ok(())
}

pub fn add() -> Result<()> {
    if !std::io::stdout().is_terminal() {
        bail!("adding an account is interactive — run it in a terminal");
    }
    tui::run(Engine::open()?, tui::Start::Add)
}

pub fn login(target: &str, email: Option<String>) -> Result<()> {
    let start = match Provider::parse(target) {
        Some(provider) => tui::Start::Login { provider, email, adding: true },
        None => {
            let e = Engine::open()?;
            let p = resolve_one(&e, target, None)?;
            tui::Start::Login { provider: p.provider, email: email.or(p.email), adding: false }
        }
    };
    if !std::io::stdout().is_terminal() {
        bail!("sign-in is interactive — run it in a terminal");
    }
    tui::run(Engine::open()?, start)
}

pub fn rename(who: &str, name: &str, provider: Option<&str>) -> Result<()> {
    let mut e = Engine::open()?;
    let p = resolve_one(&e, who, provider_arg(provider)?)?;
    e.rename(&p.id, name)?;
    println!(
        "{} {} → {}",
        paint("32", "✓"),
        p.shown_name(),
        accent(p.provider, &e.profile(&p.id)?.shown_name())
    );
    Ok(())
}

fn confirm(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

pub fn remove(who: &str, provider: Option<&str>, yes: bool) -> Result<()> {
    let mut e = Engine::open()?;
    let p = resolve_one(&e, who, provider_arg(provider)?)?;
    if !yes && !confirm(&format!("forget {}?", p.display()))? {
        return Ok(());
    }
    e.remove(&p.id)?;
    println!("{} forgot {}", paint("32", "✓"), p.display());
    Ok(())
}

pub fn totp(who: &str, set: Option<String>, clear: bool, provider: Option<&str>) -> Result<()> {
    let mut e = Engine::open()?;
    let p = resolve_one(&e, who, provider_arg(provider)?)?;
    if clear {
        e.set_totp(&p.id, None)?;
        println!("{} removed the 2FA secret of {}", paint("32", "✓"), p.display());
        return Ok(());
    }
    if let Some(secret) = set {
        let secret = if secret == "-" {
            let mut s = String::new();
            std::io::stdin().lock().read_line(&mut s)?;
            s
        } else {
            secret
        };
        e.set_totp(&p.id, Some(secret.trim()))?;
        println!("{} stored the 2FA secret of {} in {}", paint("32", "✓"), p.display(), e.vault.describe());
    }
    let secret = e.totp(&p.id)?.ok_or_else(|| {
        let target = if privacy::enabled() { p.id.as_str() } else { p.name.as_str() };
        anyhow!("no 2FA secret for {} — `accountant totp {target} --set <setup key>`", p.shown_name())
    })?;
    let (code, left) = secret.now();
    let copied = if clipboard::copy(&code).is_ok() { paint("2", " · copied") } else { String::new() };
    println!(
        "{}  {}{copied}",
        paint("1", &format!("{} {}", &code[..3], &code[3..])),
        paint("2", &format!("{left}s left"))
    );
    Ok(())
}

fn guess_imap_host(user: &str) -> Option<&'static str> {
    let domain = user.rsplit('@').next()?.to_lowercase();
    Some(match domain.as_str() {
        "icloud.com" | "me.com" | "mac.com" => "imap.mail.me.com",
        "gmail.com" | "googlemail.com" => "imap.gmail.com",
        "outlook.com" | "hotmail.com" | "live.com" => "outlook.office365.com",
        "fastmail.com" | "fastmail.fm" => "imap.fastmail.com",
        "yahoo.com" => "imap.mail.yahoo.com",
        _ => return None,
    })
}

fn prompt_hidden(prompt: &str) -> Result<String> {
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use ratatui::crossterm::terminal;
    print!("{prompt}");
    std::io::stdout().flush()?;
    terminal::enable_raw_mode()?;
    let mut out = String::new();
    let res = loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => match k.code {
                KeyCode::Enter => break Ok(()),
                KeyCode::Esc => break Err(anyhow!("cancelled")),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(anyhow!("cancelled"));
                }
                KeyCode::Backspace => {
                    out.pop();
                }
                KeyCode::Char(c) => out.push(c),
                _ => {}
            },
            Ok(Event::Paste(s)) => out.push_str(&s),
            Ok(_) => {}
            Err(e) => break Err(e.into()),
        }
    };
    terminal::disable_raw_mode()?;
    println!();
    res.map(|_| out)
}

pub fn mail(source: Option<&str>, user: Option<String>, host: Option<String>, port: u16) -> Result<()> {
    let mut e = Engine::open()?;
    match source {
        None => {
            let m = &e.config.mail;
            let desc = match m.source {
                MailSource::Off => "off".to_string(),
                MailSource::AppleMail => "Apple Mail".to_string(),
                MailSource::Imap => {
                    format!("IMAP {} via {}:{}", privacy::email(&m.imap_user), m.imap_host, m.imap_port)
                }
            };
            println!("inbox codes: {}", paint("1", &desc));
            println!(
                "{}",
                paint("2", "change with: accountant mail apple | imap --user you@icloud.com | off | test")
            );
            return Ok(());
        }
        Some("off") => {
            e.config.mail.source = MailSource::Off;
            e.save_config()?;
            println!("{} inbox codes off", paint("32", "✓"));
            return Ok(());
        }
        Some("apple") | Some("apple-mail") | Some("mail.app") => {
            e.config.mail.source = MailSource::AppleMail;
        }
        Some("imap") => {
            let user = match user
                .or_else(|| (!e.config.mail.imap_user.is_empty()).then(|| e.config.mail.imap_user.clone()))
            {
                Some(u) => u,
                None => bail!("pass the mailbox login: accountant mail imap --user you@icloud.com"),
            };
            let host = host
                .or_else(|| guess_imap_host(&user).map(String::from))
                .ok_or_else(|| anyhow!("can't guess the IMAP server for {user} — pass --host"))?;
            let pw = prompt_hidden(&format!("app-specific password for {user} (hidden): "))?;
            if pw.trim().is_empty() {
                bail!("no password given");
            }
            e.vault.set(IMAP_PASSWORD_KEY, pw.trim())?;
            e.config.mail.source = MailSource::Imap;
            e.config.mail.imap_user = user;
            e.config.mail.imap_host = host;
            e.config.mail.imap_port = port;
        }
        Some("test") => {}
        Some(other) => bail!("unknown mail source '{other}' (off | apple | imap | test)"),
    }
    e.save_config()?;
    if e.config.mail.source == MailSource::Off {
        bail!("inbox codes are off — pick a source first");
    }
    println!("{} checking the last 3 days of sign-in emails…", paint("2", "·"));
    let pw = e.vault.get(IMAP_PASSWORD_KEY)?;
    let since = Utc::now() - chrono::Duration::days(3);
    let mut senders: Vec<&str> = vec![];
    for p in Provider::ALL {
        senders.extend(p.mail_senders());
    }
    let msgs = mail::fetch(&e.config.mail, pw.as_deref(), &senders, since).context("reading mail")?;
    println!("{} connected — {} recent message(s) from Anthropic/OpenAI", paint("32", "✓"), msgs.len());
    for m in msgs.iter().rev().take(5) {
        let code = crate::twofa::extract_code(&m.subject, &m.text);
        println!(
            "  {}  {}{}",
            paint("2", &m.date.map(|d| d.format("%b %d %H:%M").to_string()).unwrap_or_default()),
            m.subject,
            code.map(|c| paint("32", &format!("  → code {c}"))).unwrap_or_default()
        );
    }
    Ok(())
}

pub fn status() -> Result<()> {
    let mut e = Engine::open()?;
    sync_all(&mut e);
    for p in Provider::ALL {
        let live = e.live(p)?;
        let label = format!("{:<12}", p.label());
        match live {
            None => println!("{} {}", accent(p, &label), paint("2", "signed out")),
            Some((_, ident)) => {
                let name = e
                    .registry
                    .by_identity(p, &ident.key)
                    .map(|x| x.shown_name().into_owned())
                    .unwrap_or_else(|| "(unsaved)".into());
                let email = ident.email.as_deref().map(|e| privacy::email(e).into_owned());
                let detail = [email, ident.plan].into_iter().flatten().collect::<Vec<_>>().join(" · ");
                let running = engine::running_sessions(p);
                let sessions = if running > 0 { format!("  {running} running") } else { String::new() };
                println!(
                    "{} {}  {}{}",
                    accent(p, &label),
                    paint("1", &name),
                    paint("2", &detail),
                    paint("2", &sessions)
                );
            }
        }
    }
    Ok(())
}

fn version_of(bin: &str) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn doctor() -> Result<()> {
    let e = Engine::open()?;
    let ok = |s: &str| println!("  {} {s}", paint("32", "✓"));
    let warn = |s: &str| println!("  {} {s}", paint("33", "!"));
    let info = |s: &str| println!("  {} {s}", paint("2", "·"));

    println!("{}", paint("1", "accountant"));
    info(&format!("state     {}", e.paths.data.display()));
    info(&format!("secrets   {}", e.vault.describe()));
    info(&format!("accounts  {}", e.registry.profiles.len()));

    println!("{}", accent(Provider::Claude, "Claude Code"));
    match version_of("claude") {
        Some(v) => ok(&format!("claude {v}")),
        None => warn("`claude` not found on PATH — sign-in needs it (switching does not)"),
    }
    info(&format!("credentials  {}", providers::claude::CredStore::detect(&e.paths).describe()));
    info(&format!("account file {}", e.paths.claude_json.display()));
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some() && cfg!(target_os = "macos") {
        warn(
            "CLAUDE_CONFIG_DIR is set: Claude Code then uses a different Keychain item — set ACCOUNTANT_CLAUDE_KEYCHAIN_SERVICE to match",
        );
    }
    match e.live(Provider::Claude) {
        Ok(Some((_, id))) => ok(&format!(
            "signed in as {}",
            id.email.map_or("unknown account".into(), |e| privacy::email(&e).into_owned())
        )),
        Ok(None) => info("signed out"),
        Err(err) => warn(&format!("can't read the login: {err:#}")),
    }

    println!("{}", accent(Provider::Codex, "Codex"));
    match version_of("codex") {
        Some(v) => ok(&v),
        None => info("`codex` not found on PATH"),
    }
    info(&format!("credentials  {}", e.paths.codex_auth().display()));
    if providers::codex::uses_keyring(&e.paths) {
        warn(
            "config.toml stores Codex credentials in the keyring; set cli_auth_credentials_store = \"file\" for switching",
        );
    }
    match e.live(Provider::Codex) {
        Ok(Some((_, id))) => ok(&format!(
            "signed in as {}",
            id.email.map_or("API key".into(), |e| privacy::email(&e).into_owned())
        )),
        Ok(None) => info("signed out"),
        Err(err) => warn(&format!("can't read the login: {err:#}")),
    }

    println!("{}", paint("1", "sign-in"));
    let apps = browser::installed();
    info(&format!(
        "browsers  {}",
        if apps.is_empty() {
            "none detected".into()
        } else {
            apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
        }
    ));
    match browser::pick(&e.config.browser) {
        Some(b) => ok(&format!("using {} ({:?} sessions)", b.name, e.config.browser.mode).to_lowercase()),
        None => warn("no Chromium/Firefox browser found — sign-ins open in the default browser"),
    }
    match e.config.mail.source {
        MailSource::Off => {
            info("inbox codes off — `accountant mail apple` or `accountant mail imap --user …`")
        }
        MailSource::AppleMail => ok("inbox codes from Apple Mail"),
        MailSource::Imap => {
            let has_pw = e.vault.get(IMAP_PASSWORD_KEY)?.is_some();
            if has_pw {
                ok(&format!("inbox codes from {}", e.config.mail.imap_host));
            } else {
                warn("IMAP selected but no password saved — `accountant mail imap`");
            }
        }
    }
    Ok(())
}
