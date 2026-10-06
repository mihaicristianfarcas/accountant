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
use crate::usage::Window;
use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Margin, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Widget};
use std::time::Instant;

/// The main column grows with the terminal up to this width.
const MAX_W: u16 = 100;
/// Rows kept under the list: a gap, the toast, a gap, the footer.
const BOTTOM: u16 = 4;
/// Columns before an account's name: selection bar, number, status dot.
const PREFIX: usize = 7;
/// Between usage windows on an account's second line.
const SEP: &str = "  │  ";

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let buf = f.buffer_mut();
    let secs = app.secs();
    let dim = app.modal.is_some();

    let w = area.width.saturating_sub(4).min(MAX_W);
    let x = area.x + (area.width - w) / 2;
    let profiles = app.profiles();
    let plan = plan(&profiles, area, w);

    // Sit the block a little above the vertical centre when there is room.
    let head_h = if plan.logo { 5 } else { 2 };
    let block = head_h + plan.items.len() as u16;
    let mut y = area.y + 1 + area.height.saturating_sub(1 + block + BOTTOM) / 3;
    if plan.logo {
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
        put(buf, x + 1, y, &title, w);
        y += 2;
    }

    let footer_y = area.bottom().saturating_sub(1);
    let toast = toast_lines(app, w);
    let toast_y = footer_y.saturating_sub(1 + toast.len().max(1) as u16);
    let list = Rect { x, y, width: w, height: toast_y.saturating_sub(y + 1) };
    draw_list(buf, app, &profiles, &plan, list, secs, dim);
    draw_toast(buf, app, &toast, Rect { x, y: toast_y, width: w, height: toast.len() as u16 });
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

/// Line with its text pushed to the right edge of `width`.
fn right_aligned(mut left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let lw: usize = left.iter().map(Span::width).sum();
    let rw: usize = right.iter().map(Span::width).sum();
    if rw > 0 {
        left.push(Span::raw(" ".repeat(width.saturating_sub(lw + rw).max(1))));
        left.extend(right);
    }
    Line::from(left)
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
            if let Some(c @ Color::Rgb(..)) = s.style.bg {
                s.style.bg = Some(mix(c, SHADE, t));
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

/// Word wrap; words longer than a line (paths, URLs) are split.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = vec![];
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        let n = cur.chars().count();
        if n > 0 && n + 1 + word.len() > width {
            lines.push(std::mem::take(&mut cur));
        }
        while word.len() > width {
            lines.push(word.drain(..width).collect());
        }
        if !word.is_empty() {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.extend(word);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// At most `max` lines; the last one says (…) when something was left out.
fn clamp_lines(mut lines: Vec<String>, max: usize, width: usize) -> Vec<String> {
    let max = max.max(1);
    if lines.len() > max {
        lines.truncate(max);
        if let Some(last) = lines.last_mut() {
            let keep = width.saturating_sub(1).min(last.chars().count());
            *last = format!("{}…", last.chars().take(keep).collect::<String>());
        }
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
    let lines = clamp_lines(wrap(text, width as usize), max_lines, width as usize);
    for (row, l) in (y..).zip(&lines) {
        put(buf, x, row, &Line::styled(l.clone(), style), width);
    }
    lines.len() as u16
}

/// Pre-wrapped lines behind a coloured bar, for errors and notes.
fn callout(buf: &mut Buffer, x: u16, y: u16, width: u16, lines: &[String], color: Color) {
    for (row, l) in (y..).zip(lines) {
        let line = Line::from(vec![
            Span::styled("┃ ", fg(mix(color, CARD, 0.25))),
            Span::styled(l.clone(), fg(mix(color, TEXT, 0.6))),
        ]);
        put(buf, x, row, &line, width);
    }
}

fn keycap(k: &str) -> Span<'static> {
    Span::styled(format!(" {k} "), bold(TEXT).bg(KEYCAP))
}

fn hints(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![];
    for (i, (k, label)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(keycap(k));
        spans.push(Span::styled(format!(" {label}"), fg(mix(DIM, FAINT, 0.35))));
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

#[derive(Clone, Copy)]
enum Item {
    Header(Provider, usize),
    Empty(Provider),
    /// An account's first (or, compact, only) line.
    Row(usize),
    /// Its usage line underneath.
    Detail(usize),
    Gap,
}

struct Plan {
    logo: bool,
    /// Two lines per account, with every usage window; otherwise one.
    detailed: bool,
    items: Vec<Item>,
}

fn list_items(profiles: &[Profile], detailed: bool, airy: bool) -> Vec<Item> {
    let mut items = vec![];
    for (n, p) in Provider::ALL.into_iter().enumerate() {
        if n > 0 {
            items.push(Item::Gap);
        }
        let rows: Vec<usize> =
            profiles.iter().enumerate().filter(|(_, x)| x.provider == p).map(|(i, _)| i).collect();
        items.push(Item::Header(p, rows.len()));
        if rows.is_empty() {
            items.push(Item::Empty(p));
        }
        for (k, i) in rows.into_iter().enumerate() {
            if airy && k > 0 {
                items.push(Item::Gap);
            }
            items.push(Item::Row(i));
            if detailed {
                items.push(Item::Detail(i));
            }
        }
    }
    items
}

/// The roomiest layout that fits without scrolling.
fn plan(profiles: &[Profile], area: Rect, w: u16) -> Plan {
    let logo_fits = area.height >= 20 && area.width >= 46;
    let room = |logo: bool| area.height.saturating_sub(1 + if logo { 5 } else { 2 } + BOTTOM) as usize;
    // (logo, detailed, a blank line between accounts)
    let tries = [
        (true, true, true),
        (true, true, false),
        (false, true, false),
        (true, false, false),
        (false, false, false),
    ];
    for (logo, detailed, airy) in tries {
        if (logo && !logo_fits) || (detailed && w < 44) {
            continue;
        }
        let items = list_items(profiles, detailed, airy);
        if items.len() <= room(logo) {
            return Plan { logo, detailed, items };
        }
    }
    Plan { logo: false, detailed: false, items: list_items(profiles, false, false) }
}

/// Column widths shared by every row, so the list reads as a table.
struct Cols {
    name: usize,
    email: usize,
    plan: usize,
    /// Usage line: how many windows fit, meter cells, and per window column
    /// the widths of its label and reset text.
    windows: usize,
    bar: usize,
    label: Vec<usize>,
    reset: Vec<usize>,
}

fn text_w(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// The `n` most used windows, in their usual order.
fn shown_windows(windows: &[Window], n: usize, now: DateTime<Utc>) -> Vec<&Window> {
    let mut by_use: Vec<usize> = (0..windows.len()).collect();
    by_use.sort_by(|&a, &b| used_now(&windows[b], now).total_cmp(&used_now(&windows[a], now)));
    by_use.truncate(n);
    by_use.sort_unstable();
    by_use.into_iter().map(|i| &windows[i]).collect()
}

impl Cols {
    fn new(app: &App, profiles: &[Profile], width: usize, detailed: bool) -> Cols {
        let now = Utc::now();
        let widest = |f: &dyn Fn(&Profile) -> usize| profiles.iter().map(f).max().unwrap_or(0);
        let name = widest(&|p| p.shown_name().chars().count()).clamp(6, if detailed { 24 } else { 20 });
        let plan = widest(&|p| p.plan.as_deref().map_or(0, |s| s.chars().count())).min(14);
        let email_max = widest(&|p| p.email.as_deref().map_or(1, |e| privacy::email(e).chars().count()));

        // As many windows as fit on the usage line, shrinking the meters first.
        let usages: Vec<&[Window]> = profiles
            .iter()
            .filter_map(|p| app.usage.by_profile.get(&p.id))
            .map(|u| u.windows.as_slice())
            .collect();
        let most = usages.iter().map(|w| w.len()).max().unwrap_or(0);
        let widths = |n: usize| {
            let (mut label, mut reset) = (vec![0; n], vec![0; n]);
            for ws in &usages {
                for (k, w) in shown_windows(ws, n, now).into_iter().enumerate() {
                    label[k] = label[k].max(w.label.chars().count());
                    reset[k] = reset[k].max(reset_text(w, now).0.chars().count());
                }
            }
            (label, reset)
        };
        let room = width.saturating_sub(PREFIX);
        let (mut windows, mut bar, (mut label, mut reset)) = (most.min(1), 4, widths(most.min(1)));
        'fit: for n in (1..=most).rev() {
            let (l, r) = widths(n);
            for b in [10, 8, 6, 4] {
                let total: usize = (0..n).map(|k| l[k] + 1 + b + 5 + 2 + r[k]).sum::<usize>()
                    + (n - 1) * SEP.chars().count();
                if total <= room {
                    (windows, bar, label, reset) = (n, b, l, r);
                    break 'fit;
                }
            }
        }

        let mut cols = Cols { name, email: 0, plan, windows, bar, label, reset };
        // Whatever sits at the right edge: a tag, or the compact status.
        let right = if detailed {
            profiles.iter().map(|p| text_w(&tag_spans(app, p))).max().unwrap_or(0)
        } else {
            profiles.iter().map(|p| text_w(&compact_status(app, p, now))).max().unwrap_or(0).min(34)
        };
        let fixed = PREFIX + name + 2 + plan + 2 + right;
        if fixed > width {
            cols.name = name.saturating_sub(fixed - width).max(6);
        }
        cols.email = width.saturating_sub(PREFIX + cols.name + 2 + 2 + plan + 2 + right).min(email_max);
        if cols.email < 8 {
            cols.email = 0;
        }
        cols
    }
}

fn draw_list(
    buf: &mut Buffer,
    app: &App,
    profiles: &[Profile],
    plan: &Plan,
    area: Rect,
    secs: f32,
    dim: bool,
) {
    let items = &plan.items;
    let width = area.width as usize;
    let cols = Cols::new(app, profiles, width, plan.detailed);

    // Scroll so the cursor (both of its lines) stays visible.
    let h = area.height as usize;
    let cursor_pos = items.iter().position(|it| matches!(it, Item::Row(i) if *i == app.cursor)).unwrap_or(0);
    let tail = if plan.detailed { 3 } else { 2 };
    let offset =
        if items.len() <= h { 0 } else { (cursor_pos + tail).saturating_sub(h).min(items.len() - h) };

    let reduced = app.reduced_motion();
    let mut appear = 0;
    for (line_no, item) in items.iter().enumerate().skip(offset).take(h) {
        let y = area.y + (line_no - offset) as u16;
        // Staggered entrance; an account's two lines arrive together.
        if !matches!(item, Item::Detail(_)) {
            appear += 1;
        }
        let t0 = 0.35 + appear as f32 * 0.045;
        let prog = if reduced { 1.0 } else { ease_out((secs - t0) / 0.3) };
        if prog <= 0.0 {
            continue;
        }
        let shift = ((1.0 - prog) * 6.0).round() as u16;
        let fade = (1.0 - prog).max(if dim { 0.62 } else { 0.0 });
        let room = area.width.saturating_sub(shift);
        let line = match item {
            Item::Gap => continue,
            Item::Header(p, n) => header_line(*p, *n, width),
            Item::Empty(p) => Line::from(vec![
                Span::styled("    no saved accounts yet — ", fg(FAINT)),
                keycap("a"),
                Span::styled(format!(" to sign in to {}", p.label()), fg(FAINT)),
            ]),
            Item::Row(i) | Item::Detail(i) => {
                let p = &profiles[*i];
                let selected = *i == app.cursor;
                if selected && !dim {
                    let glow = mix(ROW_HL, accent(p.provider), 0.06 + 0.04 * pulse(secs, 2.4));
                    buf.set_style(
                        Rect { x: area.x, y, width: area.width, height: 1 },
                        Style::default().bg(glow),
                    );
                }
                if matches!(item, Item::Row(_)) {
                    row_line(app, p, *i, selected, &cols, plan.detailed, width, secs)
                } else {
                    detail_line(app, p, selected, &cols, width, secs)
                }
            }
        };
        put(buf, area.x + shift, y, &faded(line, fade), room);
    }
}

fn header_line(p: Provider, n: usize, width: usize) -> Line<'static> {
    let label = p.label().to_uppercase();
    let count = match n {
        0 => String::new(),
        1 => "1 account".into(),
        n => format!("{n} accounts"),
    };
    let used = 2 + label.chars().count() + 1 + if count.is_empty() { 0 } else { count.len() + 1 };
    Line::from(vec![
        Span::styled("◆ ", fg(accent(p))),
        Span::styled(label, bold(accent(p))),
        Span::raw(" "),
        Span::styled("─".repeat(width.saturating_sub(used)), fg(GHOST)),
        Span::styled(if count.is_empty() { count } else { format!(" {count}") }, fg(FAINT)),
    ])
}

/// The selection bar, number and status dot in front of every row.
fn row_prefix(app: &App, p: &Profile, index: usize, selected: bool, secs: f32) -> Vec<Span<'static>> {
    let acc = accent(p.provider);
    let bar = if selected {
        Span::styled("▌", fg(mix(acc, Color::Rgb(255, 255, 255), 0.25 * pulse(secs, 1.6))))
    } else {
        Span::raw(" ")
    };
    let idx = if index < 9 { format!("{}", index + 1) } else { " ".into() };
    let (dot, dot_style) = if p.needs_login {
        ("◌", fg(WARN))
    } else if app.is_active(p) {
        ("●", fg(mix(OK, Color::Rgb(220, 255, 225), 0.45 * pulse(secs, 2.2))))
    } else {
        ("○", fg(FAINT))
    };
    vec![
        bar,
        Span::styled(format!(" {idx}  "), fg(if selected { DIM } else { FAINT })),
        Span::styled(dot, dot_style),
        Span::raw(" "),
    ]
}

#[allow(clippy::too_many_arguments)]
fn row_line(
    app: &App,
    p: &Profile,
    index: usize,
    selected: bool,
    cols: &Cols,
    detailed: bool,
    width: usize,
    secs: f32,
) -> Line<'static> {
    let mut spans = row_prefix(app, p, index, selected, secs);
    let name_style = if selected { bold(TEXT) } else { fg(TEXT) };
    spans.push(Span::styled(pad(&p.shown_name(), cols.name), name_style));
    spans.push(Span::raw("  "));
    if cols.email > 0 {
        let email = p.email.as_deref().map_or_else(|| "—".into(), |e| privacy::email(e).into_owned());
        spans.push(Span::styled(pad(&email, cols.email), fg(if selected { DIM } else { FAINT })));
        spans.push(Span::raw("  "));
    }
    let plan = p.plan.as_deref().unwrap_or("");
    spans.push(Span::styled(pad(plan, cols.plan), fg(mix(accent(p.provider), FAINT, 0.55))));
    let right = if detailed { tag_spans(app, p) } else { compact_status(app, p, Utc::now()) };
    right_aligned(spans, right, width)
}

/// Right edge of an account's first line.
fn tag_spans(app: &App, p: &Profile) -> Vec<Span<'static>> {
    if p.needs_login {
        vec![Span::styled("needs sign-in", fg(WARN))]
    } else if app.is_active(p) {
        vec![Span::styled("live", fg(mix(OK, DIM, 0.3)))]
    } else {
        vec![]
    }
}

/// An account's second line: every usage window, or what we know instead.
fn detail_line(
    app: &App,
    p: &Profile,
    selected: bool,
    cols: &Cols,
    width: usize,
    secs: f32,
) -> Line<'static> {
    let now = Utc::now();
    let mut spans = vec![if selected {
        Span::styled("▌", fg(mix(accent(p.provider), Color::Rgb(255, 255, 255), 0.25 * pulse(secs, 1.6))))
    } else {
        Span::raw(" ")
    }];
    spans.push(Span::raw(" ".repeat(PREFIX - 1)));
    let windows = app.usage.by_profile.get(&p.id).map(|u| u.windows.as_slice()).unwrap_or_default();
    if p.needs_login {
        spans.push(Span::styled("saved session expired — ", fg(mix(WARN, DIM, 0.35))));
        spans.push(keycap("r"));
        spans.push(Span::styled(" to sign in again", fg(mix(WARN, DIM, 0.35))));
    } else if !windows.is_empty() {
        let shown = shown_windows(windows, cols.windows.max(1), now);
        for (k, w) in shown.iter().enumerate() {
            if k > 0 {
                spans.push(Span::styled(SEP, fg(mix(GHOST, FAINT, 0.4))));
            }
            let label_w = cols.label.get(k).copied().unwrap_or(0);
            let reset_w = if k + 1 < shown.len() { cols.reset.get(k).copied().unwrap_or(0) } else { 0 };
            spans.extend(window_spans(w, label_w, cols.bar, reset_w, now));
        }
        let hidden = windows.len() - shown.len();
        if hidden > 0 && text_w(&spans) + 4 <= width {
            spans.push(Span::styled(format!("  +{hidden}"), fg(FAINT)));
        }
    } else if app.is_active(p) {
        spans.push(Span::styled("in use right now", fg(FAINT)));
    } else if let Some(left) = p.left_at {
        spans.push(Span::styled(
            format!(
                "rested {} — its limits have been cooling down",
                short_duration((now - left).num_seconds())
            ),
            fg(FAINT),
        ));
    } else {
        spans.push(Span::styled("saved · ready to switch to", fg(FAINT)));
    }
    Line::from(spans)
}

fn used_now(w: &Window, now: DateTime<Utc>) -> f64 {
    if w.resets_at.is_some_and(|r| r <= now) { 0.0 } else { w.used }
}

/// The more used, the warmer.
fn level(used: f64) -> Color {
    if used >= 85.0 {
        ERR
    } else if used >= 60.0 {
        WARN
    } else {
        OK
    }
}

/// When a window frees up again, and how urgently that matters.
fn reset_text(w: &Window, now: DateTime<Utc>) -> (String, Color) {
    let limited = w.used >= 99.5;
    match w.resets_at {
        Some(r) if r <= now => ("✓ reset".into(), OK),
        Some(r) if limited => {
            (format!("back in {}", short_duration((r - now).num_seconds())), mix(ERR, WARN, 0.3))
        }
        Some(r) => (format!("↻ in {}", short_duration((r - now).num_seconds())), FAINT),
        None if limited => ("limited".into(), ERR),
        None => (String::new(), FAINT),
    }
}

/// `5h ▰▰▰▰▱▱▱▱▱▱  42%  ↻ in 2h 13m`, padded to its column.
fn window_spans(
    w: &Window,
    label_w: usize,
    cells: usize,
    reset_w: usize,
    now: DateTime<Utc>,
) -> Vec<Span<'static>> {
    let used = used_now(w, now);
    let fill = |t: &str, w: usize| format!("{t}{}", " ".repeat(w.saturating_sub(t.chars().count())));
    let mut s = vec![Span::styled(fill(&w.label, label_w), fg(DIM)), Span::raw(" ")];
    s.extend(bar(used / 100.0, cells, level(used)));
    let pct = format!("{used:.0}%");
    s.push(Span::styled(format!(" {pct:>4}"), if used >= 60.0 { bold(level(used)) } else { fg(DIM) }));
    let (reset, color) = reset_text(w, now);
    s.push(Span::raw("  "));
    s.push(Span::styled(fill(&reset, reset_w), fg(color)));
    s
}

/// One-line status for compact rows: the binding window, or what we know.
fn compact_status(app: &App, p: &Profile, now: DateTime<Utc>) -> Vec<Span<'static>> {
    if p.needs_login {
        return vec![Span::styled("⚠ sign in again ", fg(WARN)), keycap("r")];
    }
    if let Some(u) = app.usage.by_profile.get(&p.id) {
        let reset_since = u.windows.iter().any(|w| w.used >= 50.0 && w.resets_at.is_some_and(|r| r <= now));
        if let Some(w) = u.binding() {
            if w.used < 1.0 && reset_since {
                return vec![Span::styled("✓ reset · ready", fg(OK))];
            }
            let (reset, color) = reset_text(&w, now);
            if w.used >= 99.5 {
                return vec![
                    Span::styled("◷ ", fg(ERR)),
                    Span::styled(reset, fg(color)),
                    Span::styled(format!(" · {}", w.label), fg(FAINT)),
                ];
            }
            let mut s = bar(w.used / 100.0, 5, level(w.used));
            s.push(Span::styled(format!(" {:>3.0}%", w.used), fg(DIM)));
            s.push(Span::styled(format!(" {}", w.label), fg(DIM)));
            if !reset.is_empty() {
                s.push(Span::styled(format!("  {reset}"), fg(color)));
            }
            return s;
        }
    }
    if app.is_active(p) {
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
    let empty = mix(GHOST, FAINT, 0.35);
    vec![Span::styled("▰".repeat(filled), fg(color)), Span::styled("▱".repeat(cells - filled), fg(empty))]
}

// ---------------------------------------------------------------------------
// Toast & footer
// ---------------------------------------------------------------------------

/// Columns in front of a toast's text: its bar and icon.
const TOAST_INDENT: usize = 5;

fn toast_lines(app: &App, w: u16) -> Vec<String> {
    let Some(t) = &app.toast else { return vec![] };
    let width = (w as usize).saturating_sub(TOAST_INDENT + 2);
    clamp_lines(wrap(&privacy::text(&t.text), width), 4, width)
}

fn draw_toast(buf: &mut Buffer, app: &App, lines: &[String], r: Rect) {
    let Some(t) = &app.toast else { return };
    let age = t.at.elapsed().as_secs_f32();
    let life = t.life().as_secs_f32();
    let fade = ((age - (life - 0.8)) / 0.8).clamp(0.0, 1.0);
    let enter = ease_out(age / 0.2);
    let (icon, color, text) = match t.kind {
        ToastKind::Info => ("·", DIM, mix(TEXT, DIM, 0.2)),
        ToastKind::Good => ("✓", OK, TEXT),
        ToastKind::Bad => ("✕", ERR, mix(ERR, TEXT, 0.7)),
    };
    let shift = ((1.0 - enter) * 3.0) as u16;
    for (row, l) in (r.y..).zip(lines) {
        let lead = if row == r.y { format!(" {icon}  ") } else { "    ".into() };
        let line = Line::from(vec![
            Span::styled("▎", fg(mix(color, GHOST, 0.2))),
            Span::styled(lead, bold(color)),
            Span::styled(l.clone(), fg(text)),
        ]);
        put(buf, r.x + 1 + shift, row, &faded(line, fade.max(1.0 - enter)), r.width.saturating_sub(1));
    }
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
    for drop in ["d", "n", ",", "t", "r", "a"] {
        if (hints(&keys).width() as u16) <= r.width {
            break;
        }
        keys.retain(|(k, _)| *k != drop);
    }
    let line = hints(&keys);
    let line = if dim { faded(line, 0.7) } else { line };
    centered_line(buf, r, r.y, &line);
}

// ---------------------------------------------------------------------------
// Modals
// ---------------------------------------------------------------------------

/// Width of a card's content area once it is fitted to the screen.
fn card_inner_w(area: Rect, w: u16) -> u16 {
    w.min(area.width.saturating_sub(2)).saturating_sub(6)
}

/// Rows a card's content may take on this screen.
fn card_room(area: Rect) -> usize {
    area.height.saturating_sub(6) as usize
}

/// A centred card with `content_h` rows of content; returns the content
/// area. It unfolds from the middle as it opens.
fn card(
    buf: &mut Buffer,
    area: Rect,
    w: u16,
    content_h: u16,
    title: &str,
    color: Color,
    opened: f32,
) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let full_h = (content_h + 4).min(area.height.saturating_sub(2));
    let h = (((full_h as f32) * ease_out(opened / 0.14)).round().max(3.0) as u16).min(full_h);
    let r = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height.saturating_sub(full_h)) / 2 + (full_h - h) / 2,
        width: w,
        height: h,
    };
    Clear.render(r, buf);
    buf.set_style(r, Style::default().bg(CARD));
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(fg(mix(color, GHOST, 0.45)))
        .title(Line::from(Span::styled(format!(" {title} "), bold(color))))
        .render(r, buf);
    r.inner(Margin { horizontal: 3, vertical: 2 })
}

/// Draw a card's content into a buffer the size of `inner`, so nothing can
/// spill over the border on small terminals.
fn clipped(buf: &mut Buffer, inner: Rect, draw: impl FnOnce(&mut Buffer)) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let mut sub = Buffer::empty(inner);
    sub.set_style(inner, Style::default().bg(CARD));
    draw(&mut sub);
    buf.merge(&sub);
}

/// The highlighted row of a menu inside a card.
fn menu_highlight(buf: &mut Buffer, inner: Rect, y: u16, color: Color) {
    buf.set_style(
        Rect { x: inner.x, y, width: inner.width, height: 1 },
        Style::default().bg(mix(CARD, color, 0.12)),
    );
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
    // Track, gap, four stages, gap — then the error and its keys, or room
    // for the success message.
    let iw = card_inner_w(area, 74);
    let error = match &v.phase {
        Phase::Failed(msg) => {
            let width = iw.saturating_sub(4) as usize;
            clamp_lines(wrap(&privacy::text(msg), width), card_room(area).saturating_sub(9), width)
        }
        _ => vec![],
    };
    let content_h = 9 + error.len() as u16;
    let inner = card(buf, area, 74, content_h, &format!("switch · {}", v.provider.label()), acc, opened);
    clipped(buf, inner, |buf| {
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
        let fw = from.chars().count().min(22);
        let tw = to.chars().count().min(22);
        let track = (inner.width as usize).saturating_sub(fw + tw + 4).max(4);
        let head = ((prog * track as f32) as usize).min(track.saturating_sub(1));
        let t = v.started.elapsed().as_secs_f32();
        let mut spans =
            vec![Span::styled(truncate(&from, 22), fg(if success { FAINT } else { DIM })), Span::raw("  ")];
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
        spans.push(Span::styled(truncate(&to, 22), if success { bold(acc) } else { fg(TEXT) }));
        put(buf, inner.x, y0, &Line::from(spans), inner.width);

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
                    put(buf, inner.x, y0 + 2 + i as u16, &line, inner.width);
                }
                if failed {
                    callout(buf, inner.x + 2, y0 + 7, inner.width.saturating_sub(2), &error, ERR);
                    put(
                        buf,
                        inner.x + 2,
                        inner.bottom().saturating_sub(1),
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
                let ty = y0 + 3;
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
                draw_exit_bar(buf, inner, inner.bottom().saturating_sub(1), v.exit_at, *at);
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

/// Columns taken by the labels of a sign-in card's fields.
const FIELD: usize = 9;

/// `label    value…` with the value wrapped under itself.
fn field(buf: &mut Buffer, x: u16, y: u16, w: u16, label: &str, lines: &[String], color: Color) {
    for (i, (row, l)) in (y..).zip(lines).enumerate() {
        let lead = if i == 0 { format!("{label:<FIELD$}") } else { " ".repeat(FIELD) };
        put(
            buf,
            x,
            row,
            &Line::from(vec![Span::styled(lead, fg(FAINT)), Span::styled(l.clone(), fg(color))]),
            w,
        );
    }
}

fn draw_login(buf: &mut Buffer, area: Rect, v: &LoginView, opened: f32, secs: f32) {
    let acc = accent(v.provider);
    let iw = card_inner_w(area, 78);
    let vw = (iw as usize).saturating_sub(FIELD);
    let room = card_room(area);

    let inbox: (Vec<String>, Color) = match &v.mail {
        MailState::Off => (wrap("off · turn it on in settings (,) to catch codes", vw), FAINT),
        MailState::Watching(src) if v.code.is_none() => (
            vec![truncate(
                &format!("watching {src} for a code{}", ".".repeat((secs * 2.0) as usize % 4)),
                vw,
            )],
            DIM,
        ),
        MailState::Watching(_) => (vec!["code received".into()], OK),
        MailState::Error(e) => (clamp_lines(wrap(&privacy::text(e), vw), 3, vw), WARN),
    };
    let status =
        v.status.as_deref().map(|s| clamp_lines(wrap(&privacy::text(s), vw), 3, vw)).unwrap_or_default();
    let error = match &v.phase {
        Phase::Failed(msg) => {
            let width = (iw as usize).saturating_sub(2);
            clamp_lines(wrap(&privacy::text(msg), width), room.saturating_sub(4), width)
        }
        _ => vec![],
    };
    let content_h = match &v.phase {
        Phase::Running => {
            // Status line, gap, account, inbox, status (always one row, so
            // the card doesn't jump when the first one arrives), extras, keys.
            3 + 1
                + inbox.0.len()
                + status.len().max(1)
                + if v.code.is_some() { 5 } else { 0 }
                + if v.totp.is_some() { 2 } else { 0 }
                + if v.paste.is_some() { 2 } else { 0 }
                + 2
        }
        Phase::Success { .. } => 7,
        Phase::Failed(_) => 4 + error.len(),
    };
    let inner =
        card(buf, area, 78, content_h as u16, &format!("sign in · {}", v.provider.label()), acc, opened);
    clipped(buf, inner, |buf| {
        let mut y = inner.y;
        let w = inner.width;

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
                field(buf, inner.x, y, w, "account", &[truncate(&email, vw)], TEXT);
                y += 1;
                field(buf, inner.x, y, w, "inbox", &inbox.0, inbox.1);
                y += inbox.0.len() as u16;
                field(buf, inner.x, y, w, "status", &status, DIM);
                y += status.len().max(1) as u16;

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
                            Span::styled(format!(" · from {}", truncate(from, 28)), fg(FAINT)),
                        ]),
                    );
                    y += 1;
                }
                if let Some(totp) = &v.totp {
                    y += 1;
                    let (code, left) = totp.now();
                    let mut l = vec![
                        Span::styled(format!("{:<FIELD$}", "2fa"), fg(FAINT)),
                        Span::styled(format!("{} {}", &code[..3], &code[3..]), bold(TEXT)),
                        Span::raw("  "),
                    ];
                    l.extend(countdown(left as f64 / totp.period as f64, 8));
                    l.push(Span::styled(format!(" {left:>2}s   "), fg(FAINT)));
                    l.push(keycap("y"));
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
                            Span::styled(format!("{:<FIELD$}", "code"), fg(FAINT)),
                            Span::styled(buf_text.clone(), bold(TEXT)),
                            Span::styled(cursor, fg(acc)),
                            Span::raw("   "),
                            keycap("⏎"),
                            Span::styled(" send", fg(FAINT)),
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
            Phase::Failed(_) => {
                put(buf, inner.x, y, &Line::styled("✕  sign-in didn't finish", bold(ERR)), w);
                y += 2;
                callout(buf, inner.x, y, w, &error, ERR);
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
    let inner = card(buf, area, 66, 5, "add account", TEXT, opened);
    clipped(buf, inner, |buf| {
        let rows = [
            ("Claude Code", "sign in with the browser", accent(Provider::Claude)),
            ("Codex", "sign in with the browser", accent(Provider::Codex)),
            ("Save current", "keep what's signed in right now", DIM),
        ];
        for (i, (name, desc, c)) in rows.iter().enumerate() {
            let sel = i == cursor;
            let y = inner.y + i as u16;
            if sel {
                menu_highlight(buf, inner, y, *c);
            }
            let line = Line::from(vec![
                Span::styled(if sel { " ▸ " } else { "   " }, fg(*c)),
                Span::styled(pad(name, 16), if sel { bold(*c) } else { fg(TEXT) }),
                Span::styled(*desc, fg(if sel { DIM } else { FAINT })),
            ]);
            put(buf, inner.x, y, &line, inner.width);
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
    let iw = card_inner_w(area, 70) as usize;
    let hint = clamp_lines(wrap(&v.hint, iw), 3, iw);
    let error =
        v.error.as_deref().map(|e| clamp_lines(wrap(&privacy::text(e), iw), 4, iw)).unwrap_or_default();
    // Hint, gap, value, rule, error (or a blank row), gap, keys.
    let content_h = hint.len() + 3 + error.len().max(1) + 2;
    let inner = card(buf, area, 70, content_h as u16, &v.title, TEXT, opened);
    clipped(buf, inner, |buf| {
        let mut y = inner.y;
        for l in &hint {
            put(buf, inner.x, y, &Line::styled(l.clone(), fg(FAINT)), inner.width);
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
        let rule = if error.is_empty() { GHOST } else { mix(ERR, GHOST, 0.4) };
        put(buf, inner.x, y, &Line::styled("─".repeat(inner.width as usize), fg(rule)), inner.width);
        y += 1;
        for l in &error {
            put(buf, inner.x, y, &Line::styled(l.clone(), fg(mix(ERR, TEXT, 0.3))), inner.width);
            y += 1;
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
    let iw = card_inner_w(area, 62) as usize;
    let body = clamp_lines(wrap(&v.body, iw), 6, iw);
    let inner = card(buf, area, 62, body.len() as u16 + 2, &v.title, WARN, opened);
    clipped(buf, inner, |buf| {
        for (y, l) in (inner.y..).zip(&body) {
            put(buf, inner.x, y, &Line::styled(l.clone(), fg(DIM)), inner.width);
        }
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
    let iw = card_inner_w(area, 60);
    let text = format!(
        "No authenticator secret saved. Paste the setup key (or otpauth:// link) shown when you enable \
         2FA, and accountant generates the codes right here — no phone needed. It is kept in your {vault}."
    );
    let content_h = match &v.totp {
        Some(_) => 8,
        None => wrap(&text, iw as usize).len().min(6) as u16 + 2,
    };
    let inner = card(buf, area, 60, content_h, &format!("2fa · {}", v.name), acc, opened);
    clipped(buf, inner, |buf| match &v.totp {
        Some(t) => {
            if !v.code.is_empty() {
                big_code(buf, inner, inner.y, &v.code, v.changed_at, acc);
            }
            let (_, left) = t.now();
            let mut bar = countdown(left as f64 / t.period as f64, 20);
            bar.push(Span::styled(format!("  {left:>2}s"), fg(FAINT)));
            centered_line(buf, inner, inner.y + 4, &Line::from(bar));
            if v.copied {
                centered_line(buf, inner, inner.y + 5, &Line::styled("copied to clipboard", fg(OK)));
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
            paragraph(buf, inner.x, inner.y, inner.width, &text, fg(DIM), 6);
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
    let inner = card(buf, area, 70, rows.len() as u16 + 2, "settings", TEXT, opened);
    clipped(buf, inner, |buf| {
        for (i, (label, value)) in rows.iter().enumerate() {
            let sel = i == cursor;
            let y = inner.y + i as u16;
            if sel {
                menu_highlight(buf, inner, y, TEXT);
            }
            let line = Line::from(vec![
                Span::styled(if sel { " ▸ " } else { "   " }, fg(TEXT)),
                Span::styled(pad(label, 18), fg(if sel { TEXT } else { DIM })),
                Span::styled(if sel { "‹ " } else { "  " }, fg(FAINT)),
                Span::styled(value.clone(), if sel { bold(TEXT) } else { fg(DIM) }),
                Span::styled(if sel { " ›" } else { "" }, fg(FAINT)),
            ]);
            put(buf, inner.x, y, &line, inner.width);
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
    let keys: [(&str, &str); 11] = [
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
    ];
    let about = "Each account's login is saved in your Keychain. Switching swaps it into Claude Code / Codex and \
                 saves the outgoing one first, so tokens never go stale. The browser is only needed for the first \
                 sign-in or when a provider revokes a session.";
    let iw = card_inner_w(area, 76) as usize;
    let about = clamp_lines(wrap(about, iw), 5, iw);
    let inner = card(buf, area, 76, (keys.len() + 1 + about.len()) as u16, "help", TEXT, opened);
    clipped(buf, inner, |buf| {
        let mut y = inner.y;
        for (k, d) in keys {
            let cap = keycap(k);
            let gap = " ".repeat(13usize.saturating_sub(cap.width()));
            put(
                buf,
                inner.x,
                y,
                &Line::from(vec![cap, Span::raw(gap), Span::styled(d, fg(DIM))]),
                inner.width,
            );
            y += 1;
        }
        y += 1;
        for l in &about {
            put(buf, inner.x, y, &Line::styled(l.clone(), fg(FAINT)), inner.width);
            y += 1;
        }
    });
}
