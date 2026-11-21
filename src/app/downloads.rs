//! Chrome writes under opaque GUIDs. Publish a completed file under its
//! suggested name only after an atomic no-clobber link succeeds.
use super::*;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub(super) struct Downloads {
    dir: PathBuf,
    setup: Option<u64>,
    error: Option<String>,
    entries: Vec<Entry>,
    selected: usize,
    pub(super) open: bool,
    rect: Option<ui::Rect>,
}

struct Entry {
    guid: String,
    name: String,
    received: u64,
    total: u64,
    state: State,
    cancel: Option<u64>,
}

enum State {
    Active,
    Saved(PathBuf),
    Stopped(String),
}

fn publish(source: &Path, dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    let clean: String =
        name.rsplit(['/', '\\']).next().unwrap_or("").chars().filter(|c| !c.is_control()).take(100).collect();
    let end = clean.char_indices().map(|(i, _)| i).find(|i| *i > 200).unwrap_or(clean.len());
    let name = clean[..end].trim().trim_matches('.');
    let name = if name.is_empty() { "download" } else { name };
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = path.extension().map_or(String::new(), |s| format!(".{}", s.to_string_lossy()));
    for n in 0u64.. {
        let target = dir.join(if n == 0 { name.to_owned() } else { format!("{stem} ({n}){ext}") });
        match std::fs::hard_link(source, &target) {
            Ok(()) => {
                let _ = std::fs::remove_file(source);
                return Ok(target);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

impl Downloads {
    pub(super) fn start(&mut self, chrome: &mut Chrome) {
        let result = (|| -> Result<u64, String> {
            self.dir = crate::data_dir()?.canonicalize().map_err(|e| e.to_string())?.join("downloads");
            let incoming = self.dir.join(".incoming");
            for dir in [&self.dir, &incoming] {
                crate::private_directory(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            chrome
                .browser_request(
                    "Browser.setDownloadBehavior",
                    json!({"behavior":"allowAndName","downloadPath":incoming,"eventsEnabled":true}),
                )
                .map_err(|e| e.to_string())
        })();
        match result {
            Ok(id) => self.setup = Some(id),
            Err(e) => self.error = Some(e),
        }
    }

    pub(super) fn observe(&mut self, packet: &Value) -> bool {
        if let Some(id) = packet["id"].as_u64() {
            if self.setup == Some(id) {
                self.setup = None;
                self.error = packet["error"]["message"].as_str().map(str::to_owned);
                return true;
            }
            if let Some(entry) = self.entries.iter_mut().find(|e| e.cancel == Some(id)) {
                if let Some(error) = packet["error"]["message"].as_str() {
                    entry.cancel = None;
                    self.error = Some(error.to_owned());
                }
                return true;
            }
            return false;
        }
        let params = &packet["params"];
        let Some(guid) = params["guid"].as_str() else { return false };
        if guid.is_empty() || !guid.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-') {
            return false;
        }
        match packet["method"].as_str() {
            Some("Browser.downloadWillBegin") => {
                self.entries.push(Entry {
                    guid: guid.into(),
                    name: params["suggestedFilename"].as_str().unwrap_or("download").into(),
                    received: 0,
                    total: 0,
                    state: State::Active,
                    cancel: None,
                });
                if !self.open || self.entries.len() == 1 {
                    self.selected = self.entries.len() - 1;
                }
                true
            }
            Some("Browser.downloadProgress") => {
                let Some(e) = self.entries.iter_mut().find(|e| e.guid == guid && matches!(e.state, State::Active))
                else {
                    return false;
                };
                e.received = params["receivedBytes"].as_f64().unwrap_or(0.0) as u64;
                e.total = params["totalBytes"].as_f64().unwrap_or(0.0) as u64;
                match params["state"].as_str() {
                    Some("completed") => {
                        let source = self.dir.join(".incoming").join(guid);
                        e.state = match publish(&source, &self.dir, &e.name) {
                            Ok(path) => State::Saved(path),
                            Err(err) => State::Stopped(format!("Save failed: {err} ({})", source.display())),
                        };
                    }
                    Some("canceled") => {
                        e.state = State::Stopped(
                            if e.cancel.is_some() { "Cancelled" } else { "Interrupted by Chrome" }.into(),
                        )
                    }
                    _ => {}
                }
                true
            }
            _ => false,
        }
    }

    fn range(&self, rows: u16) -> std::ops::Range<usize> {
        let count = usize::from(rows.saturating_sub(9)).max(1).min(self.entries.len());
        let start = (self.selected + 1).saturating_sub(count);
        start..start + count
    }

    pub(super) fn draw(&mut self, out: &mut Vec<u8>, window: Window) -> ui::Rect {
        let mut lines = vec!["Server downloads".to_owned(), "[ Copy path ]  [ Cancel ]  [ Close ]".into()];
        let range = self.range(window.rows);
        for n in range {
            let e = &self.entries[n];
            let state = match &e.state {
                State::Saved(_) => "Saved".into(),
                State::Stopped(why) => why.clone(),
                State::Active if e.cancel.is_some() => "Cancelling".into(),
                State::Active if e.total > 0 => {
                    format!("{}% ({} / {} bytes)", e.received.saturating_mul(100) / e.total, e.received, e.total)
                }
                State::Active => format!("{} bytes", e.received),
            };
            lines.push(format!("{} {}  {state}", if n == self.selected { "●" } else { " " }, e.name));
        }
        if self.entries.is_empty() {
            lines.push("No downloads this session".into());
        }
        if let Some(Entry { state: State::Saved(path), .. }) = self.entries.get(self.selected) {
            lines.push(path.display().to_string());
        }
        if let Some(error) = &self.error {
            lines.push(format!("Download error: {error}"));
        }
        let rect = ui::render_box(out, &lines, window.cols, window.rows);
        self.rect = Some(rect);
        rect
    }
}

impl App {
    fn download_action(&mut self, key: KeyCode) {
        let panel = &mut self.downloads;
        match key {
            KeyCode::Esc => panel.open = false,
            KeyCode::Up => panel.selected = panel.selected.saturating_sub(1),
            KeyCode::Down => panel.selected = (panel.selected + 1).min(panel.entries.len().saturating_sub(1)),
            KeyCode::Enter | KeyCode::Char('c') => {
                if let Some(Entry { state: State::Saved(path), .. }) = panel.entries.get(panel.selected) {
                    let text = path.display().to_string();
                    self.copy(&text);
                }
            }
            KeyCode::Char('x') => {
                if let Some(e) = panel
                    .entries
                    .get_mut(panel.selected)
                    .filter(|e| matches!(e.state, State::Active) && e.cancel.is_none())
                {
                    match self.chrome.browser_request("Browser.cancelDownload", json!({"guid":e.guid})) {
                        Ok(id) => e.cancel = Some(id),
                        Err(err) => panel.error = Some(err.to_string()),
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn download_input(&mut self, event: &Event) -> bool {
        if !self.downloads.open {
            return false;
        }
        match event {
            Event::Key(key) => {
                if self.bindings.action(key, false) == Some(Action::Quit) {
                    return false;
                }
                if self.bindings.action(key, false) == Some(Action::Downloads) {
                    self.downloads.open = false;
                } else {
                    self.download_action(key.code);
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseKind::Wheel(_, n) => self.download_action(if n < 0 { KeyCode::Up } else { KeyCode::Down }),
                MouseKind::Press if mouse.button == Some(Button::Left) => {
                    let Some(r) = self.downloads.rect else {
                        return true;
                    };
                    let cell = self.cell();
                    let (x, y) = if self.pixel_mouse {
                        (mouse.x / cell.0.max(1) + 1, mouse.y / cell.1.max(1) + 1)
                    } else {
                        (mouse.x, mouse.y)
                    };
                    let dx = x.checked_sub(u32::from(r.col) + 2);
                    let dy = y.checked_sub(u32::from(r.row) + 1);
                    if let (Some(dx), Some(dy)) = (dx, dy) {
                        if dx < u32::from(r.width.saturating_sub(4)) {
                            if dy == 1 {
                                match dx {
                                    0..=12 => self.download_action(KeyCode::Enter),
                                    15..=24 => self.download_action(KeyCode::Char('x')),
                                    27..=35 => self.downloads.open = false,
                                    _ => {}
                                }
                            }
                            if dy >= 2 {
                                if let Some(n) = self.downloads.range(self.window.rows).nth((dy - 2) as usize) {
                                    self.downloads.selected = n;
                                }
                            }
                        }
                    }
                }
                _ => {}
            },
            Event::Paste(_) => {}
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publishing_never_replaces_a_file_or_uses_a_suggested_directory() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        std::fs::write(dir.join("report.csv"), "first").unwrap();
        let source = dir.join("guid");
        std::fs::write(&source, "second").unwrap();
        assert_eq!(publish(&source, dir, "../report.csv").unwrap(), dir.join("report (1).csv"));
        assert_eq!(std::fs::read(dir.join("report.csv")).unwrap(), b"first");
    }
}
