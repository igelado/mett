//! The built-in editor: messages are written in mett's own screen, with
//! vim's, emacs's or Notepad's keys ([`Keys`]). The editing itself is
//! `edtui`'s, an editor widget for ratatui; mett adds what leaves the
//! editor (vim's `:wq` and `:q!`, emacs's `C-x C-c`, Notepad's `^S` and
//! Esc), emacs's region, and selecting with Shift in Notepad's keys.
//!
//! As with an external editor, the text it hands back is the text as last
//! saved: quitting without saving gives back what was saved before (at
//! first, the text it started with), which the compose flow takes as an
//! unmodified message.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use edtui::actions::motion::{MoveToFirstRow, MoveToLastRow};
use edtui::actions::search::StartSearch;
use edtui::actions::{
    Action, Chainable, DeleteChar, DeleteCharForward, DeleteWordBackward, FindNext, FindPrevious,
    InsertChar, LineBreak, MoveBackward, MoveDown, MoveForward, MovePageDown, MovePageUp,
    MoveToEndOfLine, MoveToStartOfLine, MoveUp, MoveWordBackward, MoveWordForward, Redo,
    RemoveCharFromSearch, SelectCurrentSearch, StopSearch, SwitchMode, Undo,
};
use edtui::clipboard::ClipboardTrait;
use edtui::events::{KeyEventHandler, KeyEventRegister, KeyInput};
use edtui::{EditorEventHandler, EditorMode, EditorState, EditorTheme, EditorView, Index2, Lines};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::widgets::Widget;

use super::Keys;
use crate::lineedit::{self, LineEditor};

/// What a key press did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Still editing.
    Editing,
    /// The user left the editor: the text as last saved.
    Done(String),
}

/// A yes-or-no question on the bottom line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Question {
    /// Leaving with unsaved changes: yes saves them, no drops them.
    SaveChanges,
    /// Dropping unsaved changes: yes leaves, no goes on editing.
    DiscardChanges,
}

impl Question {
    fn prompt(self) -> &'static str {
        match self {
            Self::SaveChanges => "Save changes? (y or n) ",
            Self::DiscardChanges => "Discard changes? (y or n) ",
        }
    }
}

/// What the bottom line shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bottom {
    /// A prompt and what was typed after it (a command, a question, a
    /// search), with the cursor's column in `text`.
    Prompt {
        /// The prompt.
        prompt: String,
        /// The part of the typed text that fits.
        text: String,
        /// The cursor's column in `text`.
        cursor: usize,
    },
    /// A message, until the next key.
    Note(String),
    /// What the keys are, or the mode (vim's `-- INSERT --`).
    Hint(String),
}

/// The editor's clipboard, which mett copies to and pastes from too: its
/// own, and in a window (the GUI) the system's. (In a terminal, text from
/// elsewhere is pasted through the terminal.)
#[derive(Clone, Default)]
struct Clipboard {
    own: Rc<RefCell<String>>,
    #[cfg(feature = "gui")]
    system: Option<Rc<RefCell<arboard::Clipboard>>>,
}

impl Clipboard {
    fn new() -> Self {
        Self {
            own: Rc::default(),
            #[cfg(feature = "gui")]
            system: crate::platform::gui()
                .then(|| arboard::Clipboard::new().ok())
                .flatten()
                .map(|system| Rc::new(RefCell::new(system))),
        }
    }

    fn get(&self) -> String {
        #[cfg(feature = "gui")]
        if let Some(system) = &self.system
            && let Ok(text) = system.borrow_mut().get_text()
        {
            return text;
        }
        self.own.borrow().clone()
    }

    fn set(&self, text: String) {
        #[cfg(feature = "gui")]
        if let Some(system) = &self.system {
            let _ = system.borrow_mut().set_text(text.clone());
        }
        *self.own.borrow_mut() = text;
    }
}

impl ClipboardTrait for Clipboard {
    fn set_text(&mut self, text: String) {
        self.set(text);
    }

    fn get_text(&mut self) -> String {
        self.get()
    }
}

/// The editor's state.
pub struct Builtin {
    state: EditorState,
    events: EditorEventHandler,
    keys: Keys,
    clipboard: Clipboard,
    /// The text as last saved: what leaving without saving gives back.
    saved: String,
    /// The text given, and as the editor holds it (without `\r` before
    /// line ends): left unchanged, the text given goes back.
    given: (String, String),
    /// Escape was typed in emacs's keys: the next key has Meta.
    meta: bool,
    /// A command being typed on the bottom line (vim's `:`, emacs's
    /// `M-x`).
    command: Option<LineEditor>,
    /// A question on the bottom line.
    question: Option<Question>,
    /// The first key of a two-key command (vim's `Z`, emacs's `C-x`).
    prefix: Option<KeyEvent>,
    /// The key vim's normal mode had last: after `f`, `t` or `r`, a `:` or
    /// `Z` is what it waits for, not a command.
    last: Option<KeyEvent>,
    /// Where selecting started: emacs's mark, Notepad's Shift.
    anchor: Option<Index2>,
    /// A message for the bottom line, until the next key.
    note: Option<String>,
}

impl std::fmt::Debug for Builtin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builtin")
            .field("keys", &self.keys)
            .field("text", &self.text())
            .field("mode", &self.state.mode)
            .finish_non_exhaustive()
    }
}

impl Builtin {
    /// An editor on `text`, with `keys`.
    pub fn new(text: &str, keys: Keys) -> Self {
        let mut state = EditorState::new(Lines::from(text));
        let clipboard = Clipboard::new();
        state.set_clipboard(clipboard.clone());
        let events = match keys {
            Keys::Vim => EditorEventHandler::vim_mode(),
            Keys::Emacs => {
                let mut events = EditorEventHandler::emacs_mode();
                // `C-/` undoes, as in emacs. Terminals send it as `C-_`,
                // which crossterm may also read as `C-7`.
                for key in ['/', '_', '7'] {
                    events
                        .key_handler
                        .insert(KeyEventRegister::i(vec![KeyInput::ctrl(key)]), Undo);
                }
                events
            }
            Keys::Notepad => EditorEventHandler::new(notepad_bindings()),
        };
        // Only vim has modes; the others are always typing.
        if keys != Keys::Vim {
            state.mode = EditorMode::Insert;
        }
        let note = match keys {
            Keys::Vim => ":wq sends the text on, :q! drops the changes",
            Keys::Emacs => "C-x C-c sends the text on, C-x k drops the changes",
            Keys::Notepad => "^S sends the text on, Esc drops the changes",
        };
        let held = String::from(state.lines.clone());
        Self {
            state,
            events,
            keys,
            clipboard,
            saved: held.clone(),
            given: (text.to_owned(), held),
            meta: false,
            command: None,
            question: None,
            prefix: None,
            last: None,
            anchor: None,
            note: Some(note.to_owned()),
        }
    }

    /// The text being edited.
    pub fn text(&self) -> String {
        String::from(self.state.lines.clone())
    }

    /// Whether the text differs from the text as last saved.
    fn modified(&self) -> bool {
        self.text() != self.saved
    }

    /// Saves the text: leaving gives it back.
    fn save(&mut self) {
        self.saved = self.text();
        let lines = self.state.lines.len();
        self.note = Some(format!(
            "{lines} line{} saved",
            if lines == 1 { "" } else { "s" }
        ));
    }

    /// Leaves, saving the text first.
    fn save_and_leave(&mut self) -> Outcome {
        let text = self.text();
        self.done(text)
    }

    /// Leaves without saving.
    fn leave(&mut self) -> Outcome {
        let text = std::mem::take(&mut self.saved);
        self.done(text)
    }

    /// Hands `text` back: the text given, if it is what the editor started
    /// with.
    fn done(&mut self, text: String) -> Outcome {
        let (given, held) = std::mem::take(&mut self.given);
        Outcome::Done(if text == held { given } else { text })
    }

    /// Applies a key press.
    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        self.note = None;
        if let Some(question) = self.question {
            return self.answer(question, key);
        }
        if let Some(command) = &mut self.command {
            return match command.key(key) {
                lineedit::Outcome::Editing => Outcome::Editing,
                lineedit::Outcome::Cancelled => {
                    self.command = None;
                    Outcome::Editing
                }
                lineedit::Outcome::Done(line) => {
                    self.command = None;
                    self.run(line.trim())
                }
            };
        }
        match self.keys {
            Keys::Vim => self.vim(key),
            Keys::Emacs => self.emacs(key),
            Keys::Notepad => self.notepad(key),
        }
    }

    fn answer(&mut self, question: Question, key: KeyEvent) -> Outcome {
        let yes = matches!(key.code, KeyCode::Char('y' | 'Y'));
        let no = matches!(key.code, KeyCode::Char('n' | 'N'));
        if !yes && !no {
            if is_ctrl(key, 'g') || key.code == KeyCode::Esc {
                self.question = None;
                self.note = Some("Quit".into());
            }
            return Outcome::Editing;
        }
        self.question = None;
        match (question, yes) {
            (Question::SaveChanges, true) => self.save_and_leave(),
            (Question::SaveChanges, false) | (Question::DiscardChanges, true) => self.leave(),
            (Question::DiscardChanges, false) => Outcome::Editing,
        }
    }

    /// Runs a command typed on the bottom line.
    fn run(&mut self, command: &str) -> Outcome {
        match (self.keys, command) {
            (_, "") => Outcome::Editing,
            (Keys::Vim, "w") | (Keys::Emacs, "save-buffer") => {
                self.save();
                Outcome::Editing
            }
            (Keys::Vim, "wq" | "wq!" | "x" | "x!" | "xit" | "exit") => self.save_and_leave(),
            (Keys::Vim, "q" | "quit") if self.modified() => {
                self.note = Some("E37: No write since last change (add ! to override)".into());
                Outcome::Editing
            }
            (Keys::Vim, "q" | "quit" | "q!" | "quit!") => self.leave(),
            (Keys::Emacs, "save-buffers-kill-terminal" | "save-buffers-kill-emacs") => {
                self.leave_asking(Question::SaveChanges)
            }
            (Keys::Emacs, "kill-buffer") => self.leave_asking(Question::DiscardChanges),
            (Keys::Vim, _) => {
                self.note = Some(format!("E492: Not an editor command: {command}"));
                Outcome::Editing
            }
            _ => {
                self.note = Some(format!("{command}: no such command"));
                Outcome::Editing
            }
        }
    }

    /// Leaves, if nothing changed since the text was saved; else asks
    /// `question` first.
    fn leave_asking(&mut self, question: Question) -> Outcome {
        if self.modified() {
            self.question = Some(question);
            Outcome::Editing
        } else {
            self.leave()
        }
    }

    fn vim(&mut self, key: KeyEvent) -> Outcome {
        if self.state.mode != EditorMode::Normal {
            self.last = None;
            self.edit(key);
            return Outcome::Editing;
        }
        // After `f`, `t` or `r` the key is a character to find or put.
        let argument = self
            .last
            .is_some_and(|last| matches!(last.code, KeyCode::Char('f' | 't' | 'r')));
        self.last = Some(key);
        if !argument {
            if let Some(prefix) = self.prefix.take() {
                match (prefix.code, key.code) {
                    (_, KeyCode::Char('Z')) => return self.save_and_leave(),
                    (_, KeyCode::Char('Q')) => return self.leave(),
                    // `Z` and another key: neither does anything.
                    _ => return Outcome::Editing,
                }
            }
            match key.code {
                KeyCode::Char(':') if plain(key) => {
                    self.command = Some(LineEditor::new(""));
                    return Outcome::Editing;
                }
                KeyCode::Char('Z') => {
                    self.prefix = Some(key);
                    return Outcome::Editing;
                }
                _ => {}
            }
        }
        self.edit(key);
        Outcome::Editing
    }

    fn emacs(&mut self, key: KeyEvent) -> Outcome {
        if let Some(_prefix) = self.prefix.take() {
            if is_ctrl(key, 'c') {
                return self.leave_asking(Question::SaveChanges);
            }
            if is_ctrl(key, 's') {
                self.save();
            } else if key.code == KeyCode::Char('k') && plain(key) {
                return self.leave_asking(Question::DiscardChanges);
            } else if !is_ctrl(key, 'g') {
                self.note = Some(format!("C-x {} is undefined", describe(key)));
            }
            return Outcome::Editing;
        }
        if self.state.mode == EditorMode::Search {
            self.edit(key);
            return Outcome::Editing;
        }
        // Escape and a key is Meta and that key, for terminals that send
        // them apart (and as emacs reads them anyway).
        let key = if std::mem::take(&mut self.meta) {
            KeyEvent::new(key.code, key.modifiers | KeyModifiers::ALT)
        } else if key.code == KeyCode::Esc && key.modifiers.is_empty() {
            self.meta = true;
            return Outcome::Editing;
        } else {
            key
        };
        if is_ctrl(key, 'x') {
            self.prefix = Some(key);
        } else if key.code == KeyCode::Char('x') && key.modifiers == KeyModifiers::ALT {
            self.command = Some(LineEditor::new(""));
        } else if is_ctrl(key, ' ') || is_ctrl(key, '@') {
            self.anchor = Some(self.state.cursor);
            self.state.selection = None;
            self.note = Some("Mark set".into());
        } else if is_ctrl(key, 'g') {
            self.deselect();
            self.note = Some("Quit".into());
        } else if is_ctrl(key, 'y') {
            self.deselect();
            self.paste_clipboard();
        } else if is_ctrl(key, 'w') {
            if let Some(region) = self.region() {
                self.copy(region);
                self.delete(region);
            }
        } else if key.code == KeyCode::Char('w') && key.modifiers == KeyModifiers::ALT {
            if let Some(region) = self.region() {
                self.copy(region);
            }
            self.deselect();
        } else if typing(key) {
            // Typing ends the region, as in emacs.
            self.deselect();
            self.edit(key);
        } else {
            self.edit(key);
            if self.anchor.is_some() {
                self.select_to_cursor();
            }
        }
        Outcome::Editing
    }

    fn notepad(&mut self, key: KeyEvent) -> Outcome {
        if self.state.mode == EditorMode::Search {
            // F3 finds the next match, Shift+F3 the one before (edtui
            // knows no F keys).
            match (key.code, key.modifiers) {
                (KeyCode::F(3), KeyModifiers::NONE) => self.state.execute(FindNext),
                (KeyCode::F(3), KeyModifiers::SHIFT) => self.state.execute(FindPrevious),
                _ => self.edit(key),
            }
            return Outcome::Editing;
        }
        if is_ctrl(key, 's') {
            return self.save_and_leave();
        }
        if key.code == KeyCode::Esc && key.modifiers.is_empty() {
            return self.leave_asking(Question::DiscardChanges);
        }
        if key.code == KeyCode::F(3) {
            self.find_again(!key.modifiers.contains(KeyModifiers::SHIFT));
            return Outcome::Editing;
        }
        let selected = self.region();
        if moves(key) && key.modifiers.contains(KeyModifiers::SHIFT) {
            let anchor = self.anchor.unwrap_or(self.state.cursor);
            self.edit(KeyEvent::new(
                key.code,
                key.modifiers.difference(KeyModifiers::SHIFT),
            ));
            self.anchor = Some(anchor);
            self.select_to_cursor();
        } else if is_ctrl(key, 'a') {
            self.state.cursor = end_of(&self.state.lines);
            self.anchor = Some(Index2::new(0, 0));
            self.select_to_cursor();
        } else if is_ctrl(key, 'c') {
            if let Some(region) = selected {
                self.copy(region);
            }
        } else if is_ctrl(key, 'x') {
            if let Some(region) = selected {
                self.copy(region);
                self.delete(region);
            }
        } else if let Some(region) = selected
            && matches!(key.code, KeyCode::Backspace | KeyCode::Delete)
        {
            self.delete(region);
        } else if let Some(region) = selected
            && let Some(text) = self.replacement(key)
        {
            // Replaced, as one change to undo; with nothing to paste, the
            // selection stays.
            if !text.is_empty() {
                self.delete(region);
                self.insert(&text);
            }
        } else if is_ctrl(key, 'v') {
            self.deselect();
            self.paste_clipboard();
        } else if let Some((start, end)) = selected
            && key.modifiers.is_empty()
            && matches!(key.code, KeyCode::Left | KeyCode::Right)
        {
            // Left and Right leave the selection at its start or end.
            self.deselect();
            self.state.cursor = if key.code == KeyCode::Left {
                start
            } else {
                end
            };
        } else {
            self.deselect();
            self.edit(key);
        }
        Outcome::Editing
    }

    /// Finds the last search's text again (Notepad's F3), after the
    /// cursor or, `backwards`, before it, and selects it. The text is
    /// searched afresh: it may have changed since.
    fn find_again(&mut self, forward: bool) {
        let pattern: Vec<char> = self.state.search_pattern().chars().collect();
        if pattern.is_empty() {
            self.note = Some("No search yet: ^F searches".into());
            return;
        }
        let rows: Vec<Vec<char>> = self
            .text()
            .split('\n')
            .map(|row| row.chars().collect())
            .collect();
        let matches: Vec<Index2> = rows
            .iter()
            .enumerate()
            .flat_map(|(row, chars)| {
                chars
                    .windows(pattern.len())
                    .enumerate()
                    .filter(|(_, window)| *window == pattern.as_slice())
                    .map(move |(col, _)| Index2::new(row, col))
            })
            .collect();
        // From the cursor, or before what the last search found, and past
        // the last match, from the other end.
        let cursor = self.state.cursor;
        let from = self.region().map_or(cursor, |(start, _)| start);
        let found = if forward {
            matches
                .iter()
                .find(|m| **m >= cursor && **m != from)
                .or(matches.first())
        } else {
            matches.iter().rev().find(|m| **m < from).or(matches.last())
        };
        let Some(&start) = found else {
            self.note = Some(format!("Can't find \"{}\"", self.state.search_pattern()));
            return;
        };
        self.anchor = Some(start);
        self.state.cursor = Index2::new(start.row, start.col + pattern.len());
        self.select_to_cursor();
    }

    /// Hands a key to edtui. Keys it doesn't know (F1, Insert, ...),
    /// which it would panic on, do nothing.
    fn edit(&mut self, key: KeyEvent) {
        if matches!(
            key.code,
            KeyCode::Char(_)
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Enter
                | KeyCode::Esc
                | KeyCode::Backspace
                | KeyCode::Delete
                | KeyCode::Tab
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
        ) {
            self.events.on_key_event(key, &mut self.state);
        }
    }

    /// Selects from the anchor to the cursor (the character under the
    /// cursor is left out, as the cursor is between characters in
    /// emacs's and Notepad's selections).
    fn select_to_cursor(&mut self) {
        let range = self
            .anchor
            .and_then(|anchor| span(&self.state.lines, anchor, self.state.cursor));
        select(&mut self.state, range);
    }

    fn deselect(&mut self) {
        self.anchor = None;
        self.state.selection = None;
    }

    /// The text from the anchor to the cursor (emacs's region, Notepad's
    /// selection): where it starts, and where it ends, the character there
    /// left out; `None` if nothing is selected.
    fn region(&self) -> Option<(Index2, Index2)> {
        let anchor = self.anchor?;
        let cursor = self.state.cursor;
        if anchor < cursor {
            Some((anchor, cursor))
        } else if cursor < anchor {
            Some((cursor, anchor))
        } else {
            None
        }
    }

    /// Inserts pasted `text` at the cursor (in place of Notepad's
    /// selection), or in the command being typed.
    pub fn paste(&mut self, text: &str) {
        if let Some(command) = &mut self.command {
            command.insert_text(text);
            return;
        }
        match self.region() {
            Some(region) if self.keys == Keys::Notepad => self.delete(region),
            // edtui keeps its undo point to itself; deleting nothing sets
            // one.
            _ => self.state.execute(DeleteChar(0)),
        }
        self.deselect();
        self.insert(text);
    }

    /// What `key` puts in place of a selection: what it types, or, for
    /// `^V`, what is on the clipboard.
    fn replacement(&self, key: KeyEvent) -> Option<String> {
        if is_ctrl(key, 'v') {
            return Some(self.clipboard.get());
        }
        match key.code {
            KeyCode::Enter | KeyCode::Tab if plain(key) => Some(
                if key.code == KeyCode::Enter {
                    "\n"
                } else {
                    "\t"
                }
                .into(),
            ),
            _ => typed(key).map(String::from),
        }
    }

    /// Inserts the clipboard's text at the cursor, as typed (edtui's
    /// paste takes text starting with a line end as whole lines, as vim
    /// does), as one change to undo.
    fn paste_clipboard(&mut self) {
        let text = self.clipboard.get();
        if !text.is_empty() {
            // edtui keeps its undo point to itself; deleting nothing sets
            // one.
            self.state.execute(DeleteChar(0));
            self.insert(&text);
        }
    }

    /// Inserts `text` at the cursor, as part of the last change to undo.
    fn insert(&mut self, text: &str) {
        for c in text.chars() {
            match c {
                '\n' => self.state.execute(LineBreak(1)),
                '\r' => {}
                c => self.state.execute(InsertChar(c)),
            }
        }
    }

    /// The text of `region`.
    fn region_text(&self, (start, end): (Index2, Index2)) -> String {
        let text = self.text();
        let mut out = String::new();
        for (row, line) in text.split('\n').enumerate() {
            if row < start.row || row > end.row {
                continue;
            }
            let from = if row == start.row { start.col } else { 0 };
            let to = if row == end.row { end.col } else { usize::MAX };
            out.extend(line.chars().take(to).skip(from));
            if row < end.row {
                out.push('\n');
            }
        }
        out
    }

    /// Puts the text of `region` on the clipboard.
    fn copy(&mut self, region: (Index2, Index2)) {
        self.clipboard.set(self.region_text(region));
    }

    /// Deletes the text of `region`, as one change to undo. (Backspacing
    /// over it: edtui's own deletion of a selection drops a line whose
    /// text is all selected.)
    fn delete(&mut self, region: (Index2, Index2)) {
        let count = self.region_text(region).chars().count();
        self.deselect();
        self.state.cursor = region.1;
        self.state.execute(DeleteChar(count));
    }

    /// What the bottom line shows, `width` cells wide.
    pub fn bottom(&self, width: usize) -> Bottom {
        if let Some(question) = self.question {
            return Bottom::Prompt {
                prompt: question.prompt().into(),
                text: String::new(),
                cursor: 0,
            };
        }
        if let Some(command) = &self.command {
            let prompt = if self.keys == Keys::Vim { ":" } else { "M-x " };
            let (text, cursor) = command.view(width.saturating_sub(prompt.len()));
            return Bottom::Prompt {
                prompt: prompt.into(),
                text,
                cursor,
            };
        }
        if self.state.mode == EditorMode::Search {
            let prompt = match self.keys {
                Keys::Vim => "/",
                Keys::Emacs => "I-search: ",
                Keys::Notepad => "Find: ",
            };
            let search = LineEditor::new(&self.state.search_pattern());
            let (text, cursor) = search.view(width.saturating_sub(prompt.len()));
            return Bottom::Prompt {
                prompt: prompt.into(),
                text,
                cursor,
            };
        }
        if let Some(note) = &self.note {
            return Bottom::Note(note.clone());
        }
        if let Some(prefix) = self.prefix {
            return Bottom::Hint(describe(prefix) + "-");
        }
        if self.meta {
            return Bottom::Hint("ESC-".into());
        }
        let mode = match (self.keys, self.state.mode) {
            (Keys::Vim, EditorMode::Insert) => "-- INSERT --",
            (Keys::Vim, EditorMode::Visual) => "-- VISUAL --",
            _ => "",
        };
        Bottom::Hint(mode.into())
    }

    /// Draws the text in `area`, and returns where the cursor is, unless
    /// the bottom line has it.
    pub fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        text: Style,
        selection: Style,
    ) -> Option<Position> {
        let theme = EditorTheme::default()
            .base(text)
            .cursor_style(text.add_modifier(ratatui::style::Modifier::REVERSED))
            .selection_style(selection)
            .hide_status_line();
        EditorView::new(&mut self.state)
            .theme(theme)
            .wrap(true)
            .render(area, buf);
        let typing_below = self.question.is_some()
            || self.command.is_some()
            || self.state.mode == EditorMode::Search;
        if typing_below {
            None
        } else {
            self.state.cursor_screen_position()
        }
    }
}

/// Notepad's keys, as those of most graphical editors. Selecting with
/// Shift, copying and cutting are [`Builtin::notepad`]'s.
fn notepad_bindings() -> KeyEventHandler {
    use edtui::events::KeyEventRegister as R;
    let key = KeyInput::new;
    let ctrl = KeyInput::ctrl;
    let bindings: Vec<(R, Action)> = vec![
        (R::i(vec![key(KeyCode::Left)]), MoveBackward(1).into()),
        (R::i(vec![key(KeyCode::Right)]), MoveForward(1).into()),
        (R::i(vec![key(KeyCode::Up)]), MoveUp(1).into()),
        (R::i(vec![key(KeyCode::Down)]), MoveDown(1).into()),
        (R::i(vec![ctrl(KeyCode::Left)]), MoveWordBackward(1).into()),
        (R::i(vec![ctrl(KeyCode::Right)]), MoveWordForward(1).into()),
        (R::i(vec![key(KeyCode::Home)]), MoveToStartOfLine().into()),
        (R::i(vec![key(KeyCode::End)]), MoveToEndOfLine().into()),
        (
            R::i(vec![ctrl(KeyCode::Home)]),
            MoveToFirstRow().chain(MoveToStartOfLine()).into(),
        ),
        (
            R::i(vec![ctrl(KeyCode::End)]),
            MoveToLastRow().chain(MoveToEndOfLine()).into(),
        ),
        (R::i(vec![key(KeyCode::PageUp)]), MovePageUp().into()),
        (R::i(vec![key(KeyCode::PageDown)]), MovePageDown().into()),
        (R::i(vec![key(KeyCode::Enter)]), LineBreak(1).into()),
        (R::i(vec![key(KeyCode::Backspace)]), DeleteChar(1).into()),
        (
            R::i(vec![key(KeyCode::Delete)]),
            DeleteCharForward(1).into(),
        ),
        (
            R::i(vec![ctrl(KeyCode::Backspace)]),
            DeleteWordBackward(1).into(),
        ),
        (R::i(vec![KeyInput::ctrl('z')]), Undo.into()),
        (R::i(vec![KeyInput::ctrl('y')]), Redo.into()),
        (
            R::i(vec![KeyInput::ctrl('f')]),
            StartSearch.chain(SwitchMode(EditorMode::Search)).into(),
        ),
        (
            R::s(vec![key(KeyCode::Enter)]),
            SelectCurrentSearch
                .chain(SwitchMode(EditorMode::Insert))
                .into(),
        ),
        (
            R::s(vec![key(KeyCode::Esc)]),
            StopSearch.chain(SwitchMode(EditorMode::Insert)).into(),
        ),
        (
            R::s(vec![key(KeyCode::Backspace)]),
            RemoveCharFromSearch.into(),
        ),
    ];
    KeyEventHandler::new(bindings.into_iter().collect::<HashMap<_, _>>(), true)
}

/// The first and last characters between the positions `a` and `b`, the
/// character at the later one left out; `None` if they are the same.
fn span(lines: &Lines, a: Index2, b: Index2) -> Option<(Index2, Index2)> {
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    if start == end {
        return None;
    }
    let (_, last) = lines.prev(end)?;
    Some((start, last))
}

/// Selects from the first to the last character of `range`, or nothing.
/// (edtui doesn't export its selection type: one is made by its visual
/// mode, on a state of its own.)
fn select(state: &mut EditorState, range: Option<(Index2, Index2)>) {
    state.selection = range.and_then(|(first, last)| {
        let mut scratch = EditorState::new(Lines::default());
        scratch.execute(SwitchMode(EditorMode::Visual));
        let mut selection = scratch.selection.take()?;
        selection.start = first;
        selection.end = last;
        Some(selection)
    });
}

/// Where the cursor is after the last character.
fn end_of(lines: &Lines) -> Index2 {
    let row = lines.len().saturating_sub(1);
    Index2::new(row, lines.len_col(row).unwrap_or(0))
}

/// Whether `key` is Control and `c`.
fn is_ctrl(key: KeyEvent, c: char) -> bool {
    key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char(c)
}

/// Whether `key` has no Control or Alt.
fn plain(key: KeyEvent) -> bool {
    key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
}

/// Whether `key` types text (a character, Enter, Tab).
fn typing(key: KeyEvent) -> bool {
    typed(key).is_some() || (plain(key) && matches!(key.code, KeyCode::Enter | KeyCode::Tab))
}

/// The character `key` types: one with no Control or Alt, or, as edtui
/// takes it, one typed with AltGr (Control and Alt, or Alt alone, and not
/// a letter: AltGr+Q is `@` on German keyboards).
fn typed(key: KeyEvent) -> Option<char> {
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    let modifiers = key.modifiers.difference(KeyModifiers::SHIFT);
    let altgr =
        modifiers == KeyModifiers::CONTROL | KeyModifiers::ALT || modifiers == KeyModifiers::ALT;
    (modifiers.is_empty() || (altgr && !c.is_ascii_alphabetic())).then_some(c)
}

/// Whether `key` moves the cursor in Notepad's keys.
fn moves(key: KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
    )
}

/// A key as emacs writes it: `C-x`, `M-x`, `k`.
fn describe(key: KeyEvent) -> String {
    let name = match key.code {
        KeyCode::Char(c) => c.to_string(),
        other => format!("{other:?}"),
    };
    let ctrl = if key.modifiers.contains(KeyModifiers::CONTROL) {
        "C-"
    } else {
        ""
    };
    let alt = if key.modifiers.contains(KeyModifiers::ALT) {
        "M-"
    } else {
        ""
    };
    format!("{alt}{ctrl}{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(editor: &mut Builtin, keys: &[KeyEvent]) -> Outcome {
        let mut outcome = Outcome::Editing;
        for &key in keys {
            outcome = editor.key(key);
        }
        outcome
    }

    fn chars(text: &str) -> Vec<KeyEvent> {
        text.chars()
            .map(|c| match c {
                '\n' => KeyEvent::from(KeyCode::Enter),
                '\x1b' => KeyEvent::from(KeyCode::Esc),
                c if c.is_ascii_uppercase() => KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT),
                c => KeyEvent::from(KeyCode::Char(c)),
            })
            .collect()
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    #[test]
    fn the_text_comes_back_as_it_went_in() {
        for text in ["", "one\n", "one\ntwo", "a\n\n-- \nMe\n"] {
            assert_eq!(Builtin::new(text, Keys::Vim).text(), text);
        }
    }

    #[test]
    fn vim_writes_and_quits() {
        let mut vim = Builtin::new("\n-- \nMe\n", Keys::Vim);
        assert_eq!(press(&mut vim, &chars("iHi!\x1b")), Outcome::Editing);
        assert_eq!(
            press(&mut vim, &chars(":wq\n")),
            Outcome::Done("Hi!\n-- \nMe\n".into())
        );

        let mut vim = Builtin::new("text\n", Keys::Vim);
        press(&mut vim, &chars("x"));
        // `:q` refuses to drop changes; `:q!` drops them.
        assert_eq!(press(&mut vim, &chars(":q\n")), Outcome::Editing);
        assert!(matches!(vim.bottom(80), Bottom::Note(n) if n.starts_with("E37")));
        assert_eq!(
            press(&mut vim, &chars(":q!\n")),
            Outcome::Done("text\n".into())
        );

        // What `:w` saved is what quitting gives back.
        let mut vim = Builtin::new("text\n", Keys::Vim);
        press(&mut vim, &chars("x:w\nx"));
        assert_eq!(press(&mut vim, &chars("ZQ")), Outcome::Done("ext\n".into()));

        let mut vim = Builtin::new("text\n", Keys::Vim);
        assert_eq!(
            press(&mut vim, &chars("xZZ")),
            Outcome::Done("ext\n".into())
        );
    }

    #[test]
    fn vim_finds_a_colon_without_starting_a_command() {
        let mut vim = Builtin::new("a: b\n", Keys::Vim);
        // `f:` moves to the colon, `x` deletes it.
        press(&mut vim, &chars("f:x"));
        assert_eq!(vim.text(), "a b\n");
        assert_eq!(
            press(&mut vim, &chars(":x\n")),
            Outcome::Done("a b\n".into())
        );
    }

    #[test]
    fn vim_says_what_it_does_not_know() {
        let mut vim = Builtin::new("", Keys::Vim);
        press(&mut vim, &chars(":frobnicate\n"));
        assert_eq!(
            vim.bottom(80),
            Bottom::Note("E492: Not an editor command: frobnicate".into())
        );
        press(&mut vim, &chars("i"));
        assert_eq!(vim.bottom(80), Bottom::Hint("-- INSERT --".into()));
    }

    #[test]
    fn emacs_saves_and_leaves() {
        let mut emacs = Builtin::new("text\n", Keys::Emacs);
        press(&mut emacs, &chars("New "));
        assert_eq!(press(&mut emacs, &[ctrl('x'), ctrl('c')]), Outcome::Editing);
        assert_eq!(
            emacs.bottom(80),
            Bottom::Prompt {
                prompt: "Save changes? (y or n) ".into(),
                text: String::new(),
                cursor: 0
            }
        );
        assert_eq!(
            press(&mut emacs, &chars("y")),
            Outcome::Done("New text\n".into())
        );

        // `C-x k` drops what wasn't saved with `C-x C-s`.
        let mut emacs = Builtin::new("text\n", Keys::Emacs);
        press(&mut emacs, &chars("A"));
        press(&mut emacs, &[ctrl('x'), ctrl('s')]);
        press(&mut emacs, &chars("B"));
        press(&mut emacs, &[ctrl('x'), KeyEvent::from(KeyCode::Char('k'))]);
        assert_eq!(
            press(&mut emacs, &chars("y")),
            Outcome::Done("Atext\n".into())
        );

        // Unmodified, `C-x C-c` leaves at once; so does `M-x kill-buffer`.
        let mut emacs = Builtin::new("text\n", Keys::Emacs);
        assert_eq!(
            press(&mut emacs, &[ctrl('x'), ctrl('c')]),
            Outcome::Done("text\n".into())
        );
        let mut emacs = Builtin::new("text\n", Keys::Emacs);
        press(&mut emacs, &[alt('x')]);
        assert_eq!(
            press(&mut emacs, &chars("kill-buffer\n")),
            Outcome::Done("text\n".into())
        );
    }

    #[test]
    fn line_ends_with_carriage_returns_go_back_as_given() {
        let given = "\r\n-- \r\nMe\r\n";
        let mut vim = Builtin::new(given, Keys::Vim);
        assert_eq!(press(&mut vim, &chars(":q\n")), Outcome::Done(given.into()));
        let mut vim = Builtin::new(given, Keys::Vim);
        assert_eq!(press(&mut vim, &chars("ZZ")), Outcome::Done(given.into()));
        // Changed, the text has the editor's line ends.
        let mut vim = Builtin::new(given, Keys::Vim);
        assert_eq!(
            press(&mut vim, &chars("iHi\x1bZZ")),
            Outcome::Done("Hi\n-- \nMe\n".into())
        );
    }

    #[test]
    fn emacs_reads_escape_and_a_key_as_meta() {
        let esc = KeyEvent::from(KeyCode::Esc);
        let mut emacs = Builtin::new("one two\n", Keys::Emacs);
        press(
            &mut emacs,
            &[ctrl(' '), ctrl('f'), ctrl('f'), ctrl('f'), esc],
        );
        assert_eq!(emacs.bottom(80), Bottom::Hint("ESC-".into()));
        press(&mut emacs, &chars("w"));
        assert_eq!(emacs.text(), "one two\n");
        press(&mut emacs, &[ctrl('e'), ctrl('y')]);
        assert_eq!(emacs.text(), "one twoone\n");
        press(&mut emacs, &[esc]);
        press(&mut emacs, &chars("x"));
        assert_eq!(
            press(&mut emacs, &chars("kill-buffer\n")),
            Outcome::Editing,
            "asks first"
        );
        assert_eq!(
            press(&mut emacs, &chars("y")),
            Outcome::Done("one two\n".into())
        );
    }

    #[test]
    fn emacs_cuts_the_region() {
        let mut emacs = Builtin::new("one two\n", Keys::Emacs);
        press(
            &mut emacs,
            &[ctrl(' '), ctrl('f'), ctrl('f'), ctrl('f'), ctrl('w')],
        );
        assert_eq!(emacs.text(), " two\n");
        press(&mut emacs, &[ctrl('e'), ctrl('y')]);
        assert_eq!(emacs.text(), " twoone\n");
        press(&mut emacs, &[ctrl('_')]);
        assert_eq!(emacs.text(), " two\n");
    }

    #[test]
    fn notepad_selects_with_shift() {
        let mut notepad = Builtin::new("one two\n", Keys::Notepad);
        let right = shift(KeyCode::Right);
        press(&mut notepad, &[right, right, right, ctrl('c')]);
        // Typing replaces the selection; the clipboard keeps the copy.
        press(&mut notepad, &chars("1"));
        assert_eq!(notepad.text(), "1 two\n");
        press(&mut notepad, &[KeyEvent::from(KeyCode::End), ctrl('v')]);
        assert_eq!(notepad.text(), "1 twoone\n");
        press(&mut notepad, &[ctrl('a'), KeyEvent::from(KeyCode::Delete)]);
        assert_eq!(notepad.text(), "");
        press(&mut notepad, &[ctrl('z')]);
        assert_eq!(notepad.text(), "1 twoone\n");
    }

    #[test]
    fn keys_edtui_does_not_know_do_nothing() {
        for keys in [Keys::Vim, Keys::Emacs, Keys::Notepad] {
            let mut editor = Builtin::new("text\n", keys);
            for code in [
                KeyCode::F(1),
                KeyCode::Insert,
                KeyCode::BackTab,
                KeyCode::Null,
            ] {
                assert_eq!(editor.key(KeyEvent::from(code)), Outcome::Editing);
            }
            assert_eq!(editor.text(), "text\n");
        }
    }

    #[test]
    fn notepad_finds_again() {
        let mut notepad = Builtin::new("one two\ntwo\n", Keys::Notepad);
        press(&mut notepad, &[ctrl('f')]);
        press(&mut notepad, &chars("two\n"));
        assert_eq!(notepad.state.cursor, Index2::new(0, 4));
        // F3 finds the next one, and selects it: typing replaces it.
        let f3 = KeyEvent::from(KeyCode::F(3));
        press(&mut notepad, &[f3]);
        assert_eq!(
            notepad.region(),
            Some((Index2::new(1, 0), Index2::new(1, 3)))
        );
        press(&mut notepad, &chars("2"));
        assert_eq!(notepad.text(), "one two\n2\n");
        // Past the last one, it starts again; Shift+F3 goes back.
        let first = Some((Index2::new(0, 4), Index2::new(0, 7)));
        press(&mut notepad, &[f3]);
        assert_eq!(notepad.region(), first);
        press(&mut notepad, &[shift(KeyCode::F(3))]);
        assert_eq!(notepad.region(), first);
        press(&mut notepad, &chars("3"));
        assert_eq!(notepad.text(), "one 3\n2\n");
        press(&mut notepad, &[f3]);
        assert_eq!(
            notepad.bottom(80),
            Bottom::Note("Can't find \"two\"".into())
        );
    }

    #[test]
    fn deleting_a_whole_line_of_text_keeps_the_line() {
        let mut notepad = Builtin::new("one\ntwo\nthree\n", Keys::Notepad);
        let down = KeyEvent::from(KeyCode::Down);
        press(&mut notepad, &[down, shift(KeyCode::End), ctrl('x')]);
        assert_eq!(notepad.text(), "one\n\nthree\n");
        press(
            &mut notepad,
            &[down, KeyEvent::from(KeyCode::Home), ctrl('v')],
        );
        assert_eq!(notepad.text(), "one\n\ntwothree\n");
        press(&mut notepad, &[ctrl('z')]);
        assert_eq!(notepad.text(), "one\n\nthree\n");
        press(&mut notepad, &[ctrl('z')]);
        assert_eq!(notepad.text(), "one\ntwo\nthree\n");
    }

    #[test]
    fn emacs_undoes_with_any_spelling_of_control_slash() {
        for c in ['/', '_', '7'] {
            let mut emacs = Builtin::new("text\n", Keys::Emacs);
            press(&mut emacs, &chars("A"));
            press(&mut emacs, &[ctrl(c)]);
            assert_eq!(emacs.text(), "text\n", "C-{c}");
        }
    }

    #[test]
    fn notepad_replaces_a_selection_as_one_change() {
        let mut notepad = Builtin::new("one two\n", Keys::Notepad);
        let right = shift(KeyCode::Right);
        // Nothing on the clipboard yet: ^V leaves the selection.
        press(&mut notepad, &[right, right, right, ctrl('v')]);
        assert_eq!(notepad.text(), "one two\n");
        assert!(notepad.region().is_some());
        press(&mut notepad, &chars("1"));
        assert_eq!(notepad.text(), "1 two\n");
        press(&mut notepad, &[ctrl('z')]);
        assert_eq!(notepad.text(), "one two\n");
        // AltGr+Q, `@` on a German keyboard, replaces it too.
        let at = KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        press(
            &mut notepad,
            &[KeyEvent::from(KeyCode::Home), right, right, right, at],
        );
        assert_eq!(notepad.text(), "@ two\n");
    }

    #[test]
    fn pastes_line_ends_as_typed() {
        // The line end after `ab`, and `cd`, copied and pasted after `cd`.
        let mut notepad = Builtin::new("ab\ncd\n", Keys::Notepad);
        let end = KeyEvent::from(KeyCode::End);
        press(&mut notepad, &[end, shift(KeyCode::Down), ctrl('c')]);
        press(
            &mut notepad,
            &[KeyEvent::from(KeyCode::Left), end, ctrl('v')],
        );
        assert_eq!(notepad.text(), "ab\ncd\ncd\n");
        press(&mut notepad, &[ctrl('z')]);
        assert_eq!(notepad.text(), "ab\ncd\n");

        let mut emacs = Builtin::new("ab\ncd\n", Keys::Emacs);
        press(
            &mut emacs,
            &[ctrl('e'), ctrl(' '), ctrl('n'), alt('w'), ctrl('y')],
        );
        assert_eq!(emacs.text(), "ab\ncd\ncd\n");
    }

    #[test]
    fn notepad_moves_like_notepad() {
        let mut notepad = Builtin::new("one two\nthree four\n", Keys::Notepad);
        let (left, right) = (
            KeyEvent::from(KeyCode::Left),
            KeyEvent::from(KeyCode::Right),
        );
        let ctrl_end = KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL);
        let ctrl_home = KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL);
        // Ctrl+End and Ctrl+Home go to the end and start of the text.
        press(&mut notepad, &[right, right, ctrl_end]);
        assert_eq!(notepad.state.cursor, Index2::new(2, 0));
        press(
            &mut notepad,
            &[KeyEvent::from(KeyCode::Up), right, right, ctrl_home],
        );
        assert_eq!(notepad.state.cursor, Index2::new(0, 0));
        // Left and Right leave a selection at its edges.
        let select = [right, shift(KeyCode::Right), shift(KeyCode::Right)];
        press(&mut notepad, &select);
        press(&mut notepad, &[left]);
        assert_eq!(notepad.state.cursor, Index2::new(0, 1));
        assert_eq!(notepad.region(), None);
        press(&mut notepad, &select[1..]);
        press(&mut notepad, &[right]);
        assert_eq!(notepad.state.cursor, Index2::new(0, 3));
    }

    #[test]
    fn pasting_inserts_the_text() {
        let mut vim = Builtin::new("ab\n", Keys::Vim);
        vim.paste("x\r\ny");
        assert_eq!(vim.text(), "x\nyab\n");
        press(&mut vim, &chars("u"));
        assert_eq!(vim.text(), "ab\n");
        // In a command being typed, on its line.
        press(&mut vim, &chars(":"));
        vim.paste("w");
        assert_eq!(press(&mut vim, &chars("q\n")), Outcome::Done("ab\n".into()));
        // In place of Notepad's selection.
        let mut notepad = Builtin::new("ab\n", Keys::Notepad);
        press(&mut notepad, &[shift(KeyCode::Right)]);
        notepad.paste("Z");
        assert_eq!(notepad.text(), "Zb\n");
    }

    #[test]
    fn notepad_sends_or_drops() {
        let mut notepad = Builtin::new("text\n", Keys::Notepad);
        press(&mut notepad, &chars("New "));
        assert_eq!(
            press(&mut notepad, &[ctrl('s')]),
            Outcome::Done("New text\n".into())
        );

        let mut notepad = Builtin::new("text\n", Keys::Notepad);
        press(&mut notepad, &chars("New "));
        press(&mut notepad, &[KeyEvent::from(KeyCode::Esc)]);
        // No: back to editing; yes: the text as it was.
        assert_eq!(press(&mut notepad, &chars("n")), Outcome::Editing);
        press(&mut notepad, &[KeyEvent::from(KeyCode::Esc)]);
        assert_eq!(
            press(&mut notepad, &chars("y")),
            Outcome::Done("text\n".into())
        );
    }
}
