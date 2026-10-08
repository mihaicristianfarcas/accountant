//! TUI state and behaviour (rendering lives in `view.rs`).

use crate::browser::{self, BrowserApp};
use crate::clipboard;
use crate::config::{BrowserMode, IMAP_PASSWORD_KEY, MailSource};
use crate::engine::{self, Engine, Stage};
use crate::login::{BrowserPlan, LoginEvent, LoginTask};
use crate::mail::{self, MailEvent};
use crate::providers::{self, Provider, SignIn};
use crate::registry::Profile;
use crate::twofa::TotpSecret;
use crate::usage::{self, Usage};
use anyhow::Result;
use chrono::Utc;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

const STAGE_MIN: Duration = Duration::from_millis(150);
const AUTO_EXIT: Duration = Duration::from_millis(1800);

pub struct App {
    pub engine: Engine,
    pub t0: Instant,
    pub cursor: usize,
    pub active: [Option<String>; Provider::ALL.len()],
    pub usage: usage::Cache,
    usage_rx: Option<Receiver<(String, Result<Usage>)>>,
    pub modal: Option<Modal>,
    /// When the current modal appeared (drives its opening animation).
    pub modal_at: Instant,
    pub toast: Option<Toast>,
    pub quit: bool,
    /// Printed to the normal screen after the TUI closes.
    pub farewell: Vec<String>,
    pub browsers: Vec<BrowserApp>,
    /// A CLI's own sign-in to run with the terminal handed over.
    pub external: Option<External>,
}

/// A sign-in that runs a CLI's own command (`cursor-agent login`, …).
pub struct External {
    pub provider: Provider,
    /// The account being signed in again, if any.
    pub target: Option<String>,
}

pub fn pidx(p: Provider) -> usize {
    Provider::ALL.iter().position(|x| *x == p).expect("every provider is in ALL")
}

pub enum Modal {
    Switch(Box<SwitchView>),
    Login(Box<LoginView>),
    Add { cursor: usize },
    Input(InputView),
    Confirm(ConfirmView),
    TwoFa(TwoFaView),
    Settings { cursor: usize },
    Help,
}

pub struct SwitchView {
    sw: Option<engine::Switch>,
    pub provider: Provider,
    pub from_name: Option<String>,
    pub to: Profile,
    /// Index of the stage on screen; 4 = all done.
    pub stage: usize,
    stage_ran: bool,
    pub stage_started: Instant,
    pub started: Instant,
    pub phase: Phase,
    pub exit_at: Option<Instant>,
    pub running: usize,
}

#[derive(Debug, Clone)]
pub enum Phase {
    Running,
    Success { at: Instant },
    Failed(String),
}

pub struct LoginView {
    pub provider: Provider,
    task: Option<LoginTask>,
    plan: BrowserPlan,
    pub email: Option<String>,
    previous: Option<String>,
    pub started: Instant,
    pub url: Option<String>,
    pub opened: Option<String>,
    pub status: Option<String>,
    pub phase: Phase,
    pub result: Option<Profile>,
    pub created: bool,
    mail_rx: Option<Receiver<MailEvent>>,
    mail_stop: Arc<AtomicBool>,
    pub mail: MailState,
    pub code: Option<FoundCode>,
    pub link: Option<String>,
    pub totp: Option<TotpSecret>,
    /// Some while typing a code for Claude Code's paste fallback.
    pub paste: Option<String>,
    pub exit_at: Option<Instant>,
    pub accepts_code: bool,
    /// Adding a new account (stay in accountant afterwards) rather than
    /// signing an existing one in again (quit, ready to go).
    pub adding: bool,
}

pub enum MailState {
    Off,
    Watching(&'static str),
    Error(String),
}

pub struct FoundCode {
    pub code: String,
    pub from: String,
    pub at: Instant,
    pub copied: bool,
}

pub struct InputView {
    pub title: String,
    pub hint: String,
    pub value: String,
    pub error: Option<String>,
    pub purpose: InputPurpose,
}

pub enum InputPurpose {
    Rename(String),
    Totp(String),
    LoginEmail(Provider),
}

pub struct ConfirmView {
    pub title: String,
    pub body: String,
    pub purpose: ConfirmPurpose,
}

pub enum ConfirmPurpose {
    Delete(String),
    RemoveTotp(String),
}

pub struct TwoFaView {
    pub id: String,
    pub name: String,
    pub provider: Provider,
    pub totp: Option<TotpSecret>,
    pub code: String,
    pub changed_at: Instant,
    pub copied: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToastKind {
    Info,
    Good,
    Bad,
}

pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub at: Instant,
}

impl Toast {
    /// Long enough to read: longer messages, and errors, stay up longer.
    pub fn life(&self) -> Duration {
        let mut secs = 3.0 + self.text.chars().count() as f32 / 14.0;
        if self.kind == ToastKind::Bad {
            secs *= 1.5;
        }
        Duration::from_secs_f32(secs.clamp(4.2, 20.0))
    }
}

impl App {
    pub fn new(engine: Engine) -> Self {
        let mut app = App {
            engine,
            t0: Instant::now(),
            cursor: 0,
            active: Default::default(),
            usage: usage::Cache::default(),
            usage_rx: None,
            modal: None,
            modal_at: Instant::now(),
            toast: None,
            quit: false,
            farewell: vec![],
            browsers: browser::installed(),
            external: None,
        };
        app.usage = usage::Cache::load(&app.engine.paths.usage_cache());

        // Keep the vault in step with whatever is logged in right now (tokens
        // rotate), and pick up accounts signed in outside accountant.
        let mut adopted = vec![];
        for p in Provider::ALL {
            match app.engine.sync_back(p) {
                Ok(engine::Synced::Adopted(id)) => adopted.push(id),
                Ok(_) => {}
                Err(e) => app.toast(ToastKind::Bad, format!("{}: {e:#}", p.label())),
            }
        }
        app.refresh_active();
        if !adopted.is_empty() {
            let names: Vec<String> = adopted
                .iter()
                .filter_map(|id| app.engine.registry.get(id))
                .map(|p| format!("{} ({})", p.shown_name(), p.provider.label()))
                .collect();
            app.toast(ToastKind::Good, format!("saved current login: {}", names.join(", ")));
        }
        app.cursor = app.suggested().unwrap_or(0);
        app.refresh_usage();
        app
    }

    pub fn secs(&self) -> f32 {
        self.t0.elapsed().as_secs_f32()
    }

    pub fn reduced_motion(&self) -> bool {
        self.engine.config.ui.reduced_motion
    }

    pub fn profiles(&self) -> Vec<Profile> {
        self.engine.registry.ordered().into_iter().cloned().collect()
    }

    pub fn selected(&self) -> Option<Profile> {
        self.profiles().get(self.cursor).cloned()
    }

    pub fn is_active(&self, p: &Profile) -> bool {
        self.active[pidx(p.provider)].as_deref() == Some(p.id.as_str())
    }

    fn refresh_active(&mut self) {
        for p in Provider::ALL {
            self.active[pidx(p)] = self.engine.active_id(p);
        }
        let n = self.engine.registry.profiles.len();
        if self.cursor >= n {
            self.cursor = n.saturating_sub(1);
        }
    }

    /// The account you most likely want next: an idle one with the most
    /// headroom, in the first provider that has a choice.
    fn suggested(&self) -> Option<usize> {
        let list = self.profiles();
        for prov in Provider::ALL {
            let mut best: Option<(f64, Option<chrono::DateTime<Utc>>, usize)> = None;
            for (i, p) in list.iter().enumerate() {
                if p.provider != prov || self.is_active(p) || p.needs_login {
                    continue;
                }
                let used =
                    self.usage.by_profile.get(&p.id).and_then(|u| u.binding()).map_or(50.0, |w| w.used);
                // Never used (None) sorts first, then the longest rested.
                let better = best.as_ref().is_none_or(|(bu, bl, _)| {
                    used.total_cmp(bu).then(p.left_at.cmp(bl)) == std::cmp::Ordering::Less
                });
                if better {
                    best = Some((used, p.left_at, i));
                }
            }
            if let Some((_, _, i)) = best {
                return Some(i);
            }
        }
        None
    }

    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), kind, at: Instant::now() });
    }

    fn refresh_usage(&mut self) {
        if !self.engine.config.ui.usage {
            return;
        }
        let vault = self.engine.vault.clone();
        let paths = self.engine.paths.clone();
        let jobs: Vec<(String, Provider, bool)> =
            self.profiles().iter().map(|p| (p.id.clone(), p.provider, self.is_active(p))).collect();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut fetches = vec![];
            for (id, provider, active) in jobs {
                let snap = if active {
                    providers::read_live(provider, &paths).ok().flatten()
                } else {
                    vault
                        .get(&format!("profile:{id}"))
                        .ok()
                        .flatten()
                        .and_then(|s| serde_json::from_str(&s).ok())
                };
                if let Some(tok) = snap.as_ref().and_then(|s| providers::fresh_access_token(provider, s)) {
                    fetches.push((id, provider, tok));
                }
            }
            let inner = usage::spawn_fetch(fetches);
            for msg in inner {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
        self.usage_rx = Some(rx);
    }

    // -----------------------------------------------------------------------
    // Ticking
    // -----------------------------------------------------------------------

    pub fn tick(&mut self) {
        if let Some(rx) = &self.usage_rx {
            let mut changed = false;
            let mut revoked = vec![];
            while let Ok((id, res)) = rx.try_recv() {
                match res {
                    Ok(u) => {
                        self.usage.by_profile.insert(id, u);
                        changed = true;
                    }
                    Err(e) if e.is::<usage::Revoked>() => revoked.push(id),
                    Err(_) => {}
                }
            }
            if changed {
                let _ = self.usage.save(&self.engine.paths.usage_cache());
            }
            // Say so now, not when the switch to it fails.
            for id in revoked {
                if self.engine.registry.get(&id).is_some_and(|p| !p.needs_login) {
                    let _ = self.engine.set_needs_login(&id, true);
                }
            }
        }
        if let Some(m) = self.modal.take() {
            self.modal = self.tick_modal(m);
        }
        if self.toast.as_ref().is_some_and(|t| t.at.elapsed() > t.life()) {
            self.toast = None;
        }
    }

    fn tick_modal(&mut self, m: Modal) -> Option<Modal> {
        match m {
            Modal::Switch(mut v) => {
                self.tick_switch(&mut v);
                Some(Modal::Switch(v))
            }
            Modal::Login(mut v) => {
                self.tick_login(&mut v);
                Some(Modal::Login(v))
            }
            Modal::TwoFa(mut v) => {
                if let Some(t) = &v.totp {
                    let (code, _) = t.now();
                    if code != v.code {
                        v.copied = clipboard::copy(&code).is_ok();
                        v.code = code;
                        v.changed_at = Instant::now();
                    }
                }
                Some(Modal::TwoFa(v))
            }
            other => Some(other),
        }
    }

    fn tick_switch(&mut self, v: &mut SwitchView) {
        let now = Instant::now();
        if matches!(v.phase, Phase::Running) {
            if v.stage < Stage::ALL.len() {
                if !v.stage_ran {
                    v.stage_ran = true;
                    let stage = Stage::ALL[v.stage];
                    let res = match v.sw.as_mut() {
                        Some(sw) => self.engine.run_stage(sw, stage),
                        None => Ok(()),
                    };
                    if let Err(e) = res {
                        v.sw = None; // release the lock
                        if stage == Stage::Load {
                            let _ = self.engine.set_needs_login(&v.to.id, true);
                        }
                        v.phase = Phase::Failed(format!("{e:#}"));
                        self.refresh_active();
                        return;
                    }
                }
                if now - v.stage_started >= STAGE_MIN {
                    v.stage += 1;
                    v.stage_ran = false;
                    v.stage_started = now;
                }
            }
            if v.stage >= Stage::ALL.len() {
                let done = v.sw.take().map(engine::Switch::finish);
                self.refresh_active();
                v.running = engine::sessions_to_restart(v.provider);
                v.phase = Phase::Success { at: now };
                if let Some(p) = self.engine.registry.get(&v.to.id) {
                    v.to = p.clone();
                }
                self.farewell = farewell_lines(&v.to, done.is_some_and(|d| d.already_active), v.running);
                if self.engine.config.ui.auto_exit {
                    v.exit_at = Some(now + AUTO_EXIT);
                }
            }
        }
        if v.exit_at.is_some_and(|t| now >= t) {
            self.quit = true;
        }
    }

    fn tick_login(&mut self, v: &mut LoginView) {
        let now = Instant::now();
        let mut events = vec![];
        if let Some(task) = &v.task {
            while let Ok(ev) = task.rx.try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            match ev {
                LoginEvent::Url(u) => v.url = Some(u),
                LoginEvent::Opened(w) => v.opened = Some(w),
                LoginEvent::Status(s) => v.status = Some(s),
                LoginEvent::Success(snap) => {
                    v.mail_stop.store(true, Ordering::Relaxed);
                    v.task = None;
                    let name = v.email.as_deref().and_then(|e| e.split('@').next()).map(String::from);
                    match self.engine.finish_login(
                        v.provider,
                        snap.as_ref(),
                        name.as_deref(),
                        v.previous.as_deref(),
                    ) {
                        Ok((id, created)) => {
                            self.refresh_active();
                            let profile = self.engine.registry.get(&id).cloned();
                            if let Some(p) = &profile {
                                if let Some(i) = self.profiles().iter().position(|x| x.id == p.id) {
                                    self.cursor = i;
                                }
                                let running = engine::sessions_to_restart(v.provider);
                                self.farewell = farewell_lines(p, false, running);
                            }
                            v.result = profile;
                            v.created = created;
                            v.phase = Phase::Success { at: now };
                            if self.engine.config.ui.auto_exit && !v.adding {
                                v.exit_at = Some(now + AUTO_EXIT + Duration::from_millis(600));
                            }
                            self.refresh_usage();
                        }
                        Err(e) => v.phase = Phase::Failed(format!("{e:#}")),
                    }
                }
                LoginEvent::Failed(msg) => {
                    v.mail_stop.store(true, Ordering::Relaxed);
                    v.task = None;
                    v.phase = Phase::Failed(msg);
                }
            }
        }
        if let Some(rx) = &v.mail_rx {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    MailEvent::Code { code, from } => {
                        let copied = clipboard::copy(&code).is_ok();
                        v.code = Some(FoundCode { code, from, at: now, copied });
                    }
                    MailEvent::Link { url } => v.link = Some(url),
                    MailEvent::Error(e) => v.mail = MailState::Error(e),
                }
            }
        }
        if v.exit_at.is_some_and(|t| now >= t) {
            self.quit = true;
        }
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    pub fn on_paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        match &mut self.modal {
            Some(Modal::Input(v)) => v.value.push_str(clean.trim()),
            Some(Modal::Login(v)) => {
                if let Some(buf) = &mut v.paste {
                    buf.push_str(clean.trim());
                }
            }
            _ => {}
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if let Some(Modal::Login(v)) = &self.modal {
                if let Some(t) = &v.task {
                    t.cancel();
                }
                v.mail_stop.store(true, Ordering::Relaxed);
            }
            self.quit = true;
            return;
        }
        let before = self.modal.as_ref().map(std::mem::discriminant);
        self.modal = match self.modal.take() {
            Some(m) => self.modal_key(m, key),
            None => self.list_key(key),
        };
        if self.modal.as_ref().map(std::mem::discriminant) != before {
            self.modal_at = Instant::now();
        }
    }

    /// Open straight into a flow (from the command line).
    pub fn open(&mut self, start: super::Start) {
        self.modal = match start {
            super::Start::Home => None,
            super::Start::Add => Some(Modal::Add { cursor: 0 }),
            super::Start::Login { provider, email, adding } => self.start_login(provider, email, adding),
        };
        self.modal_at = Instant::now();
    }

    fn list_key(&mut self, key: KeyEvent) -> Option<Modal> {
        let n = self.engine.registry.profiles.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if n > 0 => {
                self.cursor = (self.cursor + n - 1) % n;
                None
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab if n > 0 => {
                self.cursor = (self.cursor + 1) % n;
                None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.cursor = 0;
                None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.cursor = n.saturating_sub(1);
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right | KeyCode::Char('l') => {
                let p = self.selected()?;
                self.start_switch(&p.id)
            }
            KeyCode::Char(c @ '1'..='9') => {
                let i = (c as u8 - b'1') as usize;
                let p = self.profiles().get(i).cloned()?;
                self.cursor = i;
                self.start_switch(&p.id)
            }
            KeyCode::Char('a') | KeyCode::Char('+') => Some(Modal::Add { cursor: 0 }),
            KeyCode::Char('r') => {
                let p = self.selected()?;
                self.sign_in(p.provider, Some(p), false)
            }
            KeyCode::Char('n') | KeyCode::Char('e') => {
                let p = self.selected()?;
                Some(Modal::Input(InputView {
                    title: format!("rename {}", p.shown_name()),
                    hint: "a short name you'll recognise".into(),
                    value: p.name.clone(),
                    error: None,
                    purpose: InputPurpose::Rename(p.id),
                }))
            }
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
                let p = self.selected()?;
                Some(Modal::Confirm(ConfirmView {
                    title: format!("remove {}?", p.shown_name()),
                    body: format!(
                        "Forgets the saved {} login{}. Whatever is signed in right now stays signed in.",
                        p.provider.label(),
                        if p.totp { " and its 2FA secret" } else { "" }
                    ),
                    purpose: ConfirmPurpose::Delete(p.id),
                }))
            }
            KeyCode::Char('t') if n > 0 => {
                let p = self.selected()?;
                Some(self.twofa_view(&p))
            }
            KeyCode::Char('u') => {
                self.refresh_usage();
                self.toast(ToastKind::Info, "refreshing usage…");
                None
            }
            KeyCode::Char('p') => {
                let on = !self.engine.config.ui.hide_emails;
                self.engine.config.ui.hide_emails = on;
                crate::privacy::set(on);
                match self.engine.save_config() {
                    Ok(()) if on => {
                        self.toast(ToastKind::Good, "emails hidden — safe to record (p shows them)")
                    }
                    Ok(()) => self.toast(ToastKind::Info, "emails visible"),
                    Err(e) => self.toast(ToastKind::Bad, format!("{e:#}")),
                }
                None
            }
            KeyCode::Char(',') | KeyCode::Char('s') => Some(Modal::Settings { cursor: 0 }),
            KeyCode::Char('?') | KeyCode::Char('h') => Some(Modal::Help),
            KeyCode::Char('q') | KeyCode::Esc => {
                self.quit = true;
                None
            }
            _ => None,
        }
    }

    fn twofa_view(&mut self, p: &Profile) -> Modal {
        let totp = self.engine.totp(&p.id).unwrap_or_else(|e| {
            self.toast(ToastKind::Bad, format!("{e:#}"));
            None
        });
        Modal::TwoFa(TwoFaView {
            id: p.id.clone(),
            name: p.shown_name().into_owned(),
            provider: p.provider,
            totp,
            code: String::new(),
            changed_at: Instant::now(),
            copied: false,
        })
    }

    fn start_switch(&mut self, id: &str) -> Option<Modal> {
        let to = self.engine.registry.get(id)?.clone();
        if self.is_active(&to) {
            self.toast(
                ToastKind::Info,
                format!("{} is already live for {}", to.shown_name(), to.provider.label()),
            );
            return None;
        }
        let from_name = self.active[pidx(to.provider)]
            .as_deref()
            .and_then(|a| self.engine.registry.get(a))
            .map(|p| p.shown_name().into_owned());
        match self.engine.begin_switch(id) {
            Ok(sw) => {
                let now = Instant::now();
                Some(Modal::Switch(Box::new(SwitchView {
                    sw: Some(sw),
                    provider: to.provider,
                    from_name,
                    to,
                    stage: 0,
                    stage_ran: false,
                    stage_started: now,
                    started: now,
                    phase: Phase::Running,
                    exit_at: None,
                    running: 0,
                })))
            }
            Err(e) => {
                self.toast(ToastKind::Bad, format!("{e:#}"));
                None
            }
        }
    }

    /// Sign in to `provider` (again, to `target`) the way that CLI signs in.
    fn sign_in(&mut self, provider: Provider, target: Option<Profile>, adding: bool) -> Option<Modal> {
        match provider.sign_in() {
            SignIn::Browser => self.start_login(provider, target.and_then(|p| p.email), adding),
            SignIn::Command(_) => {
                self.external = Some(External { provider, target: target.map(|p| p.id) });
                None
            }
            SignIn::App(app) => {
                // Whatever is signed in now is kept before the app replaces it.
                if let Err(e) = self.engine.sync_back(provider) {
                    self.toast(ToastKind::Bad, format!("{e:#}"));
                } else {
                    self.toast(ToastKind::Info, format!("sign in from the {app} app, then a → save current"));
                }
                None
            }
        }
    }

    /// Run a CLI's own sign-in while the TUI is suspended, then save what it
    /// signed in to.
    pub fn run_external(&mut self, job: External) {
        let label = job.provider.label();
        let result = (|| -> Result<(String, bool)> {
            let previous = self.engine.sync_back(job.provider)?.profile_id().map(String::from);
            if let SignIn::Command(argv) = job.provider.sign_in() {
                println!("\n  {label} · running `{}` (the current login is saved)\n", argv.join(" "));
            }
            crate::login::run_command(job.provider)?;
            self.engine.finish_external_login(job.provider, job.target.as_deref(), previous.as_deref())
        })();
        match result {
            Ok((id, created)) => {
                self.refresh_active();
                if let Some(i) = self.profiles().iter().position(|p| p.id == id) {
                    self.cursor = i;
                }
                let name =
                    self.engine.registry.get(&id).map(|p| p.shown_name().into_owned()).unwrap_or_default();
                let verb = if created { "added" } else { "signed in again" };
                self.toast(ToastKind::Good, format!("{verb}: {name} ({label})"));
            }
            Err(e) => self.toast(ToastKind::Bad, format!("{label}: {e:#}")),
        }
    }

    fn start_login(&mut self, provider: Provider, email: Option<String>, adding: bool) -> Option<Modal> {
        let email = email.map(|e| e.trim().to_string()).filter(|e| !e.is_empty());
        // `claude auth login` replaces the live login: save it first.
        let previous = match self.engine.sync_back(provider) {
            Ok(s) => s.profile_id().map(String::from),
            Err(e) => {
                self.toast(ToastKind::Bad, format!("{e:#}"));
                return None;
            }
        };
        let plan = BrowserPlan {
            config: self.engine.config.browser.clone(),
            root: self.engine.paths.browsers(),
            session_key: email.clone().unwrap_or_else(|| format!("new-{}", provider.slug())),
        };
        let task = match LoginTask::start(provider, email.clone(), plan) {
            Ok(t) => t,
            Err(e) => {
                self.toast(ToastKind::Bad, format!("{e:#}"));
                return None;
            }
        };
        let accepts_code = task.accepts_code();
        let plan = BrowserPlan {
            config: self.engine.config.browser.clone(),
            root: self.engine.paths.browsers(),
            session_key: email.clone().unwrap_or_else(|| format!("new-{}", provider.slug())),
        };

        let mail_stop = Arc::new(AtomicBool::new(false));
        let cfg = self.engine.config.mail.clone();
        let (mail_rx, mail) = match cfg.source {
            MailSource::Off => (None, MailState::Off),
            source => {
                let pw = if source == MailSource::Imap {
                    self.engine.vault.get(IMAP_PASSWORD_KEY).ok().flatten()
                } else {
                    None
                };
                let label = if source == MailSource::Imap { "your inbox" } else { "Apple Mail" };
                (
                    Some(mail::watch(cfg, pw, provider, Utc::now(), mail_stop.clone())),
                    MailState::Watching(label),
                )
            }
        };
        let totp = email
            .as_deref()
            .and_then(|e| {
                self.engine
                    .registry
                    .profiles
                    .iter()
                    .find(|p| p.provider == provider && p.email.as_deref() == Some(e))
            })
            .and_then(|p| self.engine.totp(&p.id).ok().flatten());

        Some(Modal::Login(Box::new(LoginView {
            provider,
            task: Some(task),
            plan,
            email,
            previous,
            started: Instant::now(),
            url: None,
            opened: None,
            status: None,
            phase: Phase::Running,
            result: None,
            created: false,
            mail_rx,
            mail_stop,
            mail,
            code: None,
            link: None,
            totp,
            paste: None,
            exit_at: None,
            accepts_code,
            adding,
        })))
    }

    fn modal_key(&mut self, m: Modal, key: KeyEvent) -> Option<Modal> {
        match m {
            Modal::Help => None,
            Modal::Switch(v) => self.switch_key(v, key),
            Modal::Login(v) => self.login_key(v, key),
            Modal::Add { cursor } => self.add_key(cursor, key),
            Modal::Input(v) => self.input_key(v, key),
            Modal::Confirm(v) => self.confirm_key(v, key),
            Modal::TwoFa(v) => self.twofa_key(v, key),
            Modal::Settings { cursor } => self.settings_key(cursor, key),
        }
    }

    fn switch_key(&mut self, mut v: Box<SwitchView>, key: KeyEvent) -> Option<Modal> {
        match v.phase {
            Phase::Running => Some(Modal::Switch(v)),
            Phase::Success { .. } => match key.code {
                KeyCode::Enter | KeyCode::Char('q') => {
                    self.quit = true;
                    None
                }
                KeyCode::Esc => {
                    self.farewell.clear();
                    None
                }
                _ => {
                    // Any other key: stay in accountant.
                    v.exit_at = None;
                    Some(Modal::Switch(v))
                }
            },
            Phase::Failed(_) => match key.code {
                KeyCode::Char('r') => self.sign_in(v.to.provider, Some(v.to.clone()), false),
                _ => None,
            },
        }
    }

    fn login_key(&mut self, mut v: Box<LoginView>, key: KeyEvent) -> Option<Modal> {
        if let Some(buf) = &mut v.paste {
            match key.code {
                KeyCode::Esc => v.paste = None,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Enter => {
                    let code = std::mem::take(buf);
                    v.paste = None;
                    if let Some(t) = &v.task {
                        match t.submit_code(&code) {
                            Ok(()) => v.status = Some("code sent — finishing…".into()),
                            Err(e) => v.status = Some(format!("{e:#}")),
                        }
                    }
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            }
            return Some(Modal::Login(v));
        }
        match (&v.phase, key.code) {
            (Phase::Success { .. }, KeyCode::Enter | KeyCode::Esc) if v.adding => None,
            (Phase::Success { .. }, KeyCode::Enter | KeyCode::Char('q')) => {
                self.quit = true;
                None
            }
            (Phase::Success { .. }, KeyCode::Esc) => {
                self.farewell.clear();
                None
            }
            (Phase::Success { .. }, _) => {
                v.exit_at = None;
                Some(Modal::Login(v))
            }
            (Phase::Failed(_), KeyCode::Char('r')) => self.start_login(v.provider, v.email.clone(), v.adding),
            (Phase::Failed(_), _) => None,
            (Phase::Running, KeyCode::Esc) => {
                if let Some(t) = &v.task {
                    t.cancel();
                }
                v.mail_stop.store(true, Ordering::Relaxed);
                self.toast(ToastKind::Info, "sign-in cancelled");
                None
            }
            (Phase::Running, KeyCode::Char('o')) => {
                if let Some(url) = &v.url {
                    match v.plan.open(url) {
                        Ok(w) => v.opened = Some(w),
                        Err(e) => v.status = Some(format!("{e:#}")),
                    }
                }
                Some(Modal::Login(v))
            }
            (Phase::Running, KeyCode::Char('u')) => {
                if let Some(url) = &v.url
                    && clipboard::copy(url).is_ok()
                {
                    v.status = Some("sign-in link copied — paste it into any browser".into());
                }
                Some(Modal::Login(v))
            }
            (Phase::Running, KeyCode::Char('c')) => {
                if let Some(c) = &mut v.code {
                    c.copied = clipboard::copy(&c.code).is_ok();
                    c.at = Instant::now();
                }
                Some(Modal::Login(v))
            }
            (Phase::Running, KeyCode::Char('y')) => {
                if let Some(t) = &v.totp {
                    let (code, _) = t.now();
                    if clipboard::copy(&code).is_ok() {
                        v.status = Some("2FA code copied".into());
                    }
                }
                Some(Modal::Login(v))
            }
            (Phase::Running, KeyCode::Char('m')) => {
                if let Some(link) = v.link.clone() {
                    match v.plan.open(&link) {
                        Ok(_) => v.status = Some("opened the email's sign-in link".into()),
                        Err(e) => v.status = Some(format!("{e:#}")),
                    }
                }
                Some(Modal::Login(v))
            }
            (Phase::Running, KeyCode::Char('p')) if v.accepts_code => {
                v.paste = Some(String::new());
                Some(Modal::Login(v))
            }
            _ => Some(Modal::Login(v)),
        }
    }

    fn add_key(&mut self, cursor: usize, key: KeyEvent) -> Option<Modal> {
        // One row per provider, then "save current".
        const N: usize = Provider::ALL.len() + 1;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => Some(Modal::Add { cursor: (cursor + N - 1) % N }),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                Some(Modal::Add { cursor: (cursor + 1) % N })
            }
            KeyCode::Esc | KeyCode::Char('q') => None,
            KeyCode::Char('c') => self.add_key(0, KeyEvent::from(KeyCode::Enter)),
            KeyCode::Char('x') => self.add_key(1, KeyEvent::from(KeyCode::Enter)),
            KeyCode::Char(c @ '1'..='9') if ((c as u8 - b'1') as usize) < N => {
                self.add_key((c as u8 - b'1') as usize, KeyEvent::from(KeyCode::Enter))
            }
            KeyCode::Enter | KeyCode::Char(' ') => match Provider::ALL.get(cursor).copied() {
                Some(provider) if provider.sign_in() == SignIn::Browser => Some(Modal::Input(InputView {
                    title: format!("sign in · {}", provider.label()),
                    hint:
                        "account email — prefills the sign-in and gives it its own browser session (optional)"
                            .into(),
                    value: String::new(),
                    error: None,
                    purpose: InputPurpose::LoginEmail(provider),
                })),
                Some(provider) => self.sign_in(provider, None, true),
                None => {
                    let mut saved = vec![];
                    for p in Provider::ALL {
                        match self.engine.save_current(p, None) {
                            Ok(Some((id, _))) => {
                                if let Some(pr) = self.engine.registry.get(&id) {
                                    saved.push(format!("{} ({})", pr.shown_name(), p.label()));
                                }
                            }
                            Ok(None) => {}
                            Err(e) => self.toast(ToastKind::Bad, format!("{e:#}")),
                        }
                    }
                    self.refresh_active();
                    if saved.is_empty() {
                        self.toast(ToastKind::Info, "nothing is signed in right now");
                    } else {
                        self.toast(ToastKind::Good, format!("saved {}", saved.join(", ")));
                    }
                    None
                }
            },
            _ => Some(Modal::Add { cursor }),
        }
    }

    fn input_key(&mut self, mut v: InputView, key: KeyEvent) -> Option<Modal> {
        match key.code {
            KeyCode::Esc => {
                if let InputPurpose::Totp(id) = &v.purpose {
                    let p = self.engine.registry.get(id).cloned()?;
                    return Some(self.twofa_view(&p));
                }
                None
            }
            KeyCode::Backspace => {
                v.value.pop();
                v.error = None;
                Some(Modal::Input(v))
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                v.value.clear();
                Some(Modal::Input(v))
            }
            KeyCode::Char(c) => {
                v.value.push(c);
                v.error = None;
                Some(Modal::Input(v))
            }
            KeyCode::Enter => self.submit_input(v),
            _ => Some(Modal::Input(v)),
        }
    }

    fn submit_input(&mut self, mut v: InputView) -> Option<Modal> {
        let value = v.value.trim().to_string();
        match &v.purpose {
            InputPurpose::Rename(id) => {
                if value.is_empty() {
                    return None;
                }
                match self.engine.rename(id, &value) {
                    Ok(()) => {
                        self.toast(ToastKind::Good, format!("renamed to {value}"));
                        None
                    }
                    Err(e) => {
                        v.error = Some(format!("{e:#}"));
                        Some(Modal::Input(v))
                    }
                }
            }
            InputPurpose::Totp(id) => {
                let id = id.clone();
                match self.engine.set_totp(&id, Some(&value)) {
                    Ok(()) => {
                        let p = self.engine.registry.get(&id).cloned()?;
                        Some(self.twofa_view(&p))
                    }
                    Err(e) => {
                        v.error = Some(format!("{e:#}"));
                        Some(Modal::Input(v))
                    }
                }
            }
            InputPurpose::LoginEmail(provider) => {
                if !value.is_empty() && !value.contains('@') {
                    v.error = Some("that doesn't look like an email (or leave it empty)".into());
                    return Some(Modal::Input(v));
                }
                let provider = *provider;
                self.start_login(provider, Some(value), true)
            }
        }
    }

    fn confirm_key(&mut self, v: ConfirmView, key: KeyEvent) -> Option<Modal> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => match v.purpose {
                ConfirmPurpose::Delete(id) => {
                    match self.engine.remove(&id) {
                        Ok(p) => self.toast(ToastKind::Good, format!("removed {}", p.shown_name())),
                        Err(e) => self.toast(ToastKind::Bad, format!("{e:#}")),
                    }
                    self.refresh_active();
                    None
                }
                ConfirmPurpose::RemoveTotp(id) => {
                    if let Err(e) = self.engine.set_totp(&id, None) {
                        self.toast(ToastKind::Bad, format!("{e:#}"));
                    }
                    let p = self.engine.registry.get(&id).cloned()?;
                    Some(self.twofa_view(&p))
                }
            },
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => match v.purpose {
                ConfirmPurpose::RemoveTotp(id) => {
                    let p = self.engine.registry.get(&id).cloned()?;
                    Some(self.twofa_view(&p))
                }
                ConfirmPurpose::Delete(_) => None,
            },
            _ => Some(Modal::Confirm(v)),
        }
    }

    fn twofa_key(&mut self, mut v: TwoFaView, key: KeyEvent) -> Option<Modal> {
        match key.code {
            KeyCode::Char('s') | KeyCode::Char('a') => Some(Modal::Input(InputView {
                title: format!("2FA secret · {}", v.name),
                hint: "paste the setup key or otpauth:// link (shown when enabling 2FA)".into(),
                value: String::new(),
                error: None,
                purpose: InputPurpose::Totp(v.id),
            })),
            KeyCode::Char('x') if v.totp.is_some() => Some(Modal::Confirm(ConfirmView {
                title: "remove 2FA secret?".into(),
                body: format!("accountant will stop generating codes for {}.", v.name),
                purpose: ConfirmPurpose::RemoveTotp(v.id),
            })),
            KeyCode::Char('c') | KeyCode::Char('y') | KeyCode::Enter if v.totp.is_some() => {
                v.copied = clipboard::copy(&v.code).is_ok();
                v.changed_at = Instant::now();
                Some(Modal::TwoFa(v))
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('t') => None,
            _ => Some(Modal::TwoFa(v)),
        }
    }

    pub const SETTINGS: usize = 6;

    pub fn settings_rows(&self) -> Vec<(&'static str, String)> {
        let c = &self.engine.config;
        let auto_name =
            browser::pick(&crate::config::BrowserConfig { app: "auto".into(), ..c.browser.clone() })
                .map(|b| b.name)
                .unwrap_or_else(|| "system default".into());
        vec![
            (
                "sign-in browser",
                match c.browser.mode {
                    BrowserMode::Isolated => "own profile per account".into(),
                    BrowserMode::Private => "fresh private window".into(),
                    BrowserMode::Default => "default browser".into(),
                },
            ),
            (
                "browser app",
                if c.browser.app == "auto" { format!("auto · {auto_name}") } else { c.browser.app.clone() },
            ),
            (
                "inbox codes",
                match c.mail.source {
                    MailSource::Off => "off".into(),
                    MailSource::AppleMail => "Apple Mail".into(),
                    MailSource::Imap if c.mail.imap_host.is_empty() => "IMAP · run `accountant mail`".into(),
                    MailSource::Imap => format!("IMAP · {}", crate::privacy::email(&c.mail.imap_user)),
                },
            ),
            (
                "after switching",
                if c.ui.auto_exit { "quit — ready to go".into() } else { "stay open".into() },
            ),
            ("usage meters", if c.ui.usage { "on".into() } else { "off".into() }),
            ("hide emails", if c.ui.hide_emails { "on · mi•••@icloud.com".into() } else { "off".into() }),
        ]
    }

    fn settings_key(&mut self, cursor: usize, key: KeyEvent) -> Option<Modal> {
        let n = Self::SETTINGS;
        let dir: i32 = match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                return Some(Modal::Settings { cursor: (cursor + n - 1) % n });
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                return Some(Modal::Settings { cursor: (cursor + 1) % n });
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char(',') => return None,
            KeyCode::Left | KeyCode::Char('h') => -1,
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter | KeyCode::Char(' ') => 1,
            _ => return Some(Modal::Settings { cursor }),
        };
        let cycle = |i: usize, len: usize| ((i as i32 + dir).rem_euclid(len as i32)) as usize;
        let c = &mut self.engine.config;
        match cursor {
            0 => {
                let modes = [BrowserMode::Isolated, BrowserMode::Private, BrowserMode::Default];
                let i = modes.iter().position(|m| *m == c.browser.mode).unwrap_or(0);
                c.browser.mode = modes[cycle(i, modes.len())];
            }
            1 => {
                let mut names = vec!["auto".to_string()];
                names.extend(self.browsers.iter().map(|b| b.name.clone()));
                let i = names.iter().position(|n| *n == c.browser.app).unwrap_or(0);
                c.browser.app = names[cycle(i, names.len())].clone();
            }
            2 => {
                let mut sources = vec![MailSource::Off];
                if cfg!(target_os = "macos") {
                    sources.push(MailSource::AppleMail);
                }
                sources.push(MailSource::Imap);
                let i = sources.iter().position(|s| *s == c.mail.source).unwrap_or(0);
                c.mail.source = sources[cycle(i, sources.len())];
            }
            3 => c.ui.auto_exit = !c.ui.auto_exit,
            4 => {
                c.ui.usage = !c.ui.usage;
                if c.ui.usage {
                    self.refresh_usage();
                }
            }
            _ => {
                c.ui.hide_emails = !c.ui.hide_emails;
                crate::privacy::set(c.ui.hide_emails);
            }
        }
        if let Err(e) = self.engine.save_config() {
            self.toast(ToastKind::Bad, format!("{e:#}"));
        }
        Some(Modal::Settings { cursor })
    }
}

fn farewell_lines(p: &Profile, already: bool, running: usize) -> Vec<String> {
    let mut detail = vec![];
    if let Some(e) = &p.email {
        detail.push(crate::privacy::email(e).into_owned());
    }
    if let Some(pl) = &p.plan {
        detail.push(pl.clone());
    }
    let mut lines = vec![format!(
        "\x1b[32m✓\x1b[0m {} {} \x1b[1m{}\x1b[0m  \x1b[2m{}\x1b[0m",
        p.provider.label(),
        if already { "is already on" } else { "→" },
        p.shown_name(),
        detail.join(" · ")
    )];
    if running > 0 && !already {
        lines.push(format!(
            "  \x1b[2mrestart {} running {} session{} to pick it up\x1b[0m",
            running,
            p.provider.process_name(),
            if running == 1 { "" } else { "s" }
        ));
    }
    if let Some(note) = crate::cli::credential_env_note(p.provider) {
        lines.push(format!("  \x1b[33m!\x1b[0m \x1b[2m{note}\x1b[0m"));
    }
    lines
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(Modal::Login(v)) = &self.modal {
            v.mail_stop.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use crate::providers::tests::{fake_claude, fake_codex_auth};
    use crate::secrets::Vault;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn sandbox() -> (tempfile::TempDir, App) {
        // SAFETY: every test in the crate sets the same value.
        unsafe { std::env::set_var("ACCOUNTANT_CLAUDE_CREDENTIALS", "file") };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = Paths::sandbox(root);
        let mut engine = Engine::open_at(paths).unwrap();
        engine.vault = Vault::Files(root.join("data/secrets"));
        engine.config.ui.usage = false;
        engine.config.ui.reduced_motion = true;
        for (email, uuid) in [("work@corp.com", "u1"), ("me@icloud.com", "u2")] {
            providers::write_live(Provider::Claude, &engine.paths, &fake_claude(email, uuid, "o", uuid))
                .unwrap();
            engine.save_current(Provider::Claude, None).unwrap();
        }
        std::fs::create_dir_all(&engine.paths.codex_home).unwrap();
        std::fs::write(engine.paths.codex_auth(), fake_codex_auth("me@icloud.com", "c1", "a1", "plus"))
            .unwrap();
        (dir, App::new(engine))
    }

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| super::super::view::draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn login_view(app: &App, code: Option<&str>) -> Box<LoginView> {
        Box::new(LoginView {
            provider: Provider::Claude,
            task: None,
            plan: BrowserPlan {
                config: app.engine.config.browser.clone(),
                root: app.engine.paths.browsers(),
                session_key: "x".into(),
            },
            email: Some("me@icloud.com".into()),
            previous: None,
            started: Instant::now(),
            url: Some("https://example.invalid".into()),
            opened: Some("Helium · own profile".into()),
            status: None,
            phase: Phase::Running,
            result: None,
            created: false,
            mail_rx: None,
            mail_stop: Arc::new(AtomicBool::new(false)),
            mail: MailState::Watching("Apple Mail"),
            code: code.map(|c| FoundCode {
                code: c.into(),
                from: "Anthropic <no-reply@anthropic.com>".into(),
                at: Instant::now() - Duration::from_secs(2),
                copied: true,
            }),
            link: None,
            totp: Some(TotpSecret::parse("JBSWY3DPEHPK3PXP").unwrap()),
            paste: None,
            exit_at: None,
            accepts_code: true,
            adding: false,
        })
    }

    #[test]
    fn home_lists_saved_and_adopted_accounts() {
        let (_d, app) = sandbox();
        // The live Codex login was adopted automatically on start.
        assert_eq!(app.engine.registry.profiles.len(), 3);
        let screen = render(&app, 100, 30);
        assert!(screen.contains("CLAUDE CODE"), "{screen}");
        assert!(screen.contains("work@corp.com"));
        assert!(screen.contains("me@icloud.com"));
        assert!(screen.contains("saved current login"), "adoption toast:\n{screen}");
    }

    #[test]
    fn login_view_shows_the_received_code_big() {
        let (_d, mut app) = sandbox();
        app.modal = Some(Modal::Login(login_view(&app, Some("482917"))));
        let screen = render(&app, 100, 34);
        assert!(screen.contains("waiting for you in Helium"), "{screen}");
        assert!(screen.contains("copied — paste it in the browser"), "{screen}");
        // Top row of the big "4 8 2   9 1 7".
        assert!(screen.contains("█ █ █▀█ ▀▀█   █▀█ ▀█  ▀▀█"), "{screen}");
        assert!(screen.contains("watching Apple Mail") || screen.contains("code received"));
    }

    #[test]
    fn hide_emails_masks_every_address_on_screen() {
        let (_d, mut app) = sandbox();
        crate::privacy::set(true);
        let home = render(&app, 100, 30);
        app.modal = Some(Modal::Login(login_view(&app, Some("123456"))));
        let login = render(&app, 100, 34);
        crate::privacy::set(false);
        for screen in [&home, &login] {
            assert!(!screen.contains("work@corp.com"), "{screen}");
            assert!(!screen.contains("me@icloud.com"), "{screen}");
        }
        assert!(home.contains("wo•••@co•••.com"), "{home}");
        assert!(home.contains("m•••@icloud.com"), "{home}");
        assert!(login.contains("m•••@icloud.com"), "{login}");
    }

    #[test]
    fn every_screen_renders_at_any_size() {
        let (_d, mut app) = sandbox();
        let id = app.profiles()[0].id.clone();
        type MakeModal = Box<dyn Fn(&mut App) -> Modal>;
        let mut modals: Vec<MakeModal> = vec![
            Box::new(|_| Modal::Help),
            Box::new(|_| Modal::Add { cursor: 1 }),
            Box::new(|_| Modal::Settings { cursor: 2 }),
            Box::new(|a| Modal::Login(login_view(a, Some("123456")))),
            Box::new(|a| Modal::Login(login_view(a, None))),
            Box::new(|a| {
                let mut v = login_view(a, None);
                v.phase = Phase::Failed("something went wrong with a fairly long explanation".into());
                Modal::Login(v)
            }),
        ];
        let id2 = id.clone();
        modals.push(Box::new(move |a| {
            let p = a.engine.registry.get(&id2).unwrap().clone();
            a.twofa_view(&p)
        }));
        let id3 = id.clone();
        modals.push(Box::new(move |a| a.start_switch(&id3).unwrap_or(Modal::Help)));
        for make in &modals {
            let m = make(&mut app);
            app.modal = Some(m);
            for (w, h) in [(120, 40), (80, 24), (60, 18), (40, 12), (20, 6), (8, 3), (1, 1)] {
                render(&app, w, h);
            }
        }
    }

    const LONG_ERR: &str = "Claude Code: could not read the saved login for 'work': security: \
        SecKeychainSearchCopyNext: The specified item could not be found in the keychain. (exit status 44) \
        — run `accountant doctor` for details";

    #[test]
    fn long_errors_wrap_instead_of_being_cut_off() {
        let (_d, mut app) = sandbox();
        app.toast(ToastKind::Bad, LONG_ERR);
        let home = render(&app, 100, 30);
        assert!(home.contains("SecKeychainSearchCopyNext"), "{home}");
        assert!(home.contains("for details"), "{home}");

        let to = app.profiles()[1].clone();
        app.toast = None;
        app.modal = Some(Modal::Switch(Box::new(SwitchView {
            sw: None,
            provider: to.provider,
            from_name: Some("work".into()),
            to,
            stage: 1,
            stage_ran: true,
            stage_started: Instant::now(),
            started: Instant::now(),
            phase: Phase::Failed(LONG_ERR.into()),
            exit_at: None,
            running: 0,
        })));
        app.modal_at = Instant::now() - Duration::from_secs(1);
        let switch = render(&app, 100, 30);
        assert!(switch.contains("for details"), "{switch}");
        assert!(switch.contains("sign in again"), "{switch}");

        let mut v = login_view(&app, None);
        v.phase = Phase::Failed(LONG_ERR.into());
        app.modal = Some(Modal::Login(v));
        let login = render(&app, 100, 30);
        assert!(login.contains("for details"), "{login}");
    }

    #[test]
    fn usage_shows_every_window_and_when_it_resets() {
        let (_d, mut app) = sandbox();
        let now = Utc::now();
        let window = |label: &str, used: f64, mins: i64| usage::Window {
            label: label.into(),
            used,
            resets_at: Some(now + chrono::Duration::minutes(mins) + chrono::Duration::seconds(30)),
        };
        let ids: Vec<String> = app.profiles().iter().map(|p| p.id.clone()).collect();
        let usage = |windows| Usage { windows, fetched_at: now };
        app.usage
            .by_profile
            .insert(ids[0].clone(), usage(vec![window("5h", 100.0, 226), window("7d", 61.0, 4560)]));
        app.usage
            .by_profile
            .insert(ids[1].clone(), usage(vec![window("5h", 42.0, 132), window("7d opus", 88.0, 7200)]));
        let screen = render(&app, 100, 30);
        for want in ["back in 3h 46m", "↻ in 3d 4h", "↻ in 2h 12m", "7d opus", "88%", "↻ in 5d"] {
            assert!(screen.contains(want), "{want}:\n{screen}");
        }
    }

    #[test]
    fn switching_runs_all_stages_and_reports_success() {
        let (_d, mut app) = sandbox();
        let target = app.profiles().iter().find(|p| !app.is_active(p)).unwrap().id.clone();
        app.modal = app.start_switch(&target);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            app.tick();
            if let Some(Modal::Switch(v)) = &app.modal {
                if matches!(v.phase, Phase::Success { .. }) {
                    break;
                }
                assert!(!matches!(v.phase, Phase::Failed(_)), "{:?}", v.phase);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(app.active[0].as_deref(), Some(target.as_str()));
        assert!(render(&app, 100, 34).contains("you're on"));
        assert!(!app.farewell.is_empty());
    }
}
