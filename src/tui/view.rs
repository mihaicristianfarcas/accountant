//! Rendering. Everything is drawn straight into the frame buffer so effects
//! (the logo decode, the transfer track, rolling digits) stay cheap.

use super::app::{
    App, ConfirmView, InputView, LoginView, MailState, Modal, Phase, SwitchView, ToastKind, TwoFaView,
};
use super::theme::{self, *};
use crate::engine::Stage;
use crate::privacy;
use crate::providers::Provider;
use crate::registry::Profile;
use chrono::Utc;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Margin, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};
use std::time::Instant;

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let buf = f.buffer_mut();
    let secs = app.secs();
    let dim = app.modal.is_some();

    let w = area.width.saturating_sub(4).min(80);
    let x = area.x + (area.width - w) / 2;
    // Sit the block a little above the vertical centre when there is room.
    let rows = app.engine.registry.profiles.len() as u16 + 2 * Provider::ALL.len() as u16 + 1;
    let block = 5 + rows + 3;
    let mut y = area.y + 1 + area.height.saturating_sub(block + 2) / 3;
    if area.height >= 20 && area.width >= 46 {
        draw_logo(buf, area, y, secs, dim, app.reduced_motion());
        y += 3;
        let tag = "switch accounts, not browsers";
        let tx = area.x + (area.width.saturating_sub(tag.chars().count() as u16)) / 2;
        put(buf, tx, y, &Line::styled(tag, fg(if dim { GHOST } else { FAINT })), w);
        y += 2;
    } else {
        let title = Line::from(vec![
            Span::styled("accountant", bold(theme::gradient(secs * 0.05))),
            Span::styled("  switch accounts, not browsers", fg(FAINT)),
        ]);
        put(buf, x, y, &title, w);
        y += 2;
    }

    let footer_y = area.bottom().saturating_sub(1);
    let toast_y = footer_y.saturating_sub(1);
    let list = Rect { x, y, width: w, height: toast_y.saturating_sub(y + 1) };
    draw_list(buf, app, list, secs, dim);
    draw_toast(buf, app, Rect { x, y: toast_y, width: w, height: 1 });
    draw_footer(buf, Rect { x, y: footer_y, width: w, height: 1 }, dim);

    if let Some(m) = &app.modal {
        draw_modal(buf, area, app, m, secs);
    }
}

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

fn put(buf: &mut Buffer, x: u16, y: u16, line: &Line, max_w: u16) {
    let a = buf.area;
    if y < a.y || y >= a.bottom() || x >= a.right() {
        return;
    }
    let w = max_w.min(a.right() - x);
    buf.set_line(x, y, line, w);
}

fn put_char(buf: &mut Buffer, x: u16, y: u16, ch: char, style: Style) {
    if buf.area.contains(Position { x, y }) {
        buf[(x, y)].set_char(ch).set_style(style);
    }
}

fn centered_line(buf: &mut Buffer, r: Rect, y: u16, line: &Line) {
    let lw = line.width() as u16;
    let x = r.x + r.width.saturating_sub(lw) / 2;
    put(buf, x, y, line, r.width);
}

fn faded(line: Line<'static>, t: f32) -> Line<'static> {
    if t <= 0.0 {
        return line;
    }
    let spans = line
        .spans
        .into_iter()
        .map(|mut s| {
            if let Some(c) = s.style.fg {
                s.style.fg = Some(mix(c, GHOST, t));
            }
            s
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn truncate(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(w - 1).collect();
    out.push('…');
    out
}

fn pad(s: &str, w: usize) -> String {
    let t = truncate(s, w);
    let n = t.chars().count();
    format!("{t}{}", " ".repeat(w.saturating_sub(n)))
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![];
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Word-wrapped text; returns the number of lines drawn.
fn paragraph(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    style: Style,
    max_lines: usize,
) -> u16 {
    let lines = wrap(text, width as usize);
    let n = lines.len().min(max_lines);
    for (row, l) in (y..).zip(lines.into_iter().take(n)) {
        put(buf, x, row, &Line::styled(l, style), width);
    }
    n as u16
}

fn hints(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![];
    for (i, (k, label)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", fg(FAINT)));
        }
        spans.push(Span::styled(k.to_string(), bold(DIM)));
        spans.push(Span::styled(format!(" {label}"), fg(FAINT)));
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// Header & logo
// ---------------------------------------------------------------------------

fn draw_logo(buf: &mut Buffer, area: Rect, y: u16, secs: f32, dim: bool, reduced: bool) {
    let lw = LOGO[0].chars().count() as u16;
    let x0 = area.x + area.width.saturating_sub(lw) / 2;
    let frame = (secs * 24.0) as u64;
    for (row, line) in LOGO.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            if ch == ' ' {
                continue;
            }
            let reveal = if reduced {
                0.0
            } else {
                0.05 + col as f32 / lw as f32 * 0.5 + noise(col as u64, row as u64) * 0.2
            };
            let (x, yy) = (x0 + col as u16, y + row as u16);
            if secs < reveal {
                if secs > reveal - 0.3 {
                    let g = SCRAMBLE[(noise(col as u64 * 7 + row as u64, frame) * 6.0) as usize % 6];
                    put_char(buf, x, yy, g, fg(mix(GHOST, DIM, noise(col as u64, frame))));
                }
                continue;
            }
            // A slow gradient drifting across the letters.
            let flow = col as f32 / 64.0 - secs * 0.06 + row as f32 * 0.03;
            let mut c = theme::gradient(flow);
            let since = secs - reveal;
            if since < 0.3 {
                c = mix(Color::Rgb(255, 255, 255), c, since / 0.3);
            }
            if dim {
                c = mix(c, GHOST, 0.65);
            }
            put_char(buf, x, yy, ch, fg(c));
        }
    }
}

// ---------------------------------------------------------------------------
// Account list
// ---------------------------------------------------------------------------

enum Item {
    Header(Provider),
    Row(usize),
    Empty(Provider),
    Gap,
}

fn draw_list(buf: &mut Buffer, app: &App, area: Rect, secs: f32, dim: bool) {
    let profiles = app.profiles();
    let mut items = vec![];
    for (n, p) in Provider::ALL.into_iter().enumerate() {
        if n > 0 {
            items.push(Item::Gap);
        }
        items.push(Item::Header(p));
        let rows: Vec<usize> =
            profiles.iter().enumerate().filter(|(_, x)| x.provider == p).map(|(i, _)| i).collect();
        if rows.is_empty() {
            items.push(Item::Empty(p));
        }
        items.extend(rows.into_iter().map(Item::Row));
    }

    // Scroll so the cursor stays visible.
    let h = area.height as usize;
    let cursor_pos = items.iter().position(|it| matches!(it, Item::Row(i) if *i == app.cursor)).unwrap_or(0);
    let offset = if items.len() <= h { 0 } else { (cursor_pos + 2).saturating_sub(h).min(items.len() - h) };

    let reduced = app.reduced_motion();
    for (appear, (line_no, item)) in items.iter().enumerate().skip(offset).take(h).enumerate() {
        let y = area.y + (line_no - offset) as u16;
        // Staggered entrance.
        let t0 = 0.35 + appear as f32 * 0.045;
        let prog = if reduced { 1.0 } else { ease_out((secs - t0) / 0.3) };
        if prog <= 0.0 {
            continue;
        }
        let shift = ((1.0 - prog) * 6.0).round() as u16;
        let fade = (1.0 - prog).max(if dim { 0.62 } else { 0.0 });
        match item {
            Item::Gap => {}
            Item::Header(p) => {
                let label = p.label().to_uppercase();
                let mut spans = vec![
                    Span::styled("◆ ", fg(accent(*p))),
                    Span::styled(label.clone(), bold(accent(*p))),
                    Span::raw(" "),
                ];
                let rule = (area.width as usize).saturating_sub(label.chars().count() + 4);
                spans.push(Span::styled("─".repeat(rule), fg(GHOST)));
                put(buf, area.x + shift, y, &faded(Line::from(spans), fade), area.width);
            }
            Item::Empty(p) => {
                let line = Line::from(vec![
                    Span::styled("    no saved accounts — ", fg(FAINT)),
                    Span::styled("a", bold(DIM)),
                    Span::styled(format!(" to sign in to {}", p.label()), fg(FAINT)),
                ]);
                put(buf, area.x + shift, y, &faded(line, fade), area.width);
            }
            Item::Row(i) => {
                let p = &profiles[*i];
                let selected = *i == app.cursor;
                if selected && !dim {
                    let glow = mix(ROW_HL, accent(p.provider), 0.06 + 0.04 * pulse(secs, 2.4));
                    buf.set_style(
                        Rect { x: area.x, y, width: area.width, height: 1 },
                        Style::default().bg(glow),
                    );
                }
                let line = row_line(app, p, *i, selected, area.width as usize, secs);
                put(buf, area.x + shift, y, &faded(line, fade), area.width - shift.min(area.width));
            }
        }
    }
}

fn row_line(app: &App, p: &Profile, index: usize, selected: bool, width: usize, secs: f32) -> Line<'static> {
    let active = app.is_active(p);
    let acc = accent(p.provider);
    let mut spans = vec![];

    spans.push(if selected {
        Span::styled("▌", fg(mix(acc, Color::Rgb(255, 255, 255), 0.25 * pulse(secs, 1.6))))
    } else {
        Span::raw(" ")
    });
    let idx = if index < 9 { format!("{}", index + 1) } else { " ".into() };
    spans.push(Span::styled(format!(" {idx} "), fg(if selected { DIM } else { FAINT })));

    let (dot, dot_style) = if p.needs_login {
        ("◌", fg(WARN))
    } else if active {
        ("●", fg(mix(OK, Color::Rgb(220, 255, 225), 0.45 * pulse(secs, 2.2))))
    } else {
        ("○", fg(FAINT))
    };
    spans.push(Span::styled(dot, dot_style));
    spans.push(Span::raw(" "));

    // Fixed columns; the email takes what is left.
    const NAME: usize = 14;
    const PLAN: usize = 9;
    const STATUS: usize = 17;
    let fixed = 1 + 3 + 2 + NAME + 2 + PLAN + 1 + STATUS;
    let email_w = width.saturating_sub(fixed + 2);

    let name_style = if selected { bold(TEXT) } else { fg(TEXT) };
    spans.push(Span::styled(pad(&p.shown_name(), NAME), name_style));
    spans.push(Span::raw("  "));
    if email_w >= 8 {
        let email = p.email.as_deref().map_or_else(|| "—".into(), |e| privacy::email(e).into_owned());
        spans.push(Span::styled(pad(&email, email_w), fg(if selected { DIM } else { FAINT })));
        spans.push(Span::raw("  "));
    }
    spans.push(Span::styled(pad(p.plan.as_deref().unwrap_or(""), PLAN), fg(FAINT)));
    spans.push(Span::raw(" "));

    let status = status_spans(app, p, active, secs);
    let sw: usize = status.iter().map(|s| s.width()).sum();
    spans.push(Span::raw(" ".repeat(STATUS.saturating_sub(sw))));
    spans.extend(status);
    Line::from(spans)
}

/// A usage meter: the more used, the warmer.
fn meter(used: f64, cells: usize) -> Vec<Span<'static>> {
    let color = if used >= 85.0 {
        ERR
    } else if used >= 60.0 {
        WARN
    } else {
        OK
    };
    bar(used / 100.0, cells, color)
}

/// A countdown: full and green when fresh, red as it runs out.
fn countdown(left: f64, cells: usize) -> Vec<Span<'static>> {
    let color = if left > 0.5 {
        OK
    } else if left > 0.2 {
        WARN
    } else {
        ERR
    };
    bar(left, cells, color)
}

fn bar(frac: f64, cells: usize, color: Color) -> Vec<Span<'static>> {
    let filled = (frac * cells as f64).round().clamp(0.0, cells as f64) as usize;
    vec![Span::styled("▰".repeat(filled), fg(color)), Span::styled("▱".repeat(cells - filled), fg(GHOST))]
}

fn status_spans(app: &App, p: &Profile, active: bool, _secs: f32) -> Vec<Span<'static>> {
    let now = Utc::now();
    if p.needs_login {
        return vec![Span::styled("⚠ sign in (r)", fg(WARN))];
    }
    if let Some(u) = app.usage.by_profile.get(&p.id) {
        let reset_since = u.windows.iter().any(|w| w.used >= 50.0 && w.resets_at.is_some_and(|r| r <= now));
        if let Some(w) = u.binding() {
            if w.used < 1.0 && reset_since {
                return vec![Span::styled("✓ reset · ready", fg(OK))];
            }
            if w.used >= 99.5 {
                let left = w.resets_at.map(|r| short_duration((r - now).num_seconds()));
                return vec![
                    Span::styled("◷ ", fg(ERR)),
                    Span::styled(left.unwrap_or_else(|| "limited".into()), fg(mix(ERR, WARN, 0.3))),
                    Span::styled(format!(" {}", w.label), fg(FAINT)),
                ];
            }
            let mut s = meter(w.used, 5);
            s.push(Span::styled(format!(" {:>3.0}%", w.used), fg(DIM)));
            s.push(Span::styled(format!(" {}", pad(&w.label, 2)), fg(FAINT)));
            return s;
        }
    }
    if active {
        return vec![Span::styled("live", fg(mix(OK, DIM, 0.4)))];
    }
    if let Some(left) = p.left_at {
        return vec![Span::styled(
            format!("rested {}", short_duration((now - left).num_seconds())),
            fg(FAINT),
        )];
    }
    vec![]
}

fn draw_toast(buf: &mut Buffer, app: &App, r: Rect) {
    let Some(t) = &app.toast else { return };
    let age = t.at.elapsed().as_secs_f32();
    let fade = ((age - 3.4) / 0.8).clamp(0.0, 1.0);
    let enter = ease_out(age / 0.2);
    let (icon, color) = match t.kind {
        ToastKind::Info => ("·", DIM),
        ToastKind::Good => ("✓", OK),
        ToastKind::Bad => ("✕", ERR),
    };
    let line = Line::from(vec![
        Span::styled(format!(" {icon} "), fg(color)),
        Span::styled(truncate(&privacy::text(&t.text), r.width.saturating_sub(4) as usize), fg(TEXT)),
    ]);
    let shift = ((1.0 - enter) * 3.0) as u16;
    put(buf, r.x + shift, r.y, &faded(line, fade.max(1.0 - enter)), r.width);
}

fn draw_footer(buf: &mut Buffer, r: Rect, dim: bool) {
    let mut keys: Vec<(&str, &str)> = vec![
        ("⏎", "switch"),
        ("a", "add"),
        ("r", "sign in"),
        ("t", "2fa"),
        ("n", "rename"),
        ("d", "delete"),
        (",", "settings"),
        ("?", "help"),
        ("q", "quit"),
    ];
    for drop in ["d", "n", ",", "t", "r"] {
        if (hints(&keys).width() as u16) <= r.width {
            break;
        }
        keys.retain(|(k, _)| *k != drop);
    }
    let line = hints(&keys);
    let line = if dim { faded(line, 0.7) } else { line };
    put(buf, r.x + 1, r.y, &line, r.width);
}

// ---------------------------------------------------------------------------
// Modals
// ---------------------------------------------------------------------------

fn card(buf: &mut Buffer, area: Rect, w: u16, h: u16, title: &str, color: Color, opened: f32) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let full_h = h.min(area.height.saturating_sub(2));
    let h = ((full_h as f32) * ease_out(opened / 0.14)).round().max(3.0) as u16;
    let r = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height.saturating_sub(full_h)) / 2 + (full_h - h.min(full_h)) / 2,
        width: w,
        height: h.min(full_h),
    };
    Clear.render(r, buf);
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(fg(mix(color, GHOST, 0.45)))
        .title(Line::from(Span::styled(format!(" {title} "), bold(color))))
        .render(r, buf);
    r.inner(Margin { horizontal: 3, vertical: 1 })
}

/// Draw a card's content into a buffer the size of `inner`, so nothing can
/// spill over the border on small terminals.
fn clipped(buf: &mut Buffer, inner: Rect, draw: impl FnOnce(&mut Buffer)) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let mut sub = Buffer::empty(inner);
    draw(&mut sub);
    buf.merge(&sub);
}

fn draw_modal(buf: &mut Buffer, area: Rect, app: &App, m: &Modal, secs: f32) {
    let opened = app.modal_at.elapsed().as_secs_f32();
    match m {
        Modal::Switch(v) => draw_switch(buf, area, v, opened),
        Modal::Login(v) => draw_login(buf, area, v, opened, secs),
        Modal::Add { cursor } => draw_add(buf, area, *cursor, opened),
        Modal::Input(v) => draw_input(buf, area, v, opened, secs),
        Modal::Confirm(v) => draw_confirm(buf, area, v, opened),
        Modal::TwoFa(v) => draw_twofa(buf, area, v, &app.engine.vault.describe(), opened),
        Modal::Settings { cursor } => draw_settings(buf, area, app, *cursor, opened),
        Modal::Help => draw_help(buf, area, opened),
    }
}

fn draw_switch(buf: &mut Buffer, area: Rect, v: &SwitchView, opened: f32) {
    let acc = accent(v.provider);
    let inner = card(buf, area, 60, 13, &format!("switch · {}", v.provider.label()), acc, opened);
    clipped(buf, inner, |buf| {
        if inner.height < 3 {
            return;
        }
        let y0 = inner.y;
        let now = Instant::now();

        // The transfer track: from ─────●───── to
        let from = v.from_name.clone().unwrap_or_else(|| "signed out".into());
        let to = v.to.shown_name().into_owned();
        let stage_frac = (v.stage_started.elapsed().as_secs_f32() / 0.15).min(1.0);
        let prog = match &v.phase {
            Phase::Running => ease_in_out((v.stage as f32 + stage_frac) / Stage::ALL.len() as f32),
            Phase::Success { .. } => 1.0,
            Phase::Failed(_) => v.stage as f32 / Stage::ALL.len() as f32,
        };
        let success = matches!(v.phase, Phase::Success { .. });
        let fw = from.chars().count().min(16);
        let tw = to.chars().count().min(16);
        let track = (inner.width as usize).saturating_sub(fw + tw + 4).max(4);
        let head = ((prog * track as f32) as usize).min(track.saturating_sub(1));
        let t = v.started.elapsed().as_secs_f32();
        let mut spans =
            vec![Span::styled(truncate(&from, 16), fg(if success { FAINT } else { DIM })), Span::raw("  ")];
        for i in 0..track {
            let pos = i as f32 / track as f32;
            let (ch, c) = if success {
                // A shimmer running along the completed track.
                let wave = ((pos * 6.0 - t * 2.5).sin() + 1.0) / 2.0;
                ('━', mix(mix(FAINT, acc, pos), Color::Rgb(255, 255, 255), wave * 0.25))
            } else if i < head {
                ('━', mix(FAINT, acc, pos))
            } else if i == head {
                ('●', mix(acc, Color::Rgb(255, 255, 255), pulse(t, 0.4) * 0.5))
            } else {
                ('─', GHOST)
            };
            spans.push(Span::styled(ch.to_string(), fg(c)));
        }
        spans.push(Span::raw("  "));
        spans.push(Span::styled(truncate(&to, 16), if success { bold(acc) } else { fg(TEXT) }));
        put(buf, inner.x, y0 + 1, &Line::from(spans), inner.width);

        match &v.phase {
            Phase::Running | Phase::Failed(_) => {
                let failed = matches!(v.phase, Phase::Failed(_));
                for (i, stage) in Stage::ALL.iter().enumerate() {
                    let (icon, ic, lc) = if i < v.stage {
                        ("✓".to_string(), OK, DIM)
                    } else if i == v.stage && failed {
                        ("✕".to_string(), ERR, TEXT)
                    } else if i == v.stage {
                        (theme::spinner(t).to_string(), acc, TEXT)
                    } else {
                        ("·".to_string(), GHOST, FAINT)
                    };
                    let line = Line::from(vec![
                        Span::styled(format!("  {icon}  "), fg(ic)),
                        Span::styled(stage.label(), fg(lc)),
                    ]);
                    put(buf, inner.x, y0 + 3 + i as u16, &line, inner.width);
                }
                if let Phase::Failed(msg) = &v.phase {
                    let msg = privacy::text(msg);
                    paragraph(buf, inner.x + 2, y0 + 8, inner.width.saturating_sub(2), &msg, fg(ERR), 2);
                    put(
                        buf,
                        inner.x + 2,
                        y0 + 10,
                        &hints(&[("r", "sign in again"), ("esc", "back")]),
                        inner.width,
                    );
                }
            }
            Phase::Success { at } => {
                let since = now.duration_since(*at).as_secs_f32();
                let title = Line::from(vec![
                    Span::styled("you're on ", fg(TEXT)),
                    Span::styled(v.to.shown_name().into_owned(), bold(acc)),
                ]);
                let ty = y0 + 4;
                centered_line(buf, inner, ty, &title);
                sparkles(buf, inner, ty, title.width() as u16, since);

                let mut detail = vec![];
                if let Some(e) = &v.to.email {
                    detail.push(privacy::email(e).into_owned());
                }
                if let Some(p) = &v.to.plan {
                    detail.push(p.clone());
                }
                centered_line(buf, inner, ty + 1, &Line::styled(detail.join(" · "), fg(DIM)));
                if v.running > 0 {
                    let msg = format!(
                        "↻ restart {} running {} session{} to pick it up",
                        v.running,
                        v.provider.process_name(),
                        if v.running == 1 { "" } else { "s" }
                    );
                    centered_line(buf, inner, ty + 3, &Line::styled(msg, fg(mix(WARN, DIM, 0.35))));
                }
                draw_exit_bar(buf, inner, y0 + 10, v.exit_at, *at);
            }
        }
    });
}

fn draw_exit_bar(buf: &mut Buffer, inner: Rect, y: u16, exit_at: Option<Instant>, from: Instant) {
    match exit_at {
        Some(exit) => {
            let total = exit.saturating_duration_since(from).as_secs_f32().max(0.01);
            let left = exit.saturating_duration_since(Instant::now()).as_secs_f32();
            let cells = 18usize;
            let filled = (((total - left) / total) * cells as f32).round() as usize;
            let line = Line::from(vec![
                Span::styled("▰".repeat(filled.min(cells)), fg(DIM)),
                Span::styled("▱".repeat(cells - filled.min(cells)), fg(GHOST)),
                Span::styled("  ready to go · any key stays", fg(FAINT)),
            ]);
            centered_line(buf, inner, y, &line);
        }
        None => centered_line(buf, inner, y, &hints(&[("⏎", "done"), ("esc", "back to list")])),
    }
}

fn sparkles(buf: &mut Buffer, inner: Rect, y: u16, title_w: u16, since: f32) {
    const GLYPHS: [char; 5] = ['·', '✧', '✦', '✶', '·'];
    let cx = inner.x + inner.width / 2;
    for i in 0..10u64 {
        let born = noise(i, 1) * 0.5;
        let life = 0.9 + noise(i, 2) * 0.6;
        let age = since - born;
        // Burst once, then a slow, sparse twinkle.
        let phase = if age < life { age / life } else { ((age - life) * 0.35 + noise(i, 3)).fract() };
        let g = GLYPHS[((phase * GLYPHS.len() as f32) as usize).min(GLYPHS.len() - 1)];
        let side: i32 = if i % 2 == 0 { -1 } else { 1 };
        let spread = (title_w / 2 + 2) as i32 + (noise(i, 4) * 10.0) as i32;
        let x = cx as i32 + side * spread;
        let dy = -((noise(i, 5) * 2.0) as i32);
        let color = theme::gradient(noise(i, 6) + since * 0.2);
        if age > 0.0 && x >= inner.x as i32 && x < inner.right() as i32 {
            put_char(buf, x as u16, (y as i32 + dy) as u16, g, fg(mix(color, GHOST, phase * 0.5)));
        }
    }
}

/// Big digits with a slot-machine roll: each digit spins, then lands.
fn big_code(buf: &mut Buffer, inner: Rect, y: u16, code: &str, landed_at: Instant, color: Color) {
    let digits: Vec<u8> = code.bytes().filter(u8::is_ascii_digit).map(|b| b - b'0').collect();
    let n = digits.len();
    if n == 0 {
        return;
    }
    let gap = if n == 6 { 2 } else { 0 };
    let width = n as u16 * 4 - 1 + gap;
    let x0 = inner.x + inner.width.saturating_sub(width) / 2;
    let age = landed_at.elapsed().as_secs_f32();
    let frame = (age * 30.0) as u64;
    for (i, &d) in digits.iter().enumerate() {
        let land = 0.12 + i as f32 * 0.07;
        let (shown, c) = if age < land {
            ((noise(i as u64, frame) * 10.0) as usize % 10, FAINT)
        } else {
            let flash = ((age - land) / 0.25).min(1.0);
            (d as usize, mix(Color::Rgb(255, 255, 255), color, flash))
        };
        let x = x0 + i as u16 * 4 + if i >= 3 && n == 6 { gap } else { 0 };
        for (row, glyph) in DIGITS[shown].iter().enumerate() {
            put(buf, x, y + row as u16, &Line::styled(*glyph, bold(c)), 3);
        }
    }
}

fn draw_login(buf: &mut Buffer, area: Rect, v: &LoginView, opened: f32, secs: f32) {
    let acc = accent(v.provider);
    let mut h = 13;
    if v.code.is_some() {
        h += 5;
    }
    if v.totp.is_some() {
        h += 2;
    }
    let inner = card(buf, area, 66, h, &format!("sign in · {}", v.provider.label()), acc, opened);
    clipped(buf, inner, |buf| {
        if inner.height < 3 {
            return;
        }
        let mut y = inner.y;
        let w = inner.width;
        let label = |s: &str| Span::styled(format!("{s:<9}"), fg(FAINT));

        match &v.phase {
            Phase::Running => {
                let t = v.started.elapsed().as_secs_f32();
                let place = v.opened.clone().unwrap_or_else(|| "the browser".into());
                let mut l = vec![Span::styled(format!("{} ", theme::spinner(t)), fg(acc))];
                if v.url.is_none() {
                    l.push(Span::styled("starting sign-in…", fg(TEXT)));
                } else {
                    l.push(Span::styled("waiting for you in ", fg(TEXT)));
                    l.push(Span::styled(place, bold(TEXT)));
                    // Travelling dots.
                    for i in 0..3 {
                        let b = pulse(t - i as f32 * 0.18, 1.2);
                        l.push(Span::styled(" ·", fg(mix(GHOST, acc, b))));
                    }
                }
                put(buf, inner.x, y, &Line::from(l), w);
                y += 1;
                put(
                    buf,
                    inner.x + 2,
                    y,
                    &Line::styled("approve access there — accountant takes it from here", fg(FAINT)),
                    w,
                );
                y += 2;

                let email = v.email.as_deref().map_or_else(
                    || "any — pick it in the browser".into(),
                    |e| privacy::email(e).into_owned(),
                );
                put(buf, inner.x, y, &Line::from(vec![label("account"), Span::styled(email, fg(TEXT))]), w);
                y += 1;
                let inbox = match &v.mail {
                    MailState::Off => Span::styled("off · enable in settings (,) to catch codes", fg(FAINT)),
                    MailState::Watching(src) if v.code.is_none() => Span::styled(
                        format!("watching {src} for a code{}", ".".repeat((secs * 2.0) as usize % 4)),
                        fg(DIM),
                    ),
                    MailState::Watching(_) => Span::styled("code received", fg(OK)),
                    MailState::Error(e) => {
                        Span::styled(truncate(&privacy::text(e), (w as usize).saturating_sub(10)), fg(WARN))
                    }
                };
                put(buf, inner.x, y, &Line::from(vec![label("inbox"), inbox]), w);
                y += 1;
                if let Some(s) = v.status.as_deref().map(privacy::text) {
                    put(
                        buf,
                        inner.x,
                        y,
                        &Line::from(vec![
                            label("status"),
                            Span::styled(truncate(&s, (w as usize).saturating_sub(10)), fg(DIM)),
                        ]),
                        w,
                    );
                }
                y += 1;

                if let Some(c) = &v.code {
                    y += 1;
                    big_code(buf, inner, y, &c.code, c.at, acc);
                    y += 3;
                    let from = c.from.split('<').next().unwrap_or(&c.from).trim().trim_matches('"');
                    let note = if c.copied {
                        "copied — paste it in the browser"
                    } else {
                        "type it in the browser"
                    };
                    centered_line(
                        buf,
                        inner,
                        y,
                        &Line::from(vec![
                            Span::styled(note, fg(OK)),
                            Span::styled(format!(" · from {}", truncate(from, 24)), fg(FAINT)),
                        ]),
                    );
                    y += 1;
                }
                if let Some(totp) = &v.totp {
                    y += 1;
                    let (code, left) = totp.now();
                    let mut l = vec![
                        label("2fa"),
                        Span::styled(format!("{} {}", &code[..3], &code[3..]), bold(TEXT)),
                        Span::raw("  "),
                    ];
                    l.extend(countdown(left as f64 / totp.period as f64, 6));
                    l.push(Span::styled(format!(" {left:>2}s  "), fg(FAINT)));
                    l.push(Span::styled("y", bold(DIM)));
                    l.push(Span::styled(" copy", fg(FAINT)));
                    put(buf, inner.x, y, &Line::from(l), w);
                    y += 1;
                }
                if let Some(buf_text) = &v.paste {
                    y += 1;
                    let cursor = if ((secs * 2.0) as u32).is_multiple_of(2) { "▏" } else { " " };
                    put(
                        buf,
                        inner.x,
                        y,
                        &Line::from(vec![
                            label("code"),
                            Span::styled(buf_text.clone(), bold(TEXT)),
                            Span::styled(cursor, fg(acc)),
                            Span::styled("  ⏎ send", fg(FAINT)),
                        ]),
                        w,
                    );
                }

                let mut keys = vec![("o", "reopen"), ("u", "copy link")];
                if v.accepts_code {
                    keys.push(("p", "paste code"));
                }
                if v.link.is_some() {
                    keys.push(("m", "email link"));
                }
                keys.push(("esc", "cancel"));
                put(buf, inner.x, inner.bottom().saturating_sub(1), &hints(&keys), w);
            }
            Phase::Success { at } => {
                let since = at.elapsed().as_secs_f32();
                y += 2;
                let who = v
                    .result
                    .as_ref()
                    .and_then(|p| p.email.as_deref())
                    .map_or_else(|| "your account".into(), |e| privacy::email(e).into_owned());
                let title =
                    Line::from(vec![Span::styled("signed in as ", fg(TEXT)), Span::styled(who, bold(acc))]);
                centered_line(buf, inner, y, &title);
                sparkles(buf, inner, y, title.width() as u16, since);
                y += 1;
                if let Some(p) = &v.result {
                    let what = if v.created {
                        format!("saved as “{}” · live now", p.shown_name())
                    } else {
                        format!("refreshed “{}” · live now", p.shown_name())
                    };
                    centered_line(buf, inner, y, &Line::styled(what, fg(DIM)));
                }
                let bottom = inner.bottom().saturating_sub(1);
                if v.adding {
                    centered_line(buf, inner, bottom, &hints(&[("⏎", "back to list"), ("q", "quit")]));
                } else {
                    draw_exit_bar(buf, inner, bottom, v.exit_at, *at);
                }
            }
            Phase::Failed(msg) => {
                y += 1;
                put(buf, inner.x, y, &Line::styled("✕ sign-in didn't finish", bold(ERR)), w);
                y += 2;
                for l in wrap(&privacy::text(msg), w as usize).into_iter().take(4) {
                    put(buf, inner.x, y, &Line::styled(l, fg(DIM)), w);
                    y += 1;
                }
                put(
                    buf,
                    inner.x,
                    inner.bottom().saturating_sub(1),
                    &hints(&[("r", "retry"), ("esc", "back")]),
                    w,
                );
            }
        }
    });
}

fn draw_add(buf: &mut Buffer, area: Rect, cursor: usize, opened: f32) {
    let inner = card(buf, area, 60, 9, "add account", TEXT, opened);
    clipped(buf, inner, |buf| {
        let rows = [
            ("Claude Code", "sign in with the browser", accent(Provider::Claude)),
            ("Codex", "sign in with the browser", accent(Provider::Codex)),
            ("Save current", "keep what's signed in right now", DIM),
        ];
        for (i, (name, desc, c)) in rows.iter().enumerate() {
            let sel = i == cursor;
            let line = Line::from(vec![
                Span::styled(if sel { "▸ " } else { "  " }, fg(*c)),
                Span::styled(pad(name, 15), if sel { bold(*c) } else { fg(TEXT) }),
                Span::styled(*desc, fg(if sel { DIM } else { FAINT })),
            ]);
            put(buf, inner.x, inner.y + 1 + i as u16, &line, inner.width);
        }
        put(
            buf,
            inner.x,
            inner.bottom().saturating_sub(1),
            &hints(&[("⏎", "choose"), ("esc", "back")]),
            inner.width,
        );
    });
}

fn draw_input(buf: &mut Buffer, area: Rect, v: &InputView, opened: f32, secs: f32) {
    let inner = card(buf, area, 64, 10, &v.title, TEXT, opened);
    clipped(buf, inner, |buf| {
        let mut y = inner.y;
        for l in wrap(&v.hint, inner.width as usize).into_iter().take(2) {
            put(buf, inner.x, y, &Line::styled(l, fg(FAINT)), inner.width);
            y += 1;
        }
        y += 1;
        let masked = matches!(v.purpose, super::app::InputPurpose::Totp(_));
        let shown = if masked {
            "•".repeat(v.value.chars().count())
        } else if matches!(v.purpose, super::app::InputPurpose::LoginEmail(_)) {
            privacy::email(&v.value).into_owned()
        } else {
            v.value.clone()
        };
        let room = inner.width.saturating_sub(4) as usize;
        let tail: String = shown.chars().rev().take(room).collect::<Vec<_>>().into_iter().rev().collect();
        let cursor = if ((secs * 2.0) as u32).is_multiple_of(2) { "▏" } else { " " };
        put(
            buf,
            inner.x,
            y,
            &Line::from(vec![
                Span::styled("› ", fg(DIM)),
                Span::styled(tail, bold(TEXT)),
                Span::styled(cursor, fg(TEXT)),
            ]),
            inner.width,
        );
        y += 1;
        put(buf, inner.x, y, &Line::styled("─".repeat(inner.width as usize), fg(GHOST)), inner.width);
        if let Some(e) = &v.error {
            put(buf, inner.x, y + 1, &Line::styled(truncate(e, inner.width as usize), fg(ERR)), inner.width);
        }
        put(
            buf,
            inner.x,
            inner.bottom().saturating_sub(1),
            &hints(&[("⏎", "ok"), ("esc", "cancel")]),
            inner.width,
        );
    });
}

fn draw_confirm(buf: &mut Buffer, area: Rect, v: &ConfirmView, opened: f32) {
    let inner = card(buf, area, 58, 8, &v.title, WARN, opened);
    clipped(buf, inner, |buf| {
        paragraph(buf, inner.x, inner.y, inner.width, &v.body, fg(DIM), 3);
        put(
            buf,
            inner.x,
            inner.bottom().saturating_sub(1),
            &hints(&[("y", "yes"), ("n", "no")]),
            inner.width,
        );
    });
}

fn draw_twofa(buf: &mut Buffer, area: Rect, v: &TwoFaView, vault: &str, opened: f32) {
    let acc = accent(v.provider);
    let inner = card(buf, area, 56, 11, &format!("2fa · {}", v.name), acc, opened);
    clipped(buf, inner, |buf| match &v.totp {
        Some(t) => {
            if !v.code.is_empty() {
                big_code(buf, inner, inner.y + 1, &v.code, v.changed_at, acc);
            }
            let (_, left) = t.now();
            let mut bar = countdown(left as f64 / t.period as f64, 20);
            bar.push(Span::styled(format!("  {left:>2}s"), fg(FAINT)));
            centered_line(buf, inner, inner.y + 5, &Line::from(bar));
            if v.copied {
                centered_line(buf, inner, inner.y + 6, &Line::styled("copied to clipboard", fg(OK)));
            }
            put(
                buf,
                inner.x,
                inner.bottom().saturating_sub(1),
                &hints(&[("c", "copy"), ("s", "replace"), ("x", "remove"), ("esc", "close")]),
                inner.width,
            );
        }
        None => {
            let text = format!(
                "No authenticator secret saved. Paste the setup key (or otpauth:// link) shown when you \
                 enable 2FA, and accountant generates the codes right here — no phone needed. It is kept \
                 in your {vault}."
            );
            let text = text.as_str();
            paragraph(buf, inner.x, inner.y, inner.width, text, fg(DIM), 5);
            put(
                buf,
                inner.x,
                inner.bottom().saturating_sub(1),
                &hints(&[("s", "add secret"), ("esc", "close")]),
                inner.width,
            );
        }
    });
}

fn draw_settings(buf: &mut Buffer, area: Rect, app: &App, cursor: usize, opened: f32) {
    let rows = app.settings_rows();
    let inner = card(buf, area, 64, rows.len() as u16 + 6, "settings", TEXT, opened);
    clipped(buf, inner, |buf| {
        for (i, (label, value)) in rows.iter().enumerate() {
            let sel = i == cursor;
            let line = Line::from(vec![
                Span::styled(if sel { "▸ " } else { "  " }, fg(TEXT)),
                Span::styled(pad(label, 18), fg(if sel { TEXT } else { DIM })),
                Span::styled(if sel { "‹ " } else { "  " }, fg(FAINT)),
                Span::styled(value.clone(), if sel { bold(TEXT) } else { fg(DIM) }),
                Span::styled(if sel { " ›" } else { "" }, fg(FAINT)),
            ]);
            put(buf, inner.x, inner.y + 1 + i as u16, &line, inner.width);
        }
        put(
            buf,
            inner.x,
            inner.bottom().saturating_sub(1),
            &hints(&[("←→", "change"), ("esc", "done")]),
            inner.width,
        );
    });
}

fn draw_help(buf: &mut Buffer, area: Rect, opened: f32) {
    let inner = card(buf, area, 66, 20, "help", TEXT, opened);
    clipped(buf, inner, |buf| {
        let keys: [(&str, &str); 12] = [
            ("⏎ / 1-9", "switch to the account — instant, no browser"),
            ("↑↓ / jk", "move"),
            ("a", "add an account (browser sign-in, or save current)"),
            ("r", "sign in again (expired or revoked session)"),
            ("t", "2FA codes for the account (TOTP)"),
            ("n", "rename"),
            ("d", "remove"),
            ("u", "refresh usage meters"),
            ("p", "hide / show emails (for recordings)"),
            (",", "settings: browser, inbox codes, auto-quit"),
            ("q / esc", "quit"),
            ("", ""),
        ];
        let mut y = inner.y;
        for (k, d) in keys {
            if !k.is_empty() {
                put(
                    buf,
                    inner.x,
                    y,
                    &Line::from(vec![Span::styled(pad(k, 10), bold(DIM)), Span::styled(d, fg(FAINT))]),
                    inner.width,
                );
            }
            y += 1;
        }
        let about = "Each account's login is saved in your Keychain. Switching swaps it into Claude Code / Codex and saves the outgoing one first, so tokens never go stale. The browser is only needed for the first sign-in or when a provider revokes a session.";
        for l in wrap(about, inner.width as usize).into_iter().take(5) {
            put(buf, inner.x, y, &Line::styled(l, fg(DIM)), inner.width);
            y += 1;
        }
    });
}
