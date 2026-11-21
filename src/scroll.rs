//! Terminals report individual wheel ticks, even for a continuous trackpad gesture.

use crate::term::input::{Mouse, MouseKind};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Deliver a single tick immediately. During a burst, send the accumulated
// distance at most one display interval later, even on pages that never repaint.
const INTERVAL: Duration = Duration::from_millis(16);

pub fn step(setting: Option<&str>) -> Result<u16, String> {
    match setting {
        None => Ok(32),
        Some(text) => {
            text.parse::<u16>().ok().filter(|&n| n > 0).ok_or_else(|| {
                "KB_SCROLL_STEP must be a whole number from 1 to 65535 (CSS pixels per wheel tick)".into()
            })
        }
    }
}

// Timestamp at the terminal reader. An old wheel packet must not become
// young again just because the app loop or Chrome finally caught up.
pub const MAX_AGE: Duration = Duration::from_millis(120);
const MAX_TICKS: usize = 8;
// A healthy Chrome can take several display intervals to acknowledge a wheel.
// Allow a short pipeline only for the same gesture while replies stay recent.
const PIPELINE_AGE: Duration = Duration::from_millis(80);
const WHEEL_SLOTS: usize = 3;

/// A single wakeup covers consecutive motion reports across PTY reads. Taking
/// the reports and deciding whether the reader needs to notify again share
/// one lock, so a last movement can never be stranded when the page goes idle.
#[derive(Default)]
pub struct Reports(Mutex<VecDeque<(Mouse, Instant)>>);

impl Reports {
    pub fn push(&self, mouse: Mouse, at: Instant) -> bool {
        let mut reports = self.0.lock().unwrap();
        let wake = reports.is_empty();
        if reports.len() == 32 {
            reports.pop_front();
        }
        reports.push_back((mouse, at));
        wake
    }

    pub fn take(&self) -> VecDeque<(Mouse, Instant)> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Motion {
    pub kind: MouseKind,
    pub x: f64,
    pub y: f64,
    pub modifiers: u32,
}

impl Motion {
    fn same_wheel(self, other: Self) -> bool {
        matches!(self.kind, MouseKind::Wheel(..)) && self == other
    }
}

/// Keep the requests already owned by Chrome separate from the samples that
/// kb can still replace. Hover and a changed wheel target wait for a reply.
#[derive(Default)]
pub struct Lane {
    requests: VecDeque<(u64, Motion, Instant)>,
    pending: VecDeque<(Motion, Instant)>,
    last: Option<(Motion, Instant)>,
}

impl Lane {
    pub fn offer(&mut self, event: Motion, at: Instant) {
        if !self.pending.back().is_some_and(|(previous, _)| previous.same_wheel(event)) {
            self.pending.clear();
        }
        if self.pending.len() == MAX_TICKS {
            self.pending.pop_front();
        }
        self.pending.push_back((event, at));
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.last = None;
    }

    pub fn wait(&self, now: Instant) -> Option<Duration> {
        let (next, _) = self.pending.front()?;
        if let Some((_, first, at)) = self.requests.front() {
            if self.requests.len() >= WHEEL_SLOTS || !first.same_wheel(*next) || now.duration_since(*at) >= PIPELINE_AGE
            {
                return None;
            }
        }
        let until = self.last.filter(|(last, _)| last.same_wheel(*next)).map(|(_, at)| at + INTERVAL);
        Some(until.map_or(Duration::ZERO, |at| at.saturating_duration_since(now)))
    }

    pub fn take(&mut self, now: Instant, flush: bool) -> Option<Motion> {
        if flush && !self.requests.is_empty() {
            return None;
        }
        let delay = self.wait(now)?;
        if !flush && !delay.is_zero() {
            return None;
        }
        self.pending.retain(|(event, at)| event.kind == MouseKind::Move || now.duration_since(*at) <= MAX_AGE);
        let (mut event, _) = *self.pending.back()?;
        self.last = Some((event, now));
        if let MouseKind::Wheel(dx, dy) = event.kind {
            let count = self.pending.len() as i32;
            event.kind = MouseKind::Wheel(dx * count, dy * count);
        }
        self.pending.clear();
        Some(event)
    }

    pub fn sent(&mut self, id: u64) {
        if id != 0 {
            let (event, at) = self.last.expect("only a dispatched motion has a request ID");
            self.requests.push_back((id, event, at));
        }
    }

    pub fn reply(&mut self, id: u64) -> bool {
        let Some(index) = self.requests.iter().position(|(request, _, _)| *request == id) else {
            return false;
        };
        self.requests.remove(index);
        true
    }
}

/// A second tick distinguishes a gesture from a single wheel step. The page
/// keeps its native CSS viewport throughout; only Chrome's capture size changes.
#[derive(Default)]
pub struct ScrollCapture {
    last_tick: Option<Instant>,
    pub preview: Option<(u32, u32)>,
    awaiting_first: Option<Instant>,
}

impl ScrollCapture {
    pub fn wheel(&mut self, now: Instant, native: (u32, u32)) -> bool {
        let previous = self.last_tick.replace(now);
        if self.preview.is_some()
            || !previous.is_some_and(|at| now - at <= Duration::from_millis(120))
            || u64::from(native.0) * u64::from(native.1) < 900_000
            || native.0.min(native.1) < 4
        {
            return false;
        }
        self.preview = Some((native.0.div_ceil(2), native.1.div_ceil(2)));
        self.awaiting_first = Some(now);
        true
    }

    pub fn accepts(&self, (width, height): (u32, u32)) -> bool {
        self.preview.is_some_and(|(w, h)| width.abs_diff(w) <= 2 && height.abs_diff(h) <= 2)
    }

    pub fn arrived(&mut self) {
        self.awaiting_first = None;
    }

    /// Some Chrome/window combinations cannot provide the requested size.
    /// Return to native capture even if wheel input never stops.
    pub fn unavailable(&self, now: Instant) -> bool {
        self.awaiting_first.is_some_and(|since| now - since >= Duration::from_millis(500))
    }

    pub fn wait(&self, now: Instant) -> Option<Duration> {
        self.preview?;
        Some((self.last_tick? + Duration::from_millis(180)).saturating_duration_since(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_only_during_sustained_large_scrolls() {
        let mut capture = ScrollCapture::default();
        let start = Instant::now();
        let tick = Duration::from_millis(40);
        assert!(!capture.wheel(start, (800, 600)));
        assert!(!capture.wheel(start + tick, (800, 600)));
        assert!(!capture.wheel(start + tick * 8, (1601, 901)));
        let active = start + tick * 9;
        assert!(capture.wheel(active, (1601, 901)));
        assert!(capture.accepts((800, 450)));
        assert!(!capture.accepts((1601, 901)));
        assert!(!capture.wheel(active + tick, (1601, 901)));
        assert_eq!(capture.wait(active + tick * 5), Some(Duration::from_millis(20)));
        assert_eq!(capture.wait(active + tick * 6), Some(Duration::ZERO));
        assert!(capture.unavailable(active + Duration::from_secs(1)));
        capture.arrived();
        assert!(!capture.unavailable(active + Duration::from_secs(1)));
    }

    #[test]
    fn one_reader_wakeup_covers_a_flood_and_the_next_tick_wakes_again() {
        let reports = Reports::default();
        let now = Instant::now();
        let mut mouse =
            Mouse { kind: MouseKind::Move, button: None, x: 0, y: 1, ctrl: false, alt: false, shift: false };
        for x in 0..1000 {
            mouse.x = x;
            assert_eq!(reports.push(mouse, now), x == 0);
        }
        let batch = reports.take();
        assert_eq!(batch.len(), 32);
        assert_eq!(batch.back(), Some(&(mouse, now)));
        assert!(reports.push(mouse, now));
    }

    fn wheel(dy: i32) -> Motion {
        Motion { kind: MouseKind::Wheel(0, dy), x: 10., y: 20., modifiers: 0 }
    }

    #[test]
    fn fast_input_is_immediate_and_recent_ticks_keep_their_distance() {
        let mut lane = Lane::default();
        let now = Instant::now();
        lane.offer(wheel(1), now);
        assert_eq!(lane.take(now, false), Some(wheel(1)));
        lane.sent(7);
        for _ in 0..4 {
            lane.offer(wheel(1), now);
        }
        assert_eq!(lane.wait(now), Some(INTERVAL));
        assert!(!lane.reply(8));
        assert!(lane.reply(7));
        assert_eq!(lane.wait(now), Some(INTERVAL));
        assert_eq!(lane.take(now + INTERVAL, false), Some(wheel(4)));
    }

    #[test]
    fn stalled_wheel_is_bounded_and_aged_from_each_input_sample() {
        let mut lane = Lane::default();
        let now = Instant::now();
        lane.offer(wheel(1), now);
        lane.take(now, false).unwrap();
        lane.sent(12);
        for _ in 0..1000 {
            lane.offer(wheel(1), now);
        }
        assert_eq!(lane.pending.len(), MAX_TICKS);
        let later = now + MAX_AGE + INTERVAL;
        lane.offer(wheel(1), later);
        assert!(lane.reply(12));
        assert_eq!(lane.take(later, false), Some(wheel(1)));
        lane.sent(13);
        for event in [wheel(1), wheel(-1), Motion { x: 30., ..wheel(-1) }, Motion { modifiers: 4, ..wheel(-1) }] {
            lane.offer(event, later);
            assert_eq!(lane.pending.len(), 1);
        }
        lane.clear();
        assert!(!lane.reply(12));
        assert!(lane.reply(13));
        assert_eq!(lane.take(later, true), None);
    }

    #[test]
    fn same_target_can_pipeline_but_age_and_capacity_bound_chrome_work() {
        let mut lane = Lane::default();
        let start = Instant::now();
        for i in 0..3 {
            let at = start + INTERVAL * i;
            lane.offer(wheel(1), at);
            assert_eq!(lane.take(at, false), Some(wheel(1)));
            lane.sent(u64::from(i + 1));
        }
        lane.offer(wheel(1), start + 3 * INTERVAL);
        assert_eq!(lane.wait(start + 3 * INTERVAL), None);
        assert!(lane.reply(2)); // A later reply cannot freshen the oldest request.
        assert_eq!(lane.wait(start + PIPELINE_AGE), None);
        lane.offer(wheel(-1), start + PIPELINE_AGE);
        assert_eq!(lane.take(start + PIPELINE_AGE, true), None);
        assert!(lane.reply(1));
        assert!(lane.reply(3));
        assert_eq!(lane.take(start + PIPELINE_AGE, false), Some(wheel(-1)));
    }

    #[test]
    fn only_the_latest_hover_survives_a_slow_reply() {
        let mut lane = Lane::default();
        let now = Instant::now();
        let hover = Motion { kind: MouseKind::Move, ..wheel(0) };
        lane.offer(hover, now);
        lane.take(now, false).unwrap();
        lane.sent(42);
        for x in 0..100 {
            lane.offer(Motion { x: f64::from(x), ..hover }, now);
        }
        assert_eq!(lane.take(now, true), None);
        lane.reply(42);
        assert_eq!(lane.take(now + MAX_AGE, false), Some(Motion { x: 99., ..hover }));
    }

    #[test]
    fn step_supports_single_pixel_and_legacy_wheel_distance() {
        assert_eq!(step(None), Ok(32));
        for n in ["1", "60", "65535"] {
            assert_eq!(step(Some(n)), Ok(n.parse().unwrap()));
        }
        for n in ["0", "-1", "1.5", "65536", "NaN", ""] {
            assert!(step(Some(n)).is_err());
        }
    }
}
