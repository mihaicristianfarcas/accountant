//! "Hide emails" mode: partially mask email addresses wherever they are shown,
//! so the tool can appear in screenshots and screen recordings.
//!
//! The flag is per thread: everything that renders runs on the main thread,
//! and tests stay independent of each other.

use regex::Regex;
use std::borrow::Cow;
use std::cell::Cell;
use std::sync::LazyLock;

thread_local! {
    static HIDE: Cell<bool> = const { Cell::new(false) };
}

pub fn set(on: bool) {
    HIDE.with(|h| h.set(on));
}

pub fn enabled() -> bool {
    HIDE.with(Cell::get)
}

/// Mailbox providers whose domain says nothing about you; kept readable.
const PUBLIC_DOMAINS: &[&str] = &[
    "gmail.com",
    "googlemail.com",
    "icloud.com",
    "me.com",
    "mac.com",
    "outlook.com",
    "hotmail.com",
    "live.com",
    "yahoo.com",
    "proton.me",
    "protonmail.com",
    "pm.me",
    "fastmail.com",
    "hey.com",
    "aol.com",
    "gmx.com",
    "gmx.net",
    "zoho.com",
];

static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}").unwrap()
});

/// `mihai@icloud.com` → `mi•••@icloud.com`, `work@corp.com` → `wo•••@co•••.com`.
/// The mask has a fixed length so it does not reveal how long the part is.
pub fn mask(email: &str) -> String {
    let Some((local, domain)) = email.rsplit_once('@') else {
        return hide(email);
    };
    let domain = if domain.is_empty() || PUBLIC_DOMAINS.contains(&domain.to_lowercase().as_str()) {
        domain.to_string()
    } else {
        match domain.rsplit_once('.') {
            Some((name, tld)) if !name.is_empty() && !tld.is_empty() => format!("{}.{tld}", hide(name)),
            _ => hide(domain),
        }
    };
    format!("{}@{domain}", hide(local))
}

fn hide(s: &str) -> String {
    let n = s.chars().count();
    if n == 0 {
        return String::new();
    }
    let keep = if n <= 3 { 1 } else { 2 };
    format!("{}•••", s.chars().take(keep).collect::<String>())
}

/// An email address as it should be displayed right now.
pub fn email(s: &str) -> Cow<'_, str> {
    if enabled() { Cow::Owned(mask(s)) } else { Cow::Borrowed(s) }
}

/// Free text (status lines, errors, toasts) with any addresses in it masked.
pub fn text(s: &str) -> Cow<'_, str> {
    if enabled() { EMAIL.replace_all(s, |c: &regex::Captures| mask(&c[0])) } else { Cow::Borrowed(s) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_local_part_and_private_domains() {
        assert_eq!(mask("mihai@icloud.com"), "mi•••@icloud.com");
        assert_eq!(mask("work@corp.com"), "wo•••@co•••.com");
        assert_eq!(mask("side.project@proton.me"), "si•••@proton.me");
        assert_eq!(mask("a@x.io"), "a•••@x•••.io");
        assert_eq!(mask("me@Mail.Corp.co.uk"), "m•••@Ma•••.uk");
        // Partial input while typing.
        assert_eq!(mask("mihai"), "mi•••");
        assert_eq!(mask("mihai@"), "mi•••@");
    }

    #[test]
    fn only_applies_when_enabled() {
        set(false);
        assert_eq!(email("work@corp.com"), "work@corp.com");
        assert_eq!(text("signed in as work@corp.com."), "signed in as work@corp.com.");
        set(true);
        assert_eq!(email("work@corp.com"), "wo•••@co•••.com");
        assert_eq!(text("signed in as work@corp.com."), "signed in as wo•••@co•••.com.");
        set(false);
    }
}
