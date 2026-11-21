//! Choose screencast pacing from visible changes, including canvas and embedded video.
use crate::term::graphics::{decode_png, Pixels};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const HOLD: Duration = Duration::from_secs(1);
const COLS: usize = 32;
const ROWS: usize = 24;
const MIN_CHANGED_POINTS: usize = 8;
type Sample = [[u8; 3]; COLS * ROWS];

fn sample(pixels: &Pixels) -> Sample {
    let (width, height) = (pixels.width as usize, pixels.height as usize);
    let channels = if pixels.alpha { 4 } else { 3 };
    std::array::from_fn(|i| {
        let x = (2 * (i % COLS) + 1) * width / (2 * COLS);
        let y = (2 * (i / COLS) + 1) * height / (2 * ROWS);
        let start = (y * width + x) * channels;
        [pixels.data[start], pixels.data[start + 1], pixels.data[start + 2]]
    })
}

#[derive(Default)]
struct Motion {
    previous: Option<(Instant, Sample)>,
    changed_last_time: bool,
    last_motion: Option<Instant>,
}

impl Motion {
    fn observe(&mut self, at: Instant, current: Sample) {
        // Two consecutive changes across the viewport distinguish sustained
        // motion from a single repaint. Ignore minor colour noise and cursors.
        let changed = self.previous.as_ref().is_some_and(|(last, previous)| {
            at.duration_since(*last) < HOLD
                && previous
                    .iter()
                    .zip(&current)
                    .filter(|(a, b)| a.iter().zip(b.iter()).any(|(a, b)| a.abs_diff(*b) >= 8))
                    .nth(MIN_CHANGED_POINTS - 1)
                    .is_some()
        });
        if changed && self.changed_last_time {
            self.last_motion = Some(at);
        }
        self.changed_last_time = changed;
        self.previous = Some((at, current));
    }

    fn moving(&self, now: Instant) -> bool {
        self.last_motion.is_some_and(|at| now.duration_since(at) < HOLD)
    }
}

pub struct FramePacing {
    motion: Motion,
    submit: mpsc::SyncSender<(Instant, Arc<String>)>,
    results: mpsc::Receiver<(Instant, Sample)>,
    last_submit: Option<Instant>,
    reset_at: Instant,
}

impl FramePacing {
    pub fn new() -> Self {
        // Inline PNG output normally needs no decoding. Keep sampling off the
        // input thread and skip a sample entirely if the worker is still busy.
        let (submit, frames) = mpsc::sync_channel::<(Instant, Arc<String>)>(0);
        let (send, results) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            for (at, frame) in frames {
                if let Some(pixels) =
                    crate::base64::decode(&frame).and_then(|bytes| decode_png(&bytes, std::mem::take(&mut buffer)))
                {
                    let grid = sample(&pixels);
                    buffer = pixels.data;
                    if send.send((at, grid)).is_err() {
                        break;
                    }
                }
            }
        });
        Self { motion: Motion::default(), submit, results, last_submit: None, reset_at: Instant::now() }
    }

    pub fn reset(&mut self) {
        self.motion = Motion::default();
        self.reset_at = Instant::now();
        self.last_submit = None;
    }

    /// Called only for distinct, correctly sized frames in a focused idle pane.
    pub fn frame(&mut self, frame: &Arc<String>, now: Instant) {
        for (at, grid) in self.results.try_iter() {
            if at >= self.reset_at {
                self.motion.observe(at, grid);
            }
        }
        if self.last_submit.is_none_or(|at| now.duration_since(at) >= SAMPLE_INTERVAL) {
            self.last_submit = Some(now);
            let _ = self.submit.try_send((now, Arc::clone(frame)));
        }
    }

    pub fn moving(&self, now: Instant) -> bool {
        self.motion.moving(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn motion_requires_repeated_changes_and_expires_after_pause() {
        let start = Instant::now();
        let mut motion = Motion::default();
        let mut grid = [[0; 3]; COLS * ROWS];
        // The small spinner persists. A larger region changes only from tick 3.
        for tick in 0..6 {
            let at = start + SAMPLE_INTERVAL * tick;
            let changed_points = if tick < 3 { 2 } else { 16 };
            let brightness = if tick % 2 == 0 { 16 } else { 200 };
            grid[..changed_points].fill([brightness; 3]);
            motion.observe(at, grid);
            assert_eq!(motion.moving(at), tick >= 4);
        }
        let resumed = start + SAMPLE_INTERVAL * 5 + HOLD;
        assert!(!motion.moving(resumed));
        motion.observe(resumed, [[7; 3]; COLS * ROWS]);
        assert!(!motion.moving(resumed));
    }

    #[test]
    fn grid_covers_small_and_odd_viewports_in_rgb_and_rgba() {
        for (width, height, alpha) in [(1, 1, false), (41, 29, true)] {
            let mut data = Vec::new();
            for y in 0..height {
                for x in 0..width {
                    data.extend([x as u8, y as u8, 81]);
                    if alpha {
                        data.push(255);
                    }
                }
            }
            let grid = sample(&Pixels { width, height, alpha, data });
            assert_eq!(grid[0], [0, 0, 81]);
            assert_eq!(grid[COLS * ROWS - 1], [(width - 1) as u8, (height - 1) as u8, 81]);
        }
    }
}
