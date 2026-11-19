//! The opening screen: the extension's ASCII field (vscode/webview/
//! AsciiField.tsx, same waves and ramp) with the wordmark in its middle.
//! It moves while the agent starts, then stays as the header.

use std::time::{Duration, Instant};

use super::screen::{text_width, Color, Line, Style};

/// The brightness ramp, blank first (`AsciiField`'s).
const RAMP: [char; 9] = [' ', '.', ':', '-', '=', '+', '*', '#', '%'];
/// `AsciiField`'s cell size in pixels: its formulas are in pixels.
const CELL_W: f64 = 6.6;
const CELL_H: f64 = 14.0;
/// Seconds for every cell to have appeared, and for the wordmark.
const INTRO_S: f64 = 0.9;
const REVEAL_FROM_S: f64 = 0.45;
const REVEAL_S: f64 = 0.9;
/// The animation is over (the agent may still be starting).
pub(crate) const SETTLED: Duration = Duration::from_millis(1500);
const ROWS: usize = 8;
const TOP: usize = 2;

/// The wordmark, four rows: the logo's U (a stem with a gap, as in the
/// SVG), then "nharnessed".
const LOGO: [&str; 4] = ["│  ╵", "│   ", "│  ╷", "╰──╯"];
const LETTERS: &[(char, [&str; 4])] = &[
    ('n', ["   ", "╭─╮", "│ │", "╵ ╵"]),
    ('h', ["│  ", "├─╮", "│ │", "╵ ╵"]),
    ('a', ["   ", "╭─╮", "╭─┤", "╰─┘"]),
    ('r', ["   ", "╭─ ", "│  ", "╵  "]),
    ('e', ["   ", "╭─╮", "├─┘", "╰─ "]),
    ('s', ["   ", "╭─ ", "╰─╮", " ─╯"]),
    ('d', ["  │", "╭─┤", "│ │", "╰─┘"]),
];

fn wordmark() -> [String; 4] {
    let mut rows: [String; 4] = LOGO.map(str::to_string);
    for letter in "nharnessed".chars() {
        if let Some((_, glyph)) = LETTERS.iter().find(|(ch, _)| *ch == letter) {
            for (row, part) in rows.iter_mut().zip(glyph) {
                row.push(' ');
                row.push_str(part);
            }
        }
    }
    rows
}

pub(crate) struct Splash {
    started: Instant,
    seeds: Vec<f64>,
    still: bool,
}

impl Splash {
    pub fn new() -> Splash {
        // A fixed pseudo-random order for the cells' first appearance.
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15
            ^ std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| u64::from(d.subsec_nanos()));
        let seeds = (0..ROWS * 512)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                f64::from(u32::try_from(state >> 40).unwrap_or(0)) / f64::from(1u32 << 24)
            })
            .collect();
        let still = std::env::var_os("UNHARNESSED_NO_ANIMATION").is_some()
            || std::env::var("TERM").is_ok_and(|term| term == "dumb");
        Splash {
            started: Instant::now(),
            seeds,
            still,
        }
    }

    /// The animation has played out.
    pub fn settled(&self, now: Instant) -> bool {
        self.still || now.saturating_duration_since(self.started) >= SETTLED
    }

    /// The field and the wordmark at `now`, `width` columns wide.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )] // small grid coordinates and levels, all in range
    pub fn render(&self, width: usize, now: Instant) -> Vec<Line> {
        let t = if self.still {
            SETTLED.as_secs_f64()
        } else {
            now.saturating_duration_since(self.started).as_secs_f64()
        };
        let mark = wordmark();
        let mark_width = text_width(&mark[0]);
        let big = width >= mark_width + 12;
        let small = "◆ unharnessed";
        let (mark_rows, mark_cols): (Vec<String>, usize) = if big {
            (mark.to_vec(), mark_width)
        } else {
            (vec![small.to_string()], text_width(small))
        };
        let rows = if big { ROWS } else { 3 };
        let top = if big { TOP } else { 1 };
        let left = width.saturating_sub(mark_cols) / 2;
        // A clear margin around the wordmark.
        let (clear_from, clear_to) = (left.saturating_sub(3), left + mark_cols + 3);
        let intro = (t / INTRO_S).min(1.0);
        let reveal = ((t - REVEAL_FROM_S) / REVEAL_S).clamp(0.0, 1.0);
        let shown_cols = if self.still {
            mark_cols
        } else {
            4 + ((mark_cols - 4.min(mark_cols)) as f64 * reveal) as usize
        };
        let logo_on = self.still || t >= 0.12;
        let (cx, cy) = (width as f64 * CELL_W / 2.0, rows as f64 * CELL_H / 2.0);

        let mut out = Vec::with_capacity(rows);
        for r in 0..rows {
            let mut line = Line::new();
            let in_mark = r >= top && r < top + mark_rows.len();
            let y = r as f64 * CELL_H;
            for c in 0..width {
                if in_mark && c >= clear_from && c < clear_to {
                    if c == left {
                        let row = &mark_rows[r - top];
                        let (logo, rest) =
                            row.split_at(row.char_indices().nth(4).map_or(row.len(), |(i, _)| i));
                        let logo_style = if self.still || t > INTRO_S {
                            Style::PLAIN.bold()
                        } else {
                            Style::fg(Color::Accent).bold()
                        };
                        if logo_on {
                            line.push(logo, logo_style);
                        } else {
                            line.push(" ".repeat(text_width(logo)), Style::PLAIN);
                        }
                        let visible: String =
                            rest.chars().take(shown_cols.saturating_sub(4)).collect();
                        let hidden = text_width(rest).saturating_sub(text_width(&visible));
                        line.push(visible, Style::PLAIN.bold());
                        line.push(" ".repeat(hidden), Style::PLAIN);
                    } else if c < left || c >= left + mark_cols {
                        line.push(" ", Style::PLAIN);
                    }
                    continue;
                }
                let x = c as f64 * CELL_W;
                let mut v = 0.42
                    + 0.22 * (x * 0.021 + t * 0.6).sin() * (y * 0.017 - t * 0.45).cos()
                    + 0.18 * ((x * 0.7 - y) * 0.011 + t * 0.32).sin()
                    + 0.1 * ((x - cx).hypot(y - cy) * 0.03 - t * 0.9).sin();
                // Quieter near the wordmark's rows (AsciiField's band).
                let band = (y + CELL_H / 2.0 - cy) / (CELL_H * 2.2);
                v *= 1.0 - 0.55 * (-band * band).exp();
                let levels = RAMP.len();
                let mut level = ((v * levels as f64).floor().max(0.0) as usize).min(levels - 1);
                let mut hot = false;
                if intro < 1.0 {
                    let at = self.seeds[(r * 512 + c) % self.seeds.len()] * 0.85;
                    if intro < at {
                        level = 0;
                    } else if intro - at < 0.06 {
                        level = levels - 1;
                        hot = true;
                    }
                }
                let glyph = RAMP[level];
                let style = if hot {
                    Style::fg(Color::Accent)
                } else {
                    Style::fg(Color::Ansi(INK[level]))
                };
                line.push(glyph.to_string(), style);
            }
            out.push(line.truncate(width));
        }
        out
    }
}

/// The field's ink, faint to brighter (256-colour greys).
const INK: [u8; 9] = [236, 237, 238, 239, 240, 241, 242, 244, 246];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wordmark_rows_line_up() {
        let mark = wordmark();
        let widths: Vec<usize> = mark.iter().map(|row| text_width(row)).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "{widths:?}"
        );
    }

    #[test]
    fn every_row_fits_the_width() {
        let mut splash = Splash::new();
        splash.still = true;
        for width in [30, 60, 120] {
            let rows = splash.render(width, Instant::now());
            assert!(rows.iter().all(|row| row.width() <= width), "width {width}");
            assert!(rows
                .iter()
                .any(|row| row.text().contains("nharnessed") || row.text().contains("╭─╮")));
        }
    }
}
