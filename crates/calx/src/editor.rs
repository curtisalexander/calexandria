//! A multi-line text area, on edtui: the panel view's prompt (plain editing
//! keys) and its notes (vim keys). Ctrl-J starts a new line in both, and
//! pasted text goes in as typed, newlines included.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use edtui::actions::{InsertChar, LineBreak};
use edtui::{
    EditorEventHandler, EditorMode, EditorState, EditorStatusLine, EditorTheme, EditorView, Index2,
    Lines,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

/// Which keys the prompt edits with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Keys {
    #[default]
    Plain,
    Vim,
}

pub struct Editor {
    state: EditorState,
    keys: EditorEventHandler,
    vim: bool,
    /// Show vim's mode on the last row. The prompt shows it in its margin instead.
    status_line: bool,
}

impl Editor {
    /// Plain editing keys: arrows, Home and End, Ctrl-A and Ctrl-E, Ctrl-W
    /// and Ctrl-K, and Alt-F and Alt-B by word.
    pub fn plain() -> Self {
        Self::new(Keys::Plain, "", false)
    }

    /// Vim keys, starting in insert mode so a quick note needs no `i`.
    pub fn vim(text: &str) -> Self {
        Self::new(Keys::Vim, text, true)
    }

    /// The prompt, with the keys the user chose.
    pub fn prompt(keys: Keys) -> Self {
        Self::new(keys, "", false)
    }

    fn new(keys: Keys, text: &str, status_line: bool) -> Self {
        let vim = keys == Keys::Vim;
        let mut editor = Self {
            state: EditorState::new(Lines::from(text)),
            keys: if vim {
                EditorEventHandler::vim_mode()
            } else {
                EditorEventHandler::emacs_mode()
            },
            vim,
            status_line,
        };
        editor.state.mode = EditorMode::Insert;
        editor.cursor_to_end();
        editor
    }

    pub fn text(&self) -> String {
        self.state.lines.to_string()
    }

    pub fn is_empty(&self) -> bool {
        self.text().is_empty()
    }

    pub fn set(&mut self, text: &str) {
        self.state.lines = Lines::from(text);
        self.cursor_to_end();
    }

    /// Empties the editor, back in insert mode for the next message.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.set("");
        self.state.mode = EditorMode::Insert;
        text
    }

    /// In vim's normal mode, where Esc has nothing left to do.
    pub fn is_normal_mode(&self) -> bool {
        self.vim && self.state.mode == EditorMode::Normal
    }

    pub fn key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('j') && key.modifiers == KeyModifiers::CONTROL {
            self.newline();
        } else {
            self.keys.on_key_event(key, &mut self.state);
        }
    }

    pub fn newline(&mut self) {
        self.state.execute(LineBreak(1));
    }

    /// Inserts text at the cursor, as if typed.
    pub fn paste(&mut self, text: &str) {
        for c in text.replace("\r\n", "\n").replace('\r', "\n").chars() {
            match c {
                '\n' => self.state.execute(LineBreak(1)),
                c => self.state.execute(InsertChar(c)),
            }
        }
    }

    /// Rows the text takes at `width` when wrapped.
    pub fn rows(&self, width: u16) -> u16 {
        let width = width.max(1) as usize;
        let rows: usize = self
            .text()
            .split('\n')
            .map(|line| line.chars().count().div_ceil(width).max(1))
            .sum();
        rows.min(u16::MAX as usize) as u16
    }

    fn cursor_to_end(&mut self) {
        let row = self.state.lines.len().saturating_sub(1);
        let col = self.state.lines.len_col(row).unwrap_or(0);
        self.state.cursor = Index2::new(row, col);
    }

    /// Draws the text in the terminal's own colors; vim mode shows its mode
    /// on the last row.
    pub fn render(&mut self, f: &mut Frame, area: Rect, focused: bool) {
        let mut theme = EditorTheme::default()
            .base(Style::default())
            .cursor_style(Style::default().add_modifier(Modifier::REVERSED))
            .selection_style(Style::default().bg(Color::DarkGray));
        theme = if self.status_line {
            theme.status_line(
                EditorStatusLine::default()
                    .style_mode(Style::default().fg(Color::Black).bg(Color::Yellow))
                    .style_line(Style::default().fg(Color::DarkGray)),
            )
        } else {
            theme.hide_status_line()
        };
        if !focused {
            theme = theme.hide_cursor();
        }
        f.render_widget(
            EditorView::new(&mut self.state).theme(theme).wrap(true),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(editor: &mut Editor, keys: &str) {
        for c in keys.chars() {
            let code = if c == '\x1b' {
                KeyCode::Esc
            } else {
                KeyCode::Char(c)
            };
            editor.key(KeyEvent::new(code, KeyModifiers::NONE));
        }
    }

    #[test]
    fn a_vim_prompt_edits_in_normal_mode_and_takes_back_to_insert() {
        let mut editor = Editor::prompt(Keys::Vim);
        press(&mut editor, "fix the test\x1bbdw");
        assert!(editor.is_normal_mode());
        assert_eq!(editor.text(), "fix the ");
        assert_eq!(editor.take(), "fix the ");
        assert!(!editor.is_normal_mode());
        // In insert mode, letters are text again.
        press(&mut editor, "dw");
        assert_eq!(editor.text(), "dw");
    }

    #[test]
    fn pasted_lines_and_ctrl_j_keep_their_newlines() {
        let mut editor = Editor::plain();
        editor.paste("one\r\ntwo\n");
        editor.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        editor.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        editor.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(editor.text(), "one\ntwo\nx\ny");
    }
}
