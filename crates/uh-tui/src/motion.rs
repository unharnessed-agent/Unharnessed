//! What moves: every animation is a function of the clock, so a frame at a
//! given time is always the same frame.

use crate::theme::{Rgb, ACCENT, WHITE};

/// The extension's soft curve (`--uh-soft`), near enough: fast, then settling.
#[must_use]
pub fn soft(p: f64) -> f64 {
    1.0 - (1.0 - p.clamp(0.0, 1.0)).powi(4)
}

/// How far a move of `span` seconds started at `from` has come, eased.
#[must_use]
pub fn progress(now: f64, from: f64, span: f64) -> f64 {
    soft((now - from) / span)
}

/// How long something new takes to come in (`uh-fade-in`).
pub const FADE_IN: f64 = 0.32;
/// How long a fold takes to close or open.
pub const FOLD: f64 = 0.36;

/// The spinner beside what runs.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[must_use]
pub fn spinner(now: f64) -> char {
    SPINNER[((now.max(0.0) / 0.08) as usize) % SPINNER.len()]
}

/// A slow breath between 0 and 1 (`uh-breathe`).
#[must_use]
pub fn breathe(now: f64) -> f32 {
    (0.5 - 0.5 * (now * std::f64::consts::TAU / 2.2).cos()) as f32
}

/// The light that crosses a live word every 1.4 s (`uh-shine`): how bright
/// letter `at` of `of` is, 0 to 1.
#[must_use]
pub fn shine(now: f64, at: usize, of: usize) -> f32 {
    let p = now.rem_euclid(1.4) / 1.4;
    // It rests a moment at each end.
    let p = ((p - 0.08) / 0.77).clamp(0.0, 1.0);
    let p = p * p * (3.0 - 2.0 * p);
    let centre = -0.35 + 1.7 * p;
    let x = (at as f64 + 0.5) / of.max(1) as f64;
    (1.0 - (x - centre).abs() / 0.35).max(0.0) as f32
}

/// A light running through a text, letter after letter (`uh-letter-light-back`):
/// letter `at`'s colour `since` seconds after it started. From its own
/// colour it flashes white, turns cyan, and comes back.
#[must_use]
pub fn lit(base: Rgb, since: f64, at: usize) -> Rgb {
    let p = (since - 0.022 * at as f64) / 0.9;
    if !(0.0..1.0).contains(&p) {
        return base;
    }
    let between =
        |from: Rgb, to: Rgb, lo: f64, hi: f64| from.mix(to, ((p - lo) / (hi - lo)) as f32);
    if p < 0.3 {
        between(base, WHITE, 0.0, 0.3)
    } else if p < 0.6 {
        between(WHITE, ACCENT, 0.3, 0.6)
    } else {
        between(ACCENT, base, 0.6, 1.0)
    }
}

/// How long a line that lands takes to be in: its letters come in one
/// after another, blue, then turn to their own colour (`landing`).
pub const LANDS: f64 = 1.6;
/// The longest its letters take to start, first to last, and the least
/// time between two; how long one takes to turn from the blue.
const LAND_SPREAD: f64 = 0.6;
const LAND_STEP: f64 = 0.03;
const LAND_COOL: f64 = 0.75;

/// A line landing (the kernel back, a compaction over, a level chosen):
/// letter `at` of `of`, `since` seconds after the first started. Answers
/// how far in it is, and how much of the blue it still has, each 0 to 1.
#[must_use]
pub fn landing(since: f64, at: usize, of: usize) -> (f32, f32) {
    let step = (LAND_SPREAD / of.max(1) as f64).min(LAND_STEP);
    let since = since - step * at as f64;
    let inn = (since / WAVE_FADE).clamp(0.0, 1.0);
    let cooled = ((since - WAVE_FADE * 0.6) / LAND_COOL).clamp(0.0, 1.0);
    let blue = 1.0 - cooled * cooled * (3.0 - 2.0 * cooled);
    (inn as f32, blue as f32)
}

/// How long a streamed word takes to come in once the front reaches it.
pub const WAVE_FADE: f64 = 0.26;
/// The reasoning's words are grey: from the page to them is half the way to
/// the ink, too little for the front to show. So they come in as the
/// answer's do, as bright, then cool to their grey over this long.
pub const THOUGHT_COOL: f64 = 0.7;
/// How far apart the text's updates come, until the stream says otherwise; the most a pause counts for.
const WAVE_GAP: f64 = 0.06;
const WAVE_GAP_MAX: f64 = 0.25;
/// How long after a word arrives the front has crossed it, at the most; the least it takes over an update.
const WAVE_MAX_LAG: f64 = 0.15;
const WAVE_MIN_SPAN: f64 = 0.03;

/// The front that brings a streaming text in (the extension's `wave.ts`):
/// each update's characters are spread, in order, over the time until the
/// next update is expected, and fade in from the page behind the front. The
/// effect follows the stream, never the reverse.
#[derive(Clone, Debug, Default)]
pub struct Wave {
    /// Each update: where its text ends (bytes), and when the front enters and leaves it.
    updates: Vec<(u32, f64, f64)>,
    /// When the last update came, and how far apart they come (smoothed).
    last: Option<f64>,
    gap: f64,
}

impl Wave {
    /// The text grew to `end` bytes at `now`.
    pub fn arrive(&mut self, now: f64, end: u32) {
        if let Some(last) = self.last {
            let since = (now - last).clamp(0.016, WAVE_GAP_MAX);
            self.gap = self.gap * 0.6 + since * 0.4;
        } else {
            self.gap = WAVE_GAP;
        }
        self.last = Some(now);
        let front = self.updates.last().map_or(now, |update| update.2);
        let from = now.max(front);
        let to = (now + self.gap)
            .max(from + WAVE_MIN_SPAN)
            .min(now + WAVE_MAX_LAG);
        // Behind: this update's characters go faster, from sooner.
        let start = from.min(to - WAVE_MIN_SPAN);
        self.updates.push((end, start, to));
    }

    /// When the text last grew.
    #[must_use]
    pub fn last(&self) -> Option<f64> {
        self.last
    }

    /// How much of the character at byte `src` is in at `now`, 0 to 1, when
    /// a character takes `fade` seconds to come in.
    #[must_use]
    pub fn alpha(&self, src: u32, now: f64, fade: f64) -> f32 {
        let at = self.updates.partition_point(|update| update.0 <= src);
        let Some(&(end, from, to)) = self.updates.get(at) else {
            return 1.0;
        };
        let start = at.checked_sub(1).map_or(0, |before| self.updates[before].0);
        let p = f64::from(src - start) / f64::from((end - start).max(1));
        (((now - (from + p * (to - from))) / fade).clamp(0.0, 1.0)) as f32
    }

    /// How long ago the front reached the character at byte `src`; negative
    /// while it is still to come. `None` once the wave has nothing on it.
    #[must_use]
    pub fn since(&self, src: u32, now: f64) -> Option<f64> {
        let at = self.updates.partition_point(|update| update.0 <= src);
        let &(end, from, to) = self.updates.get(at)?;
        let start = at.checked_sub(1).map_or(0, |before| self.updates[before].0);
        let p = f64::from(src - start) / f64::from((end - start).max(1));
        Some(now - (from + p * (to - from)))
    }

    /// Everything is in: nothing moves any more.
    #[must_use]
    pub fn settled(&self, now: f64, fade: f64) -> bool {
        self.updates
            .last()
            .is_none_or(|update| now > update.2 + fade)
    }
}
