//! A site's dialog belongs to the CDP session that opened it. A background
//! tab can wait while the pane continues to service input and other tabs.
use super::*;
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct Dialogs {
    pending: HashMap<String, Dialog>,
    rect: Option<ui::Rect>,
}

struct Dialog {
    kind: String,
    origin: String,
    message: String,
    field: AddressBar,
    answering: Option<u64>,
}

fn origin(url: &str) -> String {
    let url = url.trim_start_matches("blob:");
    if let Some((scheme, rest)) = url.split_once("://") {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        return format!("{scheme}://{}", authority.rsplit('@').next().unwrap_or(""));
    }
    url.split_once(':').map_or_else(|| "unknown origin".into(), |(scheme, _)| format!("{scheme}:"))
}

impl App {
    pub(super) fn dialog_event(&mut self, packet: &Value) -> bool {
        if let Some(id) = packet["id"].as_u64() {
            if let Some(dialog) = self.dialogs.pending.values_mut().find(|d| d.answering == Some(id)) {
                if packet.get("error").is_some() {
                    dialog.answering = None;
                    self.note("Chrome could not answer the dialog");
                }
                return true;
            }
        }
        let Some(session) = packet["sessionId"].as_str() else {
            if packet["method"] == "Target.detachedFromTarget" {
                if let Some(session) = packet["params"]["sessionId"].as_str() {
                    self.dialogs.pending.remove(session);
                }
            }
            return false;
        };
        let params = &packet["params"];
        match packet["method"].as_str() {
            Some("Page.javascriptDialogOpening") if !self.devtools.automatic_dialogs => {
                let mut field = AddressBar::default();
                field.set_text(params["defaultPrompt"].as_str().unwrap_or(""));
                field.focus();
                self.dialogs.pending.insert(
                    session.to_owned(),
                    Dialog {
                        kind: params["type"].as_str().unwrap_or("alert").into(),
                        origin: origin(params["url"].as_str().unwrap_or("")),
                        message: params["message"].as_str().unwrap_or("").into(),
                        field,
                        answering: None,
                    },
                );
                self.dialogs.rect = None;
                true
            }
            Some("Page.javascriptDialogClosed") => {
                if let Some(dialog) = self.dialogs.pending.remove(session) {
                    self.devtools.record_answer(&dialog.kind, params["result"] == true);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    pub(super) fn dialog_input(&mut self, event: &Event) -> bool {
        let cell = self.cell();
        let Some(dialog) = self.dialogs.pending.get_mut(self.chrome.session()) else {
            return false;
        };
        let answer = match event {
            Event::Key(key) => {
                if matches!(
                    self.bindings.action(key, false),
                    Some(Action::NextWindow | Action::PreviousWindow | Action::Quit)
                ) {
                    return false;
                }
                match key.code {
                    KeyCode::Enter => Some(true),
                    KeyCode::Esc => Some(dialog.kind == "alert"),
                    _ if dialog.kind == "prompt" => {
                        dialog.field.key(key);
                        None
                    }
                    _ => None,
                }
            }
            Event::Paste(text) => {
                if dialog.kind == "prompt" {
                    dialog.field.insert(text);
                }
                None
            }
            Event::Mouse(mouse) => {
                let (x, y) = if self.pixel_mouse {
                    (mouse.x / cell.0.max(1) + 1, mouse.y / cell.1.max(1) + 1)
                } else {
                    (mouse.x, mouse.y)
                };
                if self.tab_strip && y == 2 {
                    return false;
                }
                self.dialogs.rect.and_then(|r| {
                    if mouse.kind != MouseKind::Press || mouse.button != Some(Button::Left) || y != u32::from(r.row) + 1
                    {
                        return None;
                    }
                    let offset = x.checked_sub(u32::from(r.col) + 2)?;
                    match offset {
                        0..=5 => Some(true),
                        8..=17 if dialog.kind != "alert" => Some(false),
                        _ => None,
                    }
                })
            }
            _ => return false,
        };
        if let Some(accept) = answer.filter(|_| dialog.answering.is_none()) {
            dialog.answering = Some(
                self.chrome
                    .request("Page.handleJavaScriptDialog", json!({"accept":accept,"promptText":dialog.field.text()})),
            );
        }
        true
    }

    pub(super) fn draw_dialog(&mut self, out: &mut Vec<u8>) -> bool {
        let Some(dialog) = self.dialogs.pending.get(self.chrome.session()) else {
            return false;
        };
        let buttons = if dialog.kind == "alert" { "[ OK ]" } else { "[ OK ]  [ Cancel ]" };
        let mut lines = vec![buttons.into(), format!("{} — {}", dialog.kind, dialog.origin)];
        // Reserve the first rows for the controls, regardless of message length.
        let width = usize::from(self.window.cols.saturating_sub(4)).max(1);
        let rows = usize::from(self.window.rows.saturating_sub(5)).max(1);
        for line in dialog.message.lines() {
            let text: Vec<_> = line.chars().filter(|c| !c.is_control()).take(width * rows).collect();
            for part in text.chunks(width) {
                lines.push(part.iter().collect());
                if lines.len() >= rows + 2 {
                    break;
                }
            }
            if lines.len() >= rows + 2 {
                break;
            }
        }
        let rect = ui::render_box(out, &lines, self.window.cols, self.window.rows);
        self.help_rect = Some(rect);
        self.dialogs.rect = Some(rect);
        let cols = usize::from(self.window.cols);
        if let Some(dialog) = self.dialogs.pending.get_mut(self.chrome.session()).filter(|d| d.kind == "prompt") {
            ui::render_bar(out, &mut dialog.field, cols, false, Some("Prompt"), None, (1, 1));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn origin_omits_credentials_and_paths() {
        assert_eq!(super::origin("https://user@host.example:8443/a?b"), "https://host.example:8443");
        assert_eq!(super::origin("blob:https://host.example/id"), "https://host.example");
        assert_eq!(super::origin("data:text/html,private"), "data:");
    }
}
