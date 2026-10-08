# accountant

```
▄▀█ █▀▀ █▀▀ █▀█ █ █ █▄ █ ▀█▀ ▄▀█ █▄ █ ▀█▀
█▀█ █▄▄ █▄▄ █▄█ █▄█ █ ▀█  █  █▀█ █ ▀█  █
        switch accounts, not browsers
```

Instant account switching for **Claude Code**, **Codex**, **OpenCode**, **Antigravity**, **Cursor** and
**GitHub Copilot CLI**. When you hit a usage limit:

```
accountant      →  ⏎      →  ready to go
```

**[mihaicristianfarcas.github.io/accountant](https://mihaicristianfarcas.github.io/accountant/)**: try the
switcher in your browser.

The cursor already rests on the account with the most headroom, so <kbd>⏎</kbd> switches to it. The switch
takes well under a second, opens no browser, asks for no 2FA, and accountant quits on its own afterwards.

## How it works

Each tool keeps a long-lived login on disk:

| | live login |
|---|---|
| Claude Code | Keychain item `Claude Code-credentials` (or `~/.claude/.credentials.json`) + `oauthAccount` in `~/.claude.json` |
| Codex | `~/.codex/auth.json` |
| OpenCode | rows of the `credential` table in `~/.local/share/opencode/opencode.db` (OpenCode 1: `auth.json` there) |
| Antigravity | Keychain item `gemini` / `antigravity` (Antigravity 2.0, `agy`) + `antigravityUnifiedStateSync.*` rows in the IDE's `state.vscdb` |
| Cursor | Keychain items `cursor-access-token` / `cursor-refresh-token` + `cursorAuth/*` rows in the app's `state.vscdb` (+ `~/.cursor/auth.json` where present) |
| Copilot CLI | `lastLoggedInUser` in `~/.copilot/config.json`, picking one of the GitHub users Copilot keeps a token for |

accountant keeps one saved copy per account in your **macOS Keychain**. To switch accounts, it swaps the
saved copy into place, byte for byte. A sign-in, and with it 2FA, is only needed the first time you add an
account, or if a provider later revokes the session. Copilot is the exception that proves the rule: it
already keeps a token per GitHub user, so accountant only changes which one is active and stores no secret
for it at all.

The CLIs rotate refresh tokens, so a saved copy goes stale the moment the CLI refreshes it. To prevent
that, accountant first saves the outgoing live login back into its own profile, and only then swaps. A
login is never thrown away: accounts you signed into outside accountant are adopted automatically.

```
 switch work → personal
   ✓ saving current session      live login → vault[work]       (keeps rotated tokens)
   ✓ unlocking saved login       vault[personal]
   ✓ swapping credentials        → Keychain / auth.json, patch ~/.claude.json
   ✓ verifying                   live login is personal
```

## Install

**Apple Silicon Mac** (macOS 11 or newer): the prebuilt binary from the
[latest release](https://github.com/mihaicristianfarcas/accountant/releases/latest).

```sh
mkdir -p ~/.local/bin
curl -fsSL https://github.com/mihaicristianfarcas/accountant/releases/latest/download/accountant-aarch64-apple-darwin.tar.gz | tar xz -C ~/.local/bin
```

`~/.local/bin` is where Claude Code installs itself too. If your shell can't find `accountant`, add the folder
to your PATH: `echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc`.

**Any Mac, from source** (needs [Rust](https://rustup.rs)):

```sh
cargo install --git https://github.com/mihaicristianfarcas/accountant --locked
```

Then run it once:

```sh
accountant            # the first run saves whatever you're signed in to right now
accountant doctor     # what accountant found for each tool
```

## Signing in, and 2FA

Adding an account (<kbd>a</kbd>) or refreshing a revoked one (<kbd>r</kbd>) signs in the way each tool does:

| | sign-in |
|---|---|
| Claude Code, Codex | in the browser, with accountant doing the legwork (below) |
| OpenCode | `opencode auth login` |
| Cursor | `cursor-agent login` |
| Copilot CLI | `copilot login` |
| Antigravity | from the Antigravity app; then <kbd>a</kbd> → *Save current* (or `accountant save antigravity`) |

For the commands, the switcher steps aside and hands the terminal to the tool; it saves the login that was
live first, and the new one when the command finishes. For Claude Code and Codex:

- **One browser, isolated sessions.** Each account's sign-in opens in its *own* persistent browser profile
  (Chromium `--user-data-dir` / Firefox `-profile`). This is the same "one browser per account" setup you
  had before, using a single browser app. Because those cookies persist, signing in again is usually one
  click. Other modes: a fresh private window every time, or your default browser. Configure this in
  settings (<kbd>,</kbd>).
- **Email codes, caught for you.** While a sign-in is open, accountant watches your inbox for the code from
  Anthropic or OpenAI. When the code arrives, it shows it in big digits and copies it to the clipboard, so
  you just paste. Sources: **Apple Mail** (no password; macOS asks once for Automation access) or any
  **IMAP** inbox (iCloud, Gmail, Fastmail… with an app-specific password kept in the Keychain). Magic
  links are detected too: <kbd>m</kbd> opens one in that account's browser session.
- **TOTP, no phone.** Store an account's authenticator setup key (<kbd>t</kbd> then <kbd>s</kbd>).
  accountant generates the codes and shows them during sign-in; <kbd>y</kbd> copies one.

Under the hood:

- **Claude Code:** accountant drives the official `claude auth login --email …` with `$BROWSER` pointed at a
  shim, so the authorize URL comes to accountant instead of your default browser. Claude Code then stores
  the login itself, with exactly its own scopes and format.
- **Codex:** Codex ignores `$BROWSER`, so accountant runs Codex's open-source OAuth PKCE flow itself. It uses
  the same client and the same `127.0.0.1:1455` callback, and writes an identical `auth.json`.

> **Why not fully browserless?** The sign-in pages themselves are protected by bot detection, and scripting
> them would be brittle and against the providers' terms. accountant goes the other way: it keeps sessions
> alive so the browser is almost never needed.

## Usage

```
accountant                  interactive switcher
accountant ls               list accounts (with usage, when known)
accountant use <who>        switch: name, email, number or id
accountant use work         a name shared by accounts of different tools switches them all
accountant next [tool]      switch to the account with the most headroom (claude, codex, opencode, …)
accountant add              add an account (sign in, or save current)
accountant login <who>      sign in again (expired/revoked), or a new one: `login cursor`,
                            `login claude --email me@x.com`
accountant save [name]      save what's signed in right now
accountant totp <who>       print (and copy) the current 2FA code; --set <key> | --clear
accountant mail apple       read sign-in codes from Apple Mail   (also: imap --user …, off, test)
accountant rename <who> <name> · rm <who> · status · doctor
```

| key | |
|---|---|
| <kbd>⏎</kbd> / <kbd>1</kbd>–<kbd>9</kbd> | switch |
| <kbd>a</kbd> | add an account |
| <kbd>r</kbd> | sign in again |
| <kbd>t</kbd> | 2FA codes (TOTP) |
| <kbd>n</kbd> / <kbd>d</kbd> | rename / remove |
| <kbd>u</kbd> | refresh usage meters |
| <kbd>p</kbd> | hide / show emails (for recordings) |
| <kbd>,</kbd> | settings |
| <kbd>?</kbd> | help |

Each row shows an account's usage, when known:

- `▰▰▰▱▱ 62% 5h`: usage of its tightest rate-limit window.
- `◷ 1h12m 5h`: limited, with the time until the window resets.
- `✓ reset · ready`: the window has reset since it was last seen.

Meters are best effort, and for now only Claude Code and Codex have them. accountant only queries accounts
whose short-lived access token is still valid, never refreshes tokens to do so, and talks only to each
provider's own API host.

The list shows Claude Code and Codex always, and the other tools once they have an account.

## Recording & screenshots

Press <kbd>p</kbd> (or turn on *hide emails* in settings) before you record. Every address accountant
shows is then partially masked, in the TUI and in `accountant ls` / `status` alike:

```
mihai@icloud.com   →  mi•••@icloud.com      public mail domains stay readable
work@corp.com      →  wo•••@co•••.com       custom domains are masked too
```

Account names that come from the email, such as the default "mihai", are masked the same way. Names you
choose yourself, like "personal", stay readable. The setting is saved; press <kbd>p</kbd> again to show
everything. For a one-off, set `ACCOUNTANT_HIDE_EMAILS=1`.

## Configuration

`~/.config/accountant/config.toml` (all optional; most of it is editable in the settings screen):

```toml
[browser]
mode = "isolated"          # isolated | private | default
app = "auto"               # or "Google Chrome", "Helium", "Brave Browser", "Firefox", …
# command = "open -a Arc {url}"   # fully custom opener

[mail]
source = "apple-mail"      # off | apple-mail | imap
imap_host = "imap.mail.me.com"
imap_user = "you@icloud.com"
imap_port = 993
imap_folder = "INBOX"

[ui]
auto_exit = true           # quit right after a successful switch
usage = true               # usage meters
reduced_motion = false
hide_emails = false        # mask addresses for screenshots / recordings (p)
```

## Security

- Saved logins, TOTP secrets and the IMAP password live in the macOS Keychain (service `accountant`). They
  pass through `/usr/bin/security` on stdin, never on the command line. On other systems they are `0600`
  files in a `0700` directory.
- `~/.config/accountant/profiles.json` holds only metadata: names, emails, plans, timestamps, and a
  truncated hash of each refresh token, used to recognise the live login.
- Credential files accountant writes are always created `0600`, atomically.
- Network traffic:
  - Codex sign-in talks to `auth.openai.com`.
  - Usage meters talk to `api.anthropic.com` and `chatgpt.com`.
  - Inbox codes talk to your IMAP host.
  - OpenCode, Cursor and Copilot sign-ins are those tools' own commands, talking to their own providers.
  - Nothing else leaves the machine.
- The apps' SQLite databases (OpenCode, Cursor, Antigravity) are read and written through `sqlite3` with
  the SQL on stdin and every value as a hex literal, so no token reaches the process list. A switch
  replaces only the login rows, in one transaction.
- Use it with accounts you own, within each provider's terms.

## Sessions you already have open

accountant never restarts, kills or signals a running CLI or app. It only swaps the login on disk.
Whether an open session follows depends on the tool:

- **Claude Code: follows the switch, no restart.** Claude Code checks its stored login before each request
  and adopts one that changed underneath it. Your next message in an already open session goes to the new
  account and counts against its usage. Keychain reads are cached for up to 30 seconds, so a message sent
  within half a minute of the session's previous request can still go out on the old account.
- **Codex: restart it.** Codex keeps the login it started with in memory, so an open session stays on the
  old account. accountant tells you how many are running. If such a session's token expires before you
  restart it, Codex refuses to refresh it ("signed in to another account") rather than undo the switch.
- **OpenCode, Antigravity, Cursor, Copilot CLI: restart them.** New sessions start on the new account.
  Treat open ones (and the Antigravity and Cursor apps) as staying on the old one until restarted. Quit
  the **Cursor app** before switching: it writes its own copy of the login back when it closes.

Checked against Claude Code 2.1.292 and Codex 0.160.0. Older Claude Code releases may need a restart too.

## Safe alongside a running CLI

- **Claude Code's config lock.** `~/.claude.json` is rewritten by every running Claude Code. accountant
  patches `oauthAccount` under Claude Code's own lock (the `proper-lockfile` directory beside the file),
  so neither side loses the other's change.
- **Tokens that rotate mid-switch.** If a CLI refreshes its login between the *save* and *swap* steps,
  the newer tokens are saved too before the swap.
- **A Keychain that does not answer** (locked, dialog left open) times out with a clear message instead
  of freezing the switcher.
- **Revoked sessions are spotted early.** When a provider rejects a still-valid token while reading
  usage, the account is marked *needs sign-in* right away, not when you try to switch to it.

## Caveats

- Running sessions of every tool but Claude Code keep the old login until restarted (see
  [Sessions you already have open](#sessions-you-already-have-open)).
- **How well tested each tool is.** Claude Code, Codex and OpenCode 2 are switched against their real
  installs. Antigravity, Cursor and Copilot CLI follow where each one is documented, and seen by other
  switchers, to keep its login; their flows are covered by tests, but not yet against the apps themselves.
  `accountant doctor` shows what accountant found for each.
- **Cursor and Antigravity Keychain items** belong to those apps. The first time accountant reads or
  replaces one, macOS may ask to allow it; choose *Always Allow*.
- **Copilot CLI** switches between GitHub users it has already signed in (`copilot login` once per
  account; accountant runs it for you on <kbd>a</kbd>). `copilot logout` for a user removes its token, and
  switching to that user then asks you to sign in again.
- **OpenCode** treats all its connected providers together as one account. A Claude (Anthropic) login in
  OpenCode does not say who it belongs to, so add those accounts through accountant (<kbd>a</kbd>, or
  `accountant login opencode`), which is how it tells them apart.
- An API key or token in the environment (`ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`,
  `OPENAI_API_KEY`, `CURSOR_API_KEY`, `GH_TOKEN`, …) makes the CLI ignore the switched login. accountant warns when one is set and
  keeps them out of the sign-ins it runs.
- With `CLAUDE_CONFIG_DIR` (or `CLAUDE_SECURESTORAGE_CONFIG_DIR`) set, Claude Code keeps its login in a
  Keychain item named after that directory; accountant follows it. `ACCOUNTANT_CLAUDE_KEYCHAIN_SERVICE`
  still overrides the item name.
- Codex configured with `cli_auth_credentials_store = "keyring"` is not supported. Use the default file
  store.
- `accountant doctor` checks all of the above.

## Development

```sh
cargo test             # unit + flow tests; nothing touches real credentials
cargo install --path . # install your checkout
```

Every location can be redirected, which is how the tests and a manual sandbox run work:

| variable | effect |
|---|---|
| `ACCOUNTANT_HOME` | accountant's state directory |
| `ACCOUNTANT_SECRETS=file` | file vault instead of the Keychain |
| `ACCOUNTANT_CLAUDE_CREDENTIALS=file` | Claude Code credentials from `.credentials.json` |
| `CLAUDE_CONFIG_DIR`, `CLAUDE_SECURESTORAGE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`, `COPILOT_HOME`, `HOME` | where the CLIs' files are |
| `ACCOUNTANT_APP_SUPPORT` | where desktop apps keep their data (Cursor's and Antigravity's `state.vscdb`) |
| `ACCOUNTANT_KEYCHAIN_DIR` | files instead of the Cursor and Antigravity Keychain items |
| `ACCOUNTANT_SQLITE3` | the `sqlite3` used to read and write the apps' databases |
| `ACCOUNTANT_CLAUDE_BIN` | the `claude` executable used for sign-in |
| `ACCOUNTANT_BROWSER_CMD` | custom browser command |
| `ACCOUNTANT_CODEX_ISSUER` | OAuth issuer, e.g. a local fake |
