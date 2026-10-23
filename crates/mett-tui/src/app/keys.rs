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
                    if let Some((loaded, speller)) = &self.speller
                        && loaded.words == self.settings.spell.words
                    {
                        speller.add(&word);
                    }
                    let file = self.settings.spell.words.clone();
                    self.added_words.push((file, word.clone()));
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
                self.status
                    .info(format!("Not waiting for {} any more.", path.display()));
                self.file_wait = None;
            } else {
                self.status
                    .info(format!("Waiting for {} (^G gives up)...", path.display()));
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
        self.status.info(notice.clone());
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
            || self.index.listing.is_some()
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
            && !self.status.is_error()
        {
            self.status.info(notice);
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
            let said = self.after_configured.take().unwrap_or_default();
            self.status.info(said);
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
mod tests;
