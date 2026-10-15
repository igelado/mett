//! Ordering the index (`sort`, `sort_aux`) and threads: collapsing them
//! and moving between them, as in mutt.

use std::collections::HashMap;

use mett_core::{MessageId, MessageSummary};

use super::{App, Effect, Screen};
use crate::action::Action;
use crate::sort::{self, Method, Sort};

/// What tells a thread apart, whichever message starts it: its
/// conversation (which all its messages share), or for a message on its
/// own, the message.
fn thread_key(message: &MessageSummary, root: &MessageId) -> String {
    match &message.thread {
        Some(thread) => format!("conversation {}", thread.conversation),
        None => format!("message {root}"),
    }
}

/// How the index is ordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ordering {
    /// `sort`.
    pub sort: Sort,
    /// `sort_aux`.
    pub aux: Sort,
    /// `ascii_chars`: draw threads with ASCII rather than lines.
    pub ascii: bool,
}

impl Default for Ordering {
    /// The variables' defaults: newest first.
    fn default() -> Self {
        Self {
            sort: Sort::parse("reverse-date", true).expect("valid"),
            aux: Sort::parse("date", false).expect("valid"),
            ascii: false,
        }
    }
}

/// Where a message is in its thread, as the index shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Threaded {
    /// The lines leading to it (`├─>`), empty for a thread's first
    /// message.
    pub tree: String,
    /// Whether its subject is its parent's, which isn't repeated.
    pub same_subject: bool,
    /// The thread's first message.
    pub root: MessageId,
    /// What tells the thread apart, whichever message starts it: its
    /// conversation, or its only message.
    pub key: String,
    /// For a collapsed thread's first message, how many messages are
    /// folded under it.
    pub folded: usize,
    /// For a collapsed thread's first message, whether unread mail is
    /// folded under it.
    pub folded_unread: bool,
}

impl App {
    /// Whether the index shows threads.
    pub fn threaded(&self) -> bool {
        self.ordering.sort.method == Method::Threads
    }

    /// Where a message is in its thread, if the index shows threads.
    pub fn thread_of(&self, id: &MessageId) -> Option<&Threaded> {
        self.threads.get(id)
    }

    /// Puts the index in order again, folding collapsed threads, and stays
    /// on the selected message (or the thread it was folded into).
    pub(super) fn arrange(&mut self) {
        let selected = self.messages.get(self.selected).map(|m| m.id.clone());
        let mut all = std::mem::take(&mut self.messages);
        all.append(&mut self.folded);
        let nodes = {
            let refs: Vec<&MessageSummary> = all.iter().collect();
            let positions = &self.positions;
            sort::arrange(
                &refs,
                self.ordering.sort,
                self.ordering.aux,
                &|id| positions.get(id).copied().unwrap_or(usize::MAX),
                self.ordering.ascii,
            )
        };
        let threaded = self.threaded();
        let ids: Vec<MessageId> = all.iter().map(|m| m.id.clone()).collect();
        let mut slots: Vec<Option<MessageSummary>> = all.into_iter().map(Some).collect();
        let mut folded_under: HashMap<MessageId, (usize, bool)> = HashMap::new();
        self.threads.clear();
        for node in nodes {
            let message = slots[node.index].take().expect("each message once");
            let root = ids[node.root].clone();
            let key = thread_key(&message, &root);
            let fold = threaded && node.depth > 0 && self.collapsed.contains(&key);
            if threaded {
                self.threads.insert(
                    message.id.clone(),
                    Threaded {
                        tree: node.tree,
                        same_subject: node.same_subject,
                        root: root.clone(),
                        key,
                        folded: 0,
                        folded_unread: false,
                    },
                );
            }
            if fold {
                let (count, unread) = folded_under.entry(root).or_default();
                *count += 1;
                *unread |= !message.flags.read;
                self.folded.push(message);
            } else {
                self.messages.push(message);
            }
        }
        for (root, (count, unread)) in folded_under {
            if let Some(threaded) = self.threads.get_mut(&root) {
                threaded.folded = count;
                threaded.folded_unread = unread;
            }
        }
        // (Collapsed threads hidden by a limit stay collapsed; the folder's
        // are forgotten when it is left or listed again.)
        let Some(selected) = selected else {
            self.selected = self.selected.min(self.messages.len().saturating_sub(1));
            return;
        };
        let shown = self
            .messages
            .iter()
            .position(|m| m.id == selected)
            .or_else(|| {
                let root = &self.threads.get(&selected)?.root;
                self.messages.iter().position(|m| &m.id == root)
            });
        if let Some(at) = shown {
            self.selected = at;
        }
    }

    /// Takes in a new order, if it changed.
    pub(super) fn apply_ordering(&mut self, ordering: Ordering) {
        if self.ordering != ordering {
            self.ordering = ordering;
            if !self.threaded() {
                self.collapsed.clear();
            }
            self.arrange();
        }
    }

    /// Opens the collapsed thread `message` is folded into, and selects it.
    pub(super) fn uncollapse_to(&mut self, message: &MessageId) {
        if let Some(thread) = self.threads.get(message) {
            let key = thread.key.clone();
            self.collapsed.remove(&key);
        }
        self.arrange();
        if let Some(at) = self.messages.iter().position(|m| &m.id == message) {
            self.selected = at;
        }
    }

    /// Where the thread after the selected message starts, if it is
    /// listed.
    pub(super) fn next_thread_root(&self) -> Option<usize> {
        (self.selected + 1..self.messages.len()).find(|&i| {
            let id = &self.messages[i].id;
            self.threads.get(id).is_none_or(|t| &t.root == id)
        })
    }

    /// Handles the thread actions of the index and the pager. Returns
    /// `None` for other actions.
    pub(super) fn on_thread_action(&mut self, action: Action) -> Option<Vec<Effect>> {
        let in_pager = matches!(self.screen(), Screen::Pager(_));
        match action {
            Action::NextThread | Action::PreviousThread => {}
            Action::CollapseThread | Action::CollapseAll if !in_pager => {}
            _ => return None,
        }
        if !self.threaded() {
            self.fail("Threading is not enabled.");
            return Some(Vec::new());
        }
        // In the pager, from the message it shows (the index may have been
        // listed again meanwhile).
        if let Some(Screen::Pager(pager)) = self.screens.last() {
            match self.messages.iter().position(|m| m.id == pager.id) {
                Some(at) => self.selected = at,
                None => {
                    self.fail("The folder is being listed again; wait a moment.");
                    return Some(Vec::new());
                }
            }
        }
        let selected = self.messages.get(self.selected).map(|m| m.id.clone());
        let Some(selected) = selected else {
            self.fail("No messages.");
            return Some(Vec::new());
        };
        let is_root = |app: &App, i: usize| {
            let id = &app.messages[i].id;
            app.threads.get(id).is_none_or(|t| &t.root == id)
        };
        // The start of the selected message's thread.
        let start = self
            .threads
            .get(&selected)
            .and_then(|t| self.messages.iter().position(|m| m.id == t.root))
            .unwrap_or(self.selected);
        match action {
            Action::NextThread => {
                match self.next_thread_root() {
                    Some(next) => self.selected = next,
                    // The next thread may be on a page still to come.
                    None if self.more_to_list() => {
                        return Some(self.wait_for(super::Target::NextThread));
                    }
                    None => {
                        self.fail("You are on the last thread.");
                        return Some(Vec::new());
                    }
                }
            }
            Action::PreviousThread => match (0..start).rev().find(|&i| is_root(self, i)) {
                Some(previous) => self.selected = previous,
                None => {
                    self.fail("You are on the first thread.");
                    return Some(Vec::new());
                }
            },
            Action::CollapseThread => {
                let root = &self.messages[start].id;
                let key = self
                    .threads
                    .get(root)
                    .map_or_else(|| format!("message {root}"), |t| t.key.clone());
                if !self.collapsed.remove(&key) {
                    self.collapsed.insert(key);
                }
                self.selected = start;
                self.arrange();
                return Some(Vec::new());
            }
            _ => {
                // Collapses every thread, or if they all are, opens them.
                let roots: Vec<String> = self
                    .threads
                    .iter()
                    .filter(|(id, t)| *id != &t.root)
                    .map(|(_, t)| t.key.clone())
                    .collect();
                let all_collapsed = roots.iter().all(|key| self.collapsed.contains(key));
                if all_collapsed {
                    self.collapsed.clear();
                } else {
                    self.collapsed.extend(roots);
                }
                self.selected = start;
                self.arrange();
                return Some(Vec::new());
            }
        }
        // In the pager, the message moved to is shown.
        if in_pager {
            self.screens.pop();
            return Some(self.open_selected());
        }
        Some(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta};
    use mett_core::{MessagePage, ThreadPosition, WellKnownFolder, mock};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::app::{Msg, Settings};
    use crate::config::mailbox::FolderPath;
    use crate::worker::{Event, Response};

    fn message(id: &str, minutes: i64, conversation: &str, path: &[u8]) -> MessageSummary {
        let mut m = mock::message(
            id,
            "in",
            &format!("Re: {conversation}"),
            DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes),
        )
        .summary;
        m.thread = Some(ThreadPosition {
            conversation: conversation.into(),
            path: path.iter().map(|step| vec![*step]).collect(),
        });
        m
    }

    /// An Inbox sorted by threads: a thread "a" (a0, with replies a1 and
    /// a2) and a thread "b" (b0 and b1), listed newest first.
    fn app() -> App {
        let settings = Settings {
            ordering: Ordering {
                sort: Sort::parse("threads", true).unwrap(),
                ..Ordering::default()
            },
            ..Settings::default()
        };
        let inbox = FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
        let (mut app, effects) = App::open(80, 24, inbox, settings);
        let Effect::Request(open, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: open,
            event: Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Effect::Request(list, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let messages = vec![
            message("b1", 5, "b", &[0, 1]),
            message("a2", 4, "a", &[0, 2]),
            message("b0", 3, "b", &[0]),
            message("a1", 2, "a", &[0, 1]),
            message("a0", 1, "a", &[0]),
        ];
        app.update(Msg::Response(Response {
            id: list,
            event: Event::MessagesListed(Ok(MessagePage {
                messages,
                next: None,
            })),
        }));
        app
    }

    fn shown(app: &App) -> Vec<String> {
        app.messages
            .iter()
            .map(|m| {
                let thread = app.thread_of(&m.id).unwrap();
                let folded = if thread.folded > 0 {
                    format!(" ({})", thread.folded)
                } else {
                    String::new()
                };
                format!("{}{}{folded}", thread.tree, m.id)
            })
            .collect()
    }

    fn act(app: &mut App, action: Action) -> Vec<Effect> {
        app.update(Msg::Action(action))
    }

    #[test]
    fn shows_threads_with_their_trees() {
        let app = app();
        assert_eq!(shown(&app), ["a0", "├─>a1", "└─>a2", "b0", "└─>b1"]);
        assert_eq!(app.selected, 0);
        assert!(app.thread_of(&MessageId::new("a1")).unwrap().same_subject);
    }

    #[test]
    fn moves_between_threads() {
        let mut app = app();
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::NextThread);
        assert_eq!(app.messages[app.selected].id.as_str(), "b0");
        act(&mut app, Action::NextThread);
        assert_eq!(app.status, "You are on the last thread.");
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::PreviousThread);
        assert_eq!(app.messages[app.selected].id.as_str(), "a0");
        act(&mut app, Action::PreviousThread);
        assert_eq!(app.status, "You are on the first thread.");
    }

    #[test]
    fn collapses_threads() {
        let mut app = app();
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::CollapseThread);
        assert_eq!(shown(&app), ["a0 (2)", "b0", "└─>b1"]);
        assert_eq!(app.selected, 0);
        assert_eq!(app.total_messages(), 5);
        // The key: Escape then v.
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::ALT,
        )));
        assert_eq!(shown(&app), ["a0", "├─>a1", "└─>a2", "b0", "└─>b1"]);
        act(&mut app, Action::CollapseAll);
        assert_eq!(shown(&app), ["a0 (2)", "b0 (1)"]);
        act(&mut app, Action::CollapseAll);
        assert_eq!(shown(&app).len(), 5);
    }

    #[test]
    fn a_search_opens_the_collapsed_thread_it_finds() {
        let mut app = app();
        let a2 = app
            .messages
            .iter_mut()
            .find(|m| m.id.as_str() == "a2")
            .unwrap();
        a2.subject = "Re: a, special".into();
        act(&mut app, Action::CollapseAll);
        // a2 is folded into a0's thread.
        app.messages
            .iter()
            .for_each(|m| assert_ne!(m.id.as_str(), "a2"));
        app.selected = 1;
        app.new_search("~s special");
        assert_eq!(app.messages[app.selected].id.as_str(), "a2");
        // When the start of the thread matches too, the thread opens, and
        // `n` goes on to its other matches.
        act(&mut app, Action::CollapseAll);
        app.selected = 1;
        app.new_search("~s a");
        assert_eq!(shown(&app)[0], "a0");
        assert!(shown(&app).len() > 2);
        assert_eq!(shown(&app), ["a0", "├─>a1", "└─>a2", "b0 (1)"]);
    }

    #[test]
    fn escape_then_v_collapses_too() {
        let mut app = app();
        app.update(Msg::Key(KeyEvent::from(KeyCode::Esc)));
        app.update(Msg::Key(KeyEvent::from(KeyCode::Char('v'))));
        assert_eq!(shown(&app)[0], "a0 (2)");
    }

    #[test]
    fn a_collapsed_thread_tells_of_its_unread_mail() {
        let mut app = app();
        for m in &mut app.messages {
            m.flags.read = m.id.as_str() != "a2";
        }
        act(&mut app, Action::CollapseThread);
        assert!(app.thread_of(&MessageId::new("a0")).unwrap().folded_unread);
        assert!(!app.thread_of(&MessageId::new("b0")).unwrap().folded_unread);
    }

    #[test]
    fn a_limit_works_on_collapsed_threads() {
        let mut app = app();
        act(&mut app, Action::CollapseAll);
        app.set_limit("~s b");
        assert_eq!(shown(&app), ["b0 (1)"]);
        app.set_limit("all");
        assert_eq!(shown(&app), ["a0 (2)", "b0 (1)"]);
    }

    /// An app sorted by threads whose first page is `first`, with more to
    /// come; returns the request for the next page.
    fn paged(first: Vec<MessageSummary>) -> (App, crate::worker::RequestId) {
        let settings = Settings {
            ordering: Ordering {
                sort: Sort::parse("threads", true).unwrap(),
                ..Ordering::default()
            },
            ..Settings::default()
        };
        let inbox = FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
        let (mut app, effects) = App::open(80, 24, inbox, settings);
        let Effect::Request(open, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: open,
            event: Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Effect::Request(list, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: list,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: first,
                next: Some(mett_core::PageCursor::new("in next")),
            })),
        }));
        let Effect::Request(next, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        (app, next)
    }

    fn page(app: &mut App, id: crate::worker::RequestId, messages: Vec<MessageSummary>) {
        app.update(Msg::Response(Response {
            id,
            event: Event::MessagesListed(Ok(MessagePage {
                messages,
                next: None,
            })),
        }));
    }

    #[test]
    fn a_collapsed_thread_stays_collapsed_when_its_start_is_listed() {
        let (mut app, next) = paged(vec![
            message("a2", 4, "a", &[0, 2]),
            message("a1", 2, "a", &[0, 1]),
        ]);
        act(&mut app, Action::CollapseThread);
        assert_eq!(shown(&app), ["a1 (1)"]);
        page(&mut app, next, vec![message("a0", 1, "a", &[0])]);
        assert_eq!(shown(&app), ["a0 (2)"]);
    }

    #[test]
    fn the_next_thread_may_be_on_a_page_to_come() {
        let (mut app, next) = paged(vec![
            message("a1", 4, "a", &[0, 1]),
            message("a0", 3, "a", &[0]),
        ]);
        act(&mut app, Action::NextThread);
        assert_ne!(app.status, "You are on the last thread.");
        // (A later page normally has older mail; this thread sorts after.)
        page(&mut app, next, vec![message("b0", 9, "b", &[0])]);
        assert_eq!(app.messages[app.selected].id.as_str(), "b0");
    }

    #[test]
    fn a_pending_next_entry_goes_after_its_message_whatever_comes_before() {
        let settings = Settings {
            ordering: Ordering {
                sort: Sort::parse("subject", true).unwrap(),
                ..Ordering::default()
            },
            ..Settings::default()
        };
        let inbox = FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
        let (mut app, effects) = App::open(80, 24, inbox, settings);
        let Effect::Request(open, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let effects = app.update(Msg::Response(Response {
            id: open,
            event: Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        }));
        let Effect::Request(list, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        let named = |id: &str| mock::message(id, "in", id, DateTime::UNIX_EPOCH).summary;
        let effects = app.update(Msg::Response(Response {
            id: list,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: vec![named("b")],
                next: Some(mett_core::PageCursor::new("in next")),
            })),
        }));
        let Effect::Request(next, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        act(&mut app, Action::NextEntry);
        page(&mut app, next, vec![named("a"), named("c")]);
        assert_eq!(app.messages[app.selected].id.as_str(), "c");
    }

    #[test]
    fn a_limit_keeps_a_visible_reply_selected() {
        let mut app = app();
        act(&mut app, Action::NextEntry);
        assert_eq!(app.messages[app.selected].id.as_str(), "a1");
        app.set_limit("~s a");
        assert_eq!(app.messages[app.selected].id.as_str(), "a1");
    }

    #[test]
    fn without_threads_thread_keys_say_so() {
        let mut app = app();
        app.apply_ordering(Ordering::default());
        assert_eq!(
            app.messages
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["b1", "a2", "b0", "a1", "a0"]
        );
        act(&mut app, Action::CollapseThread);
        assert_eq!(app.status, "Threading is not enabled.");
    }
}
