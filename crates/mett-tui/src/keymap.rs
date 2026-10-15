//! Key bindings per menu, as in mutt's `bind`.

use std::collections::HashMap;
use std::fmt;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::Action;

/// A mutt menu: each has its own key bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Menu {
    /// The message index.
    Index,
    /// The message pager.
    Pager,
    /// The sign-in window (a mett addition).
    SignIn,
}

/// A key press, normalised so that bindings compare reliably: Shift is
/// folded into the character (`Z`, not Shift+`z`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl Key {
    /// A plain key.
    pub const fn new(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// A character key.
    pub const fn char(c: char) -> Self {
        Self::new(KeyCode::Char(c))
    }
}

impl From<KeyEvent> for Key {
    fn from(event: KeyEvent) -> Self {
        let mut modifiers = event.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT);
        // Shift is already reflected in the character (or irrelevant).
        if !matches!(event.code, KeyCode::Char(_)) {
            modifiers |= event.modifiers & KeyModifiers::SHIFT;
        }
        Self {
            code: event.code,
            modifiers,
        }
    }
}

impl fmt::Display for Key {
    /// Formats the key the way mutt writes it in help screens.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            f.write_str("^")?;
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            f.write_str("M-")?;
        }
        match self.code {
            KeyCode::Char(' ') => f.write_str("<Space>"),
            KeyCode::Char(c) if self.modifiers.contains(KeyModifiers::CONTROL) => {
                write!(f, "{}", c.to_ascii_uppercase())
            }
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::Enter => f.write_str("<Enter>"),
            KeyCode::Esc => f.write_str("<Esc>"),
            KeyCode::Backspace => f.write_str("<Backspace>"),
            KeyCode::Up => f.write_str("<Up>"),
            KeyCode::Down => f.write_str("<Down>"),
            KeyCode::Left => f.write_str("<Left>"),
            KeyCode::Right => f.write_str("<Right>"),
            KeyCode::Home => f.write_str("<Home>"),
            KeyCode::End => f.write_str("<End>"),
            KeyCode::PageUp => f.write_str("<PageUp>"),
            KeyCode::PageDown => f.write_str("<PageDown>"),
            other => write!(f, "<{other:?}>"),
        }
    }
}

/// Key bindings for every menu.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    bindings: HashMap<(Menu, Key), Action>,
    /// Binding order per menu, so that help lists keys predictably.
    order: HashMap<Menu, Vec<Key>>,
}

impl Keymap {
    /// mutt's default bindings for the actions mett implements.
    pub fn mutt_defaults() -> Self {
        use Action::*;
        use KeyCode::{Backspace, Down, End, Enter, Esc, Home, Left, PageDown, PageUp, Right, Up};
        let mut keymap = Self::default();

        let index: &[(Key, Action)] = &[
            (Key::char('q'), Quit),
            (Key::char('j'), NextEntry),
            (Key::new(Down), NextEntry),
            (Key::char('k'), PreviousEntry),
            (Key::new(Up), PreviousEntry),
            (Key::char('='), FirstEntry),
            (Key::new(Home), FirstEntry),
            (Key::char('*'), LastEntry),
            (Key::new(End), LastEntry),
            (Key::char('z'), NextPage),
            (Key::new(PageDown), NextPage),
            (Key::new(Right), NextPage),
            (Key::char('Z'), PreviousPage),
            (Key::new(PageUp), PreviousPage),
            (Key::new(Left), PreviousPage),
            (Key::new(Enter), DisplayMessage),
            (Key::char(' '), DisplayMessage),
        ];
        for (key, action) in index {
            keymap.bind(Menu::Index, *key, *action);
        }

        let pager: &[(Key, Action)] = &[
            (Key::char('i'), Exit),
            (Key::char('q'), Exit),
            (Key::char('j'), NextEntry),
            (Key::char('k'), PreviousEntry),
            (Key::char(' '), NextPage),
            (Key::new(PageDown), NextPage),
            (Key::char('-'), PreviousPage),
            (Key::new(PageUp), PreviousPage),
            (Key::new(Enter), NextLine),
            (Key::new(Down), NextLine),
            (Key::new(Backspace), PreviousLine),
            (Key::new(Up), PreviousLine),
        ];
        for (key, action) in pager {
            keymap.bind(Menu::Pager, *key, *action);
        }

        let sign_in: &[(Key, Action)] = &[
            (Key::char('q'), Exit),
            (Key::new(Esc), Exit),
            (Key::char('o'), OpenBrowser),
            (Key::char('c'), CopyLink),
            (Key::new(Enter), Retry),
            (Key::char('j'), NextLine),
            (Key::new(Down), NextLine),
            (Key::char('k'), PreviousLine),
            (Key::new(Up), PreviousLine),
        ];
        for (key, action) in sign_in {
            keymap.bind(Menu::SignIn, *key, *action);
        }

        keymap
    }

    /// Binds `key` to `action` in `menu`, replacing any previous binding.
    pub fn bind(&mut self, menu: Menu, key: Key, action: Action) {
        if self.bindings.insert((menu, key), action).is_none() {
            self.order.entry(menu).or_default().push(key);
        }
    }

    /// The action bound to `key` in `menu`, if any.
    pub fn lookup(&self, menu: Menu, key: Key) -> Option<Action> {
        self.bindings.get(&(menu, key)).copied()
    }

    /// The first key bound to `action` in `menu`, if any.
    pub fn key_for(&self, menu: Menu, action: Action) -> Option<Key> {
        self.order
            .get(&menu)?
            .iter()
            .copied()
            .find(|key| self.lookup(menu, *key) == Some(action))
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};

    use super::*;

    fn event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn shifted_characters_match_their_bindings() {
        let keymap = Keymap::mutt_defaults();
        let key = Key::from(event(KeyCode::Char('Z'), KeyModifiers::SHIFT));
        assert_eq!(keymap.lookup(Menu::Index, key), Some(Action::PreviousPage));
    }

    #[test]
    fn menus_have_separate_bindings() {
        let keymap = Keymap::mutt_defaults();
        assert_eq!(
            keymap.lookup(Menu::Index, Key::char('q')),
            Some(Action::Quit)
        );
        assert_eq!(
            keymap.lookup(Menu::Pager, Key::char('q')),
            Some(Action::Exit)
        );
        assert_eq!(
            keymap.lookup(Menu::Index, Key::char(' ')),
            Some(Action::DisplayMessage)
        );
        assert_eq!(
            keymap.lookup(Menu::Pager, Key::char(' ')),
            Some(Action::NextPage)
        );
    }

    #[test]
    fn rebinding_replaces_and_help_uses_the_first_key() {
        let mut keymap = Keymap::mutt_defaults();
        keymap.bind(Menu::Index, Key::char('x'), Action::Quit);
        assert_eq!(
            keymap.lookup(Menu::Index, Key::char('x')),
            Some(Action::Quit)
        );
        assert_eq!(
            keymap.key_for(Menu::Index, Action::Quit),
            Some(Key::char('q'))
        );
        assert_eq!(
            keymap.key_for(Menu::Index, Action::DisplayMessage),
            Some(Key::new(KeyCode::Enter))
        );
    }

    #[test]
    fn keys_display_like_mutt() {
        assert_eq!(Key::char('j').to_string(), "j");
        let ctrl_l = Key {
            code: KeyCode::Char('l'),
            modifiers: KeyModifiers::CONTROL,
        };
        assert_eq!(ctrl_l.to_string(), "^L");
        assert_eq!(Key::char(' ').to_string(), "<Space>");
        assert_eq!(Key::new(KeyCode::PageDown).to_string(), "<PageDown>");
    }
}
