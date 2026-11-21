//! Terminals report individual wheel ticks, even for a continuous trackpad gesture.

use crate::term::input::{Mouse, MouseKind};
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

#[derive(Default)]
pub struct WheelScroll {
    burst: Option<Burst>,
}

struct Burst {
    tick: Mouse,
    deadline: Instant,
    extra: i32,
}

impl Burst {
    fn take_extra(&mut self) -> Option<Mouse> {
        let n = std::mem::take(&mut self.extra);
        let MouseKind::Wheel(x, y) = self.tick.kind else { unreachable!() };
        (n > 0).then_some(Mouse { kind: MouseKind::Wheel(x * n, y * n), ..self.tick })
    }
}

impl WheelScroll {
    /// Report an SGR tick (one unit on one axis). Keep separate targets,
    /// modifiers and axes in order. A reversal at the same target cancels
    /// unsent motion so the page follows the fingers back immediately.
    pub fn report(&mut self, tick: Mouse, now: Instant) -> [Option<Mouse>; 2] {
        let MouseKind::Wheel(x, y) = tick.kind else { unreachable!() };
        let opposite = Mouse { kind: MouseKind::Wheel(-x, -y), ..tick };
        let mut previous = None;
        if let Some(burst) = &mut self.burst {
            if burst.tick == tick {
                burst.extra = burst.extra.saturating_add(1);
                return [self.due(now), None];
            }
            if burst.tick != opposite {
                previous = burst.take_extra();
            }
        }
        self.burst = Some(Burst { tick, deadline: now + INTERVAL, extra: 0 });
        [previous, Some(tick)]
    }

    pub fn wait(&self, now: Instant) -> Option<Duration> {
        self.burst.as_ref().filter(|b| b.extra > 0).map(|b| b.deadline.saturating_duration_since(now))
    }

    pub fn due(&mut self, now: Instant) -> Option<Mouse> {
        let burst = self.burst.as_mut()?;
        if now < burst.deadline {
            return None;
        }
        let event = burst.take_extra()?;
        burst.deadline = now + INTERVAL;
        Some(event)
    }

    /// Deliver pending distance before a click, key, or coordinate change.
    pub fn finish(&mut self) -> Option<Mouse> {
        self.burst.take()?.take_extra()
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

    fn down() -> Mouse {
        Mouse { kind: MouseKind::Wheel(0, 1), button: None, x: 10, y: 20, ctrl: false, alt: false, shift: false }
    }

    #[test]
    fn isolated_ticks_are_immediate_and_bursts_preserve_distance_without_a_frame() {
        let mut scroll = WheelScroll::default();
        let now = Instant::now();
        assert_eq!(scroll.report(down(), now), [None, Some(down())]);
        assert_eq!(scroll.wait(now), None);
        for _ in 0..12 {
            assert_eq!(scroll.report(down(), now), [None, None]);
        }
        assert_eq!(scroll.wait(now), Some(INTERVAL));
        assert_eq!(scroll.due(now), None);
        assert_eq!(scroll.due(now + INTERVAL).unwrap().kind, MouseKind::Wheel(0, 12));
        assert!(scroll.due(now + INTERVAL * 2).is_none());
        assert_eq!(scroll.report(down(), now + INTERVAL * 3), [Some(down()), None]);
    }

    #[test]
    fn reversal_cancels_only_unsent_ticks_at_the_same_target() {
        let now = Instant::now();
        let up = Mouse { kind: MouseKind::Wheel(0, -1), ..down() };
        let mut scroll = WheelScroll::default();
        scroll.report(down(), now);
        scroll.report(down(), now);
        assert_eq!(scroll.report(up, now), [None, Some(up)]);
        assert_eq!(scroll.due(now + INTERVAL), None);
        for next in [
            Mouse { x: 11, ..down() },
            Mouse { ctrl: true, ..down() },
            Mouse { kind: MouseKind::Wheel(1, 0), ..down() },
        ] {
            scroll.finish();
            scroll.report(down(), now);
            scroll.report(down(), now);
            assert_eq!(scroll.report(next, now), [Some(down()), Some(next)]);
            scroll.report(next, now);
            assert_eq!(scroll.finish(), Some(next));
            assert_eq!(scroll.wait(now), None);
        }
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
