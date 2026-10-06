//! Copy text to the system clipboard via the platform's CLI tool.

use anyhow::{Result, bail};
use std::io::Write;
use std::process::{Command, Stdio};

pub fn copy(text: &str) -> Result<()> {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])]
    };
    for (bin, args) in candidates {
        let Ok(mut child) = Command::new(bin)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        child.stdin.take().unwrap().write_all(text.as_bytes())?;
        if child.wait()?.success() {
            return Ok(());
        }
    }
    bail!("no clipboard tool found")
}
