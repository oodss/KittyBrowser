//! The page's image: changed tiles by file, moved layers through the pty,
//! or one image scaled to fit, whichever suits the terminal and the frame.

use super::graphics::{self, Tiles};
use super::layers::Layers;
use std::time::{Duration, Instant};

// A lost reply must not leave a static page waiting for more Chrome frames.
const FILE_TIMEOUT: Duration = Duration::from_secs(2);
// Rex can accept a transfer while an attached client loses its placement.
// Bound how long a visible page can depend on an older base image.
const PATCH_LIFETIME: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct PageImage {
    pub files: graphics::FrameFiles,
    /// The page as tiles, when it is drawn that way.
    tiles: Tiles,
    /// The page through the pty, moved as it scrolls and only what changed
    /// sent, when it is drawn that way (the terminal can't read files, as
    /// over ssh).
    layers: Layers,
    sent: Option<Instant>,
    first_patch: Option<Instant>,
    /// Chrome or the pane changed while the terminal was reading a batch.
    deferred: bool,
    pub files_failed: bool,
    /// Which drew the images on screen, if any.
    on_screen: Option<Drawer>,
}

/// How frames reach the terminal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
    /// In files, which the terminal reads itself.
    Files,
    /// Through the pty, only what moved or changed. This needs placements
    /// with source rectangles, offsets within cells, z and `o=z`.
    Layers,
    /// Moved layers with large payloads read locally by the Rex server.
    LayerFiles,
    /// Through the pty, whole PNG frames only (`KB_TRANSFER=png`): for a
    /// terminal with no more of the kitty protocol than that.
    Png,
}

impl Route {
    pub fn preferred(setting: Option<&str>, rex: bool) -> Self {
        match setting {
            Some("png") => Self::Png,
            Some("pty") => Self::Layers,
            Some("files") => Self::Files,
            _ if rex => Self::LayerFiles,
            _ => Self::Files,
        }
    }

    pub fn without_files(self) -> Self {
        match self {
            Self::Files => Self::Png,
            Self::LayerFiles => Self::Layers,
            route => route,
        }
    }
}

/// The ways of drawing the page.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Drawer {
    Tiles,
    Layers,
    /// One image, scaled to fit the cells.
    Whole,
}

impl PageImage {
    /// The screen was cleared of images: the next draw starts again.
    pub fn forget(&mut self) {
        self.tiles.forget();
        self.layers.forget();
        self.on_screen = None;
        self.first_patch = None;
    }

    /// A focus report can mean the client lost its placements. Rebuild a
    /// complete base while retaining the IDs needed to retire any survivors.
    pub fn refresh(&mut self) {
        self.tiles.refresh();
        self.layers.refresh();
        self.first_patch = None;
    }

    /// True when the caller should redraw Chrome's latest frame, including
    /// when a delayed reader has just finished and Chrome is already idle.
    pub fn reply(&mut self, id: u32, ok: bool) -> bool {
        if let Some(index) = self.files.pending.iter().position(|(image, _)| *image == id) {
            if !ok {
                self.fallback();
                return true;
            }
            self.files.pending.swap_remove(index);
            if self.files.pending.is_empty() {
                self.sent = None;
                return std::mem::take(&mut self.deferred);
            }
        } else if !ok && self.on_screen == Some(Drawer::Layers) && self.layers.holds(id) {
            self.refresh();
            return true;
        }
        false
    }

    /// A slow PTY can keep the filename itself in transit. Start the reply
    /// timeout only once that write finishes. Later UI writes do not extend it.
    pub fn written(&mut self, now: Instant) {
        if !self.files.pending.is_empty() {
            self.sent.get_or_insert(now);
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        [self.sent.map(|at| at + FILE_TIMEOUT), self.first_patch.map(|at| at + PATCH_LIFETIME)]
            .into_iter()
            .flatten()
            .min()
    }

    pub fn expire(&mut self, now: Instant) -> bool {
        if self.sent.is_some_and(|sent| now.duration_since(sent) >= FILE_TIMEOUT) {
            self.fallback();
            return true;
        }
        if self.first_patch.is_some_and(|at| now.duration_since(at) >= PATCH_LIFETIME) {
            self.refresh();
            return true;
        }
        false
    }

    fn fallback(&mut self) {
        // No later transfers use these IDs for acknowledged files, so a
        // delayed reply cannot acknowledge a different generation.
        self.files_failed = true;
        self.files.finish_batch();
        self.deferred = false;
        self.sent = None;
        self.forget();
    }

    /// Hand the screen to `drawer`. Each knows only its own images, so when
    /// another drew what is there, all of it goes first: left, it could
    /// cover what the new one draws.
    fn hand_to(&mut self, out: &mut Vec<u8>, drawer: Drawer) {
        if self.on_screen != Some(drawer) {
            out.extend_from_slice(graphics::DELETE);
            self.forget();
            self.on_screen = Some(drawer);
        }
    }

    /// Append the escapes that draw `png`, one of Chrome's frames, over
    /// `cols` x `rows` cells of `cell` pixels from screen row `top`.
    /// `scaled` says the frame is a fixed viewport, scaled to fit. Returns
    /// what was sent, to log.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        out: &mut Vec<u8>,
        png: &str,
        route: Route,
        scaled: bool,
        cols: u16,
        rows: u16,
        cell: (u32, u32),
        top: u16,
    ) -> String {
        if !self.files.pending.is_empty() {
            self.deferred = true;
            return "waiting for terminal to read the previous frame".to_owned();
        }
        let route = if self.files_failed { route.without_files() } else { route };
        let start = out.len();
        let mut pixels = match route {
            Route::Files => crate::base64::decode(png).and_then(|bytes| graphics::decode_png(&bytes, Vec::new())),
            Route::Layers | Route::LayerFiles | Route::Png => None,
        };
        // Only what changed, when the frame lines up with the cells: tiles
        // by file, or layers through the pty. Otherwise (a fixed viewport,
        // a frame from before a resize) one image scaled to fit.
        let lined_up = !scaled && graphics::png_size(png).is_some_and(|size| graphics::fits(size, cols, rows, cell));
        let drawer = match &pixels {
            Some(_) if lined_up => Drawer::Tiles,
            None if lined_up && matches!(route, Route::LayerFiles | Route::Layers) => Drawer::Layers,
            _ => Drawer::Whole,
        };
        self.hand_to(out, drawer);
        let before_draw = out.len();
        let drawn = match drawer {
            Drawer::Tiles => pixels
                .take()
                .and_then(|pixels| self.tiles.draw(out, &mut self.files, pixels, cols, rows, cell, top))
                .map(|sent| (sent == graphics::Drawn::Whole, format!("{sent:?}"))),
            Drawer::Layers => self
                .layers
                .draw(out, png, cols, rows, cell, top, (route == Route::LayerFiles).then_some(&mut self.files))
                .map(|sent| (matches!(sent, super::layers::Sent::Whole(_)), format!("{sent:?}"))),
            Drawer::Whole => None,
        };
        if let Some((whole, drawn)) = drawn {
            if whole {
                self.first_patch = None;
            } else if out.len() > before_draw {
                self.first_patch.get_or_insert_with(Instant::now);
            }
            return drawn;
        }
        if drawer == Drawer::Tiles {
            // A batch may fail halfway through preparation. Its escapes have
            // not been written yet; discard them with the unsubmitted files.
            out.truncate(start);
            self.fallback();
        }
        self.hand_to(out, Drawer::Whole);
        out.extend_from_slice(format!("\x1b[{top};1H").as_bytes());
        if let Some(pixels) = pixels {
            if let Ok(path) = self.files.page_file(graphics::IMAGE_ID, &pixels.data) {
                graphics::put_file(out, path, &pixels, cols, rows);
                return "the whole frame, by file".to_owned();
            }
            self.fallback();
        }
        if graphics::put_png(out, png, cols, rows, (route == Route::LayerFiles).then_some(&mut self.files)) {
            "the whole PNG, by file".to_owned()
        } else {
            "the whole frame, through the pty".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::layers::tests::{encode, frame, Screen};

    // 4 columns and 10 rows of 4x4 pixel cells, from screen row 2.
    const COLS: u16 = 4;
    const ROWS: u16 = 10;
    const CELL: (u32, u32) = (4, 4);

    /// A frame `width` pixels wide whose rows differ, `scrolled` down.
    fn page(width: u32, scrolled: u32) -> graphics::Pixels {
        frame(width, 40, |x, y| ((y + scrolled) * 7 % 251 + x) as u8)
    }

    /// Draw `new` and check the screen then shows it, and only it. Returns
    /// the escapes sent.
    fn shows(image: &mut PageImage, screen: &mut Screen, new: &graphics::Pixels, route: Route) -> String {
        let mut out = Vec::new();
        let drawn = image.draw(&mut out, &encode(new), route, false, COLS, ROWS, CELL, 2);
        screen.feed(&out, CELL);
        let ids: Vec<_> = image.files.pending.iter().map(|(id, _)| *id).collect();
        for id in ids {
            assert!(!image.reply(id, true));
        }
        assert!(screen.page(2, CELL, 16, 40) == new.data, "after {drawn}");
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_frame_drawn_whole_goes_when_layers_take_over() {
        // A frame a column narrow, as one rendered before a resize, goes
        // whole between two that layers draw.
        let (mut image, mut screen) = (PageImage::default(), Screen::default());
        shows(&mut image, &mut screen, &page(16, 0), Route::Layers);
        let mut out = Vec::new();
        image.draw(&mut out, &encode(&page(12, 5)), Route::Layers, false, COLS, ROWS, CELL, 2);
        screen.feed(&out, CELL);
        shows(&mut image, &mut screen, &page(16, 10), Route::Layers);
    }

    #[test]
    fn tiles_take_over_from_layers() {
        // The terminal's answer that it reads files comes after layers
        // drew the first frame.
        let (mut image, mut screen) = (PageImage::default(), Screen::default());
        shows(&mut image, &mut screen, &page(16, 0), Route::Layers);
        shows(&mut image, &mut screen, &page(16, 3), Route::Files);
        shows(&mut image, &mut screen, &page(16, 6), Route::Files);
    }

    #[test]
    fn a_slow_reader_keeps_the_base_until_it_can_apply_the_next_frame() {
        let (mut image, mut screen) = (PageImage::default(), Screen::default());
        let mut base = Vec::new();
        let mut newer = Vec::new();
        let draw = |image: &mut PageImage, out: &mut Vec<u8>, scroll| {
            image.draw(out, &encode(&page(16, scroll)), Route::Files, false, COLS, ROWS, CELL, 2)
        };
        draw(&mut image, &mut base, 0);
        draw(&mut image, &mut newer, 4);
        // Rex receives the first filename after Chrome has produced a newer frame.
        assert!(newer.is_empty());
        assert!(!image.reply(999, true));
        screen.feed(&base, CELL);
        assert_eq!(screen.page(2, CELL, 16, 40), page(16, 0).data);
        assert!(image.reply(graphics::IMAGE_ID, true));
        shows(&mut image, &mut screen, &page(16, 4), Route::Files);
    }

    #[test]
    fn large_layer_files_preserve_pixels_and_recover_to_inline_layers() {
        let (mut image, mut screen) = (PageImage::default(), Screen::default());
        let mut pixels = frame(512, 192, |_, _| 0);
        let mut seed = 17u32;
        let mut fill = |bytes: &mut [u8]| {
            for byte in bytes {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *byte = (seed >> 24) as u8;
            }
        };
        fill(&mut pixels.data);
        let draw = |image: &mut PageImage, pixels: &graphics::Pixels| {
            let mut out = Vec::new();
            image.draw(&mut out, &encode(pixels), Route::LayerFiles, false, 64, 12, (8, 16), 2);
            out
        };
        for initial in [true, false] {
            if !initial {
                fill(&mut pixels.data[..512 * 3 * 40]);
            }
            let out = draw(&mut image, &pixels);
            let commands = String::from_utf8_lossy(&out);
            assert!(commands.contains(if initial { "f=100" } else { "o=z" }));
            assert!(commands.contains("t=t,q=0"));
            screen.feed(&out, (8, 16));
            assert_eq!(screen.page(2, (8, 16), 512, 192), pixels.data);
            let ids: Vec<_> = image.files.pending.iter().map(|(id, _)| *id).collect();
            assert!(!ids.is_empty());
            for id in ids {
                assert!(!image.reply(id, true));
            }
        }
        pixels.data[0] ^= 1;
        let small = draw(&mut image, &pixels);
        assert!(!String::from_utf8_lossy(&small).contains("t=t"));
        screen.feed(&small, (8, 16));
        image.refresh();
        draw(&mut image, &pixels);
        let (id, path) = &image.files.pending[0];
        let path = path.to_path_buf();
        assert!(image.reply(*id, false));
        assert!(!path.exists());
        screen.feed(&draw(&mut image, &pixels), (8, 16));
        pixels.data[1] ^= 1;
        let patch = draw(&mut image, &pixels);
        assert!(String::from_utf8_lossy(&patch).contains("o=z"));
        screen.feed(&patch, (8, 16));
        assert!(image.files.pending.is_empty());
        assert_eq!(screen.page(2, (8, 16), 512, 192), pixels.data);
    }

    #[test]
    fn missing_or_rejected_file_reply_recovers_even_when_chrome_is_idle() {
        for timeout in [false, true] {
            let (mut image, mut screen) = (PageImage::default(), Screen::default());
            let png = encode(&page(16, 0));
            let mut lost = Vec::new();
            image.draw(&mut lost, &png, Route::Files, true, COLS, ROWS, CELL, 2);
            let path = image.files.pending[0].1.to_path_buf();
            assert!(path.exists());
            assert!(String::from_utf8_lossy(&lost).contains("q=0"));
            let sent = Instant::now() + FILE_TIMEOUT * 3;
            assert!(!image.expire(sent));
            image.written(sent);
            image.written(sent + FILE_TIMEOUT / 2);
            assert!(!image.expire(sent));
            if timeout {
                assert!(image.expire(sent + FILE_TIMEOUT));
            } else {
                assert!(image.reply(graphics::IMAGE_ID, false));
            }
            assert!(!path.exists());
            assert!(!image.reply(graphics::IMAGE_ID, true));
            let recovered = shows(&mut image, &mut screen, &page(16, 0), Route::Files);
            assert!(recovered.contains("f=100"));
            assert!(!recovered.contains("t=t"));
            assert!(!image.expire(sent + PATCH_LIFETIME));
        }
    }

    #[test]
    fn focus_or_resize_during_delivery_waits_for_the_old_ack_then_rebuilds() {
        let mut image = PageImage::default();
        let mut screen = Screen::default();
        let png = encode(&page(16, 0));
        let mut old = Vec::new();
        image.draw(&mut old, &png, Route::Files, false, COLS, ROWS, CELL, 2);
        image.forget();
        let mut new = Vec::new();
        image.draw(&mut new, &png, Route::Files, false, COLS, ROWS, CELL, 2);
        assert!(new.is_empty());
        screen.feed(&old, CELL);
        assert!(image.reply(graphics::IMAGE_ID, true));
        screen = Screen::default();
        let rebuilt = shows(&mut image, &mut screen, &page(16, 0), Route::Files);
        assert!(rebuilt.contains("t=t,i=1,"));
    }

    #[test]
    fn reconstructs_silently_lost_base_after_partial_updates() {
        let cell = (2, 4);
        let (cols, rows) = (30, 14);
        let mut image = PageImage::default();
        let mut screen = Screen::default();
        let mut pixels = frame(60, 56, |_, _| 5);
        let draw = |image: &mut PageImage, screen: &mut Screen, pixels: &graphics::Pixels| {
            let mut out = Vec::new();
            image.draw(&mut out, &encode(pixels), Route::Files, false, cols, rows, cell, 2);
            screen.feed(&out, cell);
        };
        draw(&mut image, &mut screen, &pixels);
        assert!(!image.reply(graphics::IMAGE_ID, true));
        // The server acknowledged the base but the client lost its graphics.
        screen = Screen::default();
        pixels.data[0] = 1;
        *pixels.data.last_mut().unwrap() = 9;
        draw(&mut image, &mut screen, &pixels);
        assert_ne!(screen.page(2, cell, 60, 56), pixels.data);
        assert_eq!(image.files.pending.len(), 2);
        let second = image.files.pending[1].0;
        let first = image.files.pending[0].0;
        assert!(!image.reply(second, true));
        assert!(!image.reply(first, true));
        assert!(image.expire(image.first_patch.unwrap() + PATCH_LIFETIME));
        draw(&mut image, &mut screen, &pixels);
        assert_eq!(screen.page(2, cell, 60, 56), pixels.data);
        assert!(!image.reply(graphics::IMAGE_ID, true));
        assert!(!image.expire(Instant::now() + PATCH_LIFETIME));
    }

    #[test]
    fn refresh_places_the_complete_frame_before_retiring_old_images() {
        for route in [Route::Files, Route::Layers, Route::LayerFiles, Route::Png] {
            let (mut image, mut screen) = (PageImage::default(), Screen::default());
            shows(&mut image, &mut screen, &page(16, 0), route);
            shows(&mut image, &mut screen, &page(16, 5), route);
            image.refresh();
            let update = shows(&mut image, &mut screen, &page(16, 5), route);
            let placed = update.find("\x1b_Ga=T").unwrap();
            assert!(!update[..placed].contains("\x1b_Ga=d"));
            assert!(!update.contains("a=d,d=A"));
        }
    }

    #[test]
    fn explicit_transfer_choices_override_the_rex_default() {
        assert_eq!(Route::preferred(None, true), Route::LayerFiles);
        assert_eq!(Route::preferred(None, false), Route::Files);
        for rex in [true, false] {
            for (setting, route) in [("files", Route::Files), ("pty", Route::Layers), ("png", Route::Png)] {
                assert_eq!(Route::preferred(Some(setting), rex), route);
            }
        }
    }

    #[test]
    fn scaled_rex_frames_keep_the_requested_display_size() {
        let pixels = frame(320, 200, |x, y| x.wrapping_mul(y + 71) as u8);
        let mut image = PageImage::default();
        let mut out = Vec::new();
        image.draw(&mut out, &encode(&pixels), Route::LayerFiles, true, 29, 12, (8, 16), 3);
        let commands = String::from_utf8_lossy(&out);
        assert!(commands.contains("c=29,r=12,t=t,q=0"));
        let (_, path) = &image.files.pending[0];
        assert_eq!(graphics::decode_png(&std::fs::read(path).unwrap(), Vec::new()).unwrap().data, pixels.data);
    }

    #[test]
    fn png_frames_only_when_asked() {
        // Each frame whole, as Chrome sent it, scaled to the cells: no
        // placements, offsets or compressed pixels.
        let (mut image, mut screen) = (PageImage::default(), Screen::default());
        for scrolled in [0, 3, 6] {
            let text = shows(&mut image, &mut screen, &page(16, scrolled), Route::Png);
            assert!(text.contains("\x1b[2;1H\x1b_Ga=T,f=100,i=1,p=1,"), "{text:?}");
            assert!(text.contains(",c=4,r=10,"), "{text:?}");
            for key in ["a=p", "o=z", "X=", "Y="] {
                assert!(!text.contains(key), "{text:?}");
            }
        }
    }
}
