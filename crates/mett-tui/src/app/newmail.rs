//! New mail, as in mutt: the open folder is checked every
//! `msgraph_sync_interval` seconds, and messages that arrived are added to
//! the index; the folders named by `mailboxes` are checked too, and the
//! user is told which have new mail.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use mett_core::{Error, Folder, MessageId, MessageSummary, Page};

use super::{App, Effect, Screen, Timer, effect_id};
use crate::config::mailbox::FolderPath;
use crate::worker::{Checked, Polled, Request, RequestId};

/// The unread mail seen, by folder.
pub(super) type SeenByFolder = HashMap<mett_core::FolderId, Seen>;

/// How far back each check asks for changes, before the last one began:
/// enough for the clocks of mett's computer and the server to differ.
const OVERLAP: TimeDelta = TimeDelta::minutes(15);

/// The open folder's check for new mail, on its way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Polling {
    /// The request.
    pub(super) request: RequestId,
    /// The folder checked.
    pub(super) folder: mett_core::FolderId,
    /// When the check began.
    pub(super) started: DateTime<Utc>,
    /// The retries before it was made (see [`App::poll_epoch`]).
    pub(super) epoch: u64,
}

/// A folder from the `mailboxes` command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxEntry {
    /// How it is shown: its label, or its path with `=` for `folder`.
    pub name: String,
    /// What the user would type for it at a mailbox prompt.
    pub text: String,
    /// The folder.
    pub path: FolderPath,
    /// Whether to say when new mail arrives in it.
    pub notify: bool,
    /// Whether to check it for new mail.
    pub poll: bool,
}

/// What mett knows of a mailbox's new mail.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct MailboxState {
    /// The folder, with its counts, as last checked.
    pub(super) folder: Option<Folder>,
    /// Whether mail arrived since the user last had it open (or, before
    /// that, whether it has unread mail).
    pub(super) new: bool,
    /// Its unread messages that arrived or changed last, and when, as last
    /// checked.
    pub(super) recent: Vec<(MessageId, DateTime<Utc>)>,
    /// Whether it has been checked successfully: what it had then isn't
    /// announced as arriving.
    pub(super) sampled: bool,
    /// Why it couldn't be checked, if it couldn't.
    pub(super) error: Option<String>,
    /// Whether that was said on the message line.
    pub(super) reported: bool,
}

/// The unread mail of a folder the user has seen in its index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Seen {
    /// When the last of it arrived or changed.
    until: Option<DateTime<Utc>>,
    /// The messages.
    messages: HashSet<MessageId>,
}

impl Seen {
    /// Whether unread `message`, which arrived or changed at `at`, is new:
    /// the user hasn't seen it, and it is no older than what they have
    /// (mail that changed at the same time may be new too).
    fn is_new(&self, message: &MessageId, at: DateTime<Utc>) -> bool {
        !self.messages.contains(message) && self.until.is_none_or(|until| at >= until)
    }
}

impl App {
    /// The unread mail the open folder's index shows has been seen: it
    /// isn't new once the user leaves (as mail a check finds that hasn't
    /// reached the index yet is). It is kept by folder, so that it counts
    /// for every mailbox path naming the folder, whether or not a check
    /// has found it yet.
    pub(super) fn see_index(&mut self) {
        let Some(open) = &self.folder else {
            return;
        };
        let unread: Vec<&MessageSummary> = self
            .messages
            .iter()
            .chain(&self.hidden)
            .filter(|m| !m.flags.read)
            .collect();
        let seen = self.seen.entry(open.id.clone()).or_default();
        seen.until = seen
            .until
            .max(unread.iter().filter_map(|m| m.modified).max());
        seen.messages.extend(unread.iter().map(|m| m.id.clone()));
    }

    /// A change the user made to an unread message in the open folder was
    /// saved: it changed, but isn't new.
    pub(super) fn changed_here(&mut self, message: &MessageId) {
        if let (Some(open), Some(summary)) = (&self.folder, self.summary(message))
            && !summary.flags.read
            && summary.folder == open.id
        {
            let id = summary.id.clone();
            self.seen
                .entry(open.id.clone())
                .or_default()
                .messages
                .insert(id);
        }
    }

    /// Sets the timer for the next check, unless it is set or checking is
    /// off.
    pub(super) fn arm_poll(&mut self) -> Vec<Effect> {
        match self.sync_interval {
            Some(interval) if !self.poll_armed => {
                self.poll_armed = true;
                vec![Effect::Later(interval, Timer::Poll(self.poll_timer))]
            }
            _ => Vec::new(),
        }
    }

    /// The timer went off: check for new mail, and set it again. A timer
    /// set before the interval changed is ignored: another was set.
    pub(super) fn on_poll_timer(&mut self, timer: u64) -> Vec<Effect> {
        if timer != self.poll_timer {
            return Vec::new();
        }
        self.poll_armed = false;
        let mut effects = Vec::new();
        // After a failed sign-in, background checks would only ask the
        // user to sign in again and again: they wait for something the
        // user does to sign in.
        if !self.poll_paused && self.sign_in.is_none() {
            effects.extend(self.poll_folder());
            effects.extend(self.check_mailboxes());
        }
        effects.extend(self.arm_poll());
        effects
    }

    /// Checks the open folder for new mail, unless it is being listed
    /// (which shows that anyway).
    fn poll_folder(&mut self) -> Vec<Effect> {
        let Some(folder) = &self.folder else {
            return Vec::new();
        };
        if self.listing.is_some()
            || self.polling.is_some()
            || self.opening.is_some()
            || self.switching.is_some()
        {
            return Vec::new();
        }
        // Part of it failed to list: that is tried again instead (what a
        // check adds, that listing would add again), so a passing error
        // doesn't stop new mail reaching the index.
        if let Some(page) = self.unlisted.take() {
            return self.list(page);
        }
        let folder = folder.id.clone();
        let started = Utc::now();
        let since = self.poll_since.unwrap_or(started - OVERLAP);
        let request = self.request(Request::PollFolder {
            folder: folder.clone(),
            page: Page::first(self.page_size),
            since,
        });
        self.polling = Some(Polling {
            request: effect_id(&request),
            folder,
            started,
            epoch: self.poll_epoch,
        });
        vec![request]
    }

    /// The folder is being listed from the start: later checks look for
    /// changes from now on.
    pub(super) fn listing_started(&mut self) {
        // The listing shows what a check on its way would find.
        self.polling = None;
        let now = Utc::now();
        self.poll_since = Some(now - OVERLAP);
        self.listed_at = Some(now);
    }

    /// Reads the counts of the mailboxes to check, unless that is on its
    /// way already.
    pub(super) fn check_mailboxes(&mut self) -> Vec<Effect> {
        if self.checking.is_some() {
            return Vec::new();
        }
        let mailboxes: Vec<_> = self
            .mailboxes
            .iter()
            .filter(|m| m.poll)
            .map(|m| {
                let id = self
                    .mailbox_state
                    .get(&m.path)
                    .and_then(|s| s.folder.as_ref())
                    .map(|f| f.id.clone());
                // What changed since the user last saw it.
                let since = id
                    .as_ref()
                    .and_then(|id| self.seen.get(id))
                    .and_then(|seen| seen.until);
                (m.path.clone(), id, since)
            })
            .collect();
        if mailboxes.is_empty() {
            return Vec::new();
        }
        let request = self.request(Request::CheckMailboxes(mailboxes));
        self.checking = Some(effect_id(&request));
        vec![request]
    }

    /// Whether the folder's index is being fetched: listed, or its start
    /// listed again for new mail. Changes made meanwhile may not show in
    /// what arrives.
    pub(super) fn fetching_index(&self) -> bool {
        self.listing.is_some() || self.polling.is_some()
    }

    /// Takes in what a check of the open folder found: adds the messages
    /// that arrived, takes out those that went, and updates the rest.
    pub(super) fn on_polled(
        &mut self,
        id: RequestId,
        result: mett_core::Result<Polled>,
    ) -> Vec<Effect> {
        let Some(Polling {
            folder,
            started,
            epoch,
            ..
        }) = self.polling.take_if(|p| p.request == id)
        else {
            return Vec::new();
        };
        let still_open = self.folder.as_ref().is_some_and(|f| f.id == folder);
        let page = match result {
            // The folder changed meanwhile, or is being listed again.
            _ if !still_open || self.listing.is_some() => {
                self.forget_index_changes();
                return Vec::new();
            }
            Ok(page) => page,
            Err(e) => {
                self.forget_index_changes();
                self.background_error(e, epoch);
                return Vec::new();
            }
        };
        // The next check asks for what changed after this one began (from
        // a while before, for the clocks to differ).
        self.poll_since = Some(started - OVERLAP);
        let effects = self.merge_polled(page);
        self.forget_index_changes();
        effects
    }

    /// Forgets the changes made while the index was being fetched, once
    /// nothing is.
    pub(super) fn forget_index_changes(&mut self) {
        if !self.fetching_index() {
            self.saved_flags.clear();
            self.removed_while_listing.clear();
        }
    }

    /// Reports a failed background check without hiding what the status
    /// line already says, and holds off checking after a sign-in failed.
    /// A failure from before the user last retried doesn't hold off
    /// checking again.
    fn background_error(&mut self, e: Error, epoch: u64) {
        if matches!(e, Error::Auth(_)) && epoch == self.poll_epoch {
            self.poll_paused = true;
        }
        if self.status.is_empty() {
            self.report(e);
        }
    }

    fn merge_polled(&mut self, polled: Polled) -> Vec<Effect> {
        let Polled { newest, changed } = polled;
        let mut fresh = newest.messages;
        let mut changes = changed.messages;
        // Moved or purged away since it was asked for.
        fresh.retain(|m| !self.removed_while_listing.contains(&m.id));
        changes.retain(|m| !self.removed_while_listing.contains(&m.id));
        self.overlay_wanted_flags(&mut fresh);
        self.overlay_wanted_flags(&mut changes);
        let complete = newest.next.is_none();
        let known: HashSet<MessageId> = self
            .messages
            .iter()
            .chain(&self.hidden)
            .map(|m| m.id.clone())
            .collect();
        // A message in both lists comes as it was last changed.
        let mut arrived: Vec<MessageSummary> = Vec::new();
        for message in fresh.iter().chain(&changes) {
            if known.contains(&message.id) {
                continue;
            }
            match arrived.iter_mut().find(|m| m.id == message.id) {
                Some(earlier) if message.modified > earlier.modified => *earlier = message.clone(),
                Some(_) => {}
                None => arrived.push(message.clone()),
            }
        }
        let unread = arrived.iter().any(|m| !m.flags.read);
        // More arrived than a page holds, or more changed: list the whole
        // folder again.
        let all_new = !fresh.is_empty() && fresh.iter().all(|m| !known.contains(&m.id));
        // Changes beyond the pages read are older than those on them; if
        // those are older than the last listing (allowing for the clocks),
        // the listing had them.
        let listed_before = changes
            .iter()
            .filter_map(|m| m.modified)
            .min()
            .zip(self.listed_at)
            .is_some_and(|(oldest, listed)| oldest < listed - OVERLAP);
        if (!complete && all_new) || (changed.next.is_some() && !listed_before) {
            if unread {
                self.announce(NEW_MAIL);
            }
            let effects = self.refresh();
            // New mail may be on pages not seen yet: the listing tells.
            self.known_before_refresh = Some(known);
            return effects;
        }
        // What the page covers. Graph sorts by when messages were sent,
        // which unsent drafts lack: they come as a group (by when they were
        // created), before or after the rest. The group the page starts
        // with is all there if the other follows it on the page; a group
        // the page reaches is covered from its newest to its last message
        // on the page (one as old may be on the next page).
        let first_draft = fresh.first().map(|m| m.draft);
        let mixed = fresh.iter().any(|m| m.draft) && fresh.iter().any(|m| !m.draft);
        let oldest = |draft: bool| {
            fresh
                .iter()
                .rev()
                .find(|m| m.draft == draft)
                .map(|m| m.received)
        };
        let (oldest_draft, oldest_sent) = (oldest(true), oldest(false));
        let covered = |m: &MessageSummary| {
            if complete || (mixed && first_draft == Some(m.draft)) {
                return true;
            }
            let oldest = if m.draft { oldest_draft } else { oldest_sent };
            oldest.is_some_and(|oldest| m.received > oldest)
        };
        let listed: HashSet<&MessageId> = fresh.iter().map(|m| &m.id).collect();
        // One missing that is as old as the page's last may be on the next
        // page, or gone: the folder is listed again to tell (once, or
        // every check would).
        let tied: Vec<MessageId> = self
            .messages
            .iter()
            .chain(&self.hidden)
            .filter(|m| !listed.contains(&m.id) && !covered(m))
            .filter(|m| (if m.draft { oldest_draft } else { oldest_sent }) == Some(m.received))
            .filter(|m| !self.transferring(&m.id) && !self.tie_checked.contains(&m.id))
            .map(|m| m.id.clone())
            .collect();
        if !tied.is_empty() {
            self.tie_checked.extend(tied);
            if unread {
                self.announce(NEW_MAIL);
            }
            let effects = self.refresh();
            self.known_before_refresh = Some(known);
            return effects;
        }
        let gone: HashSet<MessageId> = self
            .messages
            .iter()
            .chain(&self.hidden)
            .filter(|m| !listed.contains(&m.id))
            .filter(|m| covered(m))
            // Moves and purges on their way take it out themselves.
            .filter(|m| !self.transferring(&m.id))
            .map(|m| m.id.clone())
            .collect();
        self.remove_messages(&gone);
        // The selected message, or the one that took its place.
        let selected = self.messages.get(self.selected).map(|m| m.id.clone());
        // The others may have changed elsewhere (read in Outlook, say).
        // When both have a message, it comes as it was last changed.
        let mut updates: HashMap<&MessageId, &MessageSummary> = HashMap::new();
        for message in fresh.iter().chain(&changes) {
            match updates.get(&message.id) {
                Some(earlier) if earlier.modified >= message.modified => {}
                _ => {
                    updates.insert(&message.id, message);
                }
            }
        }
        for summary in self.messages.iter_mut().chain(&mut self.hidden) {
            if let Some(update) = updates.get(&summary.id) {
                *summary = (*update).clone();
            }
        }
        for message in arrived {
            self.insert_arrived(message);
        }
        // Stay on the same message.
        if let Some(at) = selected.and_then(|id| self.messages.iter().position(|m| m.id == id)) {
            self.selected = at;
        }
        if complete {
            self.forget_missing_marks();
        }
        self.see_index();
        if unread {
            self.announce(NEW_MAIL);
        }
        Vec::new()
    }

    /// Adds a message that arrived to the index, where its date puts it
    /// in the folder's order, or aside if the index is limited and it
    /// doesn't match.
    fn insert_arrived(&mut self, message: MessageSummary) {
        let position =
            |m: &MessageSummary| self.positions.get(&m.id).copied().unwrap_or(usize::MAX);
        let mut order: Vec<&MessageSummary> = self.messages.iter().chain(&self.hidden).collect();
        order.sort_by_key(|m| position(m));
        // Among the messages like it (drafts are a group of their own):
        // before the first older one, or after the last of them.
        let alike: Vec<&&MessageSummary> =
            order.iter().filter(|m| m.draft == message.draft).collect();
        // Graph lists unsent drafts after the rest.
        let at = alike
            .iter()
            .find(|m| m.received < message.received)
            .map(|m| position(m))
            .or_else(|| alike.last().map(|m| position(m).saturating_add(1)))
            .or_else(|| {
                let first_draft = order.iter().find(|m| m.draft).map(|m| position(m));
                first_draft.filter(|_| !message.draft)
            });
        let at = match at {
            Some(at) => {
                // Those after it move down one.
                for later in self.positions.values_mut().filter(|p| **p >= at) {
                    *later += 1;
                }
                at
            }
            None => self.positions.values().max().map_or(0, |last| last + 1),
        };
        self.positions.insert(message.id.clone(), at);
        let list = if self.limit_shows(&message) {
            &mut self.messages
        } else {
            &mut self.hidden
        };
        let positions = &self.positions;
        let index = list
            .iter()
            .filter(|m| positions.get(&m.id).is_some_and(|p| *p < at))
            .count();
        list.insert(index, message);
    }

    /// The folder was listed again because a check found more than it
    /// could take in: says if mail arrived meanwhile.
    pub(super) fn relisted(&mut self) {
        let Some(known) = self.known_before_refresh.take() else {
            return;
        };
        let arrived = self
            .messages
            .iter()
            .chain(&self.hidden)
            .any(|m| !m.flags.read && !known.contains(&m.id));
        if arrived {
            self.announce(NEW_MAIL);
        }
    }

    /// Takes in the mailboxes' counts, and when their newest unread mail
    /// arrived.
    pub(super) fn on_mailboxes_checked(
        &mut self,
        id: RequestId,
        checked: Vec<(FolderPath, mett_core::Result<Checked>)>,
    ) -> Vec<Effect> {
        if self.checking.take_if(|request| *request == id).is_none() {
            return Vec::new();
        }
        let mut arrived = Vec::new();
        let mut failure = None;
        for (path, result) in checked {
            let found = result.as_ref().ok().map(|c| &c.folder);
            let open = self.is_open(&path, found);
            let seen = found.and_then(|f| self.seen.get(&f.id)).cloned();
            let state = self.mailbox_state.entry(path.clone()).or_default();
            match result {
                Ok(Checked { folder, recent }) => {
                    // Another folder now at its path starts afresh.
                    if state.folder.as_ref().is_some_and(|f| f.id != folder.id) {
                        *state = MailboxState::default();
                    }
                    self.folder_ids.insert(path.clone(), folder.id.clone());
                    let first = !state.sampled;
                    state.sampled = true;
                    let before: HashSet<MessageId> =
                        state.recent.iter().map(|(id, _)| id.clone()).collect();
                    // Mail that arrives is newer than what was there: an
                    // older message that only now makes the sample (one
                    // above it was read) didn't arrive.
                    let newest_before = state.recent.iter().map(|(_, at)| *at).max();
                    state.recent = recent;
                    if open {
                        // Not new while open; what was seen is what reaches
                        // the index.
                        state.new = false;
                    } else {
                        // Unread mail that arrived (or changed) after the
                        // user last had it open (or at all, before they
                        // did) is new.
                        let was_new = state.new;
                        state.new = match &seen {
                            None => !state.recent.is_empty(),
                            // So many changed that some may be left out:
                            // new mail may be among them.
                            Some(_) if state.recent.len() >= mett_core::RECENT_UNREAD => true,
                            Some(seen) => state.recent.iter().any(|(id, at)| seen.is_new(id, *at)),
                        };
                        // Said when unseen mail arrives (not when mail that
                        // was there already changes), or mail found while
                        // the folder was open turns out unseen; not at
                        // first, unless the user had seen the mailbox.
                        let arriving = state.recent.iter().any(|(id, at)| {
                            !before.contains(id)
                                && newest_before.is_none_or(|newest| *at >= newest)
                                && seen.as_ref().is_none_or(|s| s.is_new(id, *at))
                        });
                        if state.new && (arriving || !was_new) && (!first || seen.is_some()) {
                            arrived.push(path);
                        }
                    }
                    state.folder = Some(folder);
                    state.error = None;
                    state.reported = false;
                }
                Err(e) => {
                    // Said once, not at every check.
                    if !state.reported && failure.is_none() {
                        failure = Some((path, e.clone()));
                    }
                    state.error = Some(e.to_string());
                    // Gone: nothing to offer, nor counts to show.
                    if matches!(e, Error::NotFound { .. }) {
                        state.folder = None;
                        state.new = false;
                        state.recent.clear();
                    }
                    // A mailbox mett is refused (access denied) doesn't
                    // stop checking the others: a failed sign-in shows in
                    // its window, and stops the open folder's checks.
                }
            }
        }
        let names: Vec<&str> = self
            .mailboxes
            .iter()
            .filter(|m| m.notify && arrived.contains(&m.path))
            .map(|m| m.name.as_str())
            .collect();
        if !names.is_empty() {
            self.announce(&format!("New mail in {}.", names.join(", ")));
        } else if let Some((path, e)) = failure {
            let name = self
                .mailboxes
                .iter()
                .find(|m| m.path == path)
                .map_or(String::new(), |m| m.name.clone());
            // Said once the message line is free; until then, the next
            // check tries again.
            if self.status.is_empty() {
                // An error, which new mail doesn't hide.
                self.fail(format!("Couldn't check {name}: {e}"));
                if let Some(state) = self.mailbox_state.get_mut(&path) {
                    state.reported = true;
                }
            }
        }
        self.refresh_mailbox_browser();
        Vec::new()
    }

    /// Says that new mail arrived, unless an error is on the message line:
    /// that matters more (the mail is still counted as new).
    fn announce(&mut self, text: &str) {
        if self.status.is_empty() || self.error.as_deref() != Some(self.status.as_str()) {
            self.status = text.to_owned();
        }
    }

    /// Whether the mailbox at `path` (found to be `found`, if it was) is
    /// the open folder.
    fn is_open(&self, path: &FolderPath, found: Option<&Folder>) -> bool {
        match (&self.folder, found) {
            (Some(open), Some(found)) => open.id == found.id,
            (Some(_), None) => *path == self.path,
            (None, _) => false,
        }
    }

    /// The folder was opened: it has no new mail while open.
    pub(super) fn visited(&mut self) {
        let Some(open) = self.folder.clone() else {
            return;
        };
        let path = self.path.clone();
        for (mailbox, state) in &mut self.mailbox_state {
            let same = state.folder.as_ref().is_some_and(|f| f.id == open.id) || *mailbox == path;
            if same {
                state.new = false;
                state.folder = Some(open.clone());
            }
        }
    }

    /// The mailboxes with new mail, other than the open folder.
    pub fn mailboxes_with_new_mail(&self) -> usize {
        self.mailboxes
            .iter()
            .filter(|m| self.has_new_mail(m))
            .count()
    }

    /// Whether `mailbox` has new mail. One that isn't checked (`-nopoll`)
    /// can't tell, even if the folder is listed again under another name
    /// that is.
    fn has_new_mail(&self, mailbox: &MailboxEntry) -> bool {
        mailbox.poll && self.mailbox_state.get(&mailbox.path).is_some_and(|s| s.new)
    }

    /// The mailbox to offer when asked which folder to open: the next one
    /// with new mail after `after` (or the open folder), as mutt does.
    pub(super) fn next_new_mailbox(&self, after: Option<&str>) -> Option<&MailboxEntry> {
        let start = after
            .and_then(|text| self.mailboxes.iter().position(|m| m.text == text))
            .or_else(|| self.mailboxes.iter().position(|m| m.path == self.path))
            .map_or(0, |i| i + 1);
        let count = self.mailboxes.len();
        (0..count)
            .map(|i| &self.mailboxes[(start + i) % count])
            .find(|m| self.has_new_mail(m))
    }

    /// The mailboxes, as the browser shows them, with their counts if
    /// known.
    pub(super) fn mailbox_entries(&self) -> Vec<super::mailbox::BrowserEntry> {
        self.mailboxes
            .iter()
            .map(|m| {
                let state = self.mailbox_state.get(&m.path);
                let folder = state.and_then(|s| s.folder.as_ref());
                super::mailbox::BrowserEntry {
                    depth: 0,
                    name: m.name.clone(),
                    id: folder.map(|f| f.id.clone()),
                    counts: folder.map(|f| (f.unread_count, f.total_count)),
                    new: self.has_new_mail(m),
                    path: m.text.clone(),
                    target: m.path.clone(),
                }
            })
            .collect()
    }

    /// Shows fresh counts in the browser, if it lists the mailboxes.
    fn refresh_mailbox_browser(&mut self) {
        let entries = self.mailbox_entries();
        for screen in &mut self.screens {
            if let Screen::Browser(browser) = screen
                && browser.mailboxes
            {
                browser.entries = entries.clone();
                browser.selected = browser.selected.min(entries.len().saturating_sub(1));
            }
        }
    }

    /// Takes in the mailboxes and the check interval from the settings.
    pub(super) fn apply_mailboxes(
        &mut self,
        mailboxes: Vec<MailboxEntry>,
        interval: Option<Duration>,
    ) {
        // What is known of mailboxes still listed is kept.
        self.mailbox_state
            .retain(|path, _| mailboxes.iter().any(|m| m.path == *path));
        // A check of the mailboxes as they were is left to answer to
        // nothing: the next one checks the new ones.
        // (So is one when checking is turned off, for any of them.)
        let checked = |list: &[MailboxEntry]| {
            list.iter()
                .map(|m| (m.path.clone(), m.poll))
                .collect::<Vec<_>>()
        };
        if checked(&self.mailboxes) != checked(&mailboxes) || self.sync_interval != interval {
            self.checking = None;
        }
        self.mailboxes = mailboxes;
        // A timer set for another interval is replaced.
        if self.sync_interval != interval {
            self.sync_interval = interval;
            self.poll_timer += 1;
            self.poll_armed = false;
        }
        self.refresh_mailbox_browser();
    }
}

/// What mutt says when mail arrives in the open folder.
const NEW_MAIL: &str = "New mail in this mailbox.";

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta};
    use mett_core::{AccountId, FlagsUpdate, FolderId, WellKnownFolder, mock};
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    use super::*;
    use crate::action::Action;
    use crate::app::{Input, Msg, Settings};
    use crate::worker::{Event, Response};
    use mett_core::MessagePage;

    const MINUTE: Duration = Duration::from_secs(60);

    /// A message dated (and last changed) at `minutes`.
    fn summary(id: &str, minutes: i64) -> MessageSummary {
        let date = DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes);
        let mut summary = mock::message(id, "in", id, date).summary;
        summary.modified = Some(date);
        summary
    }

    fn inbox() -> FolderPath {
        FolderPath::WellKnown(WellKnownFolder::Inbox, vec![])
    }

    fn work() -> FolderPath {
        FolderPath::Named(vec!["Work".into()])
    }

    fn settings() -> Settings {
        let entry = |name: &str, path| MailboxEntry {
            name: name.into(),
            text: name.into(),
            path,
            notify: true,
            poll: true,
        };
        Settings {
            account: AccountId::new("alice@contoso.com"),
            folder: "msgraph://alice@contoso.com/".into(),
            sync_interval: Some(MINUTE),
            mailboxes: vec![entry("=@inbox", inbox()), entry("=Work", work())],
            ..Settings::default()
        }
    }

    fn respond(app: &mut App, id: RequestId, event: Event) -> Vec<Effect> {
        app.update(Msg::Response(Response { id, event }))
    }

    fn requests(effects: &[Effect]) -> Vec<(RequestId, Request)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Request(id, request) => Some((*id, request.clone())),
                _ => None,
            })
            .collect()
    }

    fn folder(id: &str, name: &str, unread: u32) -> Folder {
        Folder {
            unread_count: unread,
            ..mock::folder(id, name, None)
        }
    }

    /// A mailbox checked, whose newest unread message arrived at
    /// `minutes`, if it has any.
    fn checked(
        id: &str,
        name: &str,
        unread: u32,
        newest: Option<i64>,
    ) -> mett_core::Result<Checked> {
        Ok(Checked {
            folder: folder(id, name, unread),
            recent: newest
                .map(|m| {
                    let at = DateTime::UNIX_EPOCH + TimeDelta::minutes(m);
                    // As the test folders name their messages.
                    vec![(MessageId::new(format!("m{m}")), at)]
                })
                .unwrap_or_default(),
        })
    }

    /// An Inbox with m2, m1 and m0 (newest first), and the mailboxes check
    /// it started, not yet answered.
    fn app() -> (App, RequestId) {
        let (mut app, effects) = App::open(80, 24, inbox(), settings());
        let (open, _) = requests(&effects)[0].clone();
        let effects = respond(
            &mut app,
            open,
            Event::FolderOpened(Ok(folder("in", "Inbox", 1))),
        );
        let sent = requests(&effects);
        assert!(matches!(sent[0].1, Request::ListMessages { .. }));
        assert!(matches!(&sent[1].1, Request::CheckMailboxes(m) if m.len() == 2));
        assert!(effects.contains(&Effect::Later(MINUTE, Timer::Poll(0))));
        let page = MessagePage {
            messages: vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)],
            next: None,
        };
        respond(&mut app, sent[0].0, Event::MessagesListed(Ok(page)));
        (app, sent[1].0)
    }

    fn ids(app: &App) -> Vec<&str> {
        app.messages.iter().map(|m| m.id.as_str()).collect()
    }

    /// Fires the poll timer and returns the folder's listing request.
    fn poll(app: &mut App) -> RequestId {
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        assert!(effects.contains(&Effect::Later(MINUTE, Timer::Poll(0))));
        requests(&effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::PollFolder { .. }))
            .expect("the folder is checked")
            .0
    }

    fn page(messages: Vec<MessageSummary>) -> MessagePage {
        MessagePage {
            messages,
            next: None,
        }
    }

    fn polled(app: &mut App, id: RequestId, messages: Vec<MessageSummary>) -> Vec<Effect> {
        let polled = Polled {
            newest: page(messages),
            changed: page(Vec::new()),
        };
        respond(app, id, Event::FolderPolled(Ok(polled)))
    }

    #[test]
    fn a_message_missing_at_the_page_s_end_has_the_folder_listed_once() {
        let (mut app, _) = app();
        // m0 is as old as m1, the page's last: on the next page, or gone.
        app.messages.iter_mut().for_each(|m| {
            if m.id.as_str() == "m0" {
                m.received = DateTime::UNIX_EPOCH + TimeDelta::minutes(1);
            }
        });
        let partial = |app: &mut App| {
            let id = poll(app);
            let polled = Polled {
                newest: MessagePage {
                    messages: vec![summary("m2", 2), summary("m1", 1)],
                    next: Some(mett_core::PageCursor::new("in more")),
                },
                changed: page(Vec::new()),
            };
            respond(app, id, Event::FolderPolled(Ok(polled)))
        };
        let effects = partial(&mut app);
        let (list, _) = requests(&effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::ListMessages { .. }))
            .expect("the folder is listed again");
        // Listed again, it is there: not again at every check.
        let mut m0 = summary("m0", 1);
        m0.received = DateTime::UNIX_EPOCH + TimeDelta::minutes(1);
        respond(
            &mut app,
            list,
            Event::MessagesListed(Ok(page(vec![summary("m2", 2), summary("m1", 1), m0]))),
        );
        let effects = partial(&mut app);
        assert!(
            !requests(&effects)
                .iter()
                .any(|(_, r)| matches!(r, Request::ListMessages { .. }))
        );
    }

    #[test]
    fn new_mail_joins_the_index_where_its_date_puts_it() {
        let (mut app, _) = app();
        app.update(Msg::Action(Action::NextEntry));
        let id = poll(&mut app);
        let mut read = summary("m1", 1);
        read.flags.read = true;
        polled(
            &mut app,
            id,
            vec![summary("m3", 3), summary("m2", 2), read, summary("m0", 0)],
        );
        assert_eq!(ids(&app), ["m3", "m2", "m1", "m0"]);
        // Still on m1, which was read elsewhere.
        assert_eq!(app.selected, 2);
        assert!(app.messages[2].flags.read);
        assert_eq!(app.status, NEW_MAIL);
    }

    #[test]
    fn mail_that_arrives_with_an_old_date_joins_the_index() {
        let (mut app, _) = app();
        let id = poll(&mut app);
        let Some(Polling { started, .. }) = app.polling.clone() else {
            panic!("expected a check");
        };
        let polled = Polled {
            newest: page(vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)]),
            changed: page(vec![summary("late", -5)]),
        };
        respond(&mut app, id, Event::FolderPolled(Ok(polled)));
        assert_eq!(ids(&app), ["m2", "m1", "m0", "late"]);
        assert_eq!(app.status, NEW_MAIL);
        // The next check asks for changes since this one began.
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let since = requests(&effects).into_iter().find_map(|(_, r)| match r {
            Request::PollFolder { since, .. } => Some(since),
            _ => None,
        });
        assert_eq!(since, Some(started - OVERLAP));
    }

    #[test]
    fn a_new_interval_replaces_the_timer() {
        let (mut app, _) = app();
        let mut settings = settings();
        settings.sync_interval = Some(Duration::from_secs(10));
        app.apply_settings(settings);
        let effects = app.arm_poll();
        assert_eq!(
            effects,
            [Effect::Later(Duration::from_secs(10), Timer::Poll(1))]
        );
        // The old timer does nothing.
        assert_eq!(app.update(Msg::Timer(Timer::Poll(0))), []);
    }

    #[test]
    fn mail_gone_elsewhere_leaves_the_index() {
        let (mut app, _) = app();
        let id = poll(&mut app);
        // A full page that ends at m1: m0 may be on the next.
        let newest = MessagePage {
            messages: vec![summary("m1", 1)],
            next: Some(mett_core::PageCursor::new("in next")),
        };
        let polled = Polled {
            newest,
            changed: page(Vec::new()),
        };
        respond(&mut app, id, Event::FolderPolled(Ok(polled)));
        assert_eq!(ids(&app), ["m1", "m0"]);
        assert!(app.status.is_empty());
    }

    #[test]
    fn a_draft_beyond_the_newest_page_stays() {
        let (mut app, _) = app();
        let id = poll(&mut app);
        // A draft created after m1 was sent, but listed after the sent
        // mail, as Graph sorts unsent drafts.
        let mut draft = summary("d", 5);
        draft.draft = true;
        app.messages.push(draft);
        let newest = MessagePage {
            messages: vec![summary("m2", 2), summary("m1", 1)],
            next: Some(mett_core::PageCursor::new("in next")),
        };
        let polled = Polled {
            newest,
            changed: page(Vec::new()),
        };
        respond(&mut app, id, Event::FolderPolled(Ok(polled)));
        assert_eq!(ids(&app), ["m2", "m1", "m0", "d"]);
    }

    #[test]
    fn drafts_are_placed_and_removed_among_drafts() {
        let (mut app, _) = app();
        let draft = |id: &str, minutes| {
            let mut m = summary(id, minutes);
            m.draft = true;
            m
        };
        // The drafts were listed after the sent mail.
        app.messages.push(draft("d2", 9));
        app.messages.push(draft("d1", 8));
        app.positions.insert(MessageId::new("d2"), 3);
        app.positions.insert(MessageId::new("d1"), 4);
        let id = poll(&mut app);
        // d3 is new, and d2 went; d1 may be on the next page.
        let newest = MessagePage {
            messages: vec![
                summary("m2", 2),
                summary("m1", 1),
                summary("m0", 0),
                draft("d3", 10),
                draft("d1", 8),
            ],
            next: Some(mett_core::PageCursor::new("in next")),
        };
        let polled = Polled {
            newest,
            changed: page(Vec::new()),
        };
        respond(&mut app, id, Event::FolderPolled(Ok(polled)));
        assert_eq!(ids(&app), ["m2", "m1", "m0", "d3", "d1"]);
    }

    #[test]
    fn changes_older_than_the_listing_need_no_listing_again() {
        let (mut app, _) = app();
        let id = poll(&mut app);
        let changed = MessagePage {
            messages: vec![summary("m1", 1)],
            next: Some(mett_core::PageCursor::new("in more")),
        };
        let polled = Polled {
            newest: page(vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)]),
            changed,
        };
        let effects = respond(&mut app, id, Event::FolderPolled(Ok(polled)));
        assert!(requests(&effects).is_empty(), "{effects:?}");
        // And the next check still allows for the clocks.
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let Some(Polling { started, .. }) = app.polling.clone() else {
            panic!("expected a check");
        };
        let since = requests(&effects).into_iter().find_map(|(_, r)| match r {
            Request::PollFolder { since, .. } => Some(since),
            _ => None,
        });
        assert!(since.is_some_and(|since| since <= started - OVERLAP + TimeDelta::seconds(5)));
    }

    #[test]
    fn a_late_failure_from_before_a_retry_does_not_stop_checks() {
        let (mut app, check) = app();
        app.retry();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![(inbox(), Err(Error::Auth("expired".into())))]),
        );
        poll(&mut app);
    }

    #[test]
    fn mail_that_changed_with_what_was_seen_may_be_new() {
        let seen = Seen {
            until: Some(DateTime::UNIX_EPOCH),
            messages: HashSet::from([MessageId::new("m1")]),
        };
        assert!(!seen.is_new(&MessageId::new("m1"), DateTime::UNIX_EPOCH));
        assert!(seen.is_new(&MessageId::new("m2"), DateTime::UNIX_EPOCH));
    }

    #[test]
    fn mail_seen_before_the_first_check_is_not_new() {
        let (mut app, check) = app();
        app.change_folder(work());
        let (switch, _) = app.switching.clone().unwrap();
        respond(
            &mut app,
            switch,
            Event::FolderOpened(Ok(folder("work", "Work", 0))),
        );
        // The Inbox index had m2 (dated 2) unread when the user left.
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 3, Some(2))),
                (work(), checked("work", "Work", 0, None)),
            ]),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 0);
    }

    #[test]
    fn another_folder_at_a_mailbox_s_path_starts_afresh() {
        let (mut app, check) = app();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), checked("work", "Work", 0, None)),
            ]),
        );
        app.seen.insert(
            FolderId::new("work"),
            Seen {
                until: Some(DateTime::UNIX_EPOCH + TimeDelta::minutes(50)),
                messages: HashSet::new(),
            },
        );
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let (check, _) = requests(&effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
            .unwrap();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), checked("work2", "Work", 1, Some(3))),
            ]),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 1);
    }

    #[test]
    fn mail_found_by_listing_again_is_announced() {
        let (mut app, _) = app();
        let id = poll(&mut app);
        // More changes than were read, since the folder was listed.
        let mut recent = summary("m1", 1);
        recent.modified = Some(Utc::now());
        let changed = MessagePage {
            messages: vec![recent],
            next: Some(mett_core::PageCursor::new("in more")),
        };
        let polled = Polled {
            newest: page(vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)]),
            changed,
        };
        let effects = respond(&mut app, id, Event::FolderPolled(Ok(polled)));
        assert!(app.status.is_empty(), "nothing new seen yet");
        let (list, _) = requests(&effects)[0].clone();
        respond(
            &mut app,
            list,
            Event::MessagesListed(Ok(page(vec![
                summary("m2", 2),
                summary("m1", 1),
                summary("m0", 0),
                summary("old", -9),
            ]))),
        );
        assert_eq!(app.status, NEW_MAIL);
    }

    #[test]
    fn lifting_a_limit_keeps_new_mail_in_place() {
        let (mut app, _) = app();
        // Listed out of date order, as drafts can be.
        let mut draft = summary("d", 9);
        draft.draft = true;
        app.messages.push(draft.clone());
        app.positions.insert(draft.id.clone(), 3);
        app.update(Msg::Action(Action::Limit));
        for c in "~s m2".chars() {
            app.update(Msg::Key(KeyEvent::from(KeyCode::Char(c))));
        }
        app.update(Msg::Key(KeyEvent::from(KeyCode::Enter)));
        assert_eq!(ids(&app), ["m2"]);
        let id = poll(&mut app);
        let newest = MessagePage {
            messages: vec![summary("m3", 3), summary("m2", 2), summary("m1", 1)],
            next: Some(mett_core::PageCursor::new("in next")),
        };
        let polled = Polled {
            newest,
            changed: page(Vec::new()),
        };
        respond(&mut app, id, Event::FolderPolled(Ok(polled)));
        app.update(Msg::Action(Action::Limit));
        // The question starts with the limit in force.
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('u'),
            ratatui::crossterm::event::KeyModifiers::CONTROL,
        )));
        for c in "all".chars() {
            app.update(Msg::Key(KeyEvent::from(KeyCode::Char(c))));
        }
        app.update(Msg::Key(KeyEvent::from(KeyCode::Enter)));
        assert_eq!(ids(&app), ["m3", "m2", "m1", "m0", "d"]);
    }

    #[test]
    fn a_full_page_of_new_mail_lists_the_folder_again() {
        let (mut app, _) = app();
        let id = poll(&mut app);
        let newest = MessagePage {
            messages: vec![summary("m4", 4), summary("m3", 3)],
            next: Some(mett_core::PageCursor::new("in next")),
        };
        let polled = Polled {
            newest,
            changed: page(Vec::new()),
        };
        let effects = respond(&mut app, id, Event::FolderPolled(Ok(polled)));
        assert!(matches!(
            requests(&effects)[..],
            [(_, Request::ListMessages { .. })]
        ));
        assert_eq!(app.status, NEW_MAIL);
    }

    #[test]
    fn a_message_moved_away_meanwhile_does_not_come_back() {
        let (mut app, _) = app();
        let id = poll(&mut app);
        let gone: HashSet<MessageId> = [MessageId::new("m2")].into();
        app.remove_messages(&gone);
        polled(
            &mut app,
            id,
            vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)],
        );
        assert_eq!(ids(&app), ["m1", "m0"]);
    }

    #[test]
    fn flags_saved_meanwhile_are_kept() {
        let (mut app, _) = app();
        let id = poll(&mut app);
        let effects = app.update_flags(
            MessageId::new("m2"),
            FlagsUpdate {
                read: None,
                flagged: Some(true),
            },
        );
        let (write, _) = requests(&effects)[0].clone();
        respond(&mut app, write, Event::FlagsUpdated(Ok(())));
        polled(
            &mut app,
            id,
            vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)],
        );
        assert!(app.messages[0].flags.flagged);
    }

    #[test]
    fn a_listing_started_meanwhile_wins() {
        let (mut app, _) = app();
        let id = poll(&mut app);
        app.refresh();
        polled(&mut app, id, vec![summary("m9", 9)]);
        assert!(app.messages.is_empty(), "the relist hasn't answered yet");
    }

    #[test]
    fn checks_wait_for_a_sign_in_after_one_failed() {
        let (mut app, _) = app();
        let id = poll(&mut app);
        respond(
            &mut app,
            id,
            Event::FolderPolled(Err(Error::Auth("expired".into()))),
        );
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        assert_eq!(effects, [Effect::Later(MINUTE, Timer::Poll(0))]);
        app.retry();
        poll(&mut app);
    }

    #[test]
    fn no_timer_without_an_interval() {
        let settings = Settings {
            sync_interval: None,
            ..settings()
        };
        let (mut app, effects) = App::open(80, 24, inbox(), settings);
        let (open, _) = requests(&effects)[0].clone();
        let effects = respond(
            &mut app,
            open,
            Event::FolderOpened(Ok(folder("in", "Inbox", 0))),
        );
        assert!(!effects.iter().any(|e| matches!(e, Effect::Later(..))));
        // Nor a check of the mailboxes.
        assert!(
            !requests(&effects)
                .iter()
                .any(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
        );
    }

    #[test]
    fn says_which_mailboxes_have_new_mail() {
        let (mut app, check) = app();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), checked("work", "Work", 0, None)),
            ]),
        );
        // The open folder doesn't count.
        assert_eq!(app.mailboxes_with_new_mail(), 0);
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let (check, request) = requests(&effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
            .unwrap();
        // Checked again by ID.
        assert_eq!(
            request,
            Request::CheckMailboxes(vec![
                (
                    inbox(),
                    Some(FolderId::new("in")),
                    Some(DateTime::UNIX_EPOCH + TimeDelta::minutes(2))
                ),
                (work(), Some(FolderId::new("work")), None),
            ])
        );
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 2, Some(2))),
                (work(), checked("work", "Work", 3, Some(3))),
            ]),
        );
        assert_eq!(app.status, "New mail in =Work.");
        assert_eq!(app.mailboxes_with_new_mail(), 1);

        // `c` offers it.
        app.update(Msg::Action(Action::ChangeFolder));
        let Some(Input::Line { editor, .. }) = &app.input else {
            panic!("expected a question");
        };
        assert_eq!(editor.text(), "=Work");
        app.update(Msg::Key(KeyEvent::from(KeyCode::Enter)));
        let (switch, _) = app.switching.clone().expect("the folder is being opened");
        respond(
            &mut app,
            switch,
            Event::FolderOpened(Ok(folder("work", "Work", 3))),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 0);
    }

    #[test]
    fn mail_that_arrives_after_leaving_is_new_whatever_was_read() {
        let (mut app, check) = app();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), checked("work", "Work", 0, None)),
            ]),
        );
        // m0 is read in the Inbox; then the user leaves for Work, before
        // the change is saved.
        let effects = app.update_flags(
            MessageId::new("m0"),
            FlagsUpdate {
                read: Some(true),
                flagged: None,
            },
        );
        let (write, _) = requests(&effects)[0].clone();
        app.change_folder(work());
        let (switch, _) = app.switching.clone().unwrap();
        let effects = respond(
            &mut app,
            switch,
            Event::FolderOpened(Ok(folder("work", "Work", 0))),
        );
        respond(&mut app, write, Event::FlagsUpdated(Ok(())));
        assert!(
            requests(&effects)
                .iter()
                .all(|(_, r)| !matches!(r, Request::CheckMailboxes(_)))
        );
        // Old unread mail isn't new.
        let check_again = |app: &mut App, newest: i64| {
            let effects = app.update(Msg::Timer(Timer::Poll(0)));
            let (check, _) = requests(&effects)
                .into_iter()
                .find(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
                .unwrap();
            respond(
                app,
                check,
                Event::MailboxesChecked(vec![
                    (inbox(), checked("in", "Inbox", 1, Some(newest))),
                    (work(), checked("work", "Work", 0, None)),
                ]),
            );
        };
        check_again(&mut app, 1);
        assert_eq!(app.mailboxes_with_new_mail(), 0);
        assert!(!app.status.starts_with("New mail"), "{}", app.status);
        // Mail that arrives now is, though the count is as before.
        check_again(&mut app, 5);
        assert_eq!(app.status, "New mail in =@inbox.");
        assert_eq!(app.mailboxes_with_new_mail(), 1);
    }

    #[test]
    fn mail_seen_on_a_visit_is_not_new_after_it() {
        let (mut app, check) = app();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), checked("work", "Work", 0, None)),
            ]),
        );
        // Mail arrives in Work; the user opens it before the next check,
        // and leaves before that check answers.
        app.change_folder(work());
        let (switch, _) = app.switching.clone().unwrap();
        let effects = respond(
            &mut app,
            switch,
            Event::FolderOpened(Ok(folder("work", "Work", 1))),
        );
        let sent = requests(&effects);
        // The new mail is listed: the user sees it.
        let (list, _) = sent
            .iter()
            .find(|(_, r)| matches!(r, Request::ListMessages { .. }))
            .cloned()
            .unwrap();
        let mut arrived = summary("m3", 3);
        arrived.modified = Some(DateTime::UNIX_EPOCH + TimeDelta::minutes(3));
        respond(
            &mut app,
            list,
            Event::MessagesListed(Ok(page(vec![arrived]))),
        );
        app.change_folder(inbox());
        let (switch, _) = app.switching.clone().unwrap();
        respond(
            &mut app,
            switch,
            Event::FolderOpened(Ok(folder("in", "Inbox", 1))),
        );
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let (check, _) = requests(&effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
            .unwrap();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), checked("work", "Work", 1, Some(3))),
            ]),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 0);
        assert!(!app.status.starts_with("New mail"), "{}", app.status);
    }

    #[test]
    fn mail_arriving_after_leaving_is_new_even_if_a_check_straddles_it() {
        let (mut app, first) = app();
        respond(
            &mut app,
            first,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), checked("work", "Work", 0, None)),
            ]),
        );
        // A check starts while the Inbox is open; the user leaves; mail
        // arrives in the Inbox before the check reads it.
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let (check, _) = requests(&effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
            .unwrap();
        app.change_folder(work());
        let (switch, _) = app.switching.clone().unwrap();
        respond(
            &mut app,
            switch,
            Event::FolderOpened(Ok(folder("work", "Work", 0))),
        );
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 2, Some(9))),
                (work(), checked("work", "Work", 0, None)),
            ]),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 1);
    }

    #[test]
    fn mail_a_check_finds_before_it_is_listed_is_not_seen() {
        let (mut app, check) = app();
        // The check finds w9 in Work while it is open, before the listing
        // shows it; the user leaves before it does.
        app.change_folder(work());
        let (switch, _) = app.switching.clone().unwrap();
        respond(
            &mut app,
            switch,
            Event::FolderOpened(Ok(folder("work", "Work", 1))),
        );
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), checked("work", "Work", 1, Some(9))),
            ]),
        );
        app.change_folder(inbox());
        let (switch, _) = app.switching.clone().unwrap();
        respond(
            &mut app,
            switch,
            Event::FolderOpened(Ok(folder("in", "Inbox", 1))),
        );
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let (check, _) = requests(&effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
            .unwrap();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), checked("work", "Work", 1, Some(9))),
            ]),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 1);
    }

    #[test]
    fn a_failure_hidden_by_another_message_is_said_later() {
        let (mut app, check) = app();
        app.status = "Loading Inbox...".into();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), Err(Error::Network("down".into()))),
            ]),
        );
        assert_eq!(app.status, "Loading Inbox...");
        app.status.clear();
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let (check, _) = requests(&effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
            .unwrap();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), Err(Error::Network("down".into()))),
            ]),
        );
        assert!(
            app.status.starts_with("Couldn't check =Work"),
            "{}",
            app.status
        );
    }

    #[test]
    fn an_unchecked_mailbox_has_no_new_mail() {
        let (mut app, check) = app();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), checked("work", "Work", 2, Some(2))),
            ]),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 1);
        let mut settings = settings();
        settings.mailboxes[1].poll = false;
        app.apply_settings(settings);
        assert_eq!(app.mailboxes_with_new_mail(), 0);
    }

    #[test]
    fn mail_found_while_open_but_unseen_is_announced_after_leaving() {
        let (mut app, check) = app();
        // Found while the Inbox is open, before its index shows it.
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 4, Some(9))),
                (work(), checked("work", "Work", 0, None)),
            ]),
        );
        app.change_folder(work());
        let (switch, _) = app.switching.clone().unwrap();
        respond(
            &mut app,
            switch,
            Event::FolderOpened(Ok(folder("work", "Work", 0))),
        );
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let (check, _) = requests(&effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
            .unwrap();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 4, Some(9))),
                (work(), checked("work", "Work", 0, None)),
            ]),
        );
        assert_eq!(app.status, "New mail in =@inbox.");
    }

    #[test]
    fn a_refused_mailbox_is_said_and_checks_go_on() {
        let (mut app, check) = app();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), Err(Error::Auth("access denied".into()))),
            ]),
        );
        assert!(
            app.status.starts_with("Couldn't check =Work"),
            "{}",
            app.status
        );
        poll(&mut app);
    }

    #[test]
    fn a_check_lists_again_what_failed_to_list() {
        let (mut app, _) = app();
        app.unlisted = Some(Page::first(10));
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let requests = requests(&effects);
        assert!(
            !requests
                .iter()
                .any(|(_, r)| matches!(r, Request::PollFolder { .. }))
        );
        assert!(
            requests
                .iter()
                .any(|(_, r)| matches!(r, Request::ListMessages { .. }))
        );
        assert!(app.unlisted.is_none());
    }

    /// Leaves the Inbox for Work, then has a check find the Inbox's
    /// `recent` unread mail.
    fn leave_and_check(app: &mut App, recent: Vec<(&str, i64)>) {
        app.change_folder(work());
        let (switch, _) = app.switching.clone().unwrap();
        respond(
            app,
            switch,
            Event::FolderOpened(Ok(folder("work", "Work", 0))),
        );
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let (check, _) = requests(&effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
            .unwrap();
        let recent = recent
            .into_iter()
            .map(|(id, m)| {
                (
                    MessageId::new(id),
                    DateTime::UNIX_EPOCH + TimeDelta::minutes(m),
                )
            })
            .collect();
        respond(
            app,
            check,
            Event::MailboxesChecked(vec![
                (
                    inbox(),
                    Ok(Checked {
                        folder: folder("in", "Inbox", 3),
                        recent,
                    }),
                ),
                (work(), checked("work", "Work", 0, None)),
            ]),
        );
    }

    #[test]
    fn mail_the_user_changed_is_not_new() {
        let (mut app, check) = app();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 3, Some(2))),
                (work(), checked("work", "Work", 0, None)),
            ]),
        );
        // m1 is flagged: Graph says it changed (at 30).
        let effects = app.update_flags(
            MessageId::new("m1"),
            FlagsUpdate {
                read: None,
                flagged: Some(true),
            },
        );
        let (write, _) = requests(&effects)[0].clone();
        respond(&mut app, write, Event::FlagsUpdated(Ok(())));
        leave_and_check(&mut app, vec![("m1", 30), ("m2", 2)]);
        assert_eq!(app.mailboxes_with_new_mail(), 0);
        // Mail that really is new still counts.
        app.change_folder(inbox());
        let (switch, _) = app.switching.clone().unwrap();
        respond(
            &mut app,
            switch,
            Event::FolderOpened(Ok(folder("in", "Inbox", 3))),
        );
        leave_and_check(&mut app, vec![("m9", 40), ("m1", 30)]);
        assert_eq!(app.mailboxes_with_new_mail(), 1);
    }

    #[test]
    fn mail_arriving_after_a_visit_is_announced_at_the_first_check() {
        // The user leaves the Inbox before the first check answers; mail
        // arrives after what they saw.
        let (mut app, check) = app();
        app.change_folder(work());
        let (switch, _) = app.switching.clone().unwrap();
        respond(
            &mut app,
            switch,
            Event::FolderOpened(Ok(folder("work", "Work", 0))),
        );
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 4, Some(50))),
                (work(), checked("work", "Work", 0, None)),
            ]),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 1);
        assert_eq!(app.status, "New mail in =@inbox.");
    }

    #[test]
    fn what_was_seen_counts_for_any_path_to_the_folder() {
        // The Inbox is open as @inbox; the mailbox names it Inbox. The
        // user leaves before the first check answers.
        let (mut app, check) = app();
        app.mailboxes[0].path = FolderPath::Named(vec!["Inbox".into()]);
        app.change_folder(work());
        let (switch, _) = app.switching.clone().unwrap();
        respond(
            &mut app,
            switch,
            Event::FolderOpened(Ok(folder("work", "Work", 0))),
        );
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (
                    FolderPath::Named(vec!["Inbox".into()]),
                    checked("in", "Inbox", 3, Some(2)),
                ),
                (work(), checked("work", "Work", 0, None)),
            ]),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 0);
    }

    #[test]
    fn a_message_in_both_lists_comes_as_last_changed() {
        let (mut app, _) = app();
        let id = poll(&mut app);
        let unread = summary("m3", 3);
        let mut read = unread.clone();
        read.flags.read = true;
        read.modified = Some(DateTime::UNIX_EPOCH + TimeDelta::minutes(9));
        let polled = Polled {
            newest: page(vec![
                unread,
                summary("m2", 2),
                summary("m1", 1),
                summary("m0", 0),
            ]),
            changed: page(vec![read]),
        };
        respond(&mut app, id, Event::FolderPolled(Ok(polled)));
        assert!(app.messages[0].flags.read);
        assert_ne!(app.status, NEW_MAIL);
    }

    #[test]
    fn a_known_message_in_both_lists_comes_as_last_changed() {
        let (mut app, _) = app();
        let id = poll(&mut app);
        let mut read = summary("m1", 1);
        read.flags.read = true;
        read.modified = Some(DateTime::UNIX_EPOCH + TimeDelta::minutes(9));
        // The changes saw it before it was read.
        let polled = Polled {
            newest: page(vec![summary("m2", 2), read, summary("m0", 0)]),
            changed: page(vec![summary("m1", 1)]),
        };
        respond(&mut app, id, Event::FolderPolled(Ok(polled)));
        assert!(app.messages[1].flags.read);
    }

    #[test]
    fn too_many_changes_to_tell_count_as_new_mail() {
        let (mut app, check) = app();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 3, Some(2))),
                (work(), checked("work", "Work", 0, None)),
            ]),
        );
        let recent: Vec<(&str, i64)> = vec![("m2", 30); mett_core::RECENT_UNREAD];
        leave_and_check(&mut app, recent);
        assert_eq!(app.mailboxes_with_new_mail(), 1);
    }

    /// Has a check find Work's `recent` unread mail.
    fn check_work(app: &mut App, check: RequestId, recent: &[(&str, i64)]) {
        let recent = recent
            .iter()
            .map(|(id, m)| {
                (
                    MessageId::new(*id),
                    DateTime::UNIX_EPOCH + TimeDelta::minutes(*m),
                )
            })
            .collect();
        respond(
            app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (
                    work(),
                    Ok(Checked {
                        folder: folder("work", "Work", 2),
                        recent,
                    }),
                ),
            ]),
        );
    }

    fn next_check(app: &mut App) -> RequestId {
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        requests(&effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
            .unwrap()
            .0
    }

    #[test]
    fn older_mail_coming_into_the_sample_did_not_arrive() {
        let (mut app, check) = app();
        check_work(&mut app, check, &[("m10", 10), ("m9", 9)]);
        app.status.clear();
        // m10 is read elsewhere: m8, older, now makes the sample.
        let check = next_check(&mut app);
        check_work(&mut app, check, &[("m9", 9), ("m8", 8)]);
        assert!(app.status.is_empty(), "{}", app.status);
        // m11 arrives.
        let check = next_check(&mut app);
        check_work(&mut app, check, &[("m11", 11), ("m9", 9)]);
        assert_eq!(app.status, "New mail in =Work.");
    }

    #[test]
    fn new_mail_leaves_an_error_on_the_message_line() {
        let (mut app, check) = app();
        check_work(&mut app, check, &[("m9", 9)]);
        app.fail("The message may have been sent. Check Sent Items before sending it again.");
        let check = next_check(&mut app);
        check_work(&mut app, check, &[("m11", 11), ("m9", 9)]);
        assert!(app.status.starts_with("The message may"), "{}", app.status);
        assert_eq!(app.mailboxes_with_new_mail(), 1);
    }

    #[test]
    fn a_change_to_unread_mail_already_there_is_not_announced() {
        let (mut app, check) = app();
        // Work had unread m5 before the user ever opened it.
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), checked("work", "Work", 1, Some(5))),
            ]),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 1);
        app.status.clear();
        // m5 is flagged in Outlook: it changed, but didn't arrive.
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let (check, _) = requests(&effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
            .unwrap();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (
                    work(),
                    Ok(Checked {
                        folder: folder("work", "Work", 1),
                        recent: vec![(
                            MessageId::new("m5"),
                            DateTime::UNIX_EPOCH + TimeDelta::minutes(30),
                        )],
                    }),
                ),
            ]),
        );
        assert!(app.status.is_empty(), "{}", app.status);
    }

    #[test]
    fn a_failed_first_check_is_no_baseline() {
        let (mut app, check) = app();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), Err(Error::Network("down".into()))),
            ]),
        );
        app.status.clear();
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        let (check, _) = requests(&effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
            .unwrap();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), checked("work", "Work", 2, Some(2))),
            ]),
        );
        // Unread mail it had all along: shown, not announced.
        assert_eq!(app.mailboxes_with_new_mail(), 1);
        assert!(app.status.is_empty(), "{}", app.status);
    }

    #[test]
    fn sent_mail_goes_before_the_drafts() {
        let (mut app, _) = app();
        app.messages.clear();
        app.positions.clear();
        let mut draft = summary("d", 9);
        draft.draft = true;
        app.messages.push(draft.clone());
        app.positions.insert(draft.id.clone(), 0);
        let id = poll(&mut app);
        polled(&mut app, id, vec![summary("moved", -50), draft]);
        assert_eq!(ids(&app), ["moved", "d"]);
    }

    #[test]
    fn a_check_of_mailboxes_since_changed_is_left_aside() {
        let (mut app, check) = app();
        let mut fewer = app.mailboxes.clone();
        fewer.truncate(1);
        app.apply_mailboxes(fewer, app.sync_interval);
        check_work(&mut app, check, &[("m9", 9)]);
        assert_eq!(app.mailboxes_with_new_mail(), 0);
        // The next check is of the mailboxes as they are.
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        assert!(
            requests(&effects)
                .iter()
                .any(|(_, r)| matches!(r, Request::CheckMailboxes(m) if m.len() == 1))
        );
    }

    #[test]
    fn nopoll_counts_for_its_own_entry() {
        let (mut app, check) = app();
        // Work again, by another name, not checked.
        let mut again = app.mailboxes[1].clone();
        again.text = "Work".into();
        again.poll = false;
        app.mailboxes.push(again);
        check_work(&mut app, check, &[("m9", 9)]);
        assert!(app.has_new_mail(&app.mailboxes[1]));
        assert!(!app.has_new_mail(&app.mailboxes[2]));
        assert_eq!(app.mailboxes_with_new_mail(), 1);
    }

    #[test]
    fn the_browser_starts_on_the_mailboxes() {
        let (mut app, check) = app();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), checked("in", "Inbox", 1, Some(1))),
                (work(), Err(Error::not_found("folder", "Work"))),
            ]),
        );
        assert!(app.status.starts_with("Couldn't check =Work"));
        assert!(!app.has_new_mail(&app.mailboxes[1]));
        // New mail elsewhere leaves the failure on the message line.
        app.announce("New mail in =Elsewhere.");
        assert!(app.status.starts_with("Couldn't check =Work"));
        let effects = app.update(Msg::Action(Action::BrowseMailboxes));
        // With fresh counts.
        assert!(matches!(
            requests(&effects)[..],
            [(_, Request::CheckMailboxes(_))]
        ));
        let Screen::Browser(browser) = app.screen() else {
            panic!("expected the browser");
        };
        let names: Vec<_> = browser
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.counts))
            .collect();
        assert_eq!(names, [("=@inbox", Some((1, 0))), ("=Work", None)]);
        let effects = app.update(Msg::Action(Action::ToggleMailboxes));
        assert!(matches!(
            requests(&effects)[..],
            [(_, Request::ListFolders)]
        ));
        let Screen::Browser(browser) = app.screen() else {
            panic!("expected the browser");
        };
        assert!(!browser.mailboxes);
    }

    #[test]
    fn space_cycles_through_mailboxes_with_new_mail() {
        let (mut app, check) = app();
        let mut lists = folder("lists", "Lists", 2);
        lists.id = FolderId::new("lists");
        app.mailboxes.push(MailboxEntry {
            name: "=Lists".into(),
            text: "=Lists".into(),
            path: FolderPath::Named(vec!["Lists".into()]),
            notify: true,
            poll: true,
        });
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (work(), checked("work", "Work", 1, Some(1))),
                (
                    FolderPath::Named(vec!["Lists".into()]),
                    Ok(Checked {
                        folder: lists,
                        recent: vec![(MessageId::new("l"), DateTime::UNIX_EPOCH)],
                    }),
                ),
            ]),
        );
        app.update(Msg::Action(Action::ChangeFolder));
        let text = |app: &App| match &app.input {
            Some(Input::Line { editor, .. }) => editor.text(),
            _ => panic!("expected a question"),
        };
        assert_eq!(text(&app), "=Work");
        app.update(Msg::Key(KeyEvent::from(KeyCode::Char(' '))));
        assert_eq!(text(&app), "=Lists");
        app.update(Msg::Key(KeyEvent::from(KeyCode::Char(' '))));
        assert_eq!(text(&app), "=Work");
    }
}
