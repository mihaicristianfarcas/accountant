//! accountant — instant account switching for Claude Code and Codex.

mod browser;
mod cli;
mod clipboard;
mod config;
mod engine;
mod fsutil;
mod login;
mod mail;
mod paths;
mod privacy;
mod providers;
mod registry;
mod secrets;
mod tui;
mod twofa;
mod usage;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "accountant",
    version,
    about = "Instant account switching for Claude Code and Codex",
    long_about = "Instant account switching for Claude Code and Codex.\n\n\
        Run without arguments for the interactive switcher. Each account's login is kept in\n\
        your Keychain; switching swaps it in place — no browser, no 2FA. The browser is only\n\
        needed the first time (or when a provider revokes a session), and accountant can fetch\n\
        the emailed code or generate the TOTP code for you then."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List saved accounts
    #[command(alias = "ls")]
    List,
    /// Switch to an account: name, email, number or id. A name shared by a Claude Code
    /// and a Codex account switches both.
    #[command(alias = "switch", alias = "u")]
    Use {
        who: String,
        /// Only consider this provider (claude | codex)
        #[arg(short, long)]
        provider: Option<String>,
    },
    /// Switch to the account with the most headroom (round-robin when unknown)
    #[command(alias = "n")]
    Next {
        /// claude | codex (optional when only one has several accounts)
        provider: Option<String>,
    },
    /// Save whatever is signed in right now as an account
    Save {
        /// Name for the account (defaults to the email's local part)
        name: Option<String>,
        #[arg(short, long)]
        provider: Option<String>,
    },
    /// Add an account (opens the picker: sign in to Claude Code / Codex, or save current)
    Add,
    /// Sign in with the browser: a new account (`login claude`) or refresh one (`login work`)
    Login {
        /// A provider (claude | codex) or an existing account
        target: String,
        /// Account email: prefills the sign-in and isolates its browser session
        #[arg(short, long)]
        email: Option<String>,
    },
    /// Rename an account
    #[command(alias = "mv")]
    Rename {
        who: String,
        name: String,
        #[arg(short, long)]
        provider: Option<String>,
    },
    /// Forget an account (the live login is left alone)
    #[command(alias = "rm")]
    Remove {
        who: String,
        #[arg(short, long)]
        provider: Option<String>,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Print the current TOTP code of an account, or store its secret
    #[command(alias = "2fa", alias = "code")]
    Totp {
        who: String,
        /// Store a setup key / otpauth:// URI ("-" reads it from stdin)
        #[arg(long, conflicts_with = "clear")]
        set: Option<String>,
        /// Delete the stored secret
        #[arg(long)]
        clear: bool,
        #[arg(short, long)]
        provider: Option<String>,
    },
    /// Configure where sign-in codes are read from: off | apple | imap | test
    Mail {
        /// off | apple | imap | test (no argument shows the current setting)
        source: Option<String>,
        /// IMAP login, e.g. you@icloud.com
        #[arg(long)]
        user: Option<String>,
        /// IMAP host (guessed from the address when omitted)
        #[arg(long)]
        host: Option<String>,
        #[arg(long, default_value_t = 993)]
        port: u16,
    },
    /// Show which accounts are live right now
    #[command(alias = "st")]
    Status,
    /// Check the setup: CLIs, credential stores, browsers, mail
    Doctor,
}

fn main() {
    let args = Cli::parse();
    let result = match args.cmd {
        None => cli::interactive(tui::Start::Home),
        Some(Cmd::List) => cli::list(),
        Some(Cmd::Use { who, provider }) => cli::use_account(&who, provider.as_deref()),
        Some(Cmd::Next { provider }) => cli::next(provider.as_deref()),
        Some(Cmd::Save { name, provider }) => cli::save(name.as_deref(), provider.as_deref()),
        Some(Cmd::Add) => cli::add(),
        Some(Cmd::Login { target, email }) => cli::login(&target, email),
        Some(Cmd::Rename { who, name, provider }) => cli::rename(&who, &name, provider.as_deref()),
        Some(Cmd::Remove { who, provider, yes }) => cli::remove(&who, provider.as_deref(), yes),
        Some(Cmd::Totp { who, set, clear, provider }) => cli::totp(&who, set, clear, provider.as_deref()),
        Some(Cmd::Mail { source, user, host, port }) => cli::mail(source.as_deref(), user, host, port),
        Some(Cmd::Status) => cli::status(),
        Some(Cmd::Doctor) => cli::doctor(),
    };
    if let Err(e) = result {
        eprintln!("{} {}", cli::paint("31", "✕"), privacy::text(&format!("{e:#}")));
        std::process::exit(1);
    }
}
