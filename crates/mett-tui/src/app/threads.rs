//! Ordering the index (`sort`, `sort_aux`) and threads: collapsing them
//! and moving between them, as in mutt.

use std::collections::HashSet;
use std::sync::Arc;

use mett_core::{MessageId, MessageSummary};

use super::{App, Effect, Screen};
use crate::action::Action;
use crate::sort::{self, Method, Sort};

/// What tells a thread apart, whichever message starts it: its
/// conversation (which all its messages share), or for a message on its
/// own, the message.
fn thread_key(message: &MessageSummary, root: &MessageId) -> Arc<str> {
    match &message.thread {
        Some(thread) => format!("conversation {}", thread.conversation).into(),
        None => format!("message {root}").into(),
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
    pub key: Arc<str>,
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
        // The threads (by their first message) with unread mail, which
        // collapsing every thread leaves open.
        let unread: HashSet<usize> = if threaded {
            arranged
                .nodes
                .iter()
                .filter(|node| !all[node.index].flags.read)
                .map(|node| node.root)
                .collect()
        } else {
            HashSet::new()
        };
        self.threaded_count = all.len();
        self.settled = arranged.settled;
        let mut slots: Vec<Option<MessageSummary>> = all.into_iter().map(Some).collect();
        self.threads.clear();
        // The thread being taken in: its first message, its key, whether
        // it is collapsed, and how many messages are folded under it. A
        // thread's messages come together, its first message first.
        let mut thread: Option<(MessageId, Arc<str>, bool, usize)> = None;
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
                let collapsed = self.is_collapsed(&key, unread.contains(&node.root));
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
    fn fold_count(&mut self, thread: Option<(MessageId, Arc<str>, bool, usize)>) {
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
            let folding = self.collapse_all || self.chosen.values().any(|&c| c);
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
        // Each listed message goes before the first message after it in
        // the index, found by bisecting what follows the one listed before
        // it (with pages coming in the index's order, that is the end).
        let mut places = Vec::with_capacity(listed.len());
        let mut from = 0;
        for (listed, _) in &listed {
            from += self.messages[from..].partition_point(|m| !compare(listed, &key(m)).is_lt());
            places.push(from);
        }
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
        if selected < self.messages.len() {
            self.selected += places.partition_point(|&place| place <= selected);
        }
        // Room at the end for the listed messages (copies of the first, for
        // now). From the last listed, the messages after where each goes
        // move down past the room left, which moves up, and it takes the
        // room's last place: each message moves once.
        let mut end = self.messages.len();
        if let Some((_, first)) = listed.first() {
            let room = std::iter::repeat_n(first.clone(), listed.len());
            self.messages.extend(room);
        }
        for (k, (&place, (_, message))) in places.iter().zip(listed).enumerate().rev() {
            // The room is end..end + k + 1.
            let room = k + 1;
            while end > place {
                let step = room.min(end - place);
                let (before, after) = self.messages.split_at_mut(end);
                before[end - step..].swap_with_slice(&mut after[room - step..room]);
                end -= step;
            }
            self.messages[place + k] = message;
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

    /// Whether the thread `key`, which holds unread mail if `unread`, is
    /// collapsed: as the user chose, or as every thread is (but one with
    /// unread mail, kept open).
    fn is_collapsed(&self, key: &str, unread: bool) -> bool {
        self.chosen
            .get(key)
            .copied()
            .unwrap_or(self.collapse_all && !unread)
    }

    /// Collapses or opens the thread `key`, as the user chose.
    fn set_collapsed(&mut self, key: Arc<str>, collapsed: bool) {
        self.chosen.insert(key, collapsed);
    }

    /// Whether the thread `root` starts holds unread mail.
    fn thread_unread(&self, root: &MessageId) -> bool {
        self.messages
            .iter()
            .chain(&self.folded)
            .any(|m| !m.flags.read && self.threads.get(&m.id).is_some_and(|t| &t.root == root))
    }

    /// Mail arrived in the threads of `arrived`: those that would be
    /// collapsed (the user collapsed them, or every thread) are opened if
    /// it is unread, and stay open once it is read (as mutt's
    /// `uncollapse_new`). Returns whether any was.
    pub(super) fn open_for_new_mail<'a>(
        &mut self,
        arrived: impl IntoIterator<Item = &'a MessageSummary>,
    ) -> bool {
        let mut opened = false;
        for message in arrived.into_iter().filter(|m| !m.flags.read) {
            if let Some(thread) = &message.thread {
                let key: Arc<str> = format!("conversation {}", thread.conversation).into();
                let collapsed = self.chosen.get(&key).copied().unwrap_or(self.collapse_all);
                if collapsed {
                    self.chosen.insert(key, false);
                    opened = true;
                }
            }
        }
        opened
    }

    /// If the selected entry is a collapsed thread with unread mail folded
    /// into it, opens it at its first unread message, which is selected.
    pub(super) fn open_at_folded_unread(&mut self) {
        // (Its first message unread is its first unread message.)
        let Some(root) = self
            .messages
            .get(self.selected)
            .filter(|m| m.flags.read)
            .map(|m| m.id.clone())
        else {
            return;
        };
        if self.thread_of(&root).is_none_or(|t| t.folded == 0) {
            return;
        }
        let first = self
            .folded
            .iter()
            .find(|m| !m.flags.read && self.thread_of(&m.id).is_some_and(|t| t.root == root))
            .map(|m| m.id.clone());
        if let Some(first) = first {
            self.uncollapse_to(&first);
        }
    }

    /// The selected collapsed thread's unread messages, if it has mail
    /// folded into it and shows as unread (its first message, or mail
    /// folded into it): `<toggle-new>` marks them all read.
    pub(super) fn folded_thread_unread(&self) -> Option<Vec<MessageId>> {
        let root = &self.messages.get(self.selected)?.id;
        if self.thread_of(root).is_none_or(|t| t.folded == 0) || !self.thread_unread(root) {
            return None;
        }
        Some(
            self.messages
                .iter()
                .chain(&self.folded)
                .filter(|m| !m.flags.read && self.thread_of(&m.id).is_some_and(|t| &t.root == root))
                .map(|m| m.id.clone())
                .collect(),
        )
    }

    /// Takes in a new order, if it changed.
    pub(super) fn apply_ordering(&mut self, ordering: Ordering) {
        if self.ordering != ordering {
            self.ordering = ordering;
            if !self.threaded() {
                self.chosen.clear();
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
        let collapsed = self.is_collapsed(&thread.key, self.thread_unread(root));
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
                    .map_or_else(|| format!("message {root}").into(), |t| t.key.clone());
                // As it shows now: collapsed if mail is folded into it;
                // with none (yet), as chosen.
                let collapsed = self.threads.get(root).is_some_and(|t| t.folded > 0)
                    || self.is_collapsed(&key, self.thread_unread(root));
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
                // (One collapsing every thread leaves open, with unread
                // mail, counts as collapsed: Esc V again opens them all.)
                let all_collapsed = if replies.peek().is_none() {
                    self.collapse_all
                } else {
                    replies.all(|(_, t)| {
                        self.threads
                            .get(&t.root)
                            .is_some_and(|root| root.folded > 0)
                            || (self.collapse_all && !self.chosen.contains_key(&t.key))
                    })
                };
                self.chosen.clear();
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
mod tests;
