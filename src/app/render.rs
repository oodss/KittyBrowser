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

/// A PTY write can finish while Rex is still parsing the previous image.
/// Its terminal status reply is ordered after those bytes. Only page frames
/// wait for it; toolbar updates and browser input remain independent.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Receipt {
    Disabled,
    Ready,
    Writing,
    Waiting(Instant),
}

impl Receipt {
    pub fn allows_frame(&mut self, now: Instant) -> bool {
        if matches!(self, Self::Waiting(at) if now.duration_since(*at) >= Duration::from_secs(1)) {
            // An unanswered status query has no ID. Stop querying so a late
            // reply can never release the wrong frame.
            *self = Self::Disabled;
        }
        matches!(self, Self::Ready | Self::Disabled)
    }

    pub fn request(&mut self) -> bool {
        if *self != Self::Ready {
            return false;
        }
        *self = Self::Writing;
        true
    }

    pub fn written(&mut self, now: Instant) {
        if *self == Self::Writing {
            *self = Self::Waiting(now);
        }
    }

    pub fn received(&mut self) {
        if matches!(self, Self::Writing | Self::Waiting(_)) {
            *self = Self::Ready;
        }
    }
}

pub(super) struct Frame {
    pub png: Arc<String>,
    pub request_receipt: bool,
    pub route: Route,
    pub scaled: bool,
    pub cells: (u16, u16),
    pub cell: (u32, u32),
    pub top: u16,
}

pub(super) enum Command {
    Draw { controls: term::PendingOutput, frame: Option<Frame>, overlay: Vec<u8> },
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
            Command::Draw { mut controls, frame, overlay } => {
                let mut bytes = b"\x1b[?2026h".to_vec();
                if controls.clear {
                    page.forget();
                }
                controls.append(&mut bytes);
                let request_receipt = frame.as_ref().is_some_and(|frame| frame.request_receipt);
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
                if request_receipt {
                    bytes.extend_from_slice(b"\x1b[5n");
                }
                let mut out = io::stdout().lock();
                out.write_all(&bytes)?;
                out.flush()?;
                page.written(Instant::now());
                if events.send(Msg::OutputReady { files_failed: page.files_failed }).is_err() {
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
    fn early_late_and_missing_replies() {
        let now = Instant::now();
        let later = now + Duration::from_secs(2);
        let mut receipt = Receipt::Ready;
        assert!(receipt.request());
        assert!(!receipt.allows_frame(later));
        receipt.received();
        receipt.written(later);
        assert!(receipt.allows_frame(later));
        assert!(receipt.request());
        receipt.written(now);
        assert!(!receipt.allows_frame(now));
        receipt.received();
        assert!(receipt.request());
        receipt.written(now);
        assert!(receipt.allows_frame(later));
        receipt.received();
        assert!(!receipt.request());
    }
}
