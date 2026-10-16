//! Key bindings per menu, as in mutt's `bind` and `macro`.

use std::collections::HashMap;
use std::fmt;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::Action;
use crate::config::KeyCommand;

/// A mutt menu: each has its own key bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Menu {
    /// mutt's generic menu: bindings for every menu but the pager, which a
    /// menu's own bindings override.
    Generic,
    /// The message index.
    Index,
    /// The message pager.
    Pager,
    /// The sign-in window (a mett addition).
    SignIn,
    /// The compose menu.
    Compose,
    /// The folder browser.
    Browser,
    /// The help screen.
    Help,
    /// The attachment menu.
    Attach,
    /// Addresses to choose from (mutt's query and alias menus).
    Query,
    /// Postponed messages to choose from.
    Postpone,
}

impl Menu {
    /// The name `bind` and `macro` use.
    pub fn name(self) -> &'static str {
        match self {
            Menu::Generic => "generic",
            Menu::Index => "index",
            Menu::Pager => "pager",
            Menu::SignIn => "signin",
            Menu::Compose => "compose",
            Menu::Browser => "browser",
            Menu::Help => "help",
            Menu::Attach => "attach",
            Menu::Query => "query",
            Menu::Postpone => "postpone",
        }
    }

    /// The menu `bind` and `macro` call `name`.
    pub fn from_name(name: &str) -> Option<Self> {
        [
            Menu::Generic,
            Menu::Index,
            Menu::Pager,
            Menu::SignIn,
            Menu::Compose,
            Menu::Browser,
            Menu::Help,
            Menu::Attach,
            Menu::Query,
            Menu::Postpone,
        ]
        .into_iter()
        .find(|m| m.name() == name)
        // mutt's alias menu is mett's query menu.
        .or((name == "alias").then_some(Menu::Query))
    }

    /// Whether the generic menu's bindings apply here. As in mutt, they
    /// don't in the pager; nor in the sign-in window, which has its own
    /// few keys.
    fn uses_generic(self) -> bool {
        !matches!(self, Menu::Generic | Menu::Pager | Menu::SignIn)
    }
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

    /// Control and a character.
    pub const fn ctrl(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
        }
    }

    /// Escape and a character, as mutt writes Meta (`\\ev`): terminals send
    /// Alt-v as Escape then `v`.
    pub const fn alt(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::ALT,
        }
    }

    /// The key press this key stands for, to replay it.
    pub fn event(self) -> KeyEvent {
        KeyEvent::new(self.code, self.modifiers)
    }

    /// Parses keys in mutt's notation: characters stand for themselves,
    /// `\Cx` and `^x` are Control-x, `\e`, `\t` and `\r` are Esc, Tab
    /// and Enter, `\n` is Control-J (which does what Enter does unless
    /// bound itself, as in mutt), and names in angle brackets (`<Enter>`, `<PageDown>`,
    /// `<F1>`) are special keys. Other `<names>` are left to `function`,
    /// which may turn them into strokes (for macros), or reject them.
    fn parse(text: &str, function: &dyn Fn(&str) -> Option<Stroke>) -> Result<Vec<Stroke>, String> {
        let mut strokes = Vec::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            let key = match c {
                '\\' => match chars.next() {
                    Some('C' | 'c') => match chars.next() {
                        Some(x) => control(x),
                        None => return Err(format!("{text}: \\C needs a key")),
                    },
                    // Escape then a character is Meta-character.
                    Some('e' | 'E') => escape(&mut chars),
                    Some('t') => Key::new(KeyCode::Tab),
                    Some('n') => Key::ctrl('j'),
                    Some('r') => Key::new(KeyCode::Enter),
                    Some(other) => Key::char(other),
                    None => Key::char('\\'),
                },
                '^' if chars.peek().is_some() => control(chars.next().expect("peeked")),
                '<' if chars.clone().any(|c| c == '>') => {
                    let name: String = chars.by_ref().take_while(|&c| c != '>').collect();
                    match named_key(&name) {
                        Some(key) => key,
                        None => match function(&name) {
                            Some(stroke) => {
                                strokes.push(stroke);
                                continue;
                            }
                            None => {
                                return Err(format!("{text}: unknown key or function <{name}>"));
                            }
                        },
                    }
                }
                // What the mettrc lexer made of `\e`, `\t`, `\Cx` and
                // friends.
                '\u{1b}' => escape(&mut chars),
                '\t' => Key::new(KeyCode::Tab),
                '\n' => Key::ctrl('j'),
                '\r' => Key::new(KeyCode::Enter),
                '\u{7f}' => Key::new(KeyCode::Backspace),
                // Control-x: 1 is ^A, 0x1f is ^_.
                c if c.is_ascii_control() => {
                    control(char::from(c as u8 | 0x40).to_ascii_lowercase())
                }
                c => Key::char(c),
            };
            strokes.push(Stroke::Key(key));
        }
        Ok(strokes)
    }
}

/// Escape, or with the character after it (taken), Meta and that
/// character: a printable one, not the start of other notation.
fn escape(chars: &mut std::iter::Peekable<std::str::Chars>) -> Key {
    chars
        .next_if(|c| c.is_ascii_graphic() && !matches!(c, '\\' | '<' | '^'))
        .map_or(Key::new(KeyCode::Esc), Key::alt)
}

/// Control-`x`, as the terminal reports it: `^M` is Enter, `^I`
/// is Tab, `^[` is Esc and `^?` is Backspace.
fn control(x: char) -> Key {
    match x.to_ascii_lowercase() {
        'm' => Key::new(KeyCode::Enter),
        'i' => Key::new(KeyCode::Tab),
        '[' => Key::new(KeyCode::Esc),
        '?' => Key::new(KeyCode::Backspace),
        // The terminal sends ^\ ^] ^^ ^_ as bytes 0x1c-0x1f and ^@ as 0,
        // which crossterm reports as Control-4 to Control-7 and
        // Control-Space.
        '\\' => Key::ctrl('4'),
        ']' => Key::ctrl('5'),
        '^' => Key::ctrl('6'),
        '_' => Key::ctrl('7'),
        '@' => Key::ctrl(' '),
        x => Key::ctrl(x),
    }
}

/// The key a `<name>` stands for.
fn named_key(name: &str) -> Option<Key> {
    let code = match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "space" => KeyCode::Char(' '),
        "esc" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        other => {
            let n: u8 = other.strip_prefix('f')?.parse().ok()?;
            KeyCode::F(n)
        }
    };
    Some(Key::new(code))
}

impl From<KeyEvent> for Key {
    fn from(event: KeyEvent) -> Self {
        let mut modifiers = event.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT);
        // Shift is already reflected in the character (or irrelevant).
        // Shift is part of BackTab too.
        if !matches!(event.code, KeyCode::Char(_) | KeyCode::BackTab) {
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
        let control = self.modifiers.contains(KeyModifiers::CONTROL);
        if control {
            f.write_str("^")?;
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            f.write_str("Esc ")?;
        }
        match self.code {
            // Written as typed in a mettrc, not as crossterm reports them.
            KeyCode::Char(' ') if control => f.write_str("@"),
            KeyCode::Char(' ') => f.write_str("<Space>"),
            KeyCode::Char(c @ '4'..='7') if control => {
                write!(f, "{}", ['\\', ']', '^', '_'][usize::from(c as u8 - b'4')])
            }
            KeyCode::Char(c) if control => write!(f, "{}", c.to_ascii_uppercase()),
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::F(n) => write!(f, "<F{n}>"),
            // The others by their names: `<Enter>`, `<PageDown>`.
            other => write!(f, "<{other:?}>"),
        }
    }
}

/// One step of a macro: a key, as if typed, or a function, run directly
/// (`<next-entry>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stroke {
    /// A key press.
    Key(Key),
    /// A function.
    Function(Action),
}

/// What a key does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    /// Runs a function.
    Action(Action),
    /// Plays a macro.
    Macro {
        /// The keys and functions it plays.
        strokes: Vec<Stroke>,
        /// What it does, for the help screen.
        description: Option<String>,
    },
}

/// Key bindings for every menu.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    bindings: HashMap<(Menu, Key), Binding>,
    /// Binding order per menu, so that help lists keys predictably.
    order: HashMap<Menu, Vec<Key>>,
}

impl Keymap {
    /// mutt's default bindings for the actions mett implements.
    pub fn mutt_defaults() -> Self {
        use Action::*;
        use KeyCode::{Backspace, Down, End, Enter, Esc, Home, Left, PageDown, PageUp, Right, Up};
        let mut keymap = Self::default();

        // Mailbox operations, in both the index and the pager.
        let mailbox: &[(Key, Action)] = &[
            (Key::char('d'), DeleteMessage),
            (Key::char('u'), UndeleteMessage),
            (Key::char('$'), SyncMailbox),
            (Key::char('F'), FlagMessage),
            (Key::char('N'), ToggleNew),
            (Key::char('s'), SaveMessage),
            (Key::char('C'), CopyMessage),
            (Key::char('c'), ChangeFolder),
            (Key::char('y'), BrowseMailboxes),
            (Key::char('/'), Search),
            (Key::char('n'), SearchNext),
            (Key::char('v'), ViewAttachments),
            (Key::char('Q'), Query),
            (Key::char('a'), CreateAlias),
            (Key::char('R'), RecallMessage),
            (Key::ctrl('n'), NextThread),
            (Key::ctrl('p'), PreviousThread),
        ];

        // mutt's generic menu: keys that work in every menu but the pager,
        // unless the menu binds them itself.
        let generic: &[(Key, Action)] = &[(Key::char('?'), Help), (Key::char(':'), EnterCommand)];

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
            (Key::char('m'), Mail),
            (Key::char('r'), Reply),
            (Key::char('g'), GroupReply),
            (Key::char('f'), Forward),
            (Key::char('l'), Limit),
            (Key::alt('v'), CollapseThread),
            (Key::alt('V'), CollapseAll),
        ];

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
            (Key::char('?'), Help),
            (Key::char(':'), EnterCommand),
            (Key::char('m'), Mail),
            (Key::char('r'), Reply),
            (Key::char('g'), GroupReply),
            (Key::char('f'), Forward),
        ];

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

        let compose: &[(Key, Action)] = &[
            (Key::char('y'), SendMessage),
            (Key::char('q'), Exit),
            (Key::char('P'), PostponeMessage),
            (Key::char('e'), EditMessage),
            (Key::char('t'), EditTo),
            (Key::char('c'), EditCc),
            (Key::char('b'), EditBcc),
            (Key::char('s'), EditSubject),
            (Key::char('a'), AttachFile),
            (Key::char('D'), DetachFile),
            (Key::char('j'), NextEntry),
            (Key::new(Down), NextEntry),
            (Key::char('k'), PreviousEntry),
            (Key::new(Up), PreviousEntry),
        ];

        let help: &[(Key, Action)] = &[
            (Key::char('q'), Exit),
            (Key::new(Esc), Exit),
            (Key::char('j'), NextLine),
            (Key::new(Down), NextLine),
            (Key::new(Enter), NextLine),
            (Key::char('k'), PreviousLine),
            (Key::new(Up), PreviousLine),
            (Key::char(' '), NextPage),
            (Key::new(PageDown), NextPage),
            (Key::char('-'), PreviousPage),
            (Key::new(PageUp), PreviousPage),
        ];

        // The list menus' keys for moving about, and for choosing an entry.
        let list: &[(Key, Action)] = &[
            (Key::char('q'), Exit),
            (Key::new(Esc), Exit),
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
            (Key::char('Z'), PreviousPage),
            (Key::new(PageUp), PreviousPage),
        ];
        let choose: &[(Key, Action)] = &[
            (Key::new(Enter), DisplayMessage),
            (Key::char(' '), DisplayMessage),
        ];
        let browser: &[(Key, Action)] = &[(Key::new(KeyCode::Tab), ToggleMailboxes)];
        let attach: &[(Key, Action)] = &[
            (Key::new(Enter), ViewAttach),
            (Key::char('m'), ViewAttach),
            (Key::char('s'), SaveEntry),
        ];
        let query: &[(Key, Action)] = &[(Key::char('m'), Mail)];

        type Table = [(Key, Action)];
        let menus: [(Menu, &[&Table]); 10] = [
            (Menu::Generic, &[generic]),
            (Menu::Index, &[index, mailbox]),
            (Menu::Pager, &[pager, mailbox]),
            (Menu::SignIn, &[sign_in]),
            (Menu::Compose, &[compose]),
            (Menu::Help, &[help]),
            (Menu::Browser, &[list, choose, browser]),
            (Menu::Attach, &[list, attach]),
            (Menu::Query, &[list, choose, query]),
            (Menu::Postpone, &[list, choose]),
        ];
        for (menu, tables) in menus {
            for (key, action) in tables.iter().copied().flatten() {
                keymap.bind_action(menu, *key, *action);
            }
        }

        keymap
    }

    /// Binds `key` to `binding` in `menu`, replacing any previous binding.
    pub fn bind(&mut self, menu: Menu, key: Key, binding: Binding) {
        if self.bindings.insert((menu, key), binding).is_none() {
            self.order.entry(menu).or_default().push(key);
        }
    }

    /// Binds `key` to `action` in `menu`.
    pub fn bind_action(&mut self, menu: Menu, key: Key, action: Action) {
        self.bind(menu, key, Binding::Action(action));
    }

    /// What `key` does in `menu`: its own binding, or the generic menu's.
    pub fn lookup(&self, menu: Menu, key: Key) -> Option<&Binding> {
        self.bindings.get(&(menu, key)).or_else(|| {
            menu.uses_generic()
                .then(|| self.bindings.get(&(Menu::Generic, key)))
                .flatten()
        })
    }

    /// The first key bound to `action` in `menu`, if any.
    /// The first key bound to `action` in `menu`, if any: the first of
    /// [`Keymap::entries`] that does it, found without listing them all
    /// (the help line asks for several each time it is drawn).
    pub fn key_for(&self, menu: Menu, action: Action) -> Option<Key> {
        let wanted = Binding::Action(action);
        let keys = |menu| self.order.get(&menu).into_iter().flatten();
        // The menu's own keys come first; a generic key counts only where
        // the menu doesn't bind it itself.
        let own = keys(menu).find(|key| self.bindings.get(&(menu, **key)) == Some(&wanted));
        own.or_else(|| {
            keys(Menu::Generic)
                .filter(|_| menu.uses_generic())
                .filter(|key| !self.bindings.contains_key(&(menu, **key)))
                .find(|key| self.bindings.get(&(Menu::Generic, **key)) == Some(&wanted))
        })
        .copied()
    }

    /// Every key that does something in `menu`, in binding order: the
    /// menu's own, then the generic menu's that it doesn't override.
    pub fn entries(&self, menu: Menu) -> Vec<(Key, &Binding)> {
        let own = self.order.get(&menu).into_iter().flatten();
        let generic = self
            .order
            .get(&Menu::Generic)
            .into_iter()
            .flatten()
            .filter(|_| menu.uses_generic());
        let mut seen = std::collections::HashSet::new();
        own.chain(generic)
            .filter(|key| seen.insert(**key))
            .filter_map(|key| Some((*key, self.lookup(menu, *key)?)))
            .collect()
    }

    /// Carries out a `bind` or `macro` command.
    pub fn apply(&mut self, command: &KeyCommand) -> Result<(), String> {
        let (menus, key) = match command {
            KeyCommand::Bind { menus, key, .. } | KeyCommand::Macro { menus, key, .. } => {
                (menus, key)
            }
        };
        let menus = menus
            .split(',')
            .map(|name| {
                Menu::from_name(name.trim())
                    .ok_or_else(|| format!("unknown menu {:?}", name.trim()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let keys = Key::parse(key, &|_| None)?;
        let [Stroke::Key(key)] = keys.as_slice() else {
            return Err(format!(
                "{key}: binding key sequences is not supported yet; bind a single key"
            ));
        };
        let binding = match command {
            KeyCommand::Bind { function, .. } => {
                let action = Action::from_name(function)
                    .ok_or_else(|| format!("unknown function {function}"))?;
                Binding::Action(action)
            }
            KeyCommand::Macro {
                sequence,
                description,
                ..
            } => Binding::Macro {
                strokes: parse_strokes(sequence)?,
                description: description.clone(),
            },
        };
        for menu in menus {
            self.bind(menu, *key, binding.clone());
        }
        Ok(())
    }
}

/// Parses a macro's keys and `<functions>`, as for `macro` and `push`.
pub fn parse_strokes(sequence: &str) -> Result<Vec<Stroke>, String> {
    Key::parse(sequence, &|name| {
        Action::from_name(name).map(Stroke::Function)
    })
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyEventKind, KeyEventState};

    use super::*;

    fn action(keymap: &Keymap, menu: Menu, key: Key) -> Option<Action> {
        match keymap.lookup(menu, key)? {
            Binding::Action(action) => Some(*action),
            Binding::Macro { .. } => None,
        }
    }

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
        assert_eq!(
            action(&keymap, Menu::Index, key),
            Some(Action::PreviousPage)
        );
    }

    #[test]
    fn menus_have_separate_bindings() {
        let keymap = Keymap::mutt_defaults();
        assert_eq!(
            action(&keymap, Menu::Index, Key::char('q')),
            Some(Action::Quit)
        );
        assert_eq!(
            action(&keymap, Menu::Pager, Key::char('q')),
            Some(Action::Exit)
        );
        assert_eq!(
            action(&keymap, Menu::Index, Key::char(' ')),
            Some(Action::DisplayMessage)
        );
        assert_eq!(
            action(&keymap, Menu::Pager, Key::char(' ')),
            Some(Action::NextPage)
        );
    }

    #[test]
    fn rebinding_replaces_and_help_uses_the_first_key() {
        let mut keymap = Keymap::mutt_defaults();
        keymap.bind_action(Menu::Index, Key::char('x'), Action::Quit);
        assert_eq!(
            action(&keymap, Menu::Index, Key::char('x')),
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
    fn the_key_for_an_action_is_the_first_entry_for_it() {
        let mut keymap = Keymap::mutt_defaults();
        // A generic key the index takes over, and one more for an action.
        keymap.bind_action(Menu::Index, Key::char(':'), Action::Quit);
        keymap.bind_action(Menu::Generic, Key::char('x'), Action::EnterCommand);
        for name in [
            "generic", "index", "pager", "signin", "compose", "browser", "help", "attach", "query",
            "postpone",
        ] {
            let menu = Menu::from_name(name).unwrap();
            for &action in Action::ALL {
                let first = keymap
                    .entries(menu)
                    .into_iter()
                    .find(|(_, binding)| *binding == &Binding::Action(action))
                    .map(|(key, _)| key);
                assert_eq!(keymap.key_for(menu, action), first, "{name}: {action:?}");
            }
        }
        assert_eq!(
            keymap.key_for(Menu::Index, Action::EnterCommand),
            Some(Key::char('x'))
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

    fn keys(text: &str) -> Vec<Stroke> {
        parse_strokes(text).unwrap()
    }

    #[test]
    fn punctuation_control_keys_match_the_terminal() {
        // As crossterm reports the bytes a terminal sends for them.
        assert_eq!(
            keys("^_\\C@^\\^]^^\u{0}"),
            [
                Stroke::Key(Key::ctrl('7')),
                Stroke::Key(Key::ctrl(' ')),
                Stroke::Key(Key::ctrl('4')),
                Stroke::Key(Key::ctrl('5')),
                Stroke::Key(Key::ctrl('6')),
                Stroke::Key(Key::ctrl(' ')),
            ]
        );
        assert_eq!(Key::ctrl('7').to_string(), "^_");
        assert_eq!(Key::ctrl(' ').to_string(), "^@");
        assert_eq!(Key::ctrl('4').to_string(), "^\\");
    }

    #[test]
    fn parses_mutt_key_notation() {
        assert_eq!(
            keys("ab"),
            [Stroke::Key(Key::char('a')), Stroke::Key(Key::char('b'))]
        );
        // Escape then a character is Meta.
        assert_eq!(
            keys("\\ev\u{1b}V\\e"),
            [
                Stroke::Key(Key::alt('v')),
                Stroke::Key(Key::alt('V')),
                Stroke::Key(Key::new(KeyCode::Esc)),
            ]
        );
        assert_eq!(Key::alt('v').to_string(), "Esc v");
        assert_eq!(keys("\\Cx"), [Stroke::Key(Key::ctrl('x'))]);
        assert_eq!(keys("^L"), [Stroke::Key(Key::ctrl('l'))]);
        // Control characters as the lexer leaves `\\C_` and `\\C?`.
        assert_eq!(
            keys("\u{1f}\u{7f}\u{1}"),
            [
                Stroke::Key(Key::ctrl('7')),
                Stroke::Key(Key::new(KeyCode::Backspace)),
                Stroke::Key(Key::ctrl('a')),
            ]
        );
        // Control keys the terminal reports as special keys.
        assert_eq!(
            keys("^M\\CJ^i^[^?"),
            [
                Stroke::Key(Key::new(KeyCode::Enter)),
                // Ctrl-J is reported as itself, not Enter.
                Stroke::Key(Key::ctrl('j')),
                Stroke::Key(Key::new(KeyCode::Tab)),
                Stroke::Key(Key::new(KeyCode::Esc)),
                Stroke::Key(Key::new(KeyCode::Backspace)),
            ]
        );
        // Control characters, as the mettrc lexer leaves `\\Cn` and `\\e`.
        assert_eq!(
            keys("\u{e}\u{1b}"),
            [
                Stroke::Key(Key::ctrl('n')),
                Stroke::Key(Key::new(KeyCode::Esc))
            ]
        );
        assert_eq!(
            keys("\\e\\t\\r\\n"),
            [
                Stroke::Key(Key::new(KeyCode::Esc)),
                Stroke::Key(Key::new(KeyCode::Tab)),
                Stroke::Key(Key::new(KeyCode::Enter)),
                Stroke::Key(Key::ctrl('j')),
            ]
        );
        assert_eq!(
            keys("<PageDown><f5><space>"),
            [
                Stroke::Key(Key::new(KeyCode::PageDown)),
                Stroke::Key(Key::new(KeyCode::F(5))),
                Stroke::Key(Key::char(' ')),
            ]
        );
        assert_eq!(
            keys("c=Archive<enter><next-entry>"),
            [
                Stroke::Key(Key::char('c')),
                Stroke::Key(Key::char('=')),
                Stroke::Key(Key::char('A')),
                Stroke::Key(Key::char('r')),
                Stroke::Key(Key::char('c')),
                Stroke::Key(Key::char('h')),
                Stroke::Key(Key::char('i')),
                Stroke::Key(Key::char('v')),
                Stroke::Key(Key::char('e')),
                Stroke::Key(Key::new(KeyCode::Enter)),
                Stroke::Function(Action::NextEntry),
            ]
        );
        // A lone < is a key.
        assert_eq!(keys("<"), [Stroke::Key(Key::char('<'))]);
        assert!(parse_strokes("<no-such-thing>").is_err());
    }

    #[test]
    fn applies_bind_and_macro() {
        let mut keymap = Keymap::mutt_defaults();
        keymap
            .apply(&KeyCommand::Bind {
                menus: "index,pager".into(),
                key: "\\Cn".into(),
                function: "next-entry".into(),
            })
            .unwrap();
        assert_eq!(
            action(&keymap, Menu::Index, Key::ctrl('n')),
            Some(Action::NextEntry)
        );
        assert_eq!(
            action(&keymap, Menu::Pager, Key::ctrl('n')),
            Some(Action::NextEntry)
        );

        keymap
            .apply(&KeyCommand::Macro {
                menus: "index".into(),
                key: "A".into(),
                sequence: "s=Archive<enter>".into(),
                description: Some("archive".into()),
            })
            .unwrap();
        assert!(matches!(
            keymap.lookup(Menu::Index, Key::char('A')),
            Some(Binding::Macro { description: Some(d), .. }) if d == "archive"
        ));

        let bad = |menus: &str, key: &str, function: &str| {
            keymap
                .clone()
                .apply(&KeyCommand::Bind {
                    menus: menus.into(),
                    key: key.into(),
                    function: function.into(),
                })
                .unwrap_err()
        };
        assert!(bad("nowhere", "x", "quit").contains("unknown menu"));
        assert!(bad("index", "x", "fly").contains("unknown function"));
        assert!(bad("index", "gg", "quit").contains("key sequences"));
    }

    #[test]
    fn the_generic_menu_applies_except_in_the_pager() {
        let mut keymap = Keymap::mutt_defaults();
        keymap.bind_action(Menu::Generic, Key::char('x'), Action::Quit);
        assert_eq!(
            action(&keymap, Menu::Index, Key::char('x')),
            Some(Action::Quit)
        );
        assert_eq!(
            action(&keymap, Menu::Browser, Key::char('x')),
            Some(Action::Quit)
        );
        assert_eq!(action(&keymap, Menu::Pager, Key::char('x')), None);
        // A menu's own binding wins, and noop unbinds.
        keymap.bind_action(Menu::Index, Key::char('x'), Action::Noop);
        assert_eq!(
            action(&keymap, Menu::Index, Key::char('x')),
            Some(Action::Noop)
        );
        assert_eq!(
            keymap.key_for(Menu::Index, Action::Help),
            Some(Key::char('?'))
        );
    }
}
