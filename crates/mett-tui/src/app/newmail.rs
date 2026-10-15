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
use crate::worker::{Polled, Request, RequestId};

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
    /// Whether mail arrived since the user last had it open (or, at
    /// first, whether it has unread mail).
    pub(super) new: bool,
    /// Why it couldn't be checked, if it couldn't.
    pub(super) error: Option<String>,
}

impl App {
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
        let folder = folder.id.clone();
        let started = Utc::now();
        let since = self.poll_since.unwrap_or(started) - OVERLAP;
        let request = self.request(Request::PollFolder {
            folder: folder.clone(),
            page: Page::first(self.page_size),
            since,
        });
        self.polling = Some(Polling {
            request: effect_id(&request),
            folder,
            started,
        });
        vec![request]
    }

    /// The folder is being listed from the start: later checks look for
    /// changes from now on.
    pub(super) fn listing_started(&mut self) {
        // The listing shows what a check on its way would find.
        self.polling = None;
        self.poll_since = Some(Utc::now());
    }

    /// Reads the counts of the mailboxes to check, unless that is on its
    /// way already.
    pub(super) fn check_mailboxes(&mut self) -> Vec<Effect> {
        if self.checking.is_some() {
            return Vec::new();
        }
        let mailboxes: Vec<(FolderPath, Option<mett_core::FolderId>)> = self
            .mailboxes
            .iter()
            .filter(|m| m.poll)
            .map(|m| {
                let id = self
                    .mailbox_state
                    .get(&m.path)
                    .and_then(|s| s.folder.as_ref())
                    .map(|f| f.id.clone());
                (m.path.clone(), id)
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
            folder, started, ..
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
                self.background_error(e);
                return Vec::new();
            }
        };
        // The next check asks for what changed after this one began.
        self.poll_since = Some(started);
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
    fn background_error(&mut self, e: Error) {
        if matches!(e, Error::Auth(_)) {
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
        let mut seen = HashSet::new();
        let arrived: Vec<MessageSummary> = fresh
            .iter()
            .chain(&changes)
            .filter(|m| !known.contains(&m.id) && seen.insert(m.id.clone()))
            .cloned()
            .collect();
        let unread = arrived.iter().any(|m| !m.flags.read);
        // More arrived than a page holds, or more changed: list the whole
        // folder again.
        let all_new = !fresh.is_empty() && fresh.iter().all(|m| !known.contains(&m.id));
        if (!complete && all_new) || changed.next.is_some() {
            if unread {
                self.status = NEW_MAIL.into();
            }
            return self.refresh();
        }
        // What the page covers: the whole folder, or the messages newer
        // than its last one (one as old may be on the next page).
        let oldest = fresh.last().map(|m| m.received);
        let listed: HashSet<&MessageId> = fresh.iter().map(|m| &m.id).collect();
        let gone: HashSet<MessageId> = self
            .messages
            .iter()
            .chain(&self.hidden)
            .filter(|m| !listed.contains(&m.id))
            .filter(|m| complete || oldest.is_some_and(|oldest| m.received > oldest))
            // Moves and purges on their way take it out themselves.
            .filter(|m| !self.transferring(&m.id))
            .map(|m| m.id.clone())
            .collect();
        self.remove_messages(&gone);
        // The selected message, or the one that took its place.
        let selected = self.messages.get(self.selected).map(|m| m.id.clone());
        // The others may have changed elsewhere (read in Outlook, say).
        // The changes are newer than the newest page when they disagree.
        let updates: HashMap<&MessageId, &MessageSummary> =
            fresh.iter().chain(&changes).map(|m| (&m.id, m)).collect();
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
        if unread {
            self.status = NEW_MAIL.into();
        }
        Vec::new()
    }

    /// Adds a message that arrived to the index, where its date puts it,
    /// or aside if the index is limited and it doesn't match.
    fn insert_arrived(&mut self, message: MessageSummary) {
        let next = self.positions.len();
        self.positions.entry(message.id.clone()).or_insert(next);
        let list = if self.limit_shows(&message) {
            &mut self.messages
        } else {
            &mut self.hidden
        };
        // Newest first, after messages as old.
        let at = list
            .iter()
            .position(|m| m.received < message.received)
            .unwrap_or(list.len());
        list.insert(at, message);
    }

    /// Takes in the mailboxes' counts.
    pub(super) fn on_mailboxes_checked(
        &mut self,
        id: RequestId,
        checked: Vec<(FolderPath, mett_core::Result<Folder>)>,
    ) -> Vec<Effect> {
        if self.checking != Some(id) {
            return Vec::new();
        }
        self.checking = None;
        let mut arrived = Vec::new();
        let mut failure = None;
        for (path, result) in checked {
            let open = self.is_open(&path, result.as_ref().ok());
            let state = self.mailbox_state.entry(path.clone()).or_default();
            match result {
                Ok(folder) => {
                    self.folder_ids.insert(path.clone(), folder.id.clone());
                    let before = state.folder.as_ref().map(|f| f.unread_count);
                    let unread = folder.unread_count;
                    match before {
                        // At first, unread mail counts as new.
                        None => state.new = unread > 0 && !open,
                        Some(before) if unread > before && !open => {
                            state.new = true;
                            arrived.push(path);
                        }
                        Some(_) => {}
                    }
                    if open || unread == 0 {
                        state.new = false;
                    }
                    state.folder = Some(folder);
                    state.error = None;
                }
                Err(e) => {
                    // Said once, not at every check.
                    if state.error.is_none() && failure.is_none() {
                        failure = Some((path, e.clone()));
                    }
                    state.error = Some(e.to_string());
                    // Gone: nothing to offer, nor counts to show.
                    if matches!(e, Error::NotFound { .. }) {
                        state.folder = None;
                        state.new = false;
                    }
                    if matches!(e, Error::Auth(_)) {
                        self.poll_paused = true;
                    }
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
            self.status = format!("New mail in {}.", names.join(", "));
        } else if let Some((path, e)) = failure {
            let name = self
                .mailboxes
                .iter()
                .find(|m| m.path == path)
                .map_or(String::new(), |m| m.name.clone());
            if self.status.is_empty() && !matches!(e, Error::Auth(_)) {
                self.status = format!("Couldn't check {name}: {e}");
            }
        }
        self.refresh_mailbox_browser();
        Vec::new()
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

    /// The folder was opened: its new mail has been seen.
    pub(super) fn visited(&mut self) {
        let Some(open) = self.folder.clone() else {
            return;
        };
        for (path, state) in &mut self.mailbox_state {
            let same = state.folder.as_ref().is_some_and(|f| f.id == open.id) || *path == self.path;
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
            .filter(|m| self.has_new_mail(&m.path))
            .count()
    }

    fn has_new_mail(&self, path: &FolderPath) -> bool {
        self.mailbox_state.get(path).is_some_and(|s| s.new)
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
            .find(|m| self.has_new_mail(&m.path))
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
                    new: state.is_some_and(|s| s.new),
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

    fn summary(id: &str, minutes: i64) -> MessageSummary {
        let date = DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes);
        mock::message(id, "in", id, date).summary
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
    }

    #[test]
    fn says_which_mailboxes_have_new_mail() {
        let (mut app, check) = app();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), Ok(folder("in", "Inbox", 1))),
                (work(), Ok(folder("work", "Work", 0))),
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
                (inbox(), Some(FolderId::new("in"))),
                (work(), Some(FolderId::new("work"))),
            ])
        );
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), Ok(folder("in", "Inbox", 2))),
                (work(), Ok(folder("work", "Work", 3))),
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
    fn the_browser_starts_on_the_mailboxes() {
        let (mut app, check) = app();
        respond(
            &mut app,
            check,
            Event::MailboxesChecked(vec![
                (inbox(), Ok(folder("in", "Inbox", 1))),
                (work(), Err(Error::not_found("folder", "Work"))),
            ]),
        );
        assert!(app.status.starts_with("Couldn't check =Work"));
        assert!(!app.has_new_mail(&work()));
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
                (work(), Ok(folder("work", "Work", 1))),
                (FolderPath::Named(vec!["Lists".into()]), Ok(lists)),
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
