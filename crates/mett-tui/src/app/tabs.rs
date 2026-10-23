//! The accounts' tabs: what the main loop shows of the other accounts, and
//! the actions that go to them (`docs/MUTTRC.md` §4.14).
//!
//! Each account has an [`App`] of its own, with its own mailbox, folders and
//! sign-in. The main loop keeps them all up to date, shows the one that is
//! current, and gives it the [`Tabs`] to draw.

use super::{App, Effect};
use crate::action::Action;

/// Where the tab bar is drawn (`tab_position`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TabPosition {
    /// The top line of the screen.
    #[default]
    Top,
    /// The bottom line of the screen.
    Bottom,
}

impl std::str::FromStr for TabPosition {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "top" => Ok(Self::Top),
            "bottom" => Ok(Self::Bottom),
            other => Err(format!(
                "tab_position: expected top or bottom, got {other:?}"
            )),
        }
    }
}

/// An account's tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    /// What it says (`account LABEL FILE`).
    pub label: String,
    /// Whether any of the account's mailboxes has new mail
    /// ([`App::mailboxes_with_new_mail`]).
    pub new_mail: bool,
}

/// The tabs of every account, as the account shown draws them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tabs {
    /// The tabs, in the order of the `account` commands.
    /// Shared by every account's app, which compare them by identity first.
    pub tabs: std::sync::Arc<[Tab]>,
    /// The account shown.
    pub active: usize,
    /// Where they are drawn.
    pub position: TabPosition,
}

/// Which account to go to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    /// The one after the current, and the first after the last.
    Next,
    /// The one before the current, and the last before the first.
    Previous,
    /// This one, counting from 0.
    To(usize),
}

impl Switch {
    /// The account to show, of `count`, with `active` shown now. `None` if
    /// there is no such account.
    pub fn target(self, active: usize, count: usize) -> Option<usize> {
        match self {
            Self::Next => Some((active + 1) % count),
            Self::Previous => Some((active + count - 1) % count),
            Self::To(account) => (account < count).then_some(account),
        }
    }
}

impl App {
    /// Whether messages are marked deleted that nothing has purged yet: on
    /// quitting, the user is asked what to do with them.
    pub fn has_deleted(&self) -> bool {
        !self.index.deleted.is_empty()
    }

    /// Whether a message is being written (or is about to be: its recipients
    /// are being asked, or its original is being fetched): it isn't lost by
    /// quitting another account.
    pub fn is_composing(&self) -> bool {
        self.composing.is_some()
            || self.compose_fetch.is_some()
            || self.awaiting_addresses.is_some()
            || self
                .screens
                .iter()
                .any(|screen| matches!(screen, crate::app::Screen::Compose(_)))
    }

    /// The keys still to play (a macro, `push`), and how many have been
    /// played in a row, which go on in the account that is shown next when
    /// one is gone to: a macro that goes back and forth between accounts
    /// stops at the same limit as one that doesn't.
    pub fn take_strokes(&mut self) -> (std::collections::VecDeque<crate::keymap::Stroke>, usize) {
        (std::mem::take(&mut self.strokes), self.played)
    }

    /// Plays `strokes` first, counting on from `played` (see
    /// [`App::take_strokes`]).
    pub fn give_strokes(
        &mut self,
        (strokes, played): (std::collections::VecDeque<crate::keymap::Stroke>, usize),
    ) {
        for stroke in strokes.into_iter().rev() {
            self.strokes.push_front(stroke);
        }
        self.played = self.played.max(played);
    }

    /// Goes to another account, if there are any.
    pub(super) fn switch_account(&mut self, action: Action) -> Vec<Effect> {
        let Some(tabs) = &self.tabs else {
            self.fail("There is only one account.");
            return Vec::new();
        };
        let switch = match action {
            Action::NextAccount => Switch::Next,
            Action::PreviousAccount => Switch::Previous,
            Action::Account(number) => Switch::To(usize::from(number.saturating_sub(1))),
            _ => return Vec::new(),
        };
        match switch.target(tabs.active, tabs.tabs.len()) {
            Some(to) if to == tabs.active => Vec::new(),
            Some(_) => vec![Effect::Account(switch)],
            None => {
                let number = match switch {
                    Switch::To(account) => account + 1,
                    _ => 0,
                };
                self.fail(format!("There is no account {number}."));
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests;
