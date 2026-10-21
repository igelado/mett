//! Keys: what a key press does in the current menu (a function, or a
//! macro's keys played one by one), the help screen, and mettrc commands
//! typed at the `:` prompt.

use std::borrow::Cow;
use std::collections::VecDeque;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::composing::Question;
use super::{App, Effect, Msg, Screen, Settings};
use crate::action::Action;
use crate::config::KeyCommand;
use crate::keymap::{Binding, Key, Menu, Stroke};

/// How many queued keys play in a row before mett suspects a macro loop.
const MAX_PLAYED: usize = 10_000;

/// The help screen: the keys of the menu it was opened from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Help {
    /// The menu whose keys it lists.
    pub menu: Menu,
    /// The first visible line.
    pub scroll: usize,
}

/// The outcome of a command typed at the `:` prompt, with the
/// configuration it left behind.
#[derive(Debug)]
pub struct Reconfigured {
    /// The command's warnings, or its error.
    pub outcome: Result<Vec<String>, String>,
    /// The settings now, or why they can't be used.
    pub settings: Result<Settings, String>,
    /// `bind` and `macro` commands to carry out.
    pub key_commands: Vec<KeyCommand>,
    /// Keys `push` and `exec` asked to play.
    pub pushed: Vec<String>,
}

/// How soon after Escape a key is Meta and that key.
const ESCAPE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

impl App {
    /// Handles a key press: as typed, while a question is asked; otherwise
    /// through the key bindings of the current menu.
    pub(super) fn on_key_press(&mut self, event: KeyEvent) -> Vec<Effect> {
        // Ctrl-J is a newline: it does what Enter does, unless bound.
        let ctrl_j = Key::from(event) == Key::ctrl('j');
        self.input_note = None;
        // The built-in editor takes the keys (unless the sign-in window
        // is up), until the user leaves it.
        if self.sign_in.is_none()
            && let Some(editing) = &mut self.editing
        {
            return match editing.get_mut().key(event) {
                crate::editor::builtin::Outcome::Editing => Vec::new(),
                crate::editor::builtin::Outcome::Done(text) => {
                    self.editing = None;
                    self.on_edited(Ok(text))
                }
                // `:set spell`, say: mett's own settings follow.
                crate::editor::builtin::Outcome::Configure(command) => {
                    self.configuring += 1;
                    vec![Effect::Command(command)]
                }
                crate::editor::builtin::Outcome::AddWord(word) => {
                    // Also in the dictionaries kept for later, which the
                    // editor may not have (with spelling off).
                    if let Some((_, speller)) = &self.speller {
                        speller.add(&word);
                    }
                    match self.settings.spell.words.clone() {
                        Some(path) => vec![Effect::AddWord { path, word }],
                        None => Vec::new(),
                    }
                }
            };
        }
        // Keys wait for a file being read or saved; `^G` gives up on it.
        if let Some((_, path)) = &self.file_wait
            && self.sign_in.is_none()
        {
            let key = Key::from(event);
            if key == Key::ctrl('g') || key == Key::new(KeyCode::Esc) {
                self.status = format!("Not waiting for {} any more.", path.display());
                self.file_wait = None;
            } else {
                self.status = format!("Waiting for {} (^G gives up)...", path.display());
            }
            return Vec::new();
        }
        if self.input.is_some() && self.sign_in.is_none() {
            // A question took over: an Escape before it is forgotten.
            self.escape = None;
            let event = if ctrl_j {
                KeyEvent::from(KeyCode::Enter)
            } else {
                event
            };
            return self.on_key(event);
        }
        let mut key = Key::from(event);
        let menu = self.menu();
        // Escape where it is bound to nothing starts Meta and the next key,
        // for terminals that send them apart.
        if key == Key::new(KeyCode::Esc) && self.keymap.lookup(menu, key).is_none() {
            self.escape = Some((menu, std::time::Instant::now()));
            return Vec::new();
        }
        // Only in the menu it was typed in: another may have taken over
        // since (the sign-in window, say).
        // A key long after it is a key of its own, as with curses'
        // ESCDELAY.
        if self
            .escape
            .take()
            .is_some_and(|(at, when)| at == menu && when.elapsed() < ESCAPE_DELAY)
            && let KeyCode::Char(c) = event.code
            // A key with Control (or Alt) of its own stays as it is.
            && event.modifiers.difference(KeyModifiers::SHIFT).is_empty()
        {
            key = Key::alt(c);
        }
        let binding = self.keymap.lookup(menu, key).or_else(|| {
            ctrl_j
                .then(|| self.keymap.lookup(menu, Key::new(KeyCode::Enter)))
                .flatten()
        });
        match binding.cloned() {
            Some(Binding::Action(action)) => self.update(Msg::Action(action)),
            Some(Binding::Macro { strokes, .. }) => {
                // Played before anything already queued, in order. In the
                // sign-in window they play there, while other keys wait
                // for the sign-in.
                let queue = if self.menu() == Menu::SignIn {
                    &mut self.sign_in_strokes
                } else {
                    &mut self.strokes
                };
                push_front_all(queue, strokes);
                Vec::new()
            }
            None => {
                let unbound = match self.keymap.key_for(self.menu(), Action::Help) {
                    Some(help) => format!("Key is not bound.  Press '{help}' for help."),
                    None => "Key is not bound.".into(),
                };
                self.fail(unbound);
                Vec::new()
            }
        }
    }

    /// The next key or function a macro or `push` queued, as a message to
    /// handle before reading more input.
    ///
    /// A macro that plays itself (`macro index x x`) would never let the
    /// user type again: after [`MAX_PLAYED`] keys in a row, the queue is
    /// dropped.
    ///
    /// Queued keys wait for what the last action is waiting for (see
    /// [`App::waiting`]), except a macro run in the sign-in window.
    pub fn next_stroke(&mut self) -> Option<Msg> {
        if self.sign_in.is_none() {
            // The window closed: the rest of its macro goes on, first.
            push_front_all(&mut self.strokes, std::mem::take(&mut self.sign_in_strokes));
        }
        let stroke = if self.sign_in.is_some() && !self.sign_in_strokes.is_empty() {
            self.sign_in_strokes.pop_front()
        } else if self.waiting() {
            return None;
        } else {
            self.strokes.pop_front()
        };
        let Some(stroke) = stroke else {
            self.played = 0;
            return None;
        };
        self.played += 1;
        if self.played > MAX_PLAYED {
            self.strokes.clear();
            self.sign_in_strokes.clear();
            self.played = 0;
            self.fail("Macro loop detected: stopped.");
            // The screen was drawn before; show why the keys stopped.
            self.redraw = true;
            return None;
        }
        Some(match stroke {
            Stroke::Key(key) => Msg::Key(key.event()),
            Stroke::Function(action) => Msg::Action(action),
        })
    }

    /// The next key or function of a macro run in the sign-in window,
    /// while it is open. (When quitting, only the sign-in window takes
    /// keys.)
    pub fn next_sign_in_stroke(&mut self) -> Option<Msg> {
        if self.sign_in.is_none() || self.sign_in_strokes.is_empty() {
            return None;
        }
        self.next_stroke()
    }

    /// Notes a key the user typed (not one a macro played). Typed while
    /// a macro's keys wait for the last action, it takes over, as in mutt:
    /// it may well cancel or change what they were waiting for (a reply
    /// given up for a move elsewhere), so the rest of them are dropped.
    pub fn typed(&mut self) {
        // Keys for the sign-in window serve the sign-in the macro waits
        // for, rather than taking over from it; so do those typed in the
        // built-in editor, as they would in an external one.
        if self.sign_in.is_some() || self.editing.is_some() {
            return;
        }
        if !self.strokes.is_empty() && self.waiting() {
            self.strokes.clear();
        }
    }

    /// Whether the screen needs drawing again before waiting for input.
    pub fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.redraw)
    }

    /// Queues keys to play next, in mutt's notation, as `push` does: before
    /// the rest of a macro that pushed them.
    pub fn push_keys(&mut self, keys: &str) -> Result<(), String> {
        push_front_all(&mut self.strokes, crate::keymap::parse_strokes(keys)?);
        Ok(())
    }

    /// Queues keys to play once the folder has been listed (or failed to
    /// be), as `push` in a mettrc does.
    pub fn push_after_load(&mut self, strokes: Vec<Stroke>) {
        self.after_load.extend(strokes);
    }

    /// Says `notice` on the message line now, and again once the folder
    /// has been listed (which says it is loading meanwhile), unless that
    /// failed.
    pub fn notice(&mut self, notice: String) {
        self.status.clone_from(&notice);
        self.notice = Some(notice);
    }

    /// Whether the last action is still waiting for the mailbox: a macro's
    /// next keys wait for it, as they would in mutt, which waits for each
    /// command to finish. Its result decides what the keys apply to.
    pub fn waiting(&self) -> bool {
        let loading_pager = matches!(self.screen(), Screen::Pager(p) if p.message.is_none());
        self.compose_fetch.is_some()
            // A reply waiting for the user's own addresses to choose its
            // recipients.
            || self.awaiting_addresses.is_some()
            || self.opening.is_some()
            || self.switching.is_some()
            || self.listing.is_some()
            || loading_pager
            || self.browser_request().is_some()
            // Attachments being listed, or one fetched to show or save.
            || self.attachments_request().is_some()
            || self.query_request().is_some()
            // A postponed message being looked for.
            || self.recall.is_some()
            || self.fetching.is_some()
            // A file being read or saved.
            || self.file_wait.is_some()
            || self.delivering()
            // A move or purge decides what the next keys act on (and the
            // mailbox refuses to touch a message on its way).
            || self.writing.is_some()
            || !self.writes.is_empty()
            // The user has to sign in first; the keys play afterwards.
            || self.sign_in.is_some()
            // A message being written: the next keys act on what it
            // becomes (`e` then `<send-message>` sends the edited text).
            || self.editing.is_some()
            // A `:` command may bind the keys that come next.
            || self.configuring > 0
    }

    /// Plays the keys that waited for the folder.
    pub(super) fn loaded(&mut self) {
        self.strokes.extend(std::mem::take(&mut self.after_load));
        if let Some(notice) = self.notice.take()
            && self.error.as_deref() != Some(self.status.as_str())
        {
            self.status = notice;
            self.redraw = true;
        }
    }

    /// Opens the help screen for the current menu.
    pub(super) fn open_help(&mut self) -> Vec<Effect> {
        let busy = [
            // The folder list would arrive under the help screen, which
            // can't take it.
            (self.browser_request().is_some(), "Listing folders"),
            // The message would arrive under the help screen; a retry
            // after a failed sign-in looks for it on top.
            (
                matches!(self.screen(), Screen::Pager(p) if p.message.is_none()),
                "Loading message",
            ),
            // A delivery's sign-in retry looks for its compose menu on top.
            (self.delivering(), "Sending message"),
            // Opening another folder starts it afresh, without the help.
            (self.switching.is_some(), "Opening folder"),
            // A lookup's answer may take its menu away (and bring back the
            // question it came from): not from under the help.
            (
                matches!(self.screens.last(), Some(Screen::Query(menu)) if menu.request.is_some()),
                "Looking up addresses",
            ),
        ];
        if let Some((_, busy)) = busy.into_iter().find(|(busy, _)| *busy) {
            self.fail(format!("{busy}; wait a moment."));
            return Vec::new();
        }
        let menu = self.menu();
        if menu != Menu::Help {
            self.screens.push(Screen::Help(Help { menu, scroll: 0 }));
        }
        Vec::new()
    }

    /// How many lines the help screen has: one for each key.
    pub fn help_count(&self, menu: Menu) -> usize {
        self.keymap.entries(menu).len()
    }

    /// The help screen's lines: each key, its function and what it does.
    /// Made as they are taken, so that drawing a page makes only its own.
    pub fn help_lines(&self, menu: Menu) -> impl Iterator<Item = String> {
        self.keymap.entries(menu).into_iter().map(|(key, binding)| {
            let (name, what): (_, Cow<str>) = match binding {
                Binding::Action(action) => (action.name(), action.description().into()),
                Binding::Macro {
                    strokes,
                    description,
                } => (
                    "macro",
                    description
                        .as_deref()
                        .map_or_else(|| macro_text(strokes).into(), Cow::from),
                ),
            };
            format!("{:<12} {:<20} {what}", key.to_string(), name)
        })
    }

    pub(super) fn on_help_action(&mut self, action: Action) -> Vec<Effect> {
        let page = self.body_height.max(1);
        let Some(Screen::Help(help)) = self.screens.last() else {
            return Vec::new();
        };
        let last = self.help_count(help.menu).saturating_sub(self.body_height);
        let Some(Screen::Help(help)) = self.screens.last_mut() else {
            return Vec::new();
        };
        match action {
            Action::Exit | Action::Quit => {
                self.screens.pop();
            }
            Action::NextLine | Action::NextEntry => help.scroll = (help.scroll + 1).min(last),
            Action::PreviousLine | Action::PreviousEntry => {
                help.scroll = help.scroll.saturating_sub(1);
            }
            Action::NextPage => help.scroll = (help.scroll + page).min(last),
            Action::PreviousPage => help.scroll = help.scroll.saturating_sub(page),
            Action::FirstEntry => help.scroll = 0,
            Action::LastEntry => help.scroll = last,
            _ => {}
        }
        Vec::new()
    }

    /// Asks for a mettrc command (`:`).
    pub(super) fn ask_command(&mut self) -> Vec<Effect> {
        self.ask(Question::Command, ":", "");
        Vec::new()
    }

    /// Runs the command typed at the `:` prompt.
    pub(super) fn answer_command(&mut self, text: String) -> Vec<Effect> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        self.configuring += 1;
        vec![Effect::Command(text)]
    }

    /// Takes in what a `:` command changed.
    pub(super) fn reconfigure(&mut self, reconfigured: Reconfigured) -> Vec<Effect> {
        self.configuring = self.configuring.saturating_sub(1);
        let Reconfigured {
            outcome,
            settings,
            key_commands,
            pushed,
        } = reconfigured;
        let mut problems: Vec<String> = Vec::new();
        match outcome {
            Ok(warnings) => problems.extend(warnings),
            Err(error) => problems.push(error),
        }
        match settings {
            Ok(settings) => self.apply_settings(settings),
            Err(error) => problems.push(error),
        }
        for command in &key_commands {
            if let Err(error) = self.keymap.apply(command) {
                problems.push(error);
            }
        }
        // Each push goes in front of what is queued: push the last first,
        // so that they play in the order the commands gave them.
        let mut push_problems: Vec<String> = pushed
            .iter()
            .rev()
            .filter_map(|keys| self.push_keys(keys).err())
            .collect();
        push_problems.reverse();
        problems.extend(push_problems);
        if problems.is_empty() {
            self.status = self.after_configured.take().unwrap_or_default();
        } else {
            self.fail(problems.join("; "));
        }
        // The spelling settings may have changed.
        let mut effects = self.respell();
        // Checking for new mail may have been turned on.
        effects.extend(self.arm_poll());
        effects
    }
}

/// Puts `strokes` in front of `queue`, in order.
fn push_front_all(
    queue: &mut VecDeque<Stroke>,
    strokes: impl IntoIterator<Item = Stroke, IntoIter: DoubleEndedIterator>,
) {
    for stroke in strokes.into_iter().rev() {
        queue.push_front(stroke);
    }
}

/// A macro's keys, for the help screen when it has no description.
fn macro_text(strokes: &[Stroke]) -> String {
    strokes
        .iter()
        .map(|stroke| match stroke {
            Stroke::Key(key) => key.to_string(),
            Stroke::Function(action) => format!("<{}>", action.name()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use mett_core::{MessageSummary, mock};
    use ratatui::crossterm::event::KeyCode;

    use super::*;
    use crate::app::composing::Input;
    use crate::app::testing::*;
    use crate::worker::Event;

    /// Messages m0, m1 and m2.
    fn three() -> Vec<MessageSummary> {
        (0..3)
            .map(|i| mock::message(&format!("m{i}"), "in", "s", DateTime::UNIX_EPOCH).summary)
            .collect()
    }

    /// An app with three messages in its index.
    fn app() -> App {
        opened(Settings::default(), 10, three(), None).0
    }

    fn press(app: &mut App, key: Key) -> Vec<Effect> {
        app.update(Msg::Key(key.event()))
    }

    /// Plays what is queued, as the main loop does: not while the last
    /// action waits for the mailbox.
    fn play(app: &mut App) {
        while let Some(msg) = app.next_stroke() {
            app.update(msg);
        }
    }

    #[test]
    fn keys_run_their_functions() {
        let mut app = app();
        press(&mut app, Key::char('j'));
        assert_eq!(app.selected, 1);
        press(&mut app, Key::char('#'));
        assert_eq!(app.status, "Key is not bound.  Press '?' for help.");
        // The next key clears the message.
        press(&mut app, Key::char('k'));
        assert_eq!(app.status, "");
    }

    #[test]
    fn macros_play_their_keys_and_functions() {
        let mut app = app();
        app.keymap
            .apply(&KeyCommand::Macro {
                menus: "index".into(),
                key: "X".into(),
                sequence: "<last-entry>k/s<enter>".into(),
                description: None,
            })
            .unwrap();
        press(&mut app, Key::char('X'));
        play(&mut app);
        // To the last message, up one, then a search that finds the next.
        assert_eq!(app.selected, 2);
        assert_eq!(app.input, None, "the search question was answered");
    }

    #[test]
    fn push_queues_keys_to_play() {
        let mut app = app();
        app.push_keys("jj").unwrap();
        play(&mut app);
        assert_eq!(app.selected, 2);
        assert!(app.push_keys("<frobnicate>").is_err());
    }

    #[test]
    fn the_help_screen_lists_the_menu_s_keys() {
        let mut app = app();
        press(&mut app, Key::char('?'));
        assert_eq!(app.menu(), Menu::Help);
        let lines: Vec<String> = app.help_lines(Menu::Index).collect();
        assert!(lines[0].starts_with("q            quit"), "{lines:#?}");
        assert!(
            lines.iter().any(|l| l.contains("enter-command")),
            "generic keys too"
        );
        // Scrolls, within the list.
        for _ in 0..100 {
            press(&mut app, Key::char('j'));
        }
        let Screen::Help(help) = app.screen() else {
            panic!("not in help");
        };
        assert_eq!(help.scroll, lines.len() - app.body_height);
        press(&mut app, Key::char('q'));
        assert_eq!(app.menu(), Menu::Index);
    }

    #[test]
    fn commands_typed_at_the_prompt_reconfigure_the_app() {
        let mut app = app();
        press(&mut app, Key::char(':'));
        for c in "bind index J last-entry".chars() {
            press(&mut app, Key::char(c));
        }
        let effects = press(&mut app, Key::new(KeyCode::Enter));
        assert_eq!(effects, [Effect::Command("bind index J last-entry".into())]);

        // What the main loop sends back.
        app.update(Msg::Configured(Reconfigured {
            outcome: Ok(vec![]),
            settings: Ok(Settings {
                resolve: false,
                ..Settings::default()
            }),
            key_commands: vec![KeyCommand::Bind {
                menus: "index".into(),
                key: "J".into(),
                function: "last-entry".into(),
            }],
            pushed: vec!["J".into()],
        }));
        assert_eq!(app.status, "");
        assert!(!app.resolve);
        play(&mut app);
        assert_eq!(app.selected, 2);

        app.update(Msg::Configured(Reconfigured {
            outcome: Err("timeout: expected a number".into()),
            settings: Ok(Settings::default()),
            key_commands: vec![KeyCommand::Bind {
                menus: "nowhere".into(),
                key: "J".into(),
                function: "quit".into(),
            }],
            pushed: vec![],
        }));
        assert_eq!(
            app.status,
            "timeout: expected a number; unknown menu \"nowhere\""
        );
    }

    #[test]
    fn a_macro_that_plays_itself_is_stopped() {
        let mut app = app();
        app.keymap
            .apply(&KeyCommand::Macro {
                menus: "index".into(),
                key: "x".into(),
                sequence: "x".into(),
                description: None,
            })
            .unwrap();
        press(&mut app, Key::char('x'));
        play(&mut app);
        assert_eq!(app.status, "Macro loop detected: stopped.");
        assert!(app.take_redraw(), "the status is shown");
        assert!(!app.take_redraw());
        assert!(app.next_stroke().is_none());
    }

    #[test]
    fn startup_keys_wait_for_the_folder() {
        let (mut app, effects) = App::open(80, 10, inbox(), Settings::default());
        app.push_after_load(crate::keymap::parse_strokes("<last-entry>").unwrap());
        assert!(app.next_stroke().is_none(), "nothing to move through yet");
        let open = first_request(&effects);
        let list = folder_opened(&mut app, open, mock::folder("in", "Inbox", None));
        listed(&mut app, list, three(), None);
        play(&mut app);
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn a_notice_is_said_again_once_the_folder_is_listed() {
        let (mut app, effects) = App::open(80, 10, inbox(), Settings::default());
        app.notice("warning: gui_font: x: no such file".into());
        assert_eq!(app.status, "warning: gui_font: x: no such file");
        let open = first_request(&effects);
        let list = folder_opened(&mut app, open, mock::folder("in", "Inbox", None));
        listed(&mut app, list, three(), None);
        assert_eq!(app.status, "warning: gui_font: x: no such file");
    }

    #[test]
    fn a_macro_waits_for_what_its_keys_started() {
        let mut app = app();
        app.keymap
            .apply(&KeyCommand::Macro {
                menus: "index".into(),
                key: "R".into(),
                sequence: "r<last-entry>".into(),
                description: None,
            })
            .unwrap();
        press(&mut app, Key::char('R'));
        // `r` fetches the message: <last-entry> waits, rather than
        // cancelling the reply.
        play(&mut app);
        assert!(app.waiting());
        assert_eq!(app.strokes.len(), 1);
    }

    #[test]
    fn help_and_commands_cancel_a_pending_reply() {
        let mut app = app();
        press(&mut app, Key::char('r'));
        assert!(app.waiting());
        press(&mut app, Key::char('?'));
        assert!(!app.waiting(), "the reply was given up");
    }

    #[test]
    fn keys_pushed_by_a_macro_play_before_its_rest() {
        let mut app = app();
        app.strokes.push_back(Stroke::Key(Key::char('k')));
        app.push_keys("<last-entry>").unwrap();
        play(&mut app);
        assert_eq!(app.selected, 1, "last entry, then up one");
    }

    #[test]
    fn queued_keys_wait_for_a_sign_in() {
        let mut app = app();
        app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
            account: mett_core::AccountId::new("a@b.c"),
            outcome: Err("expired".into()),
        }));
        app.push_keys("j").unwrap();
        assert!(app.waiting());
    }

    #[test]
    fn help_waits_for_the_folder_list() {
        let mut app = app();
        let effects = act(&mut app, Action::BrowseMailboxes);
        assert!(matches!(effects[..], [Effect::Request(..)]));
        act(&mut app, Action::Help);
        assert!(matches!(app.screen(), Screen::Browser(_)));
        assert_eq!(app.status, "Listing folders; wait a moment.");
    }

    #[test]
    fn pushed_sequences_play_in_order() {
        let mut app = app();
        app.update(Msg::Configured(Reconfigured {
            outcome: Ok(vec![]),
            settings: Ok(Settings::default()),
            key_commands: vec![],
            pushed: vec!["<last-entry>".into(), "<previous-entry>".into()],
        }));
        play(&mut app);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn macros_wait_for_mailbox_changes() {
        let mut app = app();
        app.push_keys("<save-message>=Archive<enter><delete-message>")
            .unwrap();
        let mut effects = Vec::new();
        while let Some(msg) = app.next_stroke() {
            effects.extend(app.update(msg));
            if app.waiting() {
                break;
            }
        }
        assert!(app.waiting(), "the move is on its way");
        assert!(!app.strokes.is_empty(), "d waits for it");
        let Some(Effect::Request(id, _)) = effects.last().cloned() else {
            panic!("expected a request, got {effects:?}");
        };
        respond(
            &mut app,
            id,
            Event::Transferred(Ok((
                mett_core::FolderId::new("archive"),
                vec![("m0".into(), Ok("a0".into()))],
            ))),
        );
        assert!(!app.waiting());
    }

    #[test]
    fn sign_in_macros_play_in_the_window() {
        let mut app = app();
        app.keymap
            .apply(&crate::config::KeyCommand::Macro {
                menus: "signin".into(),
                key: "x".into(),
                sequence: "<copy-link>".into(),
                description: None,
            })
            .unwrap();
        app.push_keys("<last-entry>").unwrap();
        app.update(Msg::SignIn(browser_prompt()));
        press(&mut app, Key::char('x'));
        let Some(msg) = app.next_stroke() else {
            panic!("the macro waits");
        };
        assert_eq!(
            app.update(msg),
            [Effect::Copy("https://login.example/authorize".into())]
        );
        assert!(app.next_stroke().is_none(), "the pushed keys wait");
        // The same, while quitting: only the window's macro plays.
        press(&mut app, Key::char('x'));
        assert!(app.next_sign_in_stroke().is_some());
        assert!(app.next_sign_in_stroke().is_none());
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn back_tab_bindings_match_shift_tab() {
        let event = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(
            Key::from(event),
            crate::keymap::parse_strokes("<BackTab>")
                .unwrap()
                .into_iter()
                .find_map(|s| match s {
                    Stroke::Key(key) => Some(key),
                    Stroke::Function(_) => None,
                })
                .unwrap()
        );
    }

    #[test]
    fn ctrl_j_is_enter_unless_bound() {
        let mut first = app();
        press(&mut first, Key::ctrl('j'));
        assert!(
            matches!(first.screen(), Screen::Pager(_)),
            "opens the message"
        );
        let mut app = app();
        app.keymap
            .apply(&crate::config::KeyCommand::Bind {
                menus: "index".into(),
                key: "\\Cj".into(),
                function: "last-entry".into(),
            })
            .unwrap();
        press(&mut app, Key::ctrl('j'));
        assert_eq!(app.selected, 2);
        assert_eq!(app.screen(), &Screen::Index, "Enter is left alone");
        press(&mut app, Key::new(KeyCode::Enter));
        assert!(matches!(app.screen(), Screen::Pager(_)));
    }

    #[test]
    fn help_waits_for_another_folder_to_open() {
        let mut app = app();
        act(&mut app, Action::ChangeFolder);
        for c in "=Archive".chars() {
            press(&mut app, Key::char(c));
        }
        press(&mut app, Key::new(KeyCode::Enter));
        assert!(app.switching.is_some());
        act(&mut app, Action::Help);
        assert_eq!(app.screen(), &Screen::Index);
        assert_eq!(app.status, "Opening folder; wait a moment.");
    }

    #[test]
    fn help_waits_for_the_message() {
        let mut app = app();
        act(&mut app, Action::DisplayMessage);
        act(&mut app, Action::Help);
        assert!(matches!(app.screen(), Screen::Pager(_)));
        assert_eq!(app.status, "Loading message; wait a moment.");
    }

    #[test]
    fn a_taller_help_screen_scrolls_back() {
        let mut app = app();
        act(&mut app, Action::Help);
        act(&mut app, Action::LastEntry);
        let Screen::Help(help) = app.screen() else {
            panic!("not in help");
        };
        assert!(help.scroll > 0);
        app.update(Msg::Resize {
            width: 80,
            height: 500,
        });
        let Screen::Help(help) = app.screen() else {
            panic!("not in help");
        };
        assert_eq!(help.scroll, 0, "it all fits now");
    }

    #[test]
    fn typing_while_a_macro_waits_stops_it() {
        let mut app = app();
        app.push_keys("<reply><delete-message>").unwrap();
        let msg = app.next_stroke().expect("the reply plays");
        app.update(msg);
        assert!(app.waiting(), "for the message to reply to");
        // The user moves on meanwhile.
        app.typed();
        press(&mut app, Key::char('j'));
        assert!(app.next_stroke().is_none());
        assert!(!app.is_deleted(&mett_core::MessageId::new("m1")));
    }

    #[test]
    fn typing_between_macro_keys_that_wait_for_nothing_keeps_them() {
        let mut app = app();
        app.push_keys("<next-entry><next-entry>").unwrap();
        app.typed();
        play(&mut app);
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn help_waits_for_a_delivery() {
        let mut app = app();
        app.screens.push(Screen::Compose(crate::app::Compose {
            draft: mett_core::Draft::default(),
            delivery: Some((
                crate::worker::RequestId(99),
                crate::app::composing::Delivery::Send,
            )),
            unauthorized: None,
            forwarded_attachments: false,
            selected: 0,
            left_draft: false,
            kept_draft: false,
            kept: Vec::new(),
        }));
        act(&mut app, Action::Help);
        assert!(matches!(app.screen(), Screen::Compose(_)));
        assert_eq!(app.status, "Sending message; wait a moment.");
    }

    #[test]
    fn commands_change_the_mailbox_shortcuts() {
        let mut app = app();
        app.update(configured(Settings {
            shortcuts: crate::app::Shortcuts {
                mbox: "=@archive".into(),
                ..Default::default()
            },
            ..Settings::default()
        }));
        assert_eq!(app.shortcuts.mbox, "=@archive");
    }

    #[test]
    fn signing_in_keeps_a_waiting_macro() {
        let mut app = app();
        app.push_keys("<last-entry>").unwrap();
        app.update(Msg::SignIn(browser_prompt()));
        assert!(app.waiting());
        app.typed();
        assert!(!app.strokes.is_empty(), "the macro waits for the sign-in");
    }

    #[test]
    fn a_prompt_ignores_a_macro_s_functions() {
        for then in ["<help>", "<enter-command>"] {
            let mut app = app();
            app.push_keys(&format!("<save-message>{then}")).unwrap();
            play(&mut app);
            assert_eq!(app.screen(), &Screen::Index);
            assert!(matches!(
                app.input,
                Some(Input::Line {
                    question: Question::SaveTo,
                    ..
                })
            ));
            assert_eq!(app.status, "Not available in a prompt.");
        }
    }

    #[test]
    fn macros_wait_for_a_reply_s_recipients() {
        let mut app = app();
        let message = mock::message("m0", "in", "s", DateTime::UNIX_EPOCH);
        app.awaiting_addresses = Some((crate::compose::Kind::Reply, message));
        app.push_keys("<last-entry>").unwrap();
        assert!(app.waiting());
        assert!(app.next_stroke().is_none());
    }

    #[test]
    fn startup_keys_wait_for_a_sign_in_to_open_the_folder() {
        let (mut app, effects) = App::open(80, 10, inbox(), Settings::default());
        app.push_after_load(crate::keymap::parse_strokes("<last-entry>").unwrap());
        let open = first_request(&effects);
        respond(&mut app, open, Event::FolderOpened(Err(expired())));
        assert!(app.next_stroke().is_none(), "not before the folder is open");
        assert!(!app.after_load.is_empty(), "still waiting");
    }

    #[test]
    fn commands_keep_the_mailbox_s_addresses() {
        let mut app = app();
        respond(
            &mut app,
            crate::worker::RequestId(999),
            Event::OwnAddresses(Ok(vec!["Alias@Contoso.com".into()])),
        );
        assert!(app.settings.me.contains(&"alias@contoso.com".to_owned()));
        app.update(configured(Settings::default()));
        assert!(app.settings.me.contains(&"alias@contoso.com".to_owned()));
    }
}
