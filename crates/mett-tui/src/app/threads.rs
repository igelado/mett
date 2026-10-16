//! Ordering the index (`sort`, `sort_aux`) and threads: collapsing them
//! and moving between them, as in mutt.

use std::collections::HashSet;

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

    /// The first messages of the collapsed threads with unread mail folded
    /// under them, as their flags are now. Found for all of them at once,
    /// so that a page of collapsed threads looks through the folded mail
    /// only once.
    pub fn roots_with_folded_unread(&self) -> HashSet<&MessageId> {
        self.folded
            .iter()
            .filter(|m| !m.flags.read)
            .filter_map(|m| self.thread_of(&m.id))
            .map(|t| &t.root)
            .filter(|root| self.thread_of(root).is_some_and(|t| t.folded > 0))
            .collect()
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
        let arranged = {
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
        self.settled = arranged.settled;
        let mut slots: Vec<Option<MessageSummary>> = all.into_iter().map(Some).collect();
        self.threads.clear();
        // The thread being taken in: its first message, its key, whether
        // it is collapsed, and how many messages are folded under it. A
        // thread's messages come together, its first message first.
        let mut thread: Option<(MessageId, String, bool, usize)> = None;
        for node in arranged.nodes {
            let message = slots[node.index].take().expect("each message once");
            if !threaded {
                self.messages.push(message);
                continue;
            }
            if node.index == node.root {
                self.fold_count(thread.take());
                let root = message.id.clone();
                let key = thread_key(&message, &root);
                let collapsed = self.is_collapsed(&key);
                thread = Some((root, key, collapsed, 0));
            }
            let (root, key, collapsed, folded) = thread.as_mut().expect("the root comes first");
            self.threads.insert(
                message.id.clone(),
                Threaded {
                    tree: node.tree,
                    same_subject: node.same_subject,
                    root: root.clone(),
                    key: key.clone(),
                    folded: 0,
                },
            );
            if *collapsed && node.depth > 0 {
                *folded += 1;
                self.folded.push(message);
            } else {
                self.messages.push(message);
            }
        }
        self.fold_count(thread);
        // (Collapsed threads hidden by a limit stay collapsed; the folder's
        // are forgotten when it is left or listed again.)
        let Some(selected) = selected else {
            self.selected = self.selected.min(self.messages.len().saturating_sub(1));
            return;
        };
        if let Some(at) = self.entry_of(&selected) {
            self.selected = at;
        }
    }

    /// Says how many messages are folded under a thread [`Self::arrange`]
    /// took in, given as its first message, key, whether it is collapsed
    /// and that count.
    fn fold_count(&mut self, thread: Option<(MessageId, String, bool, usize)>) {
        if let Some((root, _, _, folded)) = thread
            && folded > 0
            && let Some(threaded) = self.threads.get_mut(&root)
        {
            threaded.folded = folded;
        }
    }

    /// Where the index shows `id`: its entry, or that of the collapsed
    /// thread it is folded into.
    pub(super) fn entry_of(&self, id: &MessageId) -> Option<usize> {
        let shown = |id: &MessageId| self.messages.iter().position(|m| &m.id == id);
        shown(id).or_else(|| shown(&self.thread_of(id)?.root))
    }

    /// Puts newly listed messages in the index. Without threads, they are
    /// sorted and merged in, rather than the whole index sorted again at
    /// every page. With threads, they go at the end until the threads are
    /// built again: once the index has doubled since they last were, and
    /// once the folder is listed (`last`), so that a large folder isn't
    /// threaded again at every page.
    pub(super) fn arrange_listed(&mut self, listed: Vec<MessageSummary>, last: bool) {
        if self.threaded() {
            self.messages.extend(listed);
            let count = self.shown_count();
            // Collapsed threads fold their replies as they come.
            let folding = self.collapse_all || !self.collapsed.is_empty();
            if last || folding || count >= self.threaded_count.saturating_mul(2) {
                self.arrange();
            }
            return;
        }
        let (sort, aux) = (self.ordering.sort, self.ordering.aux);
        let positions = &self.positions;
        let key = |m: &MessageSummary| {
            let at = positions.get(&m.id).copied().unwrap_or(usize::MAX);
            sort::Key::new(m, sort, aux, at)
        };
        let compare = |a: &sort::Key, b: &sort::Key| sort::compare(a, b, sort, aux);
        debug_assert!(self.in_order(), "the index is in order");
        let mut listed: Vec<(sort::Key, MessageSummary)> =
            listed.into_iter().map(|m| (key(&m), m)).collect();
        listed.sort_by(|(a, _), (b, _)| compare(a, b));
        // The messages that go before the first listed stay where they
        // are (with pages coming in the index's order, all of them): only
        // those after it are merged.
        let cut = match listed.first() {
            Some((first, _)) => self
                .messages
                .partition_point(|m| !compare(first, &key(m)).is_lt()),
            None => self.messages.len(),
        };
        let selected = self.selected;
        // A message listed twice may be shown twice: the selection goes to
        // the first, as when it is looked for.
        let twice = self.listed_twice
            || self
                .messages
                .get(selected)
                .is_some_and(|s| listed.iter().any(|(_, m)| m.id == s.id));
        let selected_id = self
            .messages
            .get(selected)
            .filter(|_| twice)
            .map(|m| m.id.clone());
        let tail = self.messages.split_off(cut);
        let mut tail = tail
            .into_iter()
            .enumerate()
            .map(|(i, m)| (cut + i, key(&m), m))
            .peekable();
        let mut listed = listed.into_iter().peekable();
        loop {
            let next = match (tail.peek(), listed.peek()) {
                (Some((_, a, _)), Some((b, _))) if compare(b, a).is_lt() => listed.next(),
                (Some(_), _) => tail.next().map(|(at, key, message)| {
                    if at == selected {
                        self.selected = self.messages.len();
                    }
                    (key, message)
                }),
                (None, _) => listed.next(),
            };
            match next {
                Some((_, message)) => self.messages.push(message),
                None => break,
            }
        }
        if let Some(at) = selected_id.and_then(|id| self.messages.iter().position(|m| m.id == id)) {
            self.selected = at;
        }
    }

    /// Whether the index is in order, as it always is without threads
    /// (with them, until messages listed since are threaded).
    pub(super) fn in_order(&self) -> bool {
        let (sort, aux) = (self.ordering.sort, self.ordering.aux);
        let key = |m: &MessageSummary| {
            let at = self.positions.get(&m.id).copied().unwrap_or(usize::MAX);
            sort::Key::new(m, sort, aux, at)
        };
        self.threaded()
            || self
                .messages
                .windows(2)
                .all(|pair| sort::compare(&key(&pair[0]), &key(&pair[1]), sort, aux).is_le())
    }

    /// Threads the messages listed so far, if some aren't yet.
    pub(super) fn thread_listed(&mut self) {
        if self.threaded() && self.shown_count() != self.threaded_count {
            self.arrange();
        }
    }

    /// Whether the thread `key` is collapsed.
    fn is_collapsed(&self, key: &str) -> bool {
        self.collapse_all != self.collapsed.contains(key)
    }

    /// Collapses or opens the thread `key`.
    fn set_collapsed(&mut self, key: String, collapsed: bool) {
        if collapsed == self.collapse_all {
            self.collapsed.remove(&key);
        } else {
            self.collapsed.insert(key);
        }
    }

    /// Takes in a new order, if it changed.
    pub(super) fn apply_ordering(&mut self, ordering: Ordering) {
        if self.ordering != ordering {
            self.ordering = ordering;
            if !self.threaded() {
                self.collapsed.clear();
                self.collapse_all = false;
            }
            self.arrange();
        }
    }

    /// Opens the collapsed thread `message` is folded into, and selects it.
    pub(super) fn uncollapse_to(&mut self, message: &MessageId) {
        let thread = self
            .threads
            .get(message)
            .map(|t| (t.key.clone(), t.root.clone()));
        let opened = thread.is_some_and(|(key, root)| {
            self.set_collapsed(key, false);
            let start = self.messages.iter().position(|m| m.id == root);
            start.is_some_and(|start| self.refold(start))
        });
        if !opened {
            self.arrange();
        }
        if let Some(at) = self.messages.iter().position(|m| &m.id == message) {
            self.selected = at;
        }
    }

    /// Folds the thread that starts at entry `start` into it, or unfolds
    /// it, as it is now collapsed or open: what [`Self::arrange`] would do,
    /// without ordering the whole index again, but for the selection,
    /// which the caller makes. Returns false, changing nothing, unless the
    /// index is threaded as arranging it again would thread it (see
    /// [`crate::sort::Arranged::settled`]), with nothing listed since.
    fn refold(&mut self, start: usize) -> bool {
        if !self.settled || self.shown_count() != self.threaded_count {
            return false;
        }
        let root = &self.messages[start].id;
        let Some(thread) = self.threads.get(root).filter(|t| &t.root == root) else {
            return false;
        };
        let collapsed = self.is_collapsed(&thread.key);
        // The messages folded into collapsed threads go in the threads'
        // order: those of the threads before this one come first. They are
        // counted on the shorter side.
        let folded = |m: &MessageSummary| self.threads.get(&m.id).map_or(0, |t| t.folded);
        let at = if start <= self.messages.len() / 2 {
            self.messages[..start].iter().map(folded).sum()
        } else {
            let after: usize = self.messages[start + 1..].iter().map(folded).sum();
            self.folded.len() - after - thread.folded
        };
        let moved = if collapsed {
            // Its replies follow it.
            let replies = self.messages[start + 1..]
                .iter()
                .take_while(|m| self.threads.get(&m.id).is_some_and(|t| &t.root == root))
                .count();
            let replies: Vec<MessageSummary> = self
                .messages
                .drain(start + 1..start + 1 + replies)
                .collect();
            let count = replies.len();
            self.folded.splice(at..at, replies);
            count
        } else {
            let replies: Vec<MessageSummary> = self.folded.drain(at..at + thread.folded).collect();
            let count = replies.len();
            self.messages.splice(start + 1..start + 1, replies);
            count
        };
        let root = self.messages[start].id.clone();
        if let Some(thread) = self.threads.get_mut(&root) {
            thread.folded = if collapsed { thread.folded + moved } else { 0 };
        }
        true
    }

    /// Whether the entry `at` starts a thread.
    fn is_root(&self, at: usize) -> bool {
        let id = &self.messages[at].id;
        self.threads.get(id).is_none_or(|t| &t.root == id)
    }

    /// Where the thread of `id`, the selected message, starts.
    fn thread_start(&self, id: &MessageId) -> usize {
        self.threads
            .get(id)
            .and_then(|t| self.messages.iter().position(|m| m.id == t.root))
            .unwrap_or(self.selected)
    }

    /// Where the thread after the selected message (if it is listed), or
    /// the one before the selected message's (`next` false), starts; or
    /// why there is none.
    pub(super) fn thread_root_toward(&self, next: bool) -> Result<usize, &'static str> {
        if next {
            (self.selected + 1..self.messages.len())
                .find(|&i| self.is_root(i))
                .ok_or("You are on the last thread.")
        } else {
            self.messages
                .get(self.selected)
                .and_then(|m| {
                    (0..self.thread_start(&m.id))
                        .rev()
                        .find(|&i| self.is_root(i))
                })
                .ok_or("You are on the first thread.")
        }
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
        // Folded into a collapsed thread since, from the thread.
        if let Some(Screen::Pager(pager)) = self.screens.last() {
            match self.entry_of(&pager.id) {
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
        // The start of the selected message's thread.
        let start = self.thread_start(&selected);
        match action {
            Action::NextThread | Action::PreviousThread => {
                let next = action == Action::NextThread;
                // While pages come, a thread's start (and with it, where
                // the thread goes) may still be on its way, and an older
                // page may bring a thread that goes before it: wait for
                // them.
                if self.more_to_list() {
                    return Some(self.wait_for(if next {
                        super::Target::NextThread
                    } else {
                        super::Target::PreviousThread
                    }));
                }
                match self.thread_root_toward(next) {
                    Ok(at) => self.selected = at,
                    Err(reason) => {
                        self.fail(reason);
                        return Some(Vec::new());
                    }
                }
            }
            Action::CollapseThread => {
                let root = &self.messages[start].id;
                let key = self
                    .threads
                    .get(root)
                    .map_or_else(|| format!("message {root}"), |t| t.key.clone());
                let collapsed = self.is_collapsed(&key);
                self.set_collapsed(key, !collapsed);
                self.selected = start;
                if !self.refold(start) {
                    self.arrange();
                }
                return Some(Vec::new());
            }
            _ => {
                // Collapses every thread, or if they all are, opens them.
                let mut replies = self
                    .threads
                    .iter()
                    .filter(|(id, t)| *id != &t.root)
                    .peekable();
                // Those listed later too: every thread is, until opened.
                // (With no replies listed yet, by what was asked last.)
                let all_collapsed = if replies.peek().is_none() {
                    self.collapse_all
                } else {
                    replies.all(|(_, t)| self.is_collapsed(&t.key))
                };
                self.collapsed.clear();
                self.collapse_all = !all_collapsed;
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
    use mett_core::{ThreadPosition, mock};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::app::testing::*;
    use crate::app::{Msg, Settings};

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
        let messages = vec![
            message("b1", 5, "b", &[0, 1]),
            message("a2", 4, "a", &[0, 2]),
            message("b0", 3, "b", &[0]),
            message("a1", 2, "a", &[0, 1]),
            message("a0", 1, "a", &[0]),
        ];
        opened(sorted("threads"), 24, messages, None).0
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
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Char('v'));
        assert_eq!(shown(&app)[0], "a0 (2)");
    }

    #[test]
    fn an_escape_long_ago_leaves_the_next_key_alone() {
        let mut app = app();
        key(&mut app, KeyCode::Esc);
        if let Some((_, when)) = &mut app.escape
            && let Some(earlier) = when.checked_sub(std::time::Duration::from_secs(5))
        {
            *when = earlier;
        }
        key(&mut app, KeyCode::Char('v'));
        // `v`, not `Esc v`: the thread isn't collapsed.
        assert_eq!(shown(&app)[0], "a0");
    }

    #[test]
    fn escape_then_control_n_is_control_n() {
        let mut app = app();
        key(&mut app, KeyCode::Esc);
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
        )));
        // The next thread, not Alt-n.
        assert_eq!(app.messages[app.selected].id.as_str(), "b0");
    }

    #[test]
    fn an_escape_is_forgotten_when_another_menu_takes_over() {
        let mut app = app();
        key(&mut app, KeyCode::Esc);
        // The help screen opens before the next key (as the sign-in window
        // may): its `q` still leaves it.
        act(&mut app, Action::Help);
        key(&mut app, KeyCode::Char('q'));
        assert_eq!(app.screen(), &Screen::Index);
    }

    #[test]
    fn a_collapsed_thread_stays_selected_when_its_first_message_goes() {
        let mut app = app();
        act(&mut app, Action::CollapseThread);
        assert_eq!(shown(&app)[0], "a0 (2)");
        app.remove_messages(&std::collections::HashSet::from([MessageId::new("a0")]));
        assert_eq!(app.messages[app.selected].id.as_str(), "a1");
    }

    #[test]
    fn a_pending_page_down_counts_from_where_it_started() {
        let (mut app, list) = opening(sorted("subject"), 5);
        let named = |id: &str| mock::message(id, "in", id, DateTime::UNIX_EPOCH).summary;
        let next = page(&mut app, list, vec![named("a"), named("z")], "in next");
        // Two rows a page, from a.
        act(&mut app, Action::NextPage);
        listed(&mut app, next, vec![named("b"), named("zz")], None);
        assert_eq!(app.messages[app.selected].id.as_str(), "z");
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
        let (mut app, list) = opening(settings, 24);
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
        let next = page(&mut app, list, vec![from("c", 5)], "in next");
        act(&mut app, Action::NextEntry);
        // e may not come right after c: d, from the same time, may follow.
        let next = page(&mut app, next, vec![from("e", 3)], "in next");
        assert_eq!(app.messages[app.selected].id.as_str(), "c");
        listed(&mut app, next, vec![from("d", 3)], None);
        assert_eq!(app.messages[app.selected].id.as_str(), "d");
    }

    #[test]
    fn a_pending_move_waits_among_drafts() {
        let (mut app, list) = opening(sorted("reverse-date"), 24);
        let at = |id: &str, minutes: i64, draft: bool| {
            let mut m = mock::message(
                id,
                "in",
                id,
                DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes),
            )
            .summary;
            m.draft = draft;
            m
        };
        // Drafts first, then the rest, each by its own date.
        let next = page(&mut app, list, vec![at("d9", 9, true)], "in next");
        act(&mut app, Action::NextEntry);
        let next = page(&mut app, next, vec![at("d2", 2, true)], "in next");
        assert_eq!(app.messages[app.selected].id.as_str(), "d9");
        listed(&mut app, next, vec![at("s5", 5, false)], None);
        assert_eq!(app.messages[app.selected].id.as_str(), "s5");
    }

    #[test]
    fn a_collapsed_thread_tells_of_its_unread_mail() {
        let mut app = app();
        for m in &mut app.messages {
            m.flags.read = m.id.as_str() != "a2";
        }
        act(&mut app, Action::CollapseThread);
        let a0 = MessageId::new("a0");
        assert_eq!(app.roots_with_folded_unread(), HashSet::from([&a0]));
        // As the flags are now.
        let a2 = app
            .folded
            .iter_mut()
            .find(|m| m.id.as_str() == "a2")
            .unwrap();
        a2.flags.read = true;
        assert!(app.roots_with_folded_unread().is_empty());
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
        let (app, next) = opened(sorted("threads"), 24, first, Some("in next"));
        (app, next.unwrap())
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
        listed(
            &mut app,
            next,
            vec![
                message("x5", 5, "x", &[0, 2]),
                message("x1", 1, "x", &[0, 3]),
                message("x3", 3, "x", &[0, 4]),
            ],
            None,
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
        let more = page(&mut app, next, vec![message("b0", 3, "b", &[0])], "in more");
        // Three of the two threaded so far: listed, not threaded yet…
        assert!(app.thread_of(&MessageId::new("b0")).is_none());
        // …until a thread action needs them.
        act(&mut app, Action::NextThread);
        assert!(app.thread_of(&MessageId::new("b0")).is_some());
        listed(&mut app, more, vec![message("a0", 1, "a", &[0])], None);
        assert_eq!(shown(&app), ["a0", "├─>a1", "└─>a2", "b0"]);
    }

    #[test]
    fn replies_listed_later_fold_into_their_collapsed_thread() {
        let (mut app, next) = paged(vec![
            message("a2", 4, "a", &[0, 2]),
            message("a1", 3, "a", &[0, 1]),
            message("a0", 2, "a", &[0]),
            message("b0", 1, "b", &[0]),
        ]);
        act(&mut app, Action::CollapseAll);
        // One more message: not enough to thread the index again.
        let effects = listed(
            &mut app,
            next,
            vec![message("b1", 0, "b", &[0, 1])],
            Some("in more"),
        );
        assert!(!effects.is_empty());
        assert!(!app.messages.iter().any(|m| m.id.as_str() == "b1"));
    }

    #[test]
    fn collapsing_all_takes_in_threads_listed_later() {
        let (mut app, next) = paged(vec![message("a1", 2, "a", &[0])]);
        act(&mut app, Action::CollapseAll);
        listed(
            &mut app,
            next,
            vec![message("a2", 1, "a", &[0, 1]), message("b0", 0, "b", &[0])],
            None,
        );
        assert_eq!(shown(&app), ["b0", "a1 (1)"]);
        // And all open again.
        act(&mut app, Action::CollapseAll);
        assert_eq!(shown(&app).len(), 3);
    }

    #[test]
    fn a_collapsed_thread_stays_collapsed_when_its_start_is_listed() {
        let (mut app, next) = paged(vec![
            message("a2", 4, "a", &[0, 2]),
            message("a1", 2, "a", &[0, 1]),
        ]);
        act(&mut app, Action::CollapseThread);
        assert_eq!(shown(&app), ["a1 (1)"]);
        listed(&mut app, next, vec![message("a0", 1, "a", &[0])], None);
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
        listed(&mut app, next, vec![message("b0", 9, "b", &[0])], None);
        assert_eq!(app.messages[app.selected].id.as_str(), "b0");
    }

    #[test]
    fn a_pending_next_entry_goes_after_its_message_whatever_comes_before() {
        let (mut app, list) = opening(sorted("subject"), 24);
        let named = |id: &str| mock::message(id, "in", id, DateTime::UNIX_EPOCH).summary;
        let next = page(&mut app, list, vec![named("b")], "in next");
        act(&mut app, Action::NextEntry);
        // d comes first, but the last page may hold what goes between.
        let more = page(&mut app, next, vec![named("d")], "in more");
        assert_eq!(app.messages[app.selected].id.as_str(), "b");
        listed(&mut app, more, vec![named("a"), named("c")], None);
        assert_eq!(app.messages[app.selected].id.as_str(), "c");
    }

    #[test]
    fn a_pending_next_entry_survives_its_message_going() {
        let (mut app, list) = opening(sorted("subject"), 24);
        let named = |id: &str| mock::message(id, "in", id, DateTime::UNIX_EPOCH).summary;
        let next = page(&mut app, list, vec![named("a"), named("b")], "in next");
        act(&mut app, Action::LastEntry);
        act(&mut app, Action::NextEntry);
        // b goes (moved elsewhere, say) before the next page comes.
        app.remove_messages(&std::collections::HashSet::from([MessageId::new("b")]));
        listed(&mut app, next, vec![named("c"), named("d")], None);
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

    /// What the index shows and the folded messages, how each is
    /// threaded, and the entry selected.
    type State = (
        Vec<MessageId>,
        Vec<MessageId>,
        Vec<(MessageId, Threaded)>,
        usize,
    );

    fn state(app: &App) -> State {
        let ids = |list: &[MessageSummary]| list.iter().map(|m| m.id.clone()).collect();
        let mut threads: Vec<_> = app.threads.clone().into_iter().collect();
        threads.sort_by(|a, b| a.0.cmp(&b.0));
        (ids(&app.messages), ids(&app.folded), threads, app.selected)
    }

    #[test]
    fn collapsing_and_opening_one_thread_is_arranging_the_index_again() {
        // A small random generator, so that the test is the same each run.
        let mut seed = 7_u64;
        let mut below = |n: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize % n.max(1)
        };
        for round in 0..60 {
            // Threads of up to six messages, a few deep; listed newest
            // first, in pages.
            let mut messages = Vec::new();
            for t in 0..1 + below(8) {
                let size = 1 + below(6);
                for k in 0..size {
                    let path: Vec<u8> = match k {
                        0 => vec![0],
                        1..=3 => vec![0, k as u8],
                        _ => vec![0, 1, k as u8],
                    };
                    let minutes = (below(50) * 10 + messages.len()) as i64;
                    messages.push(message(
                        &format!("t{t}m{k}"),
                        minutes,
                        &format!("c{t}"),
                        &path,
                    ));
                }
            }
            messages.sort_by_key(|m| std::cmp::Reverse(m.received));
            let mut pages: Vec<Vec<MessageSummary>> = Vec::new();
            while !messages.is_empty() {
                let size = (1 + below(5)).min(messages.len());
                pages.push(messages.drain(..size).collect());
            }
            let last = pages.len() == 1;
            let (mut app, mut next) = opened(
                sorted(["threads", "reverse-threads", "last-threads"][below(3)]),
                24,
                pages.remove(0),
                (!last).then_some("in next"),
            );
            for step in 0..30 {
                match below(5) {
                    0 | 1 if !app.messages.is_empty() => {
                        app.selected = below(app.messages.len());
                        act(&mut app, Action::CollapseThread);
                    }
                    2 if !app.folded.is_empty() => {
                        let id = app.folded[below(app.folded.len())].id.clone();
                        app.uncollapse_to(&id);
                    }
                    3 if below(4) == 0 => {
                        act(&mut app, Action::CollapseAll);
                    }
                    _ => {
                        if let Some(id) = next.take()
                            && !pages.is_empty()
                        {
                            let page = pages.remove(0);
                            let more = (!pages.is_empty()).then_some("in next");
                            let effects = listed(&mut app, id, page, more);
                            next = more.map(|_| first_request(&effects));
                        }
                    }
                }
                // Threaded, it is as arranging it all again makes it.
                if app.shown_count() == app.threaded_count {
                    assert!(app.settled);
                    let before = state(&app);
                    app.arrange();
                    assert_eq!(state(&app), before, "round {round}, step {step}");
                }
            }
        }
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
