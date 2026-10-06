//! The interactive switcher.

mod app;
pub mod theme;
mod view;

use crate::engine::Engine;
use crate::providers::Provider;
use anyhow::Result;
use app::App;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use ratatui::crossterm::execute;
use std::time::Duration;

/// Where the TUI opens.
pub enum Start {
    Home,
    Add,
    /// `adding`: a new account (stay open afterwards) vs. signing in again.
    Login {
        provider: Provider,
        email: Option<String>,
        adding: bool,
    },
}

pub fn run(engine: Engine, start: Start) -> Result<()> {
    let mut app = App::new(engine);
    app.open(start);

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let result = (|| -> Result<()> {
        loop {
            app.tick();
            terminal.draw(|f| view::draw(f, &app))?;
            if app.quit {
                return Ok(());
            }
            // ~30 fps while animating; input is handled as soon as it lands.
            if event::poll(Duration::from_millis(33))? {
                loop {
                    match event::read()? {
                        Event::Key(k) => app.on_key(k),
                        Event::Paste(s) => app.on_paste(&s),
                        _ => {}
                    }
                    if app.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
        }
    })();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();

    for line in &app.farewell {
        println!("{line}");
    }
    result
}
