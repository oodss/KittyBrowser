//! The event loop: owns the terminal, the engine, and the UI state.
//!
//! Loop: wait for input or a message from the engine; update state; draw
//! only what changed. Row 1 is the top bar, the page image fills the rest.

mod dialog;
mod render;

use crate::control::{self, Output, Request};
mod downloads;

use crate::devtools::{Devtools, Page};
use crate::engine::chrome::{self, Chrome, PageEvent};
mod file_picker;

use crate::history::History;
use crate::keys::{Action, Bindings};
use crate::term::input::{Button, Event, Key, KeyCode, Mouse, MouseKind, Parser};
use crate::term::page::{PageImage, Route};
use crate::term::size::{self, Window};
use crate::term::{self, graphics};
use crate::ui::{self, AddressBar, Edit, Hit};
use std::fs::File;
use std::io::{Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub struct Options {
    pub url: Option<String>,
    /// Device pixels per CSS pixel.
    pub scale: f64,
    /// Serve Chrome DevTools on a local port, for `devtools` (F12).
    pub devtools: bool,
    /// Tell pages to use a dark (true) or light colour scheme, rather than
    /// following the terminal's background.
    pub dark: Option<bool>,
}

/// How long a note in the bar stays.
const NOTICE: Duration = Duration::from_millis(2500);

/// Find the element under a point and the pointer it calls for, as CSS
/// names it: what the page asks for, or where it says `auto`, a text cursor
/// over text and fields and an arrow elsewhere.
const POINTER: &str = "((x, y) => { let e = document.elementFromPoint(x, y); \
  while (e && e.shadowRoot) { const i = e.shadowRoot.elementFromPoint(x, y); if (!i || i === e) break; e = i; } \
  if (!e) return 'default'; \
  const c = getComputedStyle(e).cursor.split(',').pop().trim(); \
  if (c !== 'auto') return c; \
  if (e.isContentEditable || e.matches('textarea, input:not([type=button], [type=submit], [type=reset], \
    [type=checkbox], [type=radio], [type=range], [type=color], [type=file], [type=image])')) return 'text'; \
  const r = document.caretRangeFromPoint && document.caretRangeFromPoint(x, y); \
  if (r && r.startContainer.nodeType === 3) { const t = document.createRange(); t.selectNodeContents(r.startContainer); \
    for (const b of t.getClientRects()) if (x >= b.left && x <= b.right && y >= b.top && y <= b.bottom) return 'text'; } \
  return 'default'; })";

/// The pointer shapes terminals know, by their CSS names.
const POINTERS: &[&str] = &[
    "default",
    "context-menu",
    "help",
    "pointer",
    "progress",
    "wait",
    "cell",
    "crosshair",
    "text",
    "vertical-text",
    "alias",
    "copy",
    "move",
    "no-drop",
    "not-allowed",
    "grab",
    "grabbing",
    "e-resize",
    "n-resize",
    "ne-resize",
    "nw-resize",
    "s-resize",
    "se-resize",
    "sw-resize",
    "w-resize",
    "ew-resize",
    "ns-resize",
    "nesw-resize",
    "nwse-resize",
    "col-resize",
    "row-resize",
    "all-scroll",
    "zoom-in",
    "zoom-out",
];

/// Inspect mode: outline the element under a point the way Chrome's
/// inspector does (margin, border, padding and content, with a label), and
/// remember it as the one a click would pick. Drawn into the page, in a
/// layer that ignores the mouse.
const HOVER: &str = "((x, y) => { let e = document.elementFromPoint(x, y); \
  while (e && e.shadowRoot) { const i = e.shadowRoot.elementFromPoint(x, y); if (!i || i === e) break; e = i; } \
  let root = document.getElementById('__kb_inspect'); \
  if (!root) { root = document.createElement('div'); root.id = '__kb_inspect'; \
    root.style.cssText = 'position:fixed;inset:0;pointer-events:none;z-index:2147483647'; \
    for (let i = 0; i < 5; i++) root.appendChild(document.createElement('div')); \
    document.documentElement.appendChild(root); } \
  if (!e || root.contains(e)) return; \
  globalThis.__kbInspected = e; \
  const r = e.getBoundingClientRect(), s = getComputedStyle(e), n = v => Math.max(0, parseFloat(v) || 0); \
  const sides = what => ['Top', 'Right', 'Bottom', 'Left'].map(k => n(s[what.replace('*', k)])); \
  const m = sides('margin*'), b = sides('border*Width'), p = sides('padding*'); \
  const [margin, border, padding, content, label] = root.children; \
  const layer = (el, left, top, w, h, edge, color) => { el.style.cssText = 'position:fixed;box-sizing:content-box;' + \
    'left:' + left + 'px;top:' + top + 'px;width:' + Math.max(0, w) + 'px;height:' + Math.max(0, h) + 'px;' + \
    'border-style:solid;border-color:' + color + ';border-width:' + edge.map(v => v + 'px').join(' '); }; \
  layer(margin, r.left - m[3], r.top - m[0], r.width, r.height, m, 'rgba(246,178,107,.66)'); \
  layer(border, r.left, r.top, r.width - b[1] - b[3], r.height - b[0] - b[2], b, 'rgba(255,229,153,.66)'); \
  layer(padding, r.left + b[3], r.top + b[0], r.width - b[1] - b[3] - p[1] - p[3], r.height - b[0] - b[2] - p[0] - p[2], p, 'rgba(147,196,125,.55)'); \
  layer(content, r.left + b[3] + p[3], r.top + b[0] + p[0], r.width - b[1] - b[3] - p[1] - p[3], r.height - b[0] - b[2] - p[0] - p[2], [0, 0, 0, 0], 'transparent'); \
  content.style.background = 'rgba(111,168,220,.66)'; \
  const name = e.localName + (e.id ? '#' + e.id : '') + [...e.classList].slice(0, 3).map(c => '.' + c).join(''); \
  label.textContent = name + '  ' + Math.round(r.width) + ' × ' + Math.round(r.height); \
  const top = r.top - m[0] > 24 ? r.top - m[0] - 22 : Math.min(innerHeight - 22, r.bottom + m[2] + 4); \
  label.style.cssText = 'position:fixed;left:' + Math.max(0, Math.min(r.left, innerWidth - 200)) + 'px;top:' + top + 'px;' + \
    'background:#202124;color:#e8eaed;font:12px/20px ui-monospace,Menlo,monospace;padding:0 6px;border-radius:3px;' + \
    'white-space:nowrap;max-width:90vw;overflow:hidden;text-overflow:ellipsis'; })";

/// Take inspect mode's outline off the page.
const UNHOVER: &str = "document.getElementById('__kb_inspect')?.remove(); delete globalThis.__kbInspected;";

// Read the selection only for a copy shortcut received from the terminal.
// A page must never have a standing callback into the client clipboard.
#[derive(Default)]
struct CopyRequest {
    world: Option<u64>,
    selection: Option<u64>,
}

impl CopyRequest {
    // Only replies to the outstanding physical shortcut may reach OSC 52.
    // Each reply is consumed once, including Chrome errors.
    fn consume(pending: &mut Option<u64>, id: u64) -> bool {
        if *pending == Some(id) {
            *pending = None;
            return true;
        }
        false
    }
}

const SELECTION: &str = r#"(() => {
  const el = document.activeElement;
  if (el && typeof el.selectionStart === 'number' && el.type !== 'password') {
    return el.value.slice(el.selectionStart, el.selectionEnd);
  }
  return String(window.getSelection() ?? '');
})()"#;

/// Describe the element inspected, as lines for a box: what it is, a
/// selector for it, its box and its main styles.
const DESCRIBE: &str = "(function() { const el = this, s = getComputedStyle(el), r = el.getBoundingClientRect(), n = Math.round; \
  const name = el.localName + (el.id ? '#' + el.id : '') + [...el.classList].map(c => '.' + c).join(''); \
  const path = []; \
  for (let e = el; e && e.nodeType === 1; e = e.parentElement) { \
    if (e.id && document.querySelectorAll('#' + CSS.escape(e.id)).length === 1) { path.unshift('#' + CSS.escape(e.id)); break; } \
    const same = e.parentElement ? [...e.parentElement.children].filter(c => c.localName === e.localName) : []; \
    path.unshift(e.localName + (same.length > 1 ? ':nth-of-type(' + (same.indexOf(e) + 1) + ')' : '')); \
    if (e.localName === 'body') break; } \
  const text = (el.innerText || el.value || '').replace(/\\s+/g, ' ').trim(); \
  const lines = ['\\u0002' + name, 'selector  ' + path.join(' > '), \
    'size      ' + n(r.width) + 'x' + n(r.height) + ' at ' + n(r.left + scrollX) + ',' + n(r.top + scrollY), \
    'display   ' + s.display + (s.position !== 'static' ? ', position ' + s.position : ''), \
    'font      ' + s.fontSize + ' ' + s.fontWeight + ' ' + s.fontFamily.split(',')[0], \
    'color     ' + s.color + ' on ' + s.backgroundColor, \
    'margin    ' + s.margin, 'padding   ' + s.padding]; \
  if (text) lines.push('text      ' + (text.length > 60 ? text.slice(0, 60) + '…' : text)); \
  return {lines}; }).call(globalThis.__kbInspected)";

enum Msg {
    Term(Event, Instant),
    MotionReports(Arc<crate::scroll::Reports>),
    TermClosed,
    Chrome(serde_json::Value),
    /// A command from `kb ctl` or the MCP server.
    Control(Request),
    ChromeClosed,
    OutputReady {
        written_at: Instant,
        files_failed: bool,
    },
    Repaint,
    OutputFailed,
}

impl Msg {
    fn receive(self, messages: &mut Vec<Self>) {
        match self {
            Self::MotionReports(batch) => {
                messages.extend(batch.take().into_iter().map(|(m, at)| Self::Term(Event::Mouse(m), at)))
            }
            other => messages.push(other),
        }
    }
}

/// How often to check whether the pane changed size.
const TICK: Duration = Duration::from_millis(100);
/// Presses this close in time and place count as a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// How long after the last key, click or command the page counts as in use.
const ACTIVE: Duration = Duration::from_millis(500);
/// The least time between frames when the page isn't in use, and when the
/// pane doesn't have focus either.
const MEDIA_GAP: Duration = Duration::from_nanos(1_000_000_000 / 24);
const IDLE_GAP: Duration = Duration::from_millis(100);
const UNFOCUSED_GAP: Duration = Duration::from_millis(500);
/// Cell size to assume, in device pixels, until the terminal tells us.
const FALLBACK_CELL: (u32, u32) = (16, 34);

/// Append a line to the file named by KB_LOG, if set. For diagnosing
/// terminals: what size was detected, what frames arrived, where clicks went.
fn log(line: std::fmt::Arguments) {
    static FILE: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    let file = FILE.get_or_init(|| {
        let path = std::env::var_os("KB_LOG")?;
        File::options().create(true).append(true).open(path).ok().map(Mutex::new)
    });
    if let Some(file) = file {
        let _ = writeln!(file.lock().unwrap(), "{line}");
    }
}

/// A retry click refers to the + the user saw. Its column can change when
/// the first tab strip appears. Keep that layout until the writer has sent
/// the result, and compare against the time the terminal reader saw the click.
struct TabButtonPress {
    request: u64,
    windows: (usize, usize),
    button: Button,
    drawn_at: Option<Instant>,
}

struct App {
    output: term::PendingOutput,
    downloads: downloads::Downloads,
    wheel_step: f64,
    wheel: crate::scroll::Lane,
    hover: crate::scroll::Lane,
    scroll_capture: crate::scroll::ScrollCapture,
    preview_enabled: bool,
    chrome: Chrome,
    scale: f64,
    window: Window,
    /// Cell size from a terminal query or Rex, for terminals whose window
    /// size carries no pixels.
    reported_cell: Option<(u32, u32)>,
    pixel_mouse: bool,
    bar: AddressBar,
    /// The find field, while it is open in the address bar's place.
    find: Option<AddressBar>,
    picker: Option<file_picker::Picker>,
    /// What was last searched for, offered again next time.
    find_text: String,
    history: History,
    devtools: Devtools,
    /// The page's viewport in CSS pixels.
    viewport: (u32, u32),
    /// A viewport size asked for with `kb ctl viewport`, in place of the
    /// pane's; a height of 0 means as tall as fills the pane.
    fixed: Option<(u32, u32)>,
    /// The cells the page image covers: the whole pane below the bar, or
    /// less when a fixed viewport has another shape.
    shown: (u16, u16),
    url: String,
    loading: bool,
    /// The newest frame, kept to redraw at a new size.
    frame: Option<Arc<String>>,
    frame_is_preview: bool,
    pacing: crate::motion::FramePacing,
    route: Route,
    /// Browser zoom: 1 is 100%. It scales the device pixels per CSS pixel,
    /// so the page reflows as in a desktop browser and stays sharp.
    zoom: f64,
    /// The id of the request for the page's width, after Ctrl+0.
    fit_request: Option<u64>,
    /// Whether the help is shown, and where it was last drawn.
    help: bool,
    help_rect: Option<ui::Rect>,
    /// This kb's number among those running, for `kb ctl -i`.
    instance: u32,
    /// When kb started, to turn the loading spinner by.
    started: Instant,
    /// Whether the pane has keyboard focus, as far as the terminal says.
    focused: bool,
    /// When the user or an agent last did something.
    last_input: Instant,
    /// When Chrome was last told it may send another frame.
    last_ack: Instant,
    /// Mouse buttons held, as a DevTools mask.
    buttons: u32,
    last_press: Option<(Instant, f64, f64, u32)>,
    bindings: Bindings,
    /// A note shown at the right of the bar for a moment: "Copied", ...
    notice: Option<(String, Instant)>,
    /// The request for the pointer shape under the mouse, and where to ask
    /// next once it is answered.
    pointer_request: Option<(u64, (f64, f64))>,
    pointer_next: Option<(f64, f64)>,
    pointer: String,
    /// The terminal's text and background colours, once it has said.
    term_fg: Option<ui::Rgb>,
    term_bg: Option<ui::Rgb>,
    /// The colour scheme pages are told: set with --dark or --light or by
    /// an agent, or else following the terminal.
    dark: Option<bool>,
    /// Whether inspect mode is on, and the request describing the element
    /// picked.
    inspecting: bool,
    describe_request: Option<u64>,
    copy_request: CopyRequest,
    file_probe: Option<std::path::PathBuf>,
    /// Lines for the box, in place of the help: an element's details.
    panel: Option<Vec<String>>,
    /// Whether the bar is asking to quit, after a click on its close
    /// button, and whether it was answered yes.
    dialogs: dialog::Dialogs,
    asking_quit: bool,
    tab_strip: bool,
    opening_tab: Option<(u64, Instant)>,
    tab_button: Option<TabButtonPress>,
    quit_clicked: bool,
}

pub fn run(options: Options) -> Result<(), String> {
    // Checked before the terminal is taken over, so the reason is printed
    // plainly instead of behind a blank pane.
    crate::engine::chrome::find_binary().map_err(|why| format!("{why}\nkb doctor checks your setup."))?;
    // Before anything is written to the terminal: a failure after that
    // leaves the terminal's replies to our queries for the shell to read.
    let wheel_step = crate::scroll::step(std::env::var("KB_SCROLL_STEP").ok().as_deref())?;
    let claim = control::claim()?;
    let profile = profile(claim.instance)?;
    let window = size::window().ok_or("can't read the terminal size")?;

    // Asked last, just before raw mode, with nothing left that can fail:
    // when the terminal answers too late for the question, kb's input
    // reader takes the replies instead of the shell.
    if std::env::var_os("KB_FORCE").is_none() && term::draws_graphics(crate::doctor::ASK_KB) == Some(false) {
        let _ = std::fs::remove_file(&claim.socket);
        eprint!("{}", crate::doctor::no_graphics());
        // Explained already; main has nothing to add.
        return Err(String::new());
    }
    term::enter().map_err(|e| e.to_string())?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        term::leave();
        hook(info);
    }));
    let socket = claim.socket.clone();
    let result = browse(options, &profile, window, claim, wheel_step);
    let _ = std::fs::remove_file(socket);
    term::leave();
    result
}

/// The profile kb `instance` keeps cookies and site data in. Chrome won't
/// share a profile between processes, so each kb running beside the first
/// has its own.
pub fn profile(instance: u32) -> Result<std::path::PathBuf, String> {
    let profile = crate::data_dir()?.join(match instance {
        1 => "profile".to_owned(),
        n => format!("profile-{n}"),
    });
    std::fs::create_dir_all(&profile).map_err(|e| format!("creating {}: {e}", profile.display()))?;
    Ok(profile)
}

fn browse(
    options: Options,
    profile: &std::path::Path,
    window: Window,
    claim: control::Claim,
    wheel_step: u16,
) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let instance = claim.instance;
    let commands = tx.clone();
    claim.listen(move |request| commands.send(Msg::Control(request)).is_ok());
    let input = tx.clone();
    std::thread::spawn(move || {
        let mut parser = Parser::default();
        let mut motion = Arc::new(crate::scroll::Reports::default());
        let mut buf = [0; 4096];
        let mut stdin = std::io::stdin().lock();
        loop {
            match stdin.read(&mut buf) {
                Ok(n) if n > 0 => {
                    let arrived = Instant::now();
                    for event in parser.feed(&buf[..n]) {
                        let message = match event {
                            Event::Mouse(
                                mouse @ Mouse { kind: MouseKind::Wheel(..) | MouseKind::Move, button: None, .. },
                            ) => {
                                if !motion.push(mouse, arrived) {
                                    continue;
                                }
                                Msg::MotionReports(Arc::clone(&motion))
                            }
                            other => {
                                // A key, drag, click or terminal reply seals the preceding
                                // batch so later movement cannot overtake it in the channel.
                                if Arc::strong_count(&motion) > 1 {
                                    motion = Arc::new(Default::default());
                                }
                                Msg::Term(other, arrived)
                            }
                        };
                        if input.send(message).is_err() {
                            return;
                        }
                    }
                }
                _ => {
                    let _ = input.send(Msg::TermClosed);
                    return;
                }
            }
        }
    });

    let (chrome, mut receiver) = Chrome::launch(profile, options.scale, options.devtools)?;
    let written = tx.clone();
    std::thread::spawn(move || {
        while let Ok(Some(message)) = receiver.recv() {
            if tx.send(Msg::Chrome(message)).is_err() {
                return;
            }
        }
        let _ = tx.send(Msg::ChromeClosed);
    });

    let mut app = App {
        output: Default::default(),
        downloads: Default::default(),
        wheel_step: f64::from(wheel_step),
        wheel: Default::default(),
        hover: Default::default(),
        scroll_capture: Default::default(),
        preview_enabled: std::env::var_os("KB_SCROLL_PREVIEW").as_deref() != Some(std::ffi::OsStr::new("0")),
        chrome,
        scale: options.scale,
        window,
        reported_cell: size::rex_cell(),
        pixel_mouse: false,
        bar: AddressBar::default(),
        find: None,
        picker: None,
        find_text: String::new(),
        history: History::load(crate::data_dir()?.join("history")),
        devtools: Devtools::default(),
        viewport: (0, 0),
        fixed: None,
        shown: (window.cols, window.rows.saturating_sub(1).max(1)),
        url: String::new(),
        loading: false,
        frame: None,
        frame_is_preview: false,
        pacing: crate::motion::FramePacing::new(),
        route: Route::Layers,
        zoom: 1.0,
        fit_request: None,
        help: false,
        help_rect: None,
        instance,
        started: Instant::now(),
        focused: true,
        last_input: Instant::now(),
        last_ack: Instant::now(),
        buttons: 0,
        last_press: None,
        bindings: Bindings::load(crate::data_dir().ok().map(|dir| dir.join("keys"))),
        notice: None,
        pointer_request: None,
        pointer_next: None,
        pointer: "default".to_owned(),
        term_fg: None,
        term_bg: None,
        dark: options.dark,
        inspecting: false,
        describe_request: None,
        copy_request: CopyRequest::default(),
        file_probe: None,
        panel: None,
        dialogs: Default::default(),
        asking_quit: false,
        tab_strip: false,
        opening_tab: None,
        tab_button: None,
        quit_clicked: false,
    };
    if let Some(dark) = options.dark {
        app.chrome.color_scheme(Some(dark));
    }
    if let Some(error) = app.bindings.errors.first() {
        app.note(&format!("~/.kittybrowser/keys {error}"));
    }
    app.layout();
    let mut page = PageImage::default();
    // Rex can read a server-side file even when the display is remote.
    // Moving existing placements avoids repeatedly forwarding raw page pixels.
    let rex = std::env::var_os("REX_SESSION").is_some() && std::env::var_os("REX_BLOCK").is_some();
    let mut receipt = render::Receipt::new(rex);
    let preferred = Route::preferred(std::env::var("KB_TRANSFER").ok().as_deref(), rex);
    match preferred {
        Route::Files | Route::LayerFiles => app.file_probe = graphics::ask_about_files(&mut page.files),
        route => app.route = route,
    }
    app.downloads.start(&mut app.chrome);
    app.chrome.hold_acks();
    app.chrome.intercept_files();
    app.chrome.start_screencast();
    match options.url.as_deref().and_then(ui::normalize_url) {
        Some(url) => app.navigate(&url),
        None => app.bar.focus(),
    }
    let (frames, incoming) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let result = render::run(page, incoming, &written);
        if result.is_err() {
            let _ = written.send(Msg::OutputFailed);
        }
        result
    });

    // A single draw spans preparation and output. While it is in flight,
    // only app.frame and the dirty flags change, so the next draw is current.
    frames.send(app.draw(false, None)).map_err(|_| "terminal writer stopped")?;
    let mut writing = true;
    let mut writing_tab_result = None;
    let mut toolbar_first = false;
    let (mut new_frame, mut redraw) = (false, false);

    let mut checked = Instant::now();
    let mut tab_input = Vec::new();
    let result = loop {
        // A paste or key can arrive in the same PTY read as Ctrl+T. Hold
        // it until Chrome has attached the requested tab so it cannot leak
        // into the page the user just left.
        if app.opening_tab.is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(5)) {
            app.opening_tab = None;
            tab_input.clear();
            app.note("Chrome took too long to open the tab");
            redraw = true;
        }
        let mut messages = if app.opening_tab.is_none() { std::mem::take(&mut tab_input) } else { Vec::new() };
        // Wake in time to let Chrome send its next frame.
        let wait = match app.chrome.ack_pending() {
            true => app.frame_gap().saturating_sub(app.last_ack.elapsed()).min(TICK),
            false => TICK,
        };
        let now = Instant::now();
        let wait = [Some(wait), app.wheel.wait(now), app.scroll_capture.wait(now)].into_iter().flatten().min().unwrap();
        if messages.is_empty() {
            match rx.recv_timeout(wait) {
                Ok(message) => message.receive(&mut messages),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break Ok(()),
            }
        }
        // A continuously busy page must still yield to wheel and frame deadlines.
        for message in rx.try_iter().take(128) {
            message.receive(&mut messages);
        }

        let mut quit = None;
        if messages.iter().any(|message| {
            matches!(message, Msg::Control(_) | Msg::Term(Event::Key(_) | Event::Mouse(_) | Event::Paste(_), _))
        }) {
            app.last_input = Instant::now();
        }
        let mut messages = messages.into_iter().peekable();
        while let Some(message) = messages.next() {
            // Age replaceable input at the PTY reader, including events held
            // while a tab was opening. Clicks, keys and drags retain their order.
            if let Msg::Term(Event::Mouse(mouse), at) = &message {
                if at.elapsed() > crate::scroll::MAX_AGE
                    && (matches!(mouse.kind, MouseKind::Wheel(..))
                        || (mouse.kind == MouseKind::Move && mouse.button.is_none() && app.buttons == 0))
                {
                    continue;
                }
            }
            match &message {
                Msg::Term(
                    Event::Key(_)
                    | Event::Paste(_)
                    | Event::Mouse(Mouse { kind: MouseKind::Press | MouseKind::Release, .. }),
                    _,
                )
                | Msg::Control(_) => {
                    // Preserve the last fresh distance if Chrome is ready. A
                    // click must never wait for a blocked wheel or hover.
                    app.send_wheel(true);
                    app.discard_motion();
                }
                Msg::Term(Event::Focus(_) | Event::PixelMouse(_) | Event::CellSize { .. }, _) => app.discard_motion(),
                _ => {}
            }
            if let Msg::Term(event, _) = &message {
                if app.picker_input(event) {
                    redraw = true;
                    continue;
                }
            }
            if let Msg::Term(Event::Mouse(mouse), at) = &message {
                if app.consume_tab_retry(*mouse, *at) {
                    continue;
                }
            }
            if (app.opening_tab.is_some() || !tab_input.is_empty())
                && matches!(message, Msg::Term(Event::Key(_) | Event::Mouse(_) | Event::Paste(_), _))
            {
                if !matches!(
                    message,
                    Msg::Term(
                        Event::Mouse(Mouse { kind: MouseKind::Wheel(..) | MouseKind::Move, button: None, .. }),
                        _
                    )
                ) {
                    tab_input.push(message);
                }
                continue;
            }
            if let Msg::Term(event, _) = &message {
                if app.dialog_input(event) {
                    redraw = true;
                    continue;
                }
            }
            if let Msg::Term(event, _) = &message {
                if app.download_input(event) {
                    redraw = true;
                    continue;
                }
            }
            match message {
                Msg::Term(Event::Mouse(mouse), at) => {
                    let next_is_hover = matches!(
                        messages.peek(),
                        Some(Msg::Term(Event::Mouse(Mouse { kind: MouseKind::Move, button: None, .. }), _))
                    );
                    if !(next_is_hover && app.buttons == 0 && mouse.kind == MouseKind::Move && mouse.button.is_none()) {
                        let changed = app.on_mouse(mouse, at);
                        redraw |= changed;
                        // Let a local button show its result before preparing
                        // another image of the animated page beneath it.
                        toolbar_first |=
                            changed && mouse.kind == MouseKind::Press && app.toolbar_column(mouse).is_some();
                    }
                    if app.quit_clicked {
                        quit = Some(Ok(()));
                        break;
                    }
                }
                Msg::Term(Event::Key(key), _) if app.asking_quit => {
                    match key.code {
                        KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                            quit = Some(Ok(()));
                            break;
                        }
                        _ if app.bindings.action(&key, false) == Some(Action::Quit) => {
                            quit = Some(Ok(()));
                            break;
                        }
                        KeyCode::Char('n' | 'N') | KeyCode::Esc => app.asking_quit = false,
                        _ => {}
                    }
                    redraw = true;
                }
                Msg::Term(Event::Key(key), _) => {
                    // Any key closes the help, and does nothing else.
                    if app.help && app.bindings.action(&key, false) != Some(Action::Quit) {
                        app.close_help();
                        redraw = true;
                        continue;
                    }
                    if app.on_key(key) {
                        quit = Some(Ok(()));
                        break;
                    }
                    redraw = true;
                }
                Msg::Term(Event::Color { slot, rgb }, _) => {
                    log(format_args!("terminal colour {slot}: {rgb:?}"));
                    match slot {
                        10 => app.term_fg = Some(rgb),
                        _ => app.term_bg = Some(rgb),
                    }
                    app.follow_terminal_colors();
                    redraw = true;
                }
                Msg::Term(Event::Paste(text), _) => {
                    if app.find.is_some() {
                        app.find_edit(|field| field.insert(&text));
                        redraw = true;
                    } else if app.bar.focused() {
                        app.bar.insert(&text);
                        redraw = true;
                    } else {
                        app.chrome.insert_text(&text);
                    }
                }
                Msg::Term(Event::CellSize { width, height }, _) => {
                    log(format_args!("terminal reports cell {width}x{height}"));
                    if app.reported_cell != Some((width, height)) {
                        app.reported_cell = Some((width, height));
                        app.layout();
                        new_frame = true;
                    }
                }
                Msg::Term(Event::Graphics { id, ok }, _) => {
                    if receipt.graphics(id) {
                        continue;
                    }
                    log(format_args!("graphics reply for image {id}: {ok}"));
                    let _ = frames.send(render::Command::Reply(id, ok));
                    if id == graphics::FILE_QUERY_ID {
                        if let Some(path) = app.file_probe.take() {
                            if ok {
                                app.route = preferred;
                            }
                            let _ = std::fs::remove_file(path);
                        }
                    }
                }
                Msg::Term(Event::Focus(focused), _) => {
                    log(format_args!("focused: {focused}"));
                    if focused {
                        // A reattached client can have the same dimensions but
                        // no cached graphics. Repaint even if Chrome is idle.
                        let _ = frames.send(render::Command::Refresh);
                        new_frame = true;
                    }
                    if focused != app.focused {
                        app.end_scroll_capture();
                        app.pacing.reset();
                    }
                    app.focused = focused;
                }
                Msg::Term(Event::StatusOk, _) => receipt.status(),
                Msg::Term(Event::PixelMouse(on), _) => {
                    log(format_args!("pixel mouse: {on}"));
                    app.pixel_mouse = on;
                }
                Msg::Control(request) => {
                    log(format_args!("control: {} {}", request.command, request.args));
                    if request.command == "quit" {
                        let _ = request.reply.send(Ok(Output::Text("kb is quitting; its pane closes".to_owned())));
                        quit = Some(Ok(()));
                        break;
                    }
                    if request.command == "scheme" {
                        let _ = request.reply.send(app.set_scheme(request.args["scheme"].as_str().unwrap_or("")));
                        continue;
                    }
                    if request.command == "viewport" {
                        // The one command about the pane rather than the page.
                        let _ = request.reply.send(app.set_viewport(request.args["size"].as_str().unwrap_or("")));
                        app.clear();
                        new_frame = true;
                        continue;
                    }
                    let page = Page {
                        url: &app.url,
                        loading: app.loading,
                        viewport: app.viewport,
                        scale: app.scale,
                        zoom: app.zoom,
                        viewport_set: app.fixed.is_some(),
                    };
                    app.devtools.command(request, &mut app.chrome, page);
                }
                Msg::Chrome(message) => {
                    if app.downloads.observe(&message) {
                        if message["method"] == "Browser.downloadWillBegin" {
                            app.note("Download started on server (↓)");
                        }
                        redraw = true;
                        continue;
                    }
                    if app.fit_request.is_some() && message["id"].as_u64() == app.fit_request {
                        app.fit_request = None;
                        app.fit(message["result"]["result"]["value"].as_f64());
                        new_frame = true;
                        continue;
                    }
                    if app.picker_event(&message) {
                        app.discard_motion();
                        redraw = true;
                        continue;
                    }
                    if let Some(id) = message["id"].as_u64() {
                        if app.wheel.reply(id) || app.hover.reply(id) {
                            continue;
                        }
                        if app.reply(id, &message) {
                            redraw = true;
                            continue;
                        }
                    }
                    if app.dialog_event(&message) {
                        app.discard_motion();
                        redraw = true;
                        continue;
                    }
                    if !app.chrome.shown(&message) {
                        continue;
                    }
                    app.devtools.observe(&message, &mut app.chrome);
                    match app.chrome.handle(message) {
                        Some(PageEvent::Frame(png)) => {
                            let size = graphics::png_size(&png);
                            let preview = size.is_some_and(|size| app.scroll_capture.accepts(size));
                            if preview {
                                app.scroll_capture.arrived();
                            }
                            let fits = preview || size.is_some_and(|size| app.native_size(size));
                            log(format_args!("frame {:?}, {} bytes, fits: {fits}", size, png.len()));
                            // A frame rendered for another size (the pane's
                            // first guess, or the size before a resize) would be
                            // stretched to fill the pane: wait for the right one.
                            // Some pages repaint without changing: the same
                            // frame again is nothing to draw.
                            if fits && app.frame.as_deref() != Some(&png) {
                                let frame = Arc::new(png);
                                if app.focused && app.last_input.elapsed() >= ACTIVE {
                                    app.pacing.frame(&frame, Instant::now());
                                }
                                app.frame_is_preview = preview;
                                app.frame = Some(frame);
                                new_frame = true;
                            }
                        }
                        Some(PageEvent::Url(url)) => {
                            app.went_to(url);
                            redraw = true;
                        }
                        Some(PageEvent::Shown(url)) => {
                            app.shown(url);
                            redraw = true;
                        }
                        Some(PageEvent::TabsChanged) => redraw = true,
                        Some(PageEvent::Created(id, url)) => {
                            if app.opening_tab.is_some_and(|(pending, _)| pending == id) {
                                app.opening_tab = None;
                                toolbar_first = true;
                            }
                            app.shown(url);
                            redraw = true;
                        }
                        Some(PageEvent::CreateFailed(id, text)) => {
                            if app.opening_tab.is_some_and(|(pending, _)| pending == id) {
                                app.opening_tab = None;
                                tab_input.clear();
                            }
                            app.note(&text);
                            redraw = true;
                        }
                        Some(PageEvent::Loading(loading)) => {
                            app.loading = loading;
                            redraw = true;
                        }
                        None => {}
                    }
                }
                Msg::MotionReports(_) => unreachable!("expanded on receipt"),
                Msg::Repaint => new_frame = true,
                Msg::OutputReady { files_failed, written_at } => {
                    receipt.written(written_at);
                    if let Some(id) = writing_tab_result.take() {
                        if let Some(press) = app.tab_button.as_mut().filter(|press| press.request == id) {
                            press.drawn_at = Some(written_at);
                        }
                    }
                    writing = false;
                    if files_failed {
                        app.route = app.route.without_files();
                        app.end_scroll_capture();
                    }
                }
                // Join the writer below to collect its error, even if the
                // terminal closed or the user quit in the same batch.
                Msg::OutputFailed | Msg::TermClosed => {
                    quit = Some(Ok(()));
                    break;
                }
                Msg::ChromeClosed => {
                    quit = Some(Err("Chrome exited".to_owned()));
                    break;
                }
            }
        }
        if let Some(result) = quit {
            break result;
        }

        if checked.elapsed() >= TICK {
            checked = Instant::now();
            app.chrome.retry_capture_if_due();
            // The spinner in the bar turns while a page loads.
            redraw |= app.loading;
            app.history.save_if_due();
            app.devtools.tick(&mut app.chrome);
            if app.bindings.reload_if_changed() {
                let note = match app.bindings.errors.first() {
                    Some(error) => format!("keys {error}"),
                    None => "Key bindings reloaded".to_owned(),
                };
                app.note(&note);
                redraw = true;
            }
            if app.notice.as_ref().is_some_and(|(_, at)| at.elapsed() >= NOTICE) {
                app.notice = None;
                redraw = true;
            }
            if let Some(window) = size::window().filter(|w| *w != app.window) {
                app.window = window;
                if window.width_px == 0 {
                    app.output.query_cell = true;
                }
                app.layout();
                // Whatever was on screen is at the old size: start clean.
                app.clear();
                new_frame = true;
            }
        }
        app.send_wheel(false);
        app.send_hover();
        if app.scroll_capture.unavailable(Instant::now()) {
            app.preview_enabled = false;
            app.end_scroll_capture();
        } else if app.scroll_capture.wait(Instant::now()) == Some(Duration::ZERO) {
            app.end_scroll_capture();
        }
        let strip = app.chrome.windows().1 > 1 && app.window.rows >= 3;
        if strip != app.tab_strip {
            app.tab_strip = strip;
            app.layout();
            app.clear();
            new_frame = true;
        }
        // Keep the last page visible behind the quit question. A confirmation
        // should cost one text update even when Chrome is animating underneath.
        let paint_page = new_frame
            && !toolbar_first
            && app.opening_tab.is_none()
            && !app.asking_quit
            && receipt.allows_frame(Instant::now());
        let retry = if !writing && new_frame && !paint_page { receipt.retry(Instant::now()) } else { None };
        if !writing && (paint_page || redraw || app.output.any() || retry.is_some()) {
            let probe = if paint_page && app.frame.is_some() { receipt.request() } else { retry };
            if frames.send(app.draw(paint_page, probe)).is_err() {
                break Err("terminal writer stopped".to_owned());
            }
            writing = true;
            writing_tab_result = app
                .tab_button
                .as_ref()
                .filter(|press| app.opening_tab.is_none() && press.drawn_at.is_none())
                .map(|press| press.request);
            toolbar_first = false;
            if paint_page {
                new_frame = false;
            }
            redraw = false;
        }
        // Keep receiving Chrome updates while stdout is busy. If we withhold
        // acknowledgements, Chrome can miss the final change to a static page.
        // Only app.frame, the latest snapshot, is retained for the next draw.
        while app.chrome.ack_pending() && app.last_ack.elapsed() >= app.frame_gap() {
            app.chrome.ack();
            if !app.frame_gap().is_zero() {
                app.last_ack = Instant::now();
            }
        }
    };
    // Finish already-started escapes before restoring the user's terminal or
    // dropping the private files they refer to. Honour a copy just before quit.
    if app.output.clipboard.is_some() {
        let _ = frames.send(app.draw(false, None));
    }
    drop(frames);
    let joined = writer.join();
    app.history.save();
    app.chrome.close();
    joined.map_err(|_| "terminal writer panicked".to_owned())?.map_err(|e| format!("writing the terminal: {e}"))?;
    result
}

impl App {
    /// One cell in device pixels.
    fn cell(&self) -> (u32, u32) {
        let w = self.window;
        if w.width_px > 0 && w.height_px > 0 {
            ((w.width_px / w.cols) as u32, (w.height_px / w.rows) as u32)
        } else {
            self.reported_cell.unwrap_or(FALLBACK_CELL)
        }
    }

    /// Pacing the acknowledgements lets Chrome skip work between frames.
    fn frame_gap(&self) -> Duration {
        if !self.focused || self.opening_tab.is_some() {
            UNFOCUSED_GAP
        } else if self.last_input.elapsed() < ACTIVE {
            Duration::ZERO
        } else if self.pacing.moving(Instant::now()) {
            MEDIA_GAP
        } else {
            IDLE_GAP
        }
    }

    /// Wipe the screen, images included. The help, if shown, is drawn
    /// again with the next frame.
    fn clear(&mut self) {
        self.output.clear = true;
        self.help_rect = None;
    }

    fn page_rows(&self) -> u16 {
        self.window.rows.saturating_sub(1 + u16::from(self.tab_strip)).max(1)
    }

    /// Size the page's viewport to the cells below the top bar.
    fn layout(&mut self) {
        self.discard_motion();
        self.end_scroll_capture();
        let (cell_w, cell_h) = self.cell();
        let (cols, rows) = (self.window.cols, self.page_rows());
        // The space for the page, in device pixels.
        let (space_w, space_h) = (cols as f64 * cell_w as f64, rows as f64 * cell_h as f64);
        let (width, height) = match self.fixed {
            // A width alone: as tall as fills the pane once that width fits it.
            Some((width, 0)) => {
                let fit = (space_w / (width as f64 * self.page_scale())).min(1.0);
                let height = (space_h / (self.page_scale() * fit)).floor();
                let cells = |pixels: f64, cell: u32, most: u16| ((pixels / cell as f64).round() as u16).clamp(1, most);
                let shown_w = cells(width as f64 * self.page_scale() * fit, cell_w, cols);
                self.shown = (shown_w, rows);
                (width as f64, height)
            }
            Some((width, height)) => {
                // Shown at its own size if it fits, else shrunk to fit,
                // keeping its shape as nearly as whole cells allow.
                let (frame_w, frame_h) = (width as f64 * self.page_scale(), height as f64 * self.page_scale());
                let fit = (space_w / frame_w).min(space_h / frame_h).min(1.0);
                let cells =
                    |pixels: f64, cell: u32, most: u16| ((pixels * fit / cell as f64).round() as u16).clamp(1, most);
                self.shown = (cells(frame_w, cell_w, cols), cells(frame_h, cell_h, rows));
                (width as f64, height as f64)
            }
            None => {
                self.shown = (cols, rows);
                // Rounded up, so a frame always covers the cells and can be
                // cut into tiles.
                ((space_w / self.page_scale()).ceil(), (space_h / self.page_scale()).ceil())
            }
        };
        log(format_args!(
            "window {:?}, cell {cell_w}x{cell_h} (reported {:?}), viewport {width}x{height}, shown in {:?} cells",
            self.window, self.reported_cell, self.shown
        ));
        let viewport = (width.round().max(1.0) as u32, height.round().max(1.0) as u32);
        if viewport != self.viewport {
            self.pacing.reset();
        }
        self.viewport = viewport;
        if self.frame.as_deref().is_some_and(|png| !graphics::png_size(png).is_some_and(|size| self.native_size(size)))
        {
            self.frame = None;
        }
        self.chrome.resize(self.viewport.0, self.viewport.1, self.scale);
    }

    /// Pane pixels per CSS pixel, zoom included.
    fn page_scale(&self) -> f64 {
        self.scale * self.zoom
    }

    /// Step the zoom up or down through the levels desktop browsers use.
    fn step_zoom(&mut self, up: bool) {
        const LEVELS: [f64; 15] = [0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];
        let next = match up {
            true => LEVELS.iter().find(|level| **level > self.zoom + 0.001),
            false => LEVELS.iter().rev().find(|level| **level < self.zoom - 0.001),
        };
        if let Some(level) = next {
            self.set_zoom(*level);
        }
    }

    fn set_zoom(&mut self, zoom: f64) {
        self.zoom = zoom;
        self.layout();
        self.clear();
    }

    /// Ctrl+0: back to 100%, then ask how wide the page is there, so that
    /// `fit` can zoom out a page too wide for the pane.
    fn zoom_to_fit(&mut self) {
        self.set_zoom(1.0);
        self.fit_request = Some(self.chrome.evaluate_value("document.documentElement.scrollWidth"));
    }

    /// Zoom out so a page `width` CSS pixels wide fits the pane, if it is
    /// wider than the pane at 100%.
    fn fit(&mut self, width: Option<f64>) {
        let Some(width) = width.filter(|width| *width > self.viewport.0 as f64 + 1.0) else { return };
        let zoom = (self.viewport.0 as f64 / width).max(0.25);
        log(format_args!("zoom to fit a page {width} wide: {zoom}"));
        self.set_zoom(zoom);
    }

    /// The help: keys, and what kb knows about the pane and page.
    fn help_lines(&self) -> Vec<String> {
        let mut banner = Vec::new();
        if ui::ART.iter().all(|line| line.chars().count() + 4 <= self.window.cols as usize) {
            banner.extend(ui::ART.iter().map(|line| format!("{}{line}", ui::ART_LINE)));
            banner.push(String::new());
        }
        let mut lines = Vec::new();
        lines.push(format!("{}kb {}: a browser in your terminal", ui::TITLE_LINE, env!("CARGO_PKG_VERSION")));
        lines.push(String::new());
        // The keys as bound now, the first two of each.
        let keys = |action: Action| {
            let all = self.bindings.keys_for(action);
            all.iter().take(2).cloned().collect::<Vec<_>>().join(", ")
        };
        let rows = [
            (Action::Address, "edit the address"),
            (Action::Find, "find on the page; Enter, Shift+Enter: next, previous"),
            (Action::Back, "back (or the mouse's back button)"),
            (Action::Forward, "forward"),
            (Action::Reload, "reload"),
            (Action::Downloads, "show server downloads (↓)"),
            (Action::Copy, "copy the selection, or the address"),
            (Action::ZoomIn, "zoom in"),
            (Action::ZoomOut, "zoom out"),
            (Action::ZoomReset, "zoom to 100%, or out until the page fits"),
            (Action::Inspect, "inspect an element"),
            (Action::Devtools, "Chrome DevTools (start kb with --devtools)"),
            (Action::NewTab, "open a new tab (+)"),
            (Action::RestoreTab, "reopen the last closed tab (right-click +)"),
            (Action::NextWindow, "next window: a popup, a new tab (‹ › in the bar)"),
            (Action::PreviousWindow, "previous window"),
            (Action::CloseWindow, "close the window shown (✕); on the last one, ask to quit"),
            (Action::Help, "this help"),
            (Action::Quit, "quit (the ✕ in the bar closes a popup, or asks first)"),
        ]
        .map(|(action, does)| (keys(action), does))
        .into_iter()
        .filter(|(keys, _)| !keys.is_empty())
        .collect::<Vec<_>>();
        // As wide as the widest keys, so each description starts in line.
        let width = rows.iter().map(|(keys, _)| keys.chars().count() + 2).max().unwrap_or(0);
        for (keys, does) in rows {
            lines.push(format!("{keys:<width$}{does}"));
        }
        lines.push(String::new());
        lines.push("\tChange keys in ~/.kittybrowser/keys; kb keys lists them.".to_owned());
        let mut info = vec![String::new()];
        let (cell_w, cell_h) = self.cell();
        for (name, value) in [
            ("page", if self.url.is_empty() { "(none)".to_owned() } else { self.url.clone() }),
            ("zoom", format!("{:.0}%", self.zoom * 100.0)),
            ("viewport", format!("{}x{} CSS pixels", self.viewport.0, self.viewport.1)),
            ("pane", format!("{}x{} cells of {cell_w}x{cell_h} pixels", self.window.cols, self.window.rows)),
            (
                "frames",
                match self.route {
                    Route::Files => "by file, changed blocks only",
                    Route::Layers => "through the pty, what moved or changed",
                    Route::Preview | Route::LayerFiles => "moved layers, large images by server file",
                    Route::Png => "through the pty, whole PNG frames",
                }
                .to_owned(),
            ),
            ("agents", format!("kb ctl -i {} ...   (kb ctl --help)", self.instance)),
            ("trouble", "kb doctor checks the terminal and Chrome".to_owned()),
        ] {
            info.push(format!("\t{name:<10}{value}"));
        }
        let footer = [String::new(), "\tAny key closes this.                                 by @graiz".to_owned()];

        // In a short pane the banner goes first, then the details, so the
        // keys and the footer always show.
        let room = self.page_rows().saturating_sub(2) as usize;
        let mut all = Vec::new();
        for (with_banner, with_info) in [(true, true), (false, true), (false, false)] {
            all.clear();
            if with_banner {
                all.extend(banner.iter().cloned());
            }
            all.extend(lines.iter().cloned());
            if with_info {
                all.extend(info.iter().cloned());
            }
            all.extend(footer.iter().cloned());
            if all.len() <= room {
                break;
            }
        }
        all
    }

    /// Draw the help box, if it is open, over whatever is below the bar.
    fn render_help(&mut self, out: &mut Vec<u8>) {
        if let Some(picker) = self.picker.as_mut() {
            if let Some(old) = self.help_rect.take() {
                ui::blank(out, old);
            }
            self.help_rect = Some(picker.draw(out, self.window));
            return;
        }
        if let Some(old) = self.help_rect.take() {
            ui::blank(out, old);
        }
        if self.draw_dialog(out) {
            return;
        }
        if !self.help {
            if self.downloads.open {
                self.help_rect = Some(self.downloads.draw(out, self.window));
                out.extend_from_slice(b"\x1b[?25l");
            }
            return;
        }
        let lines = self.panel.clone().unwrap_or_else(|| self.help_lines());
        self.help_rect = Some(ui::render_box(out, &lines, self.window.cols, self.window.rows));
        // The box, not the address field, has the user's attention.
        out.extend_from_slice(b"\x1b[?25l");
    }

    /// Another window is shown, at `url`. Whether it is loading comes in
    /// its own events.
    fn shown(&mut self, url: String) {
        self.wheel = Default::default();
        self.hover = Default::default();
        self.pointer_request = None;
        self.picker = None;
        self.loading = false;
        self.find = None;
        self.buttons = 0;
        self.last_press = None;
        self.frame = None;
        self.clear();
        self.went_to(url);
        self.bar.blur(&self.url);
        if self.url.is_empty() {
            self.bar.focus();
        }
    }

    /// Close the window shown, as a browser's tab closes; the last one is
    /// kb itself, so that asks first.
    fn close_window(&mut self) {
        if self.chrome.windows().1 > 1 {
            self.chrome.close_window();
        } else {
            self.asking_quit = true;
        }
    }

    fn switch_window(&mut self, step: isize) {
        if let Some(PageEvent::Shown(url)) = self.chrome.switch(step) {
            self.shown(url);
        }
    }

    /// The page shown is at `url` now.
    fn went_to(&mut self, url: String) {
        self.discard_motion();
        self.end_scroll_capture();
        log(format_args!("url {url}"));
        self.pacing.reset();
        self.copy_request = CopyRequest::default();
        self.describe_request = None;
        self.history.record(&url);
        self.url = if url == "about:blank" { String::new() } else { url };
        // An empty field waiting for an address gives way to
        // a page opened some other way (by an agent, a link).
        if self.bar.focused() && self.bar.text().is_empty() && !self.url.is_empty() {
            self.bar.blur(&self.url);
        } else if !self.bar.focused() {
            self.bar.set_text(&self.url);
        }
    }

    fn close_help(&mut self) {
        self.help = false;
        if self.panel.take().is_some() {
            self.chrome.evaluate(UNHOVER);
        }
    }

    /// Carry out `kb ctl viewport`: lay the page out at a given size, or go
    /// back to the pane's.
    fn set_viewport(&mut self, size: &str) -> Result<Output, String> {
        let parse = |text: &str| text.parse::<u32>().ok().filter(|n| (50..=10_000).contains(n));
        self.fixed = match size.split_once(['x', 'X']) {
            _ if matches!(size, "reset" | "pane") => None,
            Some((width, height)) => match (parse(width), parse(height)) {
                (Some(width), Some(height)) => Some((width, height)),
                _ => return Err(format!("{size:?} isn't a size; give WIDTHxHEIGHT in CSS pixels, such as 1280x800")),
            },
            // A width alone: as tall as fills the pane (see layout).
            None => match parse(size) {
                Some(width) => Some((width, 0)),
                None => return Err(format!("{size:?} isn't a size; give WIDTH, WIDTHxHEIGHT, or reset")),
            },
        };
        self.layout();
        let (width, height) = self.viewport;
        let (cell_w, _) = self.cell();
        let percent = (self.shown.0 as f64 * cell_w as f64 / (width as f64 * self.page_scale()) * 100.0).round();
        Ok(Output::Text(match self.fixed {
            Some(_) if percent < 99.0 => {
                format!("the viewport is {width}x{height} CSS pixels, shown at {percent}% to fit the pane")
            }
            Some(_) => format!("the viewport is {width}x{height} CSS pixels"),
            None => format!("the viewport follows the pane again: {width}x{height} CSS pixels"),
        }))
    }

    /// Whether a frame was rendered at the size the page is laid out at now,
    /// give or take rounding.
    fn native_size(&self, (width, height): (u32, u32)) -> bool {
        let close = |actual: u32, css: u32| {
            // Chrome renders at its launch scale whatever the zoom: zoomed
            // in, the frame has fewer pixels than the cells and is stretched
            // to fill them; zoomed out, more, and is shrunk.
            let expected = css as f64 * self.scale;
            (actual as f64 - expected).abs() <= (expected / 100.0).max(2.0)
        };
        close(width, self.viewport.0) && close(height, self.viewport.1)
    }

    fn end_scroll_capture(&mut self) {
        if self.scroll_capture.preview.is_some() {
            self.chrome.scroll_capture(None);
        }
        self.scroll_capture = Default::default();
    }

    fn navigate(&mut self, url: &str) {
        // Keep the committed URL until Chrome confirms the navigation. A site
        // can still cancel it after the field shows the requested destination.
        self.loading = true;
        self.bar.blur(url);
        self.chrome.navigate(url);
    }

    fn draw(&mut self, paint_page: bool, probe: Option<render::Probe>) -> render::Command {
        let frame = self.frame.as_ref().filter(|_| paint_page).map(|png| render::Frame {
            png: Arc::clone(png),
            route: if self.frame_is_preview && self.route == Route::LayerFiles { Route::Preview } else { self.route },
            scaled: self.fixed.is_some(),
            cells: self.shown,
            cell: self.cell(),
            top: 2 + u16::from(self.tab_strip),
        });
        let mut out = Vec::new();
        let cols = self.window.cols as usize;
        let mut badge = String::new();
        if self.opening_tab.is_some() {
            badge.push_str("Opening tab… ");
        } else if let Some((note, _)) = &self.notice {
            badge.push_str(note);
            badge.push(' ');
        } else if self.inspecting {
            badge.push_str("Inspect: click an element, Esc to stop ");
        }
        if self.loading {
            const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let step = (self.started.elapsed().as_millis() / TICK.as_millis()) as usize;
            badge.push(SPINNER[step % SPINNER.len()]);
        }
        if (self.zoom - 1.0).abs() > 0.001 {
            if !badge.is_empty() {
                badge.push(' ');
            }
            badge.push_str(&format!("{:.0}%", self.zoom * 100.0));
        }
        let badge = badge.trim_end();
        let badge = (!badge.is_empty()).then_some(badge);
        if self.tab_strip {
            let (current, total) = self.chrome.windows();
            ui::TabStrip::new(cols, current - 1, total).render(&mut out, current - 1, |at| self.chrome.tab_label(at));
        }
        match &mut self.find {
            _ if self.asking_quit => ui::render_question(&mut out, ui::QUIT_QUESTION, cols),
            Some(field) => {
                ui::render_bar(&mut out, field, cols, self.loading, Some("Find"), badge, self.chrome.windows())
            }
            None => ui::render_bar(&mut out, &mut self.bar, cols, self.loading, None, badge, self.chrome.windows()),
        }
        self.render_help(&mut out);
        render::Command::Draw { controls: std::mem::take(&mut self.output), frame, overlay: out, probe }
    }

    /// Open the find field in the address bar's place.
    fn open_find(&mut self) {
        let mut field = AddressBar::default();
        field.set_text(&self.find_text);
        field.focus();
        self.find = Some(field);
    }

    /// Change the find field's text and search for the result as it is
    /// typed, staying on the current match while it still matches.
    fn find_edit(&mut self, change: impl FnOnce(&mut AddressBar)) {
        let Some(field) = &mut self.find else { return };
        change(field);
        let text = field.text();
        if text != self.find_text {
            self.find_text = text;
            self.find_in_page(false, true);
        }
    }

    /// Select the next (or previous) place the page shows the find text and
    /// scroll to it. `restart` searches from the start of the current match
    /// rather than past it.
    fn find_in_page(&mut self, backwards: bool, restart: bool) {
        let text = serde_json::to_string(&self.find_text).unwrap_or_default();
        self.chrome.evaluate(&format!(
            "(() => {{ const s = getSelection(); if ({text} === '') return s.removeAllRanges(); \
             if ({restart} && s.rangeCount) s.collapseToStart(); \
             return window.find({text}, false, {backwards}, true); }})()"
        ));
    }

    fn find_key(&mut self, key: &Key) {
        let Some(field) = &mut self.find else { return };
        match (key.code, field.key(key)) {
            (KeyCode::Enter, _) => self.find_in_page(key.shift, false),
            (_, Edit::Cancel) => self.find = None,
            (_, Edit::Typed | Edit::Changed) => self.find_edit(|_| {}),
            (_, Edit::Submit) => {}
        }
    }

    /// Handle a key. Returns whether to quit.
    fn on_key(&mut self, key: Key) -> bool {
        let focused = self.bar.focused();
        if self.inspecting && key.code == KeyCode::Esc {
            self.stop_inspecting();
            return false;
        }
        let mut action = self.bindings.action(&key, focused || self.find.is_some());
        // Ctrl+W edits a nonempty address. A freshly opened blank tab has
        // nothing to delete, so the close binding should work immediately.
        if action.is_none()
            && focused
            && self.find.is_none()
            && self.bindings.action(&key, false) == Some(Action::CloseWindow)
            && self.bar.text().is_empty()
        {
            action = Some(Action::CloseWindow);
        }
        match action {
            Some(Action::Quit) => return true,
            Some(action) if self.perform(action) => return false,
            _ => {}
        }
        match key.code {
            _ if self.find.is_some() => self.find_key(&key),
            // The page hears Command+A and Command+X as select all and cut;
            // other Command keys have no meaning there.
            KeyCode::Char('a' | 'x') if key.cmd && !focused => self.chrome.key(&Key { ctrl: true, cmd: false, ..key }),
            _ if key.cmd => {}
            _ if focused => match self.bar.key(&key) {
                Edit::Changed => {}
                Edit::Typed => {
                    let completion = self.history.complete(&self.bar.typed());
                    self.bar.set_completion(completion);
                }
                Edit::Cancel => self.bar.blur(&self.url),
                Edit::Submit => match ui::normalize_url(&self.bar.text()) {
                    Some(url) => self.navigate(&url),
                    None => self.bar.blur(&self.url),
                },
            },
            _ => self.chrome.key(&key),
        }
        false
    }

    /// Carry out a bound action. Returns false if it doesn't apply now, so
    /// the key goes on as if unbound.
    fn perform(&mut self, action: Action) -> bool {
        match action {
            Action::Address => {
                self.find = None;
                self.bar.focus();
            }
            Action::Downloads => self.downloads.open = true,
            Action::Find => self.open_find(),
            Action::FindNext | Action::FindPrevious if self.find_text.is_empty() => return false,
            Action::FindNext => self.find_in_page(false, false),
            Action::FindPrevious => self.find_in_page(true, false),
            Action::Back => self.chrome.back(),
            Action::Forward => self.chrome.forward(),
            Action::Reload => self.chrome.reload(),
            // With one window, the key goes to the page.
            Action::NextWindow | Action::PreviousWindow if self.chrome.windows().1 == 1 => return false,
            Action::NextWindow => self.switch_window(1),
            Action::PreviousWindow => self.switch_window(-1),
            Action::CloseWindow => self.close_window(),
            Action::NewTab | Action::RestoreTab => match self.chrome.new_tab(action == Action::RestoreTab) {
                Ok(id) => self.opening_tab = Some((id, Instant::now())),
                Err(message) => self.note(&message),
            },
            Action::ZoomIn => self.step_zoom(true),
            Action::ZoomOut => self.step_zoom(false),
            Action::ZoomReset => self.zoom_to_fit(),
            Action::Help => self.help = true,
            Action::Copy => {
                let field = match &self.find {
                    Some(field) => Some(field.text()),
                    None => self.bar.focused().then(|| self.bar.text()),
                };
                self.copy_request = CopyRequest::default();
                match field {
                    Some(text) => self.copy(&text),
                    None => {
                        // A dedicated JS world reads the DOM with browser-provided APIs,
                        // even when a site has replaced selection methods in its own world.
                        let id = self.chrome.request(
                            "Page.createIsolatedWorld",
                            serde_json::json!({
                                "frameId": self.chrome.main_frame(), "worldName": "kb-selection"
                            }),
                        );
                        self.copy_request = CopyRequest { world: Some(id), selection: None };
                    }
                }
            }
            Action::Inspect if self.inspecting => self.stop_inspecting(),
            Action::Inspect => {
                self.close_help();
                self.inspecting = true;
            }
            Action::Devtools => {
                if let Err(error) = self.chrome.open_devtools() {
                    self.note(&format!("{error} (Ctrl+Shift+C inspects without it)"));
                } else {
                    self.note("DevTools opened in Chrome");
                }
            }
            Action::Quit | Action::None => return false,
        }
        true
    }

    /// Show a short note at the right of the bar.
    fn note(&mut self, text: &str) {
        self.notice = Some((text.chars().filter(|c| !c.is_control()).collect(), Instant::now()));
    }

    /// Put text on the system clipboard, through the terminal.
    fn copy(&mut self, text: &str) {
        log(format_args!("copy {} characters", text.chars().count()));
        if text.is_empty() {
            self.note("Nothing to copy");
            return;
        }
        self.output.clipboard = Some(text.to_owned());
        let count = text.chars().count();
        self.note(&format!("Copied {count} character{}", if count == 1 { "" } else { "s" }));
    }

    fn stop_inspecting(&mut self) {
        self.inspecting = false;
        self.chrome.evaluate(UNHOVER);
    }

    /// Handle Chrome's reply to a request the app made. Returns whether it
    /// was one.
    fn reply(&mut self, id: u64, message: &serde_json::Value) -> bool {
        if CopyRequest::consume(&mut self.copy_request.world, id) {
            if let Some(context) = message["result"]["executionContextId"].as_u64() {
                self.copy_request.selection = Some(self.chrome.request(
                    "Runtime.evaluate",
                    serde_json::json!({
                        "expression": SELECTION, "contextId": context, "returnByValue": true
                    }),
                ));
            }
            return true;
        }
        if CopyRequest::consume(&mut self.copy_request.selection, id) {
            if let Some(text) = message["result"]["result"]["value"].as_str() {
                self.copy(text);
            }
            return true;
        }
        if let Some((_, requested)) = self.pointer_request.filter(|(request, _)| *request == id) {
            self.pointer_request = None;
            if let Some(latest) = self.pointer_next.take() {
                if latest == requested {
                    let shape = message["result"]["result"]["value"].as_str().unwrap_or("default");
                    self.set_pointer(if POINTERS.contains(&shape) { shape } else { "default" });
                } else {
                    self.ask_pointer(latest.0, latest.1);
                }
            }
            return true;
        }
        if self.describe_request == Some(id) {
            self.describe_request = None;
            let value = &message["result"]["result"]["value"];
            let mut lines: Vec<String> =
                value["lines"].as_array().into_iter().flatten().filter_map(|l| l.as_str().map(str::to_owned)).collect();
            if lines.is_empty() {
                return true;
            }
            let again = self.bindings.keys_for(Action::Inspect).into_iter().next().unwrap_or_default();
            lines.push(String::new());
            lines.push(format!("\t{again} inspects another; any key closes this."));
            self.panel = Some(lines);
            self.help = true;
            return true;
        }
        false
    }

    /// Ask which pointer the page wants at a point, one question at a time.
    fn ask_pointer(&mut self, x: f64, y: f64) {
        self.pointer_next = Some((x, y));
        if self.pointer_request.is_none() {
            let id = self.chrome.evaluate_value(&format!("{POINTER}({x}, {y})"));
            self.pointer_request = Some((id, (x, y)));
        }
    }

    fn set_pointer(&mut self, shape: &str) {
        if self.pointer != shape {
            log(format_args!("pointer {shape}"));
            self.pointer = shape.to_owned();
            self.output.pointer = Some(shape.to_owned());
        }
    }

    /// Match the bar to the terminal, and pages' colour scheme to its
    /// background, unless one was chosen.
    fn follow_terminal_colors(&mut self) {
        let (Some(fg), Some(bg)) = (self.term_fg, self.term_bg) else { return };
        ui::use_terminal_colors(fg, bg);
        if self.dark.is_none() {
            self.chrome.color_scheme(Some(ui::is_dark(bg)));
        }
    }

    /// Carry out `kb ctl scheme`: dark, light, or auto (follow the terminal).
    fn set_scheme(&mut self, scheme: &str) -> Result<Output, String> {
        self.dark = match scheme {
            "dark" => Some(true),
            "light" => Some(false),
            "auto" => None,
            other => return Err(format!("scheme takes dark, light or auto, not {other:?}")),
        };
        let effective = self.dark.or(self.term_bg.map(ui::is_dark));
        self.chrome.color_scheme(effective);
        let said = match (self.dark, effective) {
            (Some(true), _) => "pages are told the user prefers dark".to_owned(),
            (Some(false), _) => "pages are told the user prefers light".to_owned(),
            (None, Some(dark)) => {
                format!("following the terminal: {}", if dark { "dark" } else { "light" })
            }
            (None, None) => "following the terminal, which hasn't said its colours: pages get their default".to_owned(),
        };
        Ok(Output::Text(said))
    }

    fn discard_motion(&mut self) {
        self.wheel.clear();
        self.hover.clear();
        self.pointer_next = None;
    }

    fn send_wheel(&mut self, flush: bool) {
        if let Some(crate::scroll::Motion { kind: MouseKind::Wheel(dx, dy), x, y, modifiers }) =
            self.wheel.take(Instant::now(), flush)
        {
            log(format_args!("Wheel({dx}, {dy}) none at page {x:.0},{y:.0}"));
            let id =
                self.chrome.wheel(x, y, f64::from(dx) * self.wheel_step, f64::from(dy) * self.wheel_step, modifiers);
            self.wheel.sent(id);
        }
    }

    fn send_hover(&mut self) {
        if let Some(crate::scroll::Motion { x, y, modifiers, .. }) = self.hover.take(Instant::now(), false) {
            let id = if self.inspecting {
                self.chrome.evaluate_value(&format!("{HOVER}({x}, {y})"))
            } else {
                let id = self.chrome.mouse("mouseMoved", x, y, "none", 0, 0, modifiers);
                self.ask_pointer(x, y);
                id
            };
            self.hover.sent(id);
        }
    }

    fn mouse_pixels(&self, mouse: Mouse) -> (f64, f64) {
        let (cell_w, cell_h) = self.cell();
        let (cell_w, cell_h) = (cell_w as f64, cell_h as f64);
        // Position in device pixels from the pane's top left. Cell reports
        // are taken at the middle of the cell.
        if self.pixel_mouse {
            (mouse.x as f64, mouse.y as f64)
        } else {
            ((mouse.x as f64 - 0.5) * cell_w, (mouse.y as f64 - 0.5) * cell_h)
        }
    }

    fn toolbar_column(&self, mouse: Mouse) -> Option<usize> {
        let (px, py) = self.mouse_pixels(mouse);
        let (width, height) = self.cell();
        (py < f64::from(height)).then(|| (px / f64::from(width)) as usize + 1)
    }

    fn consume_tab_retry(&mut self, mouse: Mouse, at: Instant) -> bool {
        let Some(press) = &self.tab_button else { return false };
        let repeated = mouse.button == Some(press.button)
            && matches!(mouse.kind, MouseKind::Press | MouseKind::Release)
            && press.drawn_at.is_none_or(|drawn| at <= drawn)
            && self
                .toolbar_column(mouse)
                .is_some_and(|col| ui::hit(col, self.window.cols as usize, press.windows) == Hit::NewTab);
        // A repeat still belongs to the same open request after a slow
        // attach. Start the lost-reply timeout from the last such press.
        if repeated && mouse.kind == MouseKind::Press {
            if let Some((_, last)) = self.opening_tab.as_mut().filter(|(id, _)| *id == press.request) {
                *last = (*last).max(at);
            }
        }
        repeated
    }

    /// Returns whether the top bar needs redrawing.
    fn on_mouse(&mut self, mouse: Mouse, at: Instant) -> bool {
        // A click closes the help, and does nothing else.
        if self.help {
            if mouse.kind == MouseKind::Press {
                self.close_help();
                return true;
            }
            return false;
        }
        let (cell_w, cell_h) = self.cell();
        let (cell_w, cell_h) = (cell_w as f64, cell_h as f64);
        let (px, py) = self.mouse_pixels(mouse);
        if self.asking_quit || (self.buttons == 0 && py < cell_h * if self.tab_strip { 2.0 } else { 1.0 }) {
            self.discard_motion();
        }
        // The mouse's back and forward buttons, as in any browser.
        if mouse.kind == MouseKind::Press {
            match mouse.button {
                Some(Button::Back) => return self.perform(Action::Back),
                Some(Button::Forward) => return self.perform(Action::Forward),
                _ => {}
            }
        }
        if matches!(mouse.button, Some(Button::Back | Button::Forward)) {
            return false;
        }
        // While the bar asks whether to quit, only its buttons answer.
        if self.asking_quit {
            let answer = (py < cell_h).then(|| ui::answer((px / cell_w) as usize + 1, ui::QUIT_QUESTION)).flatten();
            match mouse.kind {
                MouseKind::Move => self.set_pointer(if answer.is_some() { "pointer" } else { "default" }),
                MouseKind::Press if mouse.button == Some(Button::Left) => match answer {
                    Some(true) => self.quit_clicked = true,
                    Some(false) => self.asking_quit = false,
                    None => {}
                },
                _ => {}
            }
            return true;
        }
        // A drag that began on the page stays with the page.
        if py < cell_h && self.buttons == 0 {
            let col = (px / cell_w) as usize + 1;
            if mouse.kind == MouseKind::Move {
                let shape = match ui::hit(col, self.window.cols as usize, self.chrome.windows()) {
                    Hit::Address(_) => "text",
                    Hit::Downloads
                    | Hit::Back
                    | Hit::Forward
                    | Hit::Reload
                    | Hit::NewTab
                    | Hit::Close
                    | Hit::PreviousWindow
                    | Hit::NextWindow => "pointer",
                    Hit::Nothing => "default",
                };
                self.set_pointer(shape);
            }
            if ui::hit(col, self.window.cols as usize, self.chrome.windows()) == Hit::NewTab
                && mouse.kind == MouseKind::Press
            {
                if let Some(button @ (Button::Left | Button::Right)) = mouse.button {
                    let windows = self.chrome.windows();
                    let changed =
                        self.perform(if button == Button::Left { Action::NewTab } else { Action::RestoreTab });
                    if let Some((request, _)) = self.opening_tab {
                        self.tab_button = Some(TabButtonPress { request, windows, button, drawn_at: None });
                    }
                    return changed;
                }
            }
            if mouse.kind != MouseKind::Press || mouse.button != Some(Button::Left) {
                return false;
            }
            if ui::hit(col, self.window.cols as usize, self.chrome.windows()) == Hit::Close {
                self.close_window();
                return true;
            }
            if let Some(field) = &mut self.find {
                if let Hit::Address(offset) = ui::hit(col, self.window.cols as usize, self.chrome.windows()) {
                    field.click(offset);
                }
                return true;
            }
            match ui::hit(col, self.window.cols as usize, self.chrome.windows()) {
                Hit::Downloads => self.downloads.open = true,
                Hit::Back => self.chrome.back(),
                Hit::Forward => self.chrome.forward(),
                Hit::Reload if self.loading => self.chrome.stop(),
                Hit::Reload => self.chrome.reload(),
                Hit::Address(offset) if self.bar.focused() => self.bar.click(offset),
                Hit::Address(_) => self.bar.focus(),
                Hit::PreviousWindow => self.switch_window(-1),
                Hit::NextWindow => self.switch_window(1),
                Hit::NewTab | Hit::Close | Hit::Nothing => {}
            }
            return true;
        }

        if self.tab_strip && py < 2.0 * cell_h && self.buttons == 0 {
            let (current, total) = self.chrome.windows();
            let strip = ui::TabStrip::new(self.window.cols as usize, current - 1, total);
            let at = strip.at((px / cell_w) as usize);
            if mouse.kind == MouseKind::Move {
                self.set_pointer(if at.is_some() { "pointer" } else { "default" });
            }
            if mouse.kind == MouseKind::Press && mouse.button == Some(Button::Left) {
                if let Some(PageEvent::Shown(url)) = at.and_then(|at| self.chrome.select(at)) {
                    self.shown(url);
                }
            }
            return true;
        }

        // The image covers `shown` cells and holds the whole viewport.
        let x = px / (self.shown.0 as f64 * cell_w) * self.viewport.0 as f64;
        let y = (py - if self.tab_strip { 2.0 * cell_h } else { cell_h }) / (self.shown.1 as f64 * cell_h)
            * self.viewport.1 as f64;
        let mut modifiers = 0;
        if mouse.alt {
            modifiers |= chrome::ALT;
        }
        if mouse.ctrl {
            modifiers |= chrome::CTRL;
        }
        if mouse.shift {
            modifiers |= chrome::SHIFT;
        }
        let (name, bit) = match mouse.button {
            Some(Button::Left) => ("left", 1),
            Some(Button::Right) => ("right", 2),
            Some(Button::Middle) => ("middle", 4),
            Some(Button::Back | Button::Forward) | None => ("none", 0),
        };
        if !matches!(mouse.kind, MouseKind::Move | MouseKind::Wheel(..)) {
            log(format_args!("{:?} {name} at page {x:.0},{y:.0}", mouse.kind));
        }
        // Inspect mode: the mouse points elements out, and a click picks one
        // rather than reaching the page. The wheel still scrolls.
        let motion = crate::scroll::Motion { kind: mouse.kind, x, y, modifiers };
        if self.inspecting && !matches!(mouse.kind, MouseKind::Wheel(..)) {
            match mouse.kind {
                MouseKind::Move => {
                    self.set_pointer("crosshair");
                    self.hover.offer(motion, at);
                    self.send_hover();
                }
                MouseKind::Press if mouse.button == Some(Button::Left) => {
                    self.chrome.evaluate(&format!("{HOVER}({x}, {y})"));
                    self.inspecting = false;
                    self.describe_request = Some(self.chrome.evaluate_value(DESCRIBE));
                }
                _ => {}
            }
            return true;
        }
        match mouse.kind {
            MouseKind::Press => {
                let now = Instant::now();
                let clicks = match self.last_press {
                    Some((at, px, py, clicks))
                        if now - at < DOUBLE_CLICK && (px - x).abs() < 4.0 && (py - y).abs() < 4.0 =>
                    {
                        clicks + 1
                    }
                    _ => 1,
                };
                self.last_press = Some((now, x, y, clicks));
                self.buttons |= bit;
                self.chrome.mouse("mousePressed", x, y, name, self.buttons, clicks, modifiers);
                if self.bar.focused() {
                    self.bar.blur(&self.url);
                    return true;
                }
            }
            MouseKind::Release => {
                self.buttons &= !bit;
                let clicks = self.last_press.map_or(1, |press| press.3);
                self.chrome.mouse("mouseReleased", x, y, name, self.buttons, clicks, modifiers);
            }
            MouseKind::Move => {
                if self.buttons == 0 && mouse.button.is_none() {
                    self.hover.offer(motion, at);
                    self.send_hover();
                } else {
                    self.chrome.mouse("mouseMoved", x, y, name, self.buttons, 0, modifiers);
                }
            }
            MouseKind::Wheel(..) => {
                self.wheel.offer(motion, at);
                self.send_wheel(false);
                let native = (
                    (self.viewport.0 as f64 * self.scale).round() as u32,
                    (self.viewport.1 as f64 * self.scale).round() as u32,
                );
                if self.preview_enabled
                    && self.focused
                    && self.route == Route::LayerFiles
                    && self.scroll_capture.wheel(Instant::now(), native)
                {
                    self.chrome.scroll_capture(self.scroll_capture.preview);
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod clipboard_tests {
    #[test]
    fn only_the_requested_reply_is_consumed_once() {
        let mut pending = Some(17);
        assert!(!super::CopyRequest::consume(&mut pending, 16));
        assert!(super::CopyRequest::consume(&mut pending, 17));
        assert!(!super::CopyRequest::consume(&mut pending, 17));
    }
}
