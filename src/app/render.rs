//! One worker owns the terminal's image history. Preparing a delta can take
//! longer than writing it, especially on a large animated page.
use super::{log, Msg};
use crate::term::{
    self,
    page::{PageImage, Route},
};
use std::io::{self, Write};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// A PTY write can finish while Rex is still parsing the image. A numbered
/// Kitty query after the image measures that progress and can be retried
/// without a delayed reply ever releasing a newer frame.
const FIRST_PROBE: u32 = 1 << 30; // Above page, tile and layer image IDs.
const PROBE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
pub(super) struct Probe {
    id: u32,
    also_status: bool,
}

enum Delivery {
    Disabled,
    Ready,
    Writing,
    Waiting(Instant),
}

pub(super) struct Receipt {
    delivery: Delivery,
    id: u32,
    numbered: bool,
}

impl Receipt {
    pub fn new(rex: bool) -> Self {
        Self { delivery: if rex { Delivery::Ready } else { Delivery::Disabled }, id: FIRST_PROBE, numbered: false }
    }

    pub fn allows_frame(&self, now: Instant) -> bool {
        match self.delivery {
            Delivery::Ready | Delivery::Disabled => true,
            Delivery::Writing => false,
            // Older relays may only answer status queries, or send no
            // replies at all. Still show a recent snapshot during silence.
            Delivery::Waiting(at) => !self.numbered && now.duration_since(at) >= PROBE_INTERVAL,
        }
    }

    pub fn request(&mut self) -> Option<Probe> {
        if matches!(self.delivery, Delivery::Disabled) {
            return None;
        }
        self.id = if self.id == i32::MAX as u32 { FIRST_PROBE } else { self.id + 1 };
        self.delivery = Delivery::Writing;
        Some(Probe { id: self.id, also_status: !self.numbered })
    }

    pub fn retry(&mut self, now: Instant) -> Option<Probe> {
        if self.numbered && matches!(self.delivery, Delivery::Waiting(at) if now.duration_since(at) >= PROBE_INTERVAL) {
            self.delivery = Delivery::Writing;
            Some(Probe { id: self.id, also_status: false })
        } else {
            None
        }
    }

    pub fn written(&mut self, now: Instant) {
        if matches!(self.delivery, Delivery::Writing) {
            self.delivery = Delivery::Waiting(now);
        }
    }

    pub fn graphics(&mut self, id: u32) -> bool {
        if !(FIRST_PROBE..=i32::MAX as u32).contains(&id) {
            return false;
        }
        // Even an error reply proves that Rex parsed the preceding bytes.
        // A slow first reply can confirm support after a fallback snapshot
        // was sent. Only the current ID releases that newer snapshot.
        if id <= self.id && !matches!(self.delivery, Delivery::Disabled) {
            self.numbered = true;
            if self.id == id {
                self.delivery = Delivery::Ready;
            }
        }
        true
    }

    pub fn status(&mut self) {
        if !self.numbered && matches!(self.delivery, Delivery::Writing | Delivery::Waiting(_)) {
            self.delivery = Delivery::Ready;
        }
    }
}

pub(super) struct Frame {
    pub png: Arc<String>,
    pub route: Route,
    pub scaled: bool,
    pub cells: (u16, u16),
    pub cell: (u32, u32),
    pub top: u16,
}

pub(super) enum Command {
    Draw { controls: term::PendingOutput, frame: Option<Frame>, overlay: Vec<u8>, probe: Option<Probe> },
    Reply(u32, bool),
    Refresh,
}

pub(super) fn run(mut page: PageImage, jobs: mpsc::Receiver<Command>, events: &mpsc::Sender<Msg>) -> io::Result<()> {
    loop {
        // A static page can still need a recovery frame after a lost image
        // or file acknowledgement. Sleep until that deadline or the next job.
        let job = match page.deadline() {
            Some(at) => match jobs.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(job) => job,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if page.expire(Instant::now()) && events.send(Msg::Repaint).is_err() {
                        break;
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match jobs.recv() {
                Ok(job) => job,
                Err(_) => break,
            },
        };
        match job {
            Command::Refresh => page.refresh(),
            Command::Reply(id, ok) => {
                if page.reply(id, ok) && events.send(Msg::Repaint).is_err() {
                    break;
                }
            }
            Command::Draw { mut controls, frame, overlay, probe } => {
                let mut bytes = b"\x1b[?2026h".to_vec();
                if controls.clear {
                    page.forget();
                }
                controls.append(&mut bytes);
                if let Some(frame) = frame {
                    let drawn = page.draw(
                        &mut bytes,
                        &frame.png,
                        frame.route,
                        frame.scaled,
                        frame.cells.0,
                        frame.cells.1,
                        frame.cell,
                        frame.top,
                    );
                    log(format_args!("drew {drawn}"));
                }
                bytes.extend(overlay);
                bytes.extend_from_slice(b"\x1b[?2026l");
                if let Some(probe) = probe {
                    write!(bytes, "\x1b_Ga=q,i={},f=24,s=1,v=1;AAAA\x1b\\", probe.id)?;
                    if probe.also_status {
                        bytes.extend_from_slice(b"\x1b[5n");
                    }
                }
                let mut out = io::stdout().lock();
                out.write_all(&bytes)?;
                out.flush()?;
                let written_at = Instant::now();
                page.written(written_at);
                if events.send(Msg::OutputReady { files_failed: page.files_failed, written_at }).is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_and_late_receipts_only_release_their_own_frame() {
        let now = Instant::now();
        let later = now + PROBE_INTERVAL;
        let mut receipt = Receipt::new(true);
        let first = receipt.request().unwrap();
        assert!(first.also_status);
        receipt.graphics(first.id);
        receipt.written(now); // Replies can arrive before the writer notification.
        assert!(receipt.allows_frame(now));

        let second = receipt.request().unwrap();
        assert!(!second.also_status);
        receipt.written(now);
        receipt.status();
        receipt.graphics(first.id);
        assert!(!receipt.allows_frame(later));
        assert_eq!(receipt.retry(later).unwrap().id, second.id);
        receipt.written(later);
        receipt.graphics(second.id);
        let third = receipt.request().unwrap();
        receipt.graphics(second.id);
        assert!(!receipt.allows_frame(later));
        receipt.graphics(third.id);
        assert!(receipt.allows_frame(later));
    }

    #[test]
    fn a_late_first_probe_enables_pacing_without_releasing_the_next_frame() {
        let now = Instant::now();
        let mut receipt = Receipt::new(true);
        let first = receipt.request().unwrap();
        receipt.written(now);
        assert!(receipt.allows_frame(now + PROBE_INTERVAL));
        let next = receipt.request().unwrap();
        receipt.written(now + PROBE_INTERVAL);
        receipt.graphics(first.id);
        receipt.status();
        assert!(!receipt.allows_frame(now + PROBE_INTERVAL * 2));
        assert_eq!(receipt.retry(now + PROBE_INTERVAL * 2).unwrap().id, next.id);
    }

    #[test]
    fn missing_legacy_replies_allow_only_periodic_snapshots() {
        let now = Instant::now();
        let mut receipt = Receipt::new(true);
        receipt.request();
        receipt.written(now);
        assert!(!receipt.allows_frame(now));
        assert!(receipt.allows_frame(now + PROBE_INTERVAL));
        receipt.request();
        receipt.written(now + PROBE_INTERVAL);
        assert!(!receipt.allows_frame(now + PROBE_INTERVAL));
        receipt.status();
        assert!(receipt.allows_frame(now + PROBE_INTERVAL));
        assert!(Receipt::new(false).request().is_none());
    }
}
