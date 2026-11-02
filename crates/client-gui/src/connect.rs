// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Connect dialog: server address, ticket and shared key. Keyboard-driven
//! (Tab/Shift-Tab to change field, Enter to connect, Esc to quit) — the
//! reference client keeps the dialog minimal on purpose.

use wgpu_text::glyph_brush::{OwnedSection, OwnedText};
use winit::event::KeyEvent;
use winit::keyboard::{Key, NamedKey};

/// Result of handling one key press in the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogAction {
    /// Keep editing.
    Nothing,
    /// The user confirmed a complete form.
    Connect(ConnectForm),
    /// The user asked to quit.
    Quit,
}

/// What the dialog produces on Connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectForm {
    /// `host:port` as typed.
    pub addr: String,
    /// Human ticket (hashed client-side to the wire form).
    pub ticket: String,
    /// Shared key as hex (32 bytes).
    pub key_hex: String,
}

struct Field {
    label: &'static str,
    value: String,
    secret: bool,
}

/// Dialog state.
pub struct ConnectDialog {
    fields: [Field; 3],
    focus: usize,
    status: String,
}

impl Default for ConnectDialog {
    fn default() -> Self {
        Self {
            fields: [
                Field {
                    label: "Server",
                    value: String::new(),
                    secret: false,
                },
                Field {
                    label: "Ticket",
                    value: String::new(),
                    secret: false,
                },
                Field {
                    label: "Key (hex)",
                    value: String::new(),
                    secret: true,
                },
            ],
            focus: 0,
            status: "Tab: next field · Enter: connect · Esc: quit".to_string(),
        }
    }
}

impl ConnectDialog {
    /// Seeds the fields from CLI parameters (editable before connecting).
    #[must_use]
    pub fn prefilled(form: &ConnectForm) -> Self {
        let mut dialog = Self::default();
        dialog.fields[0].value.clone_from(&form.addr);
        dialog.fields[1].value.clone_from(&form.ticket);
        dialog.fields[2].value.clone_from(&form.key_hex);
        dialog
    }

    /// Sets the status line (validation errors, connection progress).
    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }

    /// Handles one key press.
    #[must_use]
    pub fn handle_key(&mut self, event: &KeyEvent) -> DialogAction {
        let action = self.handle_named(&event.logical_key);
        if matches!(action, DialogAction::Nothing)
            && let Some(text) = &event.text
        {
            for ch in text.chars() {
                if !ch.is_control() {
                    self.fields[self.focus].value.push(ch);
                }
            }
        }
        action
    }

    /// Key handling, minus text input (split out for testability: winit
    /// `KeyEvent` cannot be constructed portably in unit tests).
    fn handle_named(&mut self, key: &Key) -> DialogAction {
        match key {
            Key::Named(NamedKey::Tab) => {
                self.focus = (self.focus + 1) % self.fields.len();
                DialogAction::Nothing
            }
            Key::Named(NamedKey::Backspace) => {
                self.fields[self.focus].value.pop();
                DialogAction::Nothing
            }
            Key::Named(NamedKey::Enter) => {
                if self.fields.iter().any(|f| f.value.is_empty()) {
                    self.status = "all fields are required".to_string();
                    DialogAction::Nothing
                } else {
                    DialogAction::Connect(ConnectForm {
                        addr: self.fields[0].value.clone(),
                        ticket: self.fields[1].value.clone(),
                        key_hex: self.fields[2].value.clone(),
                    })
                }
            }
            Key::Named(NamedKey::Escape) => DialogAction::Quit,
            _ => DialogAction::Nothing,
        }
    }

    /// Text sections for the current frame.
    #[must_use]
    pub fn sections(&self, width: f32) -> Vec<OwnedSection> {
        const WHITE: [f32; 4] = [0.92, 0.93, 0.95, 1.0];
        const GREY: [f32; 4] = [0.55, 0.58, 0.62, 1.0];
        const ACCENT: [f32; 4] = [0.35, 0.75, 1.0, 1.0];
        fn push(
            out: &mut Vec<OwnedSection>,
            y: &mut f32,
            width: f32,
            text: String,
            scale: f32,
            color: [f32; 4],
        ) {
            out.push(
                OwnedSection::default()
                    .with_screen_position((40.0, *y))
                    .with_bounds((width - 80.0, scale * 1.6))
                    .add_text(OwnedText::new(text).with_scale(scale).with_color(color)),
            );
            *y += scale * 1.9;
        }
        let mut out = Vec::new();
        let mut y = 40.0f32;
        push(
            &mut out,
            &mut y,
            width,
            "UDS Remote Protocol — Connect".to_string(),
            30.0,
            ACCENT,
        );
        y += 10.0;
        for (i, field) in self.fields.iter().enumerate() {
            let shown = if field.secret {
                "•".repeat(field.value.len())
            } else {
                field.value.clone()
            };
            let cursor = if i == self.focus { "▏" } else { "" };
            let color = if i == self.focus { WHITE } else { GREY };
            push(
                &mut out,
                &mut y,
                width,
                format!("{}: {shown}{cursor}", field.label),
                22.0,
                color,
            );
        }
        y += 10.0;
        push(&mut out, &mut y, width, self.status.clone(), 16.0, GREY);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_requires_all_fields() {
        let mut dialog = ConnectDialog::default();
        let action = dialog.handle_named(&Key::Named(NamedKey::Enter));
        assert_eq!(action, DialogAction::Nothing);
        assert!(dialog.status.contains("required"));
    }

    #[test]
    fn tab_cycles_focus_and_enter_emits() {
        let form = ConnectForm {
            addr: "host:1".to_string(),
            ticket: "t".to_string(),
            key_hex: "k".to_string(),
        };
        let mut dialog = ConnectDialog::prefilled(&form);
        assert_eq!(
            dialog.handle_named(&Key::Named(NamedKey::Tab)),
            DialogAction::Nothing
        );
        let DialogAction::Connect(done) = dialog.handle_named(&Key::Named(NamedKey::Enter))
        else {
            panic!("expected a completed form");
        };
        assert_eq!(done.addr, "host:1");
    }

    #[test]
    fn escape_quits() {
        let mut dialog = ConnectDialog::default();
        assert_eq!(
            dialog.handle_named(&Key::Named(NamedKey::Escape)),
            DialogAction::Quit
        );
    }
}
