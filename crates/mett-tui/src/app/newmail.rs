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
        // Nor while watching is being ended: that could end the new watch.
        if self.quit
            || !self.can_watch
            || self.watch_retry
            || self.watch_paused
            || self.unwatching.is_some()
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
        self.watch_epoch = self.poll_epoch;
        // What was left to end, the mailbox ends first.
        self.unwatch_failed = false;
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
        if self.can_watch
            && (!self.watched.is_empty() || self.unwatch_failed)
            && self.unwatching.is_none()
        {
            self.watched.clear();
            // Watching's sign-in failed: ended once the user signs in, not
            // to ask them again unbidden.
            if self.watch_paused {
                self.unwatch_failed = true;
                return effects;
            }
            self.unwatch_failed = false;
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
            // Tried again later (a watch would end it too), unless
            // quitting; after a failed sign-in, once the user signs in, not
            // to ask them again unbidden.
            return match result {
                Some(Err(_)) if self.quit => Vec::new(),
                Some(Err(Error::Auth(_))) => {
                    self.unwatch_failed = true;
                    self.watch_paused = true;
                    Vec::new()
                }
                Some(Err(_)) => {
                    self.unwatch_failed = true;
                    vec![Effect::Later(WATCH_RETRY, Timer::Watch)]
                }
                // Checks may have been turned on again meanwhile.
                _ => self.watch(),
            };
        }
        if self.watching != Some(id) {
            // A watch replaced as it ended: what it said changed is not
            // told again.
            return match result {
                Some(Ok(changed)) if !changed.is_empty() => {
                    self.note_changed(changed);
                    self.recheck()
                }
                _ => Vec::new(),
            };
        }
        self.watching = None;
        let mut effects = Vec::new();
        match result {
            None => self.can_watch = false,
            Some(Ok(changed)) => {
                self.note_changed(changed);
                effects.extend(self.recheck());
                effects.extend(self.watch());
            }
            // A sign-in failed (say, the user dismissed it): held off
            // until the user signs in again, rather than ask again
            // unbidden. (Unless the user has since.) Checks go on: the
            // mailbox may only refuse to be watched.
            Some(Err(Error::Auth(_))) if self.watch_epoch == self.poll_epoch => {
                self.watch_paused = true;
            }
            // The timer's checks go on meanwhile.
            Some(Err(_)) => {
                self.watch_retry = true;
                effects.push(Effect::Later(WATCH_RETRY, Timer::Watch));
            }
        }
        effects
    }

    /// Notes the folders a watch said `changed`, to be checked: only
    /// those.
    fn note_changed(&mut self, changed: Vec<mett_core::FolderId>) {
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
            self.sidebar_outdated();
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
    pub(super) fn background_error(&mut self, e: Error, epoch: u64) {
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
        // The account's tab says so too, until it is looked at.
        self.arrived = true;
        if !self.status.is_error() {
            self.status.info(text);
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
        // The folder left may have been read.
        self.sidebar_outdated();
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
                    well_known: folder.and_then(|f| f.well_known),
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
        // Without them, the sidebar lists every folder: they may have
        // changed since it last did.
        if mailboxes.is_empty() && !self.mailboxes.is_empty() {
            self.sidebar_outdated();
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
mod tests;
