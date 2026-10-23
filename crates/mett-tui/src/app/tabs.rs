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
    pub tabs: Vec<Tab>,
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
        !self.deleted.is_empty()
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
            Action::Account1 => Switch::To(0),
            Action::Account2 => Switch::To(1),
            Action::Account3 => Switch::To(2),
            Action::Account4 => Switch::To(3),
            Action::Account5 => Switch::To(4),
            Action::Account6 => Switch::To(5),
            Action::Account7 => Switch::To(6),
            Action::Account8 => Switch::To(7),
            Action::Account9 => Switch::To(8),
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
mod tests {
    use super::*;
    use crate::app::Settings;
    use crate::app::testing::{act, opened};

    fn app() -> App {
        opened(Settings::default(), 24, Vec::new(), None).0
    }

    fn tabs(active: usize, count: usize) -> Tabs {
        Tabs {
            tabs: (0..count)
                .map(|n| Tab {
                    label: format!("Account {n}"),
                    new_mail: false,
                })
                .collect(),
            active,
            position: TabPosition::Top,
        }
    }

    #[test]
    fn next_and_previous_go_round() {
        assert_eq!(Switch::Next.target(0, 3), Some(1));
        assert_eq!(Switch::Next.target(2, 3), Some(0));
        assert_eq!(Switch::Previous.target(0, 3), Some(2));
        assert_eq!(Switch::Previous.target(2, 3), Some(1));
        assert_eq!(Switch::To(1).target(0, 3), Some(1));
        assert_eq!(Switch::To(3).target(0, 3), None);
    }

    #[test]
    fn tab_position_is_top_or_bottom() {
        assert_eq!("top".parse(), Ok(TabPosition::Top));
        assert_eq!("bottom".parse(), Ok(TabPosition::Bottom));
        assert!(
            "left"
                .parse::<TabPosition>()
                .unwrap_err()
                .contains("top or bottom")
        );
    }

    #[test]
    fn the_account_actions_ask_the_main_loop_to_switch() {
        let mut app = app();
        app.tabs = Some(tabs(0, 3));
        assert_eq!(
            act(&mut app, Action::NextAccount),
            [Effect::Account(Switch::Next)]
        );
        assert_eq!(
            act(&mut app, Action::PreviousAccount),
            [Effect::Account(Switch::Previous)]
        );
        assert_eq!(
            act(&mut app, Action::Account3),
            [Effect::Account(Switch::To(2))]
        );
        // The one shown already: nothing to do.
        assert!(act(&mut app, Action::Account1).is_empty());
        // One there isn't.
        assert!(act(&mut app, Action::Account5).is_empty());
        assert_eq!(app.status, "There is no account 5.");
    }

    #[test]
    fn keys_still_to_play_can_be_handed_over() {
        let mut from = app();
        from.strokes.extend([
            crate::keymap::Stroke::Key(crate::keymap::Key::char('j')),
            crate::keymap::Stroke::Key(crate::keymap::Key::char('k')),
        ]);
        let mut to = app();
        from.played = 7;
        let strokes = from.take_strokes();
        assert_eq!(strokes.0.len(), 2);
        assert!(from.take_strokes().0.is_empty());
        to.give_strokes(strokes);
        // The count goes on, so that a macro going back and forth stops.
        let (queued, played) = to.take_strokes();
        assert_eq!(queued.len(), 2);
        assert_eq!(played, 7);
    }

    #[test]
    fn with_one_account_there_is_nothing_to_go_to() {
        let mut app = app();
        assert!(act(&mut app, Action::NextAccount).is_empty());
        assert_eq!(app.status, "There is only one account.");
    }
}
