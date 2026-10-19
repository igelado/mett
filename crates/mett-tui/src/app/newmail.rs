//! New mail, as in mutt: the open folder is checked every
//! `office365_sync_interval` seconds, and messages that arrived are added to
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

/// Where the open folder's messages were listed (their positions, if
/// any), kept up to date as mail that arrived is placed among them.
pub(super) struct Placing {
    /// Each message's position, whether it is a draft and its date, in
    /// the order listed.
    order: Vec<(Option<usize>, bool, DateTime<Utc>)>,
    /// The positions of the messages in the index, entry by entry.
    shown: Vec<Option<usize>>,
    /// Those of the messages the limit hides.
    hidden: Vec<Option<usize>>,
}

impl Placing {
    /// The messages from position `at` on moved down one.
    fn shift(&mut self, at: usize) {
        let moved = self.order.iter_mut().map(|(listed, _, _)| listed);
        for listed in moved.chain(&mut self.shown).chain(&mut self.hidden) {
            if let Some(listed) = listed
                && *listed >= at
            {
                *listed += 1;
            }
        }
    }
}

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
    /// Its label (`-label`), if it has one.
    pub label: Option<String>,
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

/// How long after watching the folders failed it is tried again.
const WATCH_RETRY: std::time::Duration = std::time::Duration::from_secs(60);

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
        let seen = self.seen.entry(open.id.clone()).or_default();
        let unread = self
            .messages
            .iter()
            .chain(&self.hidden)
            .chain(&self.folded)
            .filter(|m| !m.flags.read);
        for message in unread {
            seen.until = seen.until.max(message.modified);
            // Most were seen at the last page or check.
            if !seen.messages.contains(&message.id) {
                seen.messages.insert(message.id.clone());
            }
        }
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
        let mut effects = match self.sync_interval {
            Some(interval) if !self.poll_armed => {
                self.poll_armed = true;
                vec![Effect::Later(interval, Timer::Poll(self.poll_timer))]
            }
            _ => Vec::new(),
        };
        // Checks wait for the timer; the backend may also tell at once.
        effects.extend(self.watch());
        effects
    }

    /// Watches the open folder and the `mailboxes` found so far for
    /// changes, if the mailbox can tell, and mail is checked for, and isn't
    /// being watched already.
    pub(super) fn watch(&mut self) -> Vec<Effect> {
        // Checks turned off: so is the watch.
        if self.sync_interval.is_none() {
            return self.unwatch();
        }
        // Quitting: nothing new starts.
        if self.quit
            || !self.can_watch
            || self.watch_retry
            || self.sync_interval.is_none()
            || self.poll_paused
            || self.sign_in.is_some()
        {
            return Vec::new();
        }
        let folders = self.to_watch();
        // Watched already, or nothing to watch.
        if self.watching.is_some() && folders == self.watched || folders.is_empty() {
            return Vec::new();
        }
        // Other folders now (the mailboxes found, another folder opened):
        // the watch starts again.
        let mut effects = Vec::new();
        if let Some(old) = self.watching.take() {
            effects.push(self.request(Request::Cancel(old)));
        }
        let watch = self.request(Request::Watch(folders.clone()));
        self.watching = Some(effect_id(&watch));
        self.watched = folders;
        effects.push(watch);
        effects
    }

    /// Stops watching (say, on quitting): the watch under way ends, and
    /// so does what the mailbox keeps on the server for it (an EWS
    /// subscription), rather than linger there until it expires.
    pub fn unwatch(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(old) = self.watching.take() {
            effects.push(self.request(Request::Cancel(old)));
        }
        if self.can_watch && !self.watched.is_empty() {
            self.watched.clear();
            let unwatch = self.request(Request::Watch(Vec::new()));
            self.unwatching = Some(effect_id(&unwatch));
            effects.push(unwatch);
        }
        effects
    }

    /// Whether watching is being stopped ([`Self::unwatch`]).
    pub fn unwatching(&self) -> bool {
        self.unwatching.is_some()
    }

    /// The folders to watch: the open folder, and the `mailboxes` found.
    fn to_watch(&self) -> Vec<mett_core::FolderId> {
        let mut folders: Vec<mett_core::FolderId> = Vec::new();
        let open = self.folder.as_ref().map(|f| f.id.clone());
        let mailboxes = self
            .mailboxes
            .iter()
            .filter(|m| m.poll)
            .filter_map(|m| self.mailbox_state.get(&m.path)?.folder.as_ref())
            .map(|f| f.id.clone());
        for id in open.into_iter().chain(mailboxes) {
            if !folders.contains(&id) {
                folders.push(id);
            }
        }
        folders
    }

    /// The watch `id` ended: folders changed (`changed`), so mail is
    /// checked for at once; or its wait ran out; or it failed, and is
    /// tried again later; or the mailbox can't watch (`None`). Then the
    /// folders are watched again.
    pub(super) fn on_watched(
        &mut self,
        id: RequestId,
        result: Option<mett_core::Result<Vec<mett_core::FolderId>>>,
    ) -> Vec<Effect> {
        if self.unwatching == Some(id) {
            self.unwatching = None;
        }
        if self.watching != Some(id) {
            return Vec::new();
        }
        self.watching = None;
        let mut effects = Vec::new();
        match result {
            None => self.can_watch = false,
            Some(Ok(changed)) => {
                // Only those that changed are checked.
                if self
                    .folder
                    .as_ref()
                    .is_some_and(|f| changed.contains(&f.id))
                {
                    self.recheck_folder = true;
                }
                for folder in changed {
                    if !self.recheck_mailboxes.contains(&folder) {
                        self.recheck_mailboxes.push(folder);
                    }
                }
                effects.extend(self.recheck());
                effects.extend(self.watch());
            }
            // The timer's checks go on meanwhile.
            Some(Err(_)) => {
                self.watch_retry = true;
                effects.push(Effect::Later(WATCH_RETRY, Timer::Watch));
            }
        }
        effects
    }

    /// Checks the folders a watch said changed, as far as checks under
    /// way allow: those still busy (whose check may predate the change)
    /// are checked once it ends.
    pub(super) fn recheck(&mut self) -> Vec<Effect> {
        if self.quit || self.sync_interval.is_none() || self.poll_paused || self.sign_in.is_some() {
            return Vec::new();
        }
        let mut effects = Vec::new();
        if self.recheck_folder && self.polling.is_none() {
            effects.extend(self.poll_folder());
            // Being checked now. (A listing under way may predate the
            // change: it is checked when that ends.)
            if self.polling.is_some() {
                self.recheck_folder = false;
            }
        }
        if self.checking.is_none() && !self.recheck_mailboxes.is_empty() {
            let changed = std::mem::take(&mut self.recheck_mailboxes);
            effects.extend(self.check_mailboxes_in(Some(&changed)));
        }
        effects
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
        self.check_mailboxes_in(None)
    }

    /// Checks the `mailboxes` (with `only`, those of these folders) for new
    /// mail, unless a check is under way.
    fn check_mailboxes_in(&mut self, only: Option<&[mett_core::FolderId]>) -> Vec<Effect> {
        if self.checking.is_some() {
            return Vec::new();
        }
        let mailboxes: Vec<_> = self
            .mailboxes
            .iter()
            .filter(|m| m.poll)
            .filter(|m| {
                only.is_none_or(|only| {
                    self.mailbox_state
                        .get(&m.path)
                        .and_then(|s| s.folder.as_ref())
                        .is_some_and(|f| only.contains(&f.id))
                })
            })
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
        let known: HashSet<&MessageId> = self.folder_messages().map(|m| &m.id).collect();
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
            let known = known.into_iter().cloned().collect();
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
        // page, or gone: asked about.
        let tied: Vec<MessageId> = self
            .folder_messages()
            .filter(|m| !listed.contains(&m.id) && !covered(m))
            .filter(|m| (if m.draft { oldest_draft } else { oldest_sent }) == Some(m.received))
            .filter(|m| !self.transferring(&m.id))
            .map(|m| m.id.clone())
            .collect();
        let mut effects = Vec::new();
        if let Some(open) = self.folder.as_ref().map(|f| f.id.clone())
            && !tied.is_empty()
            && self.tie_check.is_none()
        {
            let request = self.request(Request::StillThere {
                folder: open.clone(),
                ids: tied,
            });
            self.tie_check = Some((effect_id(&request), open));
            effects.push(request);
        }
        let gone: HashSet<MessageId> = self
            .folder_messages()
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
        let mut reorder = false;
        for message in fresh.iter().chain(&changes) {
            match updates.get(&message.id) {
                Some(earlier) if earlier.modified >= message.modified => {}
                _ => {
                    updates.insert(&message.id, message);
                }
            }
        }
        for summary in self
            .messages
            .iter_mut()
            .chain(&mut self.hidden)
            .chain(&mut self.folded)
        {
            if let Some(update) = updates.get(&summary.id) {
                // Read or flagged, it stays where it is; otherwise the
                // index may change order.
                reorder |= summary.received != update.received
                    || summary.delivered != update.delivered
                    || summary.subject != update.subject
                    || summary.from != update.from
                    || summary.to != update.to
                    || summary.thread != update.thread
                    || summary.draft != update.draft;
                *summary = (*update).clone();
            }
        }
        reorder |= !arrived.is_empty();
        self.open_for_new_mail(&arrived);
        if !arrived.is_empty() {
            let mut placing = self.placing();
            for message in arrived {
                self.insert_arrived(message, &mut placing);
            }
        }
        // In order, and in their threads (only if anything changed: a
        // large folder takes a while).
        if reorder {
            self.arrange();
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
        effects
    }

    /// Takes out of the index the messages a check found gone from the
    /// folder (see [`Request::StillThere`]).
    pub(super) fn on_still_there(
        &mut self,
        id: RequestId,
        result: mett_core::Result<Vec<MessageId>>,
    ) -> Vec<Effect> {
        let Some((_, folder)) = self.tie_check.take_if(|(request, _)| *request == id) else {
            return Vec::new();
        };
        // (A failure is asked about again at the next check.)
        let Ok(gone) = result else {
            return Vec::new();
        };
        if self.folder.as_ref().is_none_or(|f| f.id != folder) {
            return Vec::new();
        }
        let gone: HashSet<MessageId> = gone
            .into_iter()
            .filter(|id| !self.transferring(id))
            .collect();
        self.remove_messages(&gone);
        Vec::new()
    }

    /// Where the folder's messages were listed, for placing those that
    /// arrive.
    fn placing(&self) -> Placing {
        let at: Vec<Option<usize>> = self
            .folder_messages()
            .map(|m| self.positions.get(&m.id).copied())
            .collect();
        let mut order: Vec<(Option<usize>, bool, DateTime<Utc>)> = self
            .folder_messages()
            .zip(&at)
            .map(|(m, at)| (*at, m.draft, m.received))
            .collect();
        order.sort_by_key(|(at, _, _)| at.unwrap_or(usize::MAX));
        let (shown, rest) = at.split_at(self.messages.len());
        Placing {
            order,
            shown: shown.to_vec(),
            hidden: rest[..self.hidden.len()].to_vec(),
        }
    }

    /// Adds a message that arrived to the index, where its date puts it
    /// in the folder's order, or aside if the index is limited and it
    /// doesn't match. `placing` is where the folder's messages are,
    /// including those that arrived before it.
    fn insert_arrived(&mut self, message: MessageSummary, placing: &mut Placing) {
        let position = |at: &Option<usize>| at.unwrap_or(usize::MAX);
        let order = &placing.order;
        // Among the messages like it (drafts are a group of their own):
        // before the first older one, or after the last of them.
        let alike = || order.iter().filter(|(_, draft, _)| *draft == message.draft);
        // Graph lists unsent drafts after the rest.
        let at = alike()
            .find(|(_, _, received)| *received < message.received)
            .map(|(at, _, _)| position(at))
            .or_else(|| {
                alike()
                    .next_back()
                    .map(|(at, _, _)| position(at).saturating_add(1))
            })
            .or_else(|| {
                let first_draft = order.iter().find(|(_, draft, _)| *draft);
                first_draft
                    .map(|(at, _, _)| position(at))
                    .filter(|_| !message.draft)
            });
        let at = match at {
            Some(at) => {
                // Those after it move down one.
                for later in self.positions.values_mut().filter(|p| **p >= at) {
                    *later += 1;
                }
                placing.shift(at);
                at
            }
            None => self.positions.values().max().map_or(0, |last| last + 1),
        };
        self.positions.insert(message.id.clone(), at);
        let entry = (Some(at), message.draft, message.received);
        let index = placing
            .order
            .partition_point(|(listed, _, _)| position(listed) < at);
        placing.order.insert(index, entry);
        let (list, positions) = if self.limit_shows(&message) {
            (&mut self.messages, &mut placing.shown)
        } else {
            (&mut self.hidden, &mut placing.hidden)
        };
        let index = positions
            .iter()
            .filter(|p| p.is_some_and(|p| p < at))
            .count();
        list.insert(index, message);
        positions.insert(index, Some(at));
    }

    /// The folder was listed again because a check found more than it
    /// could take in: says if mail arrived meanwhile.
    pub(super) fn relisted(&mut self) {
        let Some(known) = self.known_before_refresh.take() else {
            return;
        };
        let arrived: Vec<MessageSummary> = self
            .folder_messages()
            .filter(|m| !m.flags.read && !known.contains(&m.id))
            .cloned()
            .collect();
        if !arrived.is_empty() {
            self.announce(NEW_MAIL);
            // Its threads open, as for mail a check takes in.
            if self.open_for_new_mail(&arrived) {
                self.arrange();
            }
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
            let seen = found.and_then(|f| self.seen.get(&f.id));
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
        self.rebuild_sidebar(false);
        // The mailboxes found are watched too.
        self.watch()
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
        self.rebuild_sidebar(true);
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
    pub(super) fn has_new_mail(&self, mailbox: &MailboxEntry) -> bool {
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
        let browsing = self
            .screens
            .iter()
            .any(|screen| matches!(screen, Screen::Browser(browser) if browser.mailboxes));
        if !browsing {
            return;
        }
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
        self.rebuild_sidebar(false);
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
    use crate::app::testing::*;
    use crate::app::{Input, Msg, Settings};
    use crate::worker::Event;
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
            label: None,
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
        let messages = vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)];
        listed(&mut app, sent[0].0, messages, None);
        (app, sent[1].0)
    }

    fn unsorted() -> crate::app::Ordering {
        crate::app::Ordering {
            sort: crate::sort::Sort::parse("unsorted", true).unwrap(),
            ..crate::app::Ordering::default()
        }
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

    /// A page with more to come after it, at `next`.
    fn more(messages: Vec<MessageSummary>, next: &str) -> MessagePage {
        MessagePage {
            messages,
            next: Some(mett_core::PageCursor::new(next)),
        }
    }

    /// Answers the check of the open folder with its `newest` messages
    /// and those `changed` since the last check.
    fn polled_with(
        app: &mut App,
        id: RequestId,
        newest: MessagePage,
        changed: MessagePage,
    ) -> Vec<Effect> {
        respond(app, id, Event::FolderPolled(Ok(Polled { newest, changed })))
    }

    /// Answers the check of the open folder with its newest `messages`,
    /// and no changes.
    fn polled(app: &mut App, id: RequestId, messages: Vec<MessageSummary>) -> Vec<Effect> {
        polled_with(app, id, page(messages), page(Vec::new()))
    }

    /// Fires the poll timer; returns the check of the mailboxes it starts.
    fn checking(app: &mut App) -> (RequestId, Request) {
        let effects = app.update(Msg::Timer(Timer::Poll(0)));
        requests(&effects)
            .into_iter()
            .find(|(_, r)| matches!(r, Request::CheckMailboxes(_)))
            .unwrap()
    }

    fn next_check(app: &mut App) -> RequestId {
        checking(app).0
    }

    /// Answers a check of the mailboxes with what it found in the Inbox
    /// and in Work.
    fn check_both(
        app: &mut App,
        id: RequestId,
        inbox_found: mett_core::Result<Checked>,
        work_found: mett_core::Result<Checked>,
    ) -> Vec<Effect> {
        let found = vec![(inbox(), inbox_found), (work(), work_found)];
        respond(app, id, Event::MailboxesChecked(found))
    }

    /// Unread mail found by a check: each message and its minute.
    fn recent(messages: &[(&str, i64)]) -> Vec<(MessageId, DateTime<Utc>)> {
        messages
            .iter()
            .map(|(id, m)| {
                (
                    MessageId::new(*id),
                    DateTime::UNIX_EPOCH + TimeDelta::minutes(*m),
                )
            })
            .collect()
    }

    /// Opens the folder at `path`, found as `found`.
    fn switch_to(app: &mut App, path: FolderPath, found: Folder) -> Vec<Effect> {
        app.change_folder(path);
        let (switch, _) = app.switching.clone().unwrap();
        respond(app, switch, Event::FolderOpened(Ok(found)))
    }

    /// A draft dated (and last changed) at `minutes`.
    fn draft(id: &str, minutes: i64) -> MessageSummary {
        let mut draft = summary(id, minutes);
        draft.draft = true;
        draft
    }

    #[test]
    fn folders_are_checked_as_soon_as_they_change() {
        let (mut app, _) = app();
        let inbox = mett_core::FolderId::new("in");
        // Opened, the Inbox is watched.
        let watch = app.watching.expect("the folder is watched");
        // A change: it is checked at once, and watched again.
        let effects = respond(
            &mut app,
            watch,
            Event::Watched(Some(Ok(vec![inbox.clone()]))),
        );
        let sent = requests(&effects);
        assert!(
            sent.iter()
                .any(|(_, r)| matches!(r, Request::PollFolder { .. })),
            "{sent:?}"
        );
        assert!(
            sent.iter()
                .any(|(_, r)| matches!(r, Request::Watch(f) if f == std::slice::from_ref(&inbox))),
            "{sent:?}"
        );
        // Its wait ran out: watched again, nothing checked.
        let watch = app.watching.expect("watched again");
        let effects = respond(&mut app, watch, Event::Watched(Some(Ok(Vec::new()))));
        assert!(matches!(requests(&effects)[..], [(_, Request::Watch(_))]));
        // It failed: tried again later (the timer's checks go on).
        let watch = app.watching.expect("watched again");
        let failed = Event::Watched(Some(Err(mett_core::Error::Network("down".into()))));
        let effects = respond(&mut app, watch, failed);
        assert_eq!(effects, [Effect::Later(WATCH_RETRY, Timer::Watch)]);
        assert!(
            app.update(Msg::Timer(Timer::Poll(0)))
                .iter()
                .all(|e| { !matches!(e, Effect::Request(_, Request::Watch(_))) })
        );
        let effects = app.update(Msg::Timer(Timer::Watch));
        assert!(matches!(requests(&effects)[..], [(_, Request::Watch(_))]));
        // The mailbox can't watch: never again.
        let watch = app.watching.expect("watched again");
        assert!(respond(&mut app, watch, Event::Watched(None)).is_empty());
        assert!(
            app.update(Msg::Timer(Timer::Poll(0)))
                .iter()
                .all(|e| { !matches!(e, Effect::Request(_, Request::Watch(_))) })
        );
    }

    #[test]
    fn the_watch_follows_the_folders_and_checks_only_those_changed() {
        let (mut app, check) = app();
        let inbox = mett_core::FolderId::new("in");
        let work = mett_core::FolderId::new("wk");
        let first = app.watching.expect("the folder is watched");
        // The mailboxes found, they are watched too: the watch starts
        // again.
        let effects = check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, None),
            checked("wk", "Work", 0, None),
        );
        let sent = requests(&effects);
        assert!(
            sent.iter()
                .any(|(_, r)| matches!(r, Request::Cancel(id) if *id == first)),
            "{sent:?}"
        );
        assert!(
            sent.iter().any(
                |(_, r)| matches!(r, Request::Watch(f) if f == &[inbox.clone(), work.clone()])
            ),
            "{sent:?}"
        );
        // The same folders: not again.
        assert!(app.watch().is_empty());
        // The Inbox changed: its index is checked, and its counts (it is
        // one of the mailboxes too).
        let watch = app.watching.expect("watched");
        let effects = respond(
            &mut app,
            watch,
            Event::Watched(Some(Ok(vec![inbox.clone()]))),
        );
        let sent = requests(&effects);
        assert!(
            sent.iter()
                .any(|(_, r)| matches!(r, Request::PollFolder { .. })),
            "{sent:?}"
        );
        let check = sent
            .iter()
            .find_map(|(id, r)| match r {
                Request::CheckMailboxes(m) if m.len() == 1 && m[0].0 == super::tests::inbox() => {
                    Some(*id)
                }
                _ => None,
            })
            .expect("the Inbox's counts are checked");
        respond(&mut app, check, Event::MailboxesChecked(Vec::new()));
        // Work changed: only it is checked.
        let watch = app.watching.expect("watched");
        let effects = respond(
            &mut app,
            watch,
            Event::Watched(Some(Ok(vec![work.clone()]))),
        );
        let sent = requests(&effects);
        assert!(
            !sent
                .iter()
                .any(|(_, r)| matches!(r, Request::PollFolder { .. })),
            "the Inbox didn't change: {sent:?}"
        );
        assert!(
            sent.iter().any(|(_, r)| matches!(
                r,
                Request::CheckMailboxes(m) if m.len() == 1 && m[0].0 == super::tests::work()
            )),
            "{sent:?}"
        );
    }

    #[test]
    fn a_change_during_a_check_is_checked_after_it() {
        let (mut app, _) = app();
        let inbox = mett_core::FolderId::new("in");
        // A check is under way when the watch says the Inbox changed.
        let check = poll(&mut app);
        let watch = app.watching.expect("watched");
        let effects = respond(&mut app, watch, Event::Watched(Some(Ok(vec![inbox]))));
        assert!(
            !requests(&effects)
                .iter()
                .any(|(_, r)| matches!(r, Request::PollFolder { .. })),
            "one at a time"
        );
        // It may predate the change: once it ends, the Inbox is checked
        // again.
        let effects = polled(&mut app, check, Vec::new());
        assert!(
            requests(&effects)
                .iter()
                .any(|(_, r)| matches!(r, Request::PollFolder { .. })),
            "{effects:?}"
        );
    }

    #[test]
    fn turning_checks_off_ends_the_watch() {
        let (mut app, _) = app();
        let watch = app.watching.expect("watched");
        app.sync_interval = None;
        // The watch ends, and so does what the mailbox keeps for it.
        let effects = app.watch();
        let sent = requests(&effects);
        assert!(
            matches!(&sent[..], [(_, Request::Cancel(id)), (_, Request::Watch(f))]
                if *id == watch && f.is_empty()),
            "{sent:?}"
        );
        assert!(app.watching.is_none());
        assert!(app.unwatching());
        // Once.
        assert!(app.watch().is_empty());
        let unwatch = effect_id(&effects[1]);
        assert!(respond(&mut app, unwatch, Event::Watched(Some(Ok(Vec::new())))).is_empty());
        assert!(!app.unwatching());
    }

    #[test]
    fn nothing_is_watched_nor_checked_after_quitting() {
        let (mut app, _) = app();
        app.quit = true;
        let effects = app.unwatch();
        assert!(app.unwatching(), "{effects:?}");
        // Say, a sign-in a save needed ends: no watch starts again.
        app.recheck_folder = true;
        assert!(app.watch().is_empty());
        assert!(app.recheck().is_empty());
    }

    #[test]
    fn a_message_missing_at_the_page_s_end_is_asked_about() {
        let (mut app, _) = app();
        // m0 is as old as m1, the page's last: on the next page, or gone.
        app.messages.iter_mut().for_each(|m| {
            if m.id.as_str() == "m0" {
                m.received = DateTime::UNIX_EPOCH + TimeDelta::minutes(1);
            }
        });
        let partial = |app: &mut App| {
            let id = poll(app);
            let newest = more(vec![summary("m2", 2), summary("m1", 1)], "in more");
            polled_with(app, id, newest, page(Vec::new()))
        };
        let still_there = |effects: &[Effect]| {
            requests(effects)
                .into_iter()
                .find(|(_, r)| matches!(r, Request::StillThere { ids, .. } if ids == &[MessageId::new("m0")]))
                .map(|(id, _)| id)
        };
        // There still: kept, and asked about again at the next check.
        let effects = partial(&mut app);
        let check = still_there(&effects).expect("m0 is asked about");
        respond(&mut app, check, Event::StillThere(Ok(Vec::new())));
        assert_eq!(ids(&app), ["m2", "m1", "m0"]);
        // Gone since: taken out.
        let effects = partial(&mut app);
        let check = still_there(&effects).expect("m0 is asked about again");
        respond(
            &mut app,
            check,
            Event::StillThere(Ok(vec![MessageId::new("m0")])),
        );
        assert_eq!(ids(&app), ["m2", "m1"]);
    }

    #[test]
    fn new_mail_joins_the_index_where_its_date_puts_it() {
        let (mut app, _) = app();
        act(&mut app, Action::NextEntry);
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
        polled_with(
            &mut app,
            id,
            page(vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)]),
            page(vec![summary("late", -5)]),
        );
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
        polled_with(
            &mut app,
            id,
            more(vec![summary("m1", 1)], "in next"),
            page(Vec::new()),
        );
        assert_eq!(ids(&app), ["m1", "m0"]);
        assert!(app.status.is_empty());
    }

    #[test]
    fn a_draft_beyond_the_newest_page_stays() {
        let (mut app, _) = app();
        // In the order the folder is listed in.
        app.apply_ordering(unsorted());
        let id = poll(&mut app);
        // A draft created after m1 was sent, but listed after the sent
        // mail, as Graph sorts unsent drafts.
        let draft = draft("d", 5);
        app.messages.push(draft);
        polled_with(
            &mut app,
            id,
            more(vec![summary("m2", 2), summary("m1", 1)], "in next"),
            page(Vec::new()),
        );
        assert_eq!(ids(&app), ["m2", "m1", "m0", "d"]);
    }

    #[test]
    fn drafts_are_placed_and_removed_among_drafts() {
        let (mut app, _) = app();
        // In the order the folder is listed in.
        app.apply_ordering(unsorted());
        // The drafts were listed after the sent mail.
        app.messages.push(draft("d2", 9));
        app.messages.push(draft("d1", 8));
        app.positions.insert(MessageId::new("d2"), 3);
        app.positions.insert(MessageId::new("d1"), 4);
        let id = poll(&mut app);
        // d3 is new, and d2 went; d1 may be on the next page.
        polled_with(
            &mut app,
            id,
            more(
                vec![
                    summary("m2", 2),
                    summary("m1", 1),
                    summary("m0", 0),
                    draft("d3", 10),
                    draft("d1", 8),
                ],
                "in next",
            ),
            page(Vec::new()),
        );
        assert_eq!(ids(&app), ["m2", "m1", "m0", "d3", "d1"]);
    }

    #[test]
    fn changes_older_than_the_listing_need_no_listing_again() {
        let (mut app, _) = app();
        let id = poll(&mut app);
        let effects = polled_with(
            &mut app,
            id,
            page(vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)]),
            more(vec![summary("m1", 1)], "in more"),
        );
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
            Event::MailboxesChecked(vec![(inbox(), Err(expired()))]),
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
        switch_to(&mut app, work(), folder("work", "Work", 0));
        // The Inbox index had m2 (dated 2) unread when the user left.
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 3, Some(2)),
            checked("work", "Work", 0, None),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 0);
    }

    #[test]
    fn another_folder_at_a_mailbox_s_path_starts_afresh() {
        let (mut app, check) = app();
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            checked("work", "Work", 0, None),
        );
        app.seen.insert(
            FolderId::new("work"),
            Seen {
                until: Some(DateTime::UNIX_EPOCH + TimeDelta::minutes(50)),
                messages: HashSet::new(),
            },
        );
        let check = next_check(&mut app);
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            checked("work2", "Work", 1, Some(3)),
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
        let effects = polled_with(
            &mut app,
            id,
            page(vec![summary("m2", 2), summary("m1", 1), summary("m0", 0)]),
            more(vec![recent], "in more"),
        );
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
        // In the order the folder is listed in.
        app.apply_ordering(unsorted());
        // Listed out of date order, as drafts can be.
        let draft = draft("d", 9);
        app.messages.push(draft.clone());
        app.positions.insert(draft.id.clone(), 3);
        act(&mut app, Action::Limit);
        typed(&mut app, "~s m2");
        key(&mut app, KeyCode::Enter);
        assert_eq!(ids(&app), ["m2"]);
        let id = poll(&mut app);
        polled_with(
            &mut app,
            id,
            more(
                vec![summary("m3", 3), summary("m2", 2), summary("m1", 1)],
                "in next",
            ),
            page(Vec::new()),
        );
        act(&mut app, Action::Limit);
        // The question starts with the limit in force.
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('u'),
            ratatui::crossterm::event::KeyModifiers::CONTROL,
        )));
        typed(&mut app, "all");
        key(&mut app, KeyCode::Enter);
        assert_eq!(ids(&app), ["m3", "m2", "m1", "m0", "d"]);
    }

    #[test]
    fn a_full_page_of_new_mail_lists_the_folder_again() {
        let (mut app, _) = app();
        let id = poll(&mut app);
        let effects = polled_with(
            &mut app,
            id,
            more(vec![summary("m4", 4), summary("m3", 3)], "in next"),
            page(Vec::new()),
        );
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
        let effects = app.update_flags(MessageId::new("m2"), FlagsUpdate::flagged(true));
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
        respond(&mut app, id, Event::FolderPolled(Err(expired())));
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
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            checked("work", "Work", 0, None),
        );
        // The open folder doesn't count.
        assert_eq!(app.mailboxes_with_new_mail(), 0);
        let (check, request) = checking(&mut app);
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
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 2, Some(2)),
            checked("work", "Work", 3, Some(3)),
        );
        assert_eq!(app.status, "New mail in =Work.");
        assert_eq!(app.mailboxes_with_new_mail(), 1);

        // `c` offers it.
        act(&mut app, Action::ChangeFolder);
        let Some(Input::Line { editor, .. }) = &app.input else {
            panic!("expected a question");
        };
        assert_eq!(editor.text(), "=Work");
        key(&mut app, KeyCode::Enter);
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
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            checked("work", "Work", 0, None),
        );
        // m0 is read in the Inbox; then the user leaves for Work, before
        // the change is saved.
        let effects = app.update_flags(MessageId::new("m0"), FlagsUpdate::read(true));
        let (write, _) = requests(&effects)[0].clone();
        let effects = switch_to(&mut app, work(), folder("work", "Work", 0));
        respond(&mut app, write, Event::FlagsUpdated(Ok(())));
        assert!(
            requests(&effects)
                .iter()
                .all(|(_, r)| !matches!(r, Request::CheckMailboxes(_)))
        );
        // Old unread mail isn't new.
        let check_again = |app: &mut App, newest: i64| {
            let check = next_check(app);
            check_both(
                app,
                check,
                checked("in", "Inbox", 1, Some(newest)),
                checked("work", "Work", 0, None),
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
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            checked("work", "Work", 0, None),
        );
        // Mail arrives in Work; the user opens it before the next check,
        // and leaves before that check answers.
        let effects = switch_to(&mut app, work(), folder("work", "Work", 1));
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
        switch_to(&mut app, inbox(), folder("in", "Inbox", 1));
        let check = next_check(&mut app);
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            checked("work", "Work", 1, Some(3)),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 0);
        assert!(!app.status.starts_with("New mail"), "{}", app.status);
    }

    #[test]
    fn mail_arriving_after_leaving_is_new_even_if_a_check_straddles_it() {
        let (mut app, first) = app();
        check_both(
            &mut app,
            first,
            checked("in", "Inbox", 1, Some(1)),
            checked("work", "Work", 0, None),
        );
        // A check starts while the Inbox is open; the user leaves; mail
        // arrives in the Inbox before the check reads it.
        let check = next_check(&mut app);
        switch_to(&mut app, work(), folder("work", "Work", 0));
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 2, Some(9)),
            checked("work", "Work", 0, None),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 1);
    }

    #[test]
    fn mail_a_check_finds_before_it_is_listed_is_not_seen() {
        let (mut app, check) = app();
        // The check finds w9 in Work while it is open, before the listing
        // shows it; the user leaves before it does.
        switch_to(&mut app, work(), folder("work", "Work", 1));
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            checked("work", "Work", 1, Some(9)),
        );
        switch_to(&mut app, inbox(), folder("in", "Inbox", 1));
        let check = next_check(&mut app);
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            checked("work", "Work", 1, Some(9)),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 1);
    }

    #[test]
    fn a_failure_hidden_by_another_message_is_said_later() {
        let (mut app, check) = app();
        app.status = "Loading Inbox...".into();
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            Err(Error::Network("down".into())),
        );
        assert_eq!(app.status, "Loading Inbox...");
        app.status.clear();
        let check = next_check(&mut app);
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            Err(Error::Network("down".into())),
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
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            checked("work", "Work", 2, Some(2)),
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
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 4, Some(9)),
            checked("work", "Work", 0, None),
        );
        switch_to(&mut app, work(), folder("work", "Work", 0));
        let check = next_check(&mut app);
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 4, Some(9)),
            checked("work", "Work", 0, None),
        );
        assert_eq!(app.status, "New mail in =@inbox.");
    }

    #[test]
    fn a_refused_mailbox_is_said_and_checks_go_on() {
        let (mut app, check) = app();
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            Err(Error::Auth("access denied".into())),
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
    fn leave_and_check(app: &mut App, found: Vec<(&str, i64)>) {
        switch_to(app, work(), folder("work", "Work", 0));
        let check = next_check(app);
        check_both(
            app,
            check,
            Ok(Checked {
                folder: folder("in", "Inbox", 3),
                recent: recent(&found),
            }),
            checked("work", "Work", 0, None),
        );
    }

    #[test]
    fn mail_the_user_changed_is_not_new() {
        let (mut app, check) = app();
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 3, Some(2)),
            checked("work", "Work", 0, None),
        );
        // m1 is flagged: Graph says it changed (at 30).
        let effects = app.update_flags(MessageId::new("m1"), FlagsUpdate::flagged(true));
        let (write, _) = requests(&effects)[0].clone();
        respond(&mut app, write, Event::FlagsUpdated(Ok(())));
        leave_and_check(&mut app, vec![("m1", 30), ("m2", 2)]);
        assert_eq!(app.mailboxes_with_new_mail(), 0);
        // Mail that really is new still counts.
        switch_to(&mut app, inbox(), folder("in", "Inbox", 3));
        leave_and_check(&mut app, vec![("m9", 40), ("m1", 30)]);
        assert_eq!(app.mailboxes_with_new_mail(), 1);
    }

    #[test]
    fn mail_arriving_after_a_visit_is_announced_at_the_first_check() {
        // The user leaves the Inbox before the first check answers; mail
        // arrives after what they saw.
        let (mut app, check) = app();
        switch_to(&mut app, work(), folder("work", "Work", 0));
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 4, Some(50)),
            checked("work", "Work", 0, None),
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
        switch_to(&mut app, work(), folder("work", "Work", 0));
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
        polled_with(
            &mut app,
            id,
            page(vec![
                unread,
                summary("m2", 2),
                summary("m1", 1),
                summary("m0", 0),
            ]),
            page(vec![read]),
        );
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
        polled_with(
            &mut app,
            id,
            page(vec![summary("m2", 2), read, summary("m0", 0)]),
            page(vec![summary("m1", 1)]),
        );
        assert!(app.messages[1].flags.read);
    }

    #[test]
    fn too_many_changes_to_tell_count_as_new_mail() {
        let (mut app, check) = app();
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 3, Some(2)),
            checked("work", "Work", 0, None),
        );
        let recent: Vec<(&str, i64)> = vec![("m2", 30); mett_core::RECENT_UNREAD];
        leave_and_check(&mut app, recent);
        assert_eq!(app.mailboxes_with_new_mail(), 1);
    }

    /// Has a check find Work's `recent` unread mail.
    fn check_work(app: &mut App, check: RequestId, found: &[(&str, i64)]) {
        check_both(
            app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            Ok(Checked {
                folder: folder("work", "Work", 2),
                recent: recent(found),
            }),
        );
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
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            checked("work", "Work", 1, Some(5)),
        );
        assert_eq!(app.mailboxes_with_new_mail(), 1);
        app.status.clear();
        // m5 is flagged in Outlook: it changed, but didn't arrive.
        let check = next_check(&mut app);
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            Ok(Checked {
                folder: folder("work", "Work", 1),
                recent: recent(&[("m5", 30)]),
            }),
        );
        assert!(app.status.is_empty(), "{}", app.status);
    }

    #[test]
    fn a_failed_first_check_is_no_baseline() {
        let (mut app, check) = app();
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            Err(Error::Network("down".into())),
        );
        app.status.clear();
        let check = next_check(&mut app);
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            checked("work", "Work", 2, Some(2)),
        );
        // Unread mail it had all along: shown, not announced.
        assert_eq!(app.mailboxes_with_new_mail(), 1);
        assert!(app.status.is_empty(), "{}", app.status);
    }

    #[test]
    fn sent_mail_goes_before_the_drafts() {
        let (mut app, _) = app();
        // In the order the folder is listed in.
        app.apply_ordering(unsorted());
        app.messages.clear();
        app.positions.clear();
        let draft = draft("d", 9);
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
        check_both(
            &mut app,
            check,
            checked("in", "Inbox", 1, Some(1)),
            Err(Error::not_found("folder", "Work")),
        );
        assert!(app.status.starts_with("Couldn't check =Work"));
        assert!(!app.has_new_mail(&app.mailboxes[1]));
        // New mail elsewhere leaves the failure on the message line.
        app.announce("New mail in =Elsewhere.");
        assert!(app.status.starts_with("Couldn't check =Work"));
        let effects = act(&mut app, Action::BrowseMailboxes);
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
        let effects = act(&mut app, Action::ToggleMailboxes);
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
            label: None,
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
        act(&mut app, Action::ChangeFolder);
        let text = |app: &App| match &app.input {
            Some(Input::Line { editor, .. }) => editor.text(),
            _ => panic!("expected a question"),
        };
        assert_eq!(text(&app), "=Work");
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(text(&app), "=Lists");
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(text(&app), "=Work");
    }
}
