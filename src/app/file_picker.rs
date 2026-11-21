//! Browse the browser host privately. Only the review screen can send paths
//! to Chrome, bound to the original tab, frame and file-input node.
use super::*;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthChar;

struct Item {
    name: std::ffi::OsString,
    directory: bool,
}

pub(super) struct Picker {
    session: String,
    frame: String,
    node: u64,
    multiple: bool,
    dir: PathBuf,
    items: Vec<Item>,
    selected: Vec<PathBuf>,
    field: AddressBar,
    cursor: usize,
    review: bool,
    scroll: usize,
    error: String,
    rect: Option<ui::Rect>,
    sending: Option<u64>,
}

// Escape control characters so one filename cannot display as two choices.
fn display(path: &Path) -> String {
    let mut text = String::new();
    for ch in path.to_string_lossy().chars() {
        if ch.is_control() || ch == '\\' || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            text.extend(ch.escape_default());
        } else {
            text.push(ch);
        }
    }
    text
}

impl Picker {
    fn new(session: &str, params: &Value) -> Option<Self> {
        let mut picker = Self {
            session: session.into(),
            frame: params["frameId"].as_str()?.into(),
            node: params["backendNodeId"].as_u64()?,
            multiple: params["mode"] == "selectMultiple",
            dir: std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/")),
            items: vec![],
            selected: vec![],
            field: Default::default(),
            cursor: 0,
            review: false,
            scroll: 0,
            error: String::new(),
            rect: None,
            sending: None,
        };
        picker.field.focus();
        let dir = picker.dir.clone();
        if let Err(e) = picker.enter(&dir) {
            picker.error = e.to_string();
        }
        Some(picker)
    }

    fn enter(&mut self, path: &Path) -> std::io::Result<()> {
        let dir = path.canonicalize()?;
        let mut items = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() || kind.is_file() || kind.is_symlink() {
                items.push(Item { name: entry.file_name(), directory: kind.is_dir() });
            }
        }
        items.sort_by(|a, b| b.directory.cmp(&a.directory).then(a.name.cmp(&b.name)));
        if dir.parent().is_some() {
            items.insert(0, Item { name: "..".into(), directory: true });
        }
        self.dir = dir;
        self.items = items;
        self.cursor = 0;
        self.field.set_text("");
        Ok(())
    }

    fn choose(&mut self, path: &Path) -> Result<(), String> {
        let path = path.canonicalize().map_err(|e| e.to_string())?;
        if path.is_dir() {
            return self.enter(&path).map_err(|e| e.to_string());
        }
        if !path.is_file() {
            return Err("Choose a regular file".into());
        }
        if path.to_str().is_none() {
            return Err("Chrome needs a UTF-8 file path".into());
        }
        std::fs::File::open(&path).map_err(|e| e.to_string())?;
        if !self.multiple {
            self.selected.clear();
        }
        if let Some(at) = self.selected.iter().position(|p| p == &path) {
            self.selected.remove(at);
        } else {
            self.selected.push(path);
        }
        self.field.set_text("");
        if !self.multiple {
            self.review = true;
            self.scroll = 0;
        }
        Ok(())
    }

    fn pick(&mut self, path: PathBuf) {
        self.error = self.choose(&path).err().unwrap_or_default();
    }

    fn start_review(&mut self) {
        if self.selected.is_empty() {
            self.error = "Select at least one file".into();
            return;
        }
        self.review = true;
        self.scroll = 0;
        self.error.clear();
    }

    fn visible(&self, rows: u16) -> std::ops::Range<usize> {
        let count = usize::from(rows.saturating_sub(9)).max(1).min(self.items.len());
        let start = (self.cursor + 1).saturating_sub(count);
        start..start + count
    }

    fn path_lines(&self, cols: u16) -> Vec<String> {
        let width = usize::from(cols.saturating_sub(4)).max(2);
        let mut lines = Vec::new();
        for path in &self.selected {
            let mut line = String::new();
            let mut cells = 0;
            for ch in display(path).chars() {
                let n = ch.width().unwrap_or(0);
                if cells + n > width {
                    lines.push(std::mem::take(&mut line));
                    cells = 0;
                }
                cells += n;
                line.push(ch);
            }
            lines.push(line);
        }
        lines
    }

    pub(super) fn draw(&mut self, out: &mut Vec<u8>, window: Window) -> ui::Rect {
        let mut lines = vec![if self.review {
            format!("Attach {} server file{}?", self.selected.len(), if self.selected.len() == 1 { "" } else { "s" })
        } else {
            format!("Server files — {} selected", self.selected.len())
        }];
        if self.review {
            lines.push("[ Attach ]  [ Back ]  [ Cancel ]".into());
        } else {
            lines.push("[ Review ]  [ Cancel ]".into());
        }
        if !self.error.is_empty() {
            lines.push(self.error.clone());
        }
        if self.review {
            let paths = self.path_lines(window.cols);
            let capacity = usize::from(window.rows.saturating_sub(7)).max(1);
            self.scroll = self.scroll.min(paths.len().saturating_sub(capacity));
            lines.extend(paths.into_iter().skip(self.scroll).take(capacity));
        } else {
            lines.push(display(&self.dir));
            for i in self.visible(window.rows) {
                let item = &self.items[i];
                let selected = self.selected.contains(&self.dir.join(&item.name));
                lines.push(format!(
                    "{}{} {}{}",
                    if i == self.cursor { "›" } else { " " },
                    if selected { "✓" } else { " " },
                    display(Path::new(&item.name)),
                    if item.directory { "/" } else { "" }
                ));
            }
            lines.push("Enter: open/select   F2: review   Esc: cancel".into());
        }
        let rect = ui::render_box(out, &lines, window.cols, window.rows);
        self.rect = Some(rect);
        if !self.review {
            ui::render_bar(out, &mut self.field, usize::from(window.cols), false, Some("Path"), None, (1, 1));
        }
        rect
    }
}

impl App {
    pub(super) fn picker_event(&mut self, packet: &Value) -> bool {
        if let Some(p) = self.picker.as_mut() {
            if p.sending.is_some() && packet["id"].as_u64() == p.sending {
                if let Some(reason) = packet["error"]["message"].as_str() {
                    p.error = reason.to_owned();
                    p.sending = None;
                } else {
                    self.picker = None;
                    self.note("Files attached");
                }
                return true;
            }
            let frame = &packet["params"];
            if packet["sessionId"] == p.session
                && ((packet["method"] == "Page.frameNavigated" && frame["frame"]["id"] == p.frame)
                    || (packet["method"] == "Page.frameDetached" && frame["frameId"] == p.frame))
            {
                self.picker = None;
            }
        }
        if packet["method"] != "Page.fileChooserOpened" {
            return false;
        }
        let session = self.chrome.session();
        if packet["sessionId"] == session {
            self.picker = Picker::new(session, &packet["params"]);
            if self.picker.is_none() {
                self.note("This picker requires a file input");
            }
        }
        true
    }

    pub(super) fn picker_input(&mut self, event: &Event) -> bool {
        let cell = self.cell();
        let Some(p) = self.picker.as_mut() else {
            return false;
        };
        if p.session != self.chrome.session() {
            self.picker = None;
            return false;
        }
        let key = match event {
            Event::Key(k) if self.bindings.action(k, false) == Some(Action::Quit) => return false,
            Event::Key(k) => *k,
            Event::Paste(s) => {
                if !p.review {
                    p.field.insert(s);
                }
                return true;
            }
            Event::Mouse(m) => {
                let (x, y) =
                    if self.pixel_mouse { (m.x / cell.0.max(1) + 1, m.y / cell.1.max(1) + 1) } else { (m.x, m.y) };
                let code = match m.kind {
                    MouseKind::Wheel(_, n) => {
                        if n < 0 {
                            KeyCode::Up
                        } else {
                            KeyCode::Down
                        }
                    }
                    MouseKind::Press if m.button == Some(Button::Left) => {
                        let Some(r) = p.rect else {
                            return true;
                        };
                        let Some(dx) =
                            x.checked_sub(u32::from(r.col) + 2).filter(|n| *n < u32::from(r.width.saturating_sub(4)))
                        else {
                            return true;
                        };
                        let Some(row) = y.checked_sub(u32::from(r.row) + 1) else {
                            return true;
                        };
                        if row == 1 {
                            match (p.review, dx) {
                                (true, 0..=9) => KeyCode::Enter,
                                (true, 12..=19) => KeyCode::Left,
                                (true, 22..=31) => KeyCode::Esc,
                                (false, 0..=9) => KeyCode::F(2),
                                (false, 12..=21) => KeyCode::Esc,
                                _ => return true,
                            }
                        } else if !p.review {
                            let start = 3 + u32::from(!p.error.is_empty());
                            if let Some(i) =
                                row.checked_sub(start).and_then(|n| p.visible(self.window.rows).nth(n as usize))
                            {
                                p.cursor = i;
                                let path = p.dir.join(&p.items[i].name);
                                p.pick(path);
                            }
                            return true;
                        } else {
                            return true;
                        }
                    }
                    _ => return true,
                };
                Key { code, ..Key::default() }
            }
            _ => return false,
        };
        if p.sending.is_some() {
            return true;
        }
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Left if p.review => p.review = false,
            KeyCode::Up if p.review => p.scroll = p.scroll.saturating_sub(1),
            KeyCode::Down if p.review => p.scroll = p.scroll.saturating_add(1),
            KeyCode::Enter if p.review => {
                if p.selected.iter().any(|path| !path.is_file()) {
                    p.error = "A selected file is no longer available".into();
                } else {
                    p.sending = Some(
                        self.chrome
                            .request("DOM.setFileInputFiles", json!({"backendNodeId":p.node,"files":p.selected})),
                    );
                }
            }
            _ if p.review => {}
            KeyCode::Up => p.cursor = p.cursor.saturating_sub(1),
            KeyCode::Down => p.cursor = (p.cursor + 1).min(p.items.len().saturating_sub(1)),
            KeyCode::F(2) => p.start_review(),
            KeyCode::Enter => {
                let typed = p.field.text();
                if !typed.is_empty() {
                    p.pick(p.dir.join(typed));
                } else if let Some(item) = p.items.get(p.cursor) {
                    p.pick(p.dir.join(&item.name));
                }
            }
            _ => {
                p.field.key(&key);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selecting_and_reviewing_do_not_submit_and_multi_can_deselect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a");
        std::fs::write(&path, b"x").unwrap();
        let mut p = Picker::new("tab", &json!({"frameId":"frame","backendNodeId":17,"mode":"selectMultiple"})).unwrap();
        p.choose(dir.path()).unwrap();
        p.choose(&path).unwrap();
        assert!(!p.review);
        p.start_review();
        assert!(p.review);
        assert!(p.sending.is_none());
        assert_eq!(p.path_lines(200), [display(&path.canonicalize().unwrap())]);
        p.review = false;
        p.choose(&path).unwrap();
        assert!(p.selected.is_empty());
    }
}
