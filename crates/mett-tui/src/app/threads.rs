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
}

impl App {
    /// Whether the index shows threads.
    pub fn threaded(&self) -> bool {
        self.ordering.sort.method == Method::Threads
    }

    /// Whether unread mail is folded under `root` (into its collapsed
    /// thread), as its flags are now.
    pub fn folded_unread(&self, root: &MessageId) -> bool {
        self.thread_of(root).is_some_and(|t| t.folded > 0)
            && self
                .folded
                .iter()
                .any(|m| !m.flags.read && self.thread_of(&m.id).is_some_and(|t| &t.root == root))
    }

    /// How many messages the index shows, folded ones too.
    pub fn shown_count(&self) -> usize {
        self.messages.len() + self.folded.len()
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
        self.threaded_count = all.len();
        let ids: Vec<MessageId> = all.iter().map(|m| m.id.clone()).collect();
        let mut slots: Vec<Option<MessageSummary>> = all.into_iter().map(Some).collect();
        let mut folded_under: HashMap<MessageId, usize> = HashMap::new();
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
                    },
                );
            }
            if fold {
                *folded_under.entry(root).or_default() += 1;
                self.folded.push(message);
            } else {
                self.messages.push(message);
            }
        }
        for (root, count) in folded_under {
            if let Some(threaded) = self.threads.get_mut(&root) {
                threaded.folded = count;
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

    /// Puts newly listed messages in the index. Without threads, they are
    /// sorted and merged in, rather than the whole index sorted again at
    /// every page. With threads, they go at the end until the threads are
    /// built again: once the index has doubled since they last were, and
    /// once the folder is listed (`last`), so that a large folder isn't
    /// threaded again at every page.
    pub(super) fn arrange_listed(&mut self, mut listed: Vec<MessageSummary>, last: bool) {
        if self.threaded() {
            self.messages.extend(listed);
            let count = self.messages.len() + self.folded.len();
            if last || count >= self.threaded_count.saturating_mul(2) {
                self.arrange();
            }
            return;
        }
        let (sort, aux) = (self.ordering.sort, self.ordering.aux);
        let positions = &self.positions;
        let position = |id: &MessageId| positions.get(id).copied().unwrap_or(usize::MAX);
        let compare =
            |a: &MessageSummary, b: &MessageSummary| sort::compare(a, b, sort, aux, &position);
        listed.sort_by(compare);
        let selected = self.messages.get(self.selected).map(|m| m.id.clone());
        let old = std::mem::take(&mut self.messages);
        let mut merged = Vec::with_capacity(old.len() + listed.len());
        let (mut old, mut listed) = (old.into_iter().peekable(), listed.into_iter().peekable());
        loop {
            let next = match (old.peek(), listed.peek()) {
                (Some(a), Some(b)) if compare(b, a).is_lt() => listed.next(),
                (Some(_), _) => old.next(),
                (None, _) => listed.next(),
            };
            match next {
                Some(message) => merged.push(message),
                None => break,
            }
        }
        self.messages = merged;
        if let Some(at) = selected.and_then(|id| self.messages.iter().position(|m| m.id == id)) {
            self.selected = at;
        }
    }

    /// Threads the messages listed so far, if some aren't yet.
    pub(super) fn thread_listed(&mut self) {
        if self.threaded() && self.messages.len() + self.folded.len() != self.threaded_count {
            self.arrange();
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
        // Messages listed since the threads were last built are threaded
        // first.
        self.thread_listed();
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
    fn an_escape_is_forgotten_when_another_menu_takes_over() {
        let mut app = app();
        app.update(Msg::Key(KeyEvent::from(KeyCode::Esc)));
        // The help screen opens before the next key (as the sign-in window
        // may): its `q` still leaves it.
        act(&mut app, Action::Help);
        app.update(Msg::Key(KeyEvent::from(KeyCode::Char('q'))));
        assert_eq!(app.screen(), &Screen::Index);
    }

    #[test]
    fn a_pending_move_waits_for_mail_from_the_same_time() {
        let settings = Settings {
            ordering: Ordering {
                sort: Sort::parse("reverse-date", true).unwrap(),
                aux: Sort::parse("from", false).unwrap(),
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
        let from = |id: &str, minutes: i64| {
            let mut m = mock::message(
                id,
                "in",
                id,
                DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes),
            )
            .summary;
            m.from = Some(mett_core::Address::new(format!("{id}@x.org")));
            m
        };
        let listed = |app: &mut App, id, messages, more: bool| {
            let effects = app.update(Msg::Response(Response {
                id,
                event: Event::MessagesListed(Ok(MessagePage {
                    messages,
                    next: more.then(|| mett_core::PageCursor::new("in next")),
                })),
            }));
            match effects.first() {
                Some(Effect::Request(next, _)) => Some(*next),
                _ => None,
            }
        };
        let next = listed(&mut app, list, vec![from("c", 5)], true).unwrap();
        act(&mut app, Action::NextEntry);
        // e may not come right after c: d, from the same time, may follow.
        let next = listed(&mut app, next, vec![from("e", 3)], true).unwrap();
        assert_eq!(app.messages[app.selected].id.as_str(), "c");
        listed(&mut app, next, vec![from("d", 3)], false);
        assert_eq!(app.messages[app.selected].id.as_str(), "d");
    }

    #[test]
    fn a_collapsed_thread_tells_of_its_unread_mail() {
        let mut app = app();
        for m in &mut app.messages {
            m.flags.read = m.id.as_str() != "a2";
        }
        act(&mut app, Action::CollapseThread);
        assert!(app.folded_unread(&MessageId::new("a0")));
        assert!(!app.folded_unread(&MessageId::new("b0")));
        // As the flags are now.
        let a2 = app
            .folded
            .iter_mut()
            .find(|m| m.id.as_str() == "a2")
            .unwrap();
        a2.flags.read = true;
        assert!(!app.folded_unread(&MessageId::new("a0")));
    }

    #[test]
    fn a_search_looks_under_the_selected_collapsed_thread_first() {
        let mut app = app();
        let a1 = app
            .messages
            .iter_mut()
            .find(|m| m.id.as_str() == "a1")
            .unwrap();
        a1.subject = "Re: a, found".into();
        let b1 = app
            .messages
            .iter_mut()
            .find(|m| m.id.as_str() == "b1")
            .unwrap();
        b1.subject = "Re: b, found".into();
        act(&mut app, Action::CollapseAll);
        app.selected = 0;
        app.new_search("~s found");
        assert_eq!(app.messages[app.selected].id.as_str(), "a1");
    }

    #[test]
    fn a_limit_works_on_collapsed_threads() {
        let mut app = app();
        act(&mut app, Action::CollapseAll);
        app.set_limit("~s b");
        assert_eq!(shown(&app), ["b0 (1)"]);
        // The limit shows both, one folded.
        assert_eq!(app.shown_count(), 2);
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
    fn pages_are_merged_into_a_sorted_index() {
        let (mut app, next) = paged(vec![
            message("x4", 4, "x", &[0]),
            message("x2", 2, "x", &[0, 1]),
        ]);
        app.apply_ordering(Ordering {
            sort: Sort::parse("date", true).unwrap(),
            ..Ordering::default()
        });
        app.selected = 1;
        page(
            &mut app,
            next,
            vec![
                message("x5", 5, "x", &[0, 2]),
                message("x1", 1, "x", &[0, 3]),
                message("x3", 3, "x", &[0, 4]),
            ],
        );
        let ids: Vec<&str> = app.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["x1", "x2", "x3", "x4", "x5"]);
        // Still on x4.
        assert_eq!(app.selected, 3);
    }

    #[test]
    fn threads_are_built_as_the_index_doubles_and_once_it_is_listed() {
        let (mut app, next) = paged(vec![
            message("a2", 4, "a", &[0, 2]),
            message("a1", 2, "a", &[0, 1]),
        ]);
        let effects = app.update(Msg::Response(Response {
            id: next,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: vec![message("b0", 3, "b", &[0])],
                next: Some(mett_core::PageCursor::new("in more")),
            })),
        }));
        // Three of the two threaded so far: listed, not threaded yet…
        assert!(app.thread_of(&MessageId::new("b0")).is_none());
        // …until a thread action needs them.
        act(&mut app, Action::NextThread);
        assert!(app.thread_of(&MessageId::new("b0")).is_some());
        let Effect::Request(more, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        page(&mut app, more, vec![message("a0", 1, "a", &[0])]);
        assert_eq!(shown(&app), ["a0", "├─>a1", "└─>a2", "b0"]);
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
        // d comes first, but the last page may hold what goes between.
        let effects = app.update(Msg::Response(Response {
            id: next,
            event: Event::MessagesListed(Ok(MessagePage {
                messages: vec![named("d")],
                next: Some(mett_core::PageCursor::new("in more")),
            })),
        }));
        assert_eq!(app.messages[app.selected].id.as_str(), "b");
        let Effect::Request(more, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        page(&mut app, more, vec![named("a"), named("c")]);
        assert_eq!(app.messages[app.selected].id.as_str(), "c");
    }

    #[test]
    fn a_pending_next_entry_survives_its_message_going() {
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
                messages: vec![named("a"), named("b")],
                next: Some(mett_core::PageCursor::new("in next")),
            })),
        }));
        let Effect::Request(next, _) = effects[0].clone() else {
            panic!("expected a request, got {effects:?}");
        };
        act(&mut app, Action::LastEntry);
        act(&mut app, Action::NextEntry);
        // b goes (moved elsewhere, say) before the next page comes.
        app.remove_messages(&std::collections::HashSet::from([MessageId::new("b")]));
        page(&mut app, next, vec![named("c"), named("d")]);
        assert_eq!(app.messages[app.selected].id.as_str(), "c");
    }

    #[test]
    fn a_pending_move_from_a_folded_message_goes_after_its_thread() {
        let mut app = app();
        act(&mut app, Action::CollapseAll);
        app.pending = Some(super::super::Target::After(MessageId::new("a1"), 1));
        app.apply_pending();
        assert_eq!(app.messages[app.selected].id.as_str(), "b0");
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
