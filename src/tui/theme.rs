//! Palette, easing, glyph fonts — the visual vocabulary of the TUI.

use crate::providers::Provider;
use ratatui::style::{Color, Modifier, Style};

pub const TEXT: Color = Color::Rgb(226, 227, 234);
pub const DIM: Color = Color::Rgb(128, 131, 148);
pub const FAINT: Color = Color::Rgb(72, 75, 90);
pub const GHOST: Color = Color::Rgb(44, 46, 56);
pub const ROW_HL: Color = Color::Rgb(30, 32, 42);
pub const OK: Color = Color::Rgb(126, 211, 135);
pub const WARN: Color = Color::Rgb(240, 184, 96);
pub const ERR: Color = Color::Rgb(240, 104, 104);

pub const CLAUDE: Color = Color::Rgb(217, 119, 87);
pub const CODEX: Color = Color::Rgb(94, 196, 170);

/// Stops of the signature gradient (Claude coral → rose → violet → Codex teal).
const STOPS: [(u8, u8, u8); 4] = [(217, 119, 87), (226, 108, 146), (150, 118, 230), (94, 196, 170)];

pub fn accent(p: Provider) -> Color {
    match p {
        Provider::Claude => CLAUDE,
        Provider::Codex => CODEX,
    }
}

pub fn rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (200, 200, 200),
    }
}

pub fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let (ar, ag, ab) = rgb(a);
    let (br, bg, bb) = rgb(b);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color::Rgb(l(ar, br), l(ag, bg), l(ab, bb))
}

/// Position `t` (any real) on the looping gradient.
pub fn gradient(t: f32) -> Color {
    let t = t.rem_euclid(1.0) * STOPS.len() as f32;
    let i = t.floor() as usize % STOPS.len();
    let j = (i + 1) % STOPS.len();
    let f = t - t.floor();
    let c = |(r, g, b): (u8, u8, u8)| Color::Rgb(r, g, b);
    mix(c(STOPS[i]), c(STOPS[j]), f)
}

pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

pub fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

/// 0..1..0 with the given period in seconds.
pub fn pulse(secs: f32, period: f32) -> f32 {
    ((secs / period * std::f32::consts::TAU).sin() + 1.0) / 2.0
}

pub fn fg(c: Color) -> Style {
    Style::default().fg(c)
}

pub fn bold(c: Color) -> Style {
    Style::default().fg(c).add_modifier(Modifier::BOLD)
}

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner(secs: f32) -> &'static str {
    SPINNER[(secs * 12.0) as usize % SPINNER.len()]
}

/// Two-line block lettering of "accountant".
pub const LOGO: [&str; 2] =
    ["▄▀█ █▀▀ █▀▀ █▀█ █ █ █▄ █ ▀█▀ ▄▀█ █▄ █ ▀█▀", "█▀█ █▄▄ █▄▄ █▄█ █▄█ █ ▀█  █  █▀█ █ ▀█  █ "];

pub const SCRAMBLE: [char; 6] = ['░', '▒', '▓', '▚', '▞', '▖'];

/// Three-row digits for one-time codes.
pub const DIGITS: [[&str; 3]; 10] = [
    ["█▀█", "█ █", "█▄█"],
    ["▀█ ", " █ ", "▄█▄"],
    ["▀▀█", "█▀▀", "█▄▄"],
    ["▀▀█", " ▀█", "▄▄█"],
    ["█ █", "▀▀█", "  █"],
    ["█▀▀", "▀▀█", "▄▄█"],
    ["█▀▀", "█▀█", "█▄█"],
    ["▀▀█", "  █", "  █"],
    ["█▀█", "█▀█", "█▄█"],
    ["█▀█", "▀▀█", "▄▄█"],
];

/// Cheap deterministic hash → [0, 1), for stable per-cell randomness.
pub fn noise(a: u64, b: u64) -> f32 {
    let mut x = a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    x ^= x >> 31;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 29;
    (x % 10_000) as f32 / 10_000.0
}

/// Human "in 1h 12m" / "3d" style durations.
pub fn short_duration(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        let (h, m) = (s / 3600, (s % 3600) / 60);
        if m == 0 { format!("{h}h") } else { format!("{h}h{m:02}m") }
    } else {
        format!("{}d", s / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_lines_align() {
        assert_eq!(LOGO[0].chars().count(), LOGO[1].chars().count());
        for d in DIGITS {
            assert!(d.iter().all(|row| row.chars().count() == 3));
        }
    }

    #[test]
    fn durations() {
        assert_eq!(short_duration(42), "42s");
        assert_eq!(short_duration(600), "10m");
        assert_eq!(short_duration(3600 + 12 * 60), "1h12m");
        assert_eq!(short_duration(7200), "2h");
        assert_eq!(short_duration(3 * 86_400 + 5), "3d");
    }

    #[test]
    fn gradient_loops() {
        assert_eq!(gradient(0.0), gradient(1.0));
        assert_eq!(mix(TEXT, TEXT, 0.3), TEXT);
    }
}
