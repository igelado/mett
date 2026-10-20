//! A one-line text editor for prompts such as `To:` and `Subject:`, with
//! mutt's editor keys (its `editor` menu).

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// What a key press did to the line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Still editing.
    Editing,
    /// The user accepted the line (Enter).
    Done(String),
    /// The user gave up (`^G` or Esc).
    Cancelled,
}

/// The text being edited and the cursor position.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineEditor {
    /// The text, as grapheme clusters, so the cursor never splits one.
    graphemes: Vec<String>,
    /// The cursor, as an index into `graphemes`.
    cursor: usize,
}

impl LineEditor {
    /// An editor holding `text`, with the cursor at the end.
    pub fn new(text: &str) -> Self {
        let graphemes: Vec<String> = text.graphemes(true).map(str::to_owned).collect();
        let cursor = graphemes.len();
        Self { graphemes, cursor }
    }

    /// The text.
    pub fn text(&self) -> String {
        self.graphemes.concat()
    }

    /// The text before the cursor, and after it.
    pub fn split_at_cursor(&self) -> (String, String) {
        (
            self.graphemes[..self.cursor].concat(),
            self.graphemes[self.cursor..].concat(),
        )
    }

    /// An editor holding `before` then `after`, with the cursor between
    /// them.
    pub fn with_cursor(before: &str, after: &str) -> Self {
        let mut editor = Self::new(before);
        editor
            .graphemes
            .extend(after.graphemes(true).map(str::to_owned));
        editor
    }

    /// Applies a key press.
    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => return Outcome::Done(self.text()),
            KeyCode::Esc => return Outcome::Cancelled,
            KeyCode::Char('g') if ctrl => return Outcome::Cancelled,
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char('b') if ctrl => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.graphemes.len()),
            KeyCode::Char('f') if ctrl => {
                self.cursor = (self.cursor + 1).min(self.graphemes.len());
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::End => self.cursor = self.graphemes.len(),
            KeyCode::Char('e') if ctrl => self.cursor = self.graphemes.len(),
            KeyCode::Backspace => self.delete_before(),
            KeyCode::Char('h') if ctrl => self.delete_before(),
            KeyCode::Delete => self.delete_at(),
            KeyCode::Char('d') if ctrl => self.delete_at(),
            // Kill to the start or the end of the line.
            KeyCode::Char('u') if ctrl => {
                self.graphemes.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('k') if ctrl => self.graphemes.truncate(self.cursor),
            // Kill the word before the cursor.
            KeyCode::Char('w') if ctrl => {
                let mut start = self.cursor;
                while start > 0 && self.graphemes[start - 1].trim().is_empty() {
                    start -= 1;
                }
                while start > 0 && !self.graphemes[start - 1].trim().is_empty() {
                    start -= 1;
                }
                self.graphemes.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.insert(c);
            }
            _ => {}
        }
        Outcome::Editing
    }

    fn delete_before(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.graphemes.remove(self.cursor);
        }
    }

    fn delete_at(&mut self) {
        if self.cursor < self.graphemes.len() {
            self.graphemes.remove(self.cursor);
        }
    }

    /// Inserts `text` at the cursor, its line ends as spaces.
    pub fn insert_text(&mut self, text: &str) {
        let mut last = None;
        for c in text.chars() {
            match c {
                // `\r\n` is one line end.
                '\n' if last == Some('\r') => {}
                '\r' | '\n' => self.insert(' '),
                c => self.insert(c),
            }
            last = Some(c);
        }
    }

    fn insert(&mut self, c: char) {
        // A combining character joins the grapheme before the cursor.
        if self.cursor > 0 {
            let joined = format!("{}{c}", self.graphemes[self.cursor - 1]);
            if joined.graphemes(true).count() == 1 {
                self.graphemes[self.cursor - 1] = joined;
                return;
            }
        }
        self.graphemes.insert(self.cursor, c.to_string());
        self.cursor += 1;
    }

    /// The part of the text to show in `width` cells, and the cursor's
    /// column within it. The view scrolls so that the cursor stays visible.
    pub fn view(&self, width: usize) -> (String, usize) {
        let width = width.max(1);
        // Start as far right as possible while keeping the cursor visible,
        // leaving a cell for it at the end of the line.
        let mut before_cursor: usize = self.graphemes[..self.cursor]
            .iter()
            .map(|g| g.width())
            .sum();
        let mut start = 0;
        while before_cursor + 1 > width && start < self.cursor {
            before_cursor -= self.graphemes[start].width();
            start += 1;
        }
        let mut shown = String::new();
        let mut used = 0;
        for g in &self.graphemes[start..] {
            if used + g.width() > width {
                break;
            }
            used += g.width();
            shown.push_str(g);
        }
        (shown, before_cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(editor: &mut LineEditor, code: KeyCode) -> Outcome {
        editor.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(editor: &mut LineEditor, c: char) -> Outcome {
        editor.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn typed(editor: &mut LineEditor, text: &str) {
        for c in text.chars() {
            press(editor, KeyCode::Char(c));
        }
    }

    #[test]
    fn edits_like_mutts_line_editor() {
        let mut editor = LineEditor::new("bob@x.org");
        ctrl(&mut editor, 'a');
        typed(&mut editor, "Bob <");
        ctrl(&mut editor, 'e');
        typed(&mut editor, ">");
        assert_eq!(editor.text(), "Bob <bob@x.org>");
        press(&mut editor, KeyCode::Left);
        press(&mut editor, KeyCode::Backspace);
        press(&mut editor, KeyCode::Delete);
        assert_eq!(editor.text(), "Bob <bob@x.or");
        ctrl(&mut editor, 'w');
        assert_eq!(editor.text(), "Bob ");
        ctrl(&mut editor, 'u');
        assert_eq!(editor.text(), "");
        typed(&mut editor, "abc");
        press(&mut editor, KeyCode::Home);
        ctrl(&mut editor, 'k');
        assert_eq!(
            press(&mut editor, KeyCode::Enter),
            Outcome::Done(String::new())
        );
        assert_eq!(press(&mut editor, KeyCode::Esc), Outcome::Cancelled);
        assert_eq!(ctrl(&mut editor, 'g'), Outcome::Cancelled);
    }

    #[test]
    fn keeps_graphemes_whole() {
        let mut editor = LineEditor::new("");
        typed(&mut editor, "e\u{301}");
        // The combining accent joins the e: one Backspace removes both.
        assert_eq!(editor.text(), "e\u{301}");
        press(&mut editor, KeyCode::Backspace);
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn scrolls_to_keep_the_cursor_visible() {
        let editor = LineEditor::new("abcdefghij");
        assert_eq!(editor.view(5), ("ghij".to_owned(), 4));
        let mut editor = editor;
        press(&mut editor, KeyCode::Home);
        assert_eq!(editor.view(5), ("abcde".to_owned(), 0));
        let wide = LineEditor::new("日本語");
        assert_eq!(wide.view(5), ("本語".to_owned(), 4));
    }
}
