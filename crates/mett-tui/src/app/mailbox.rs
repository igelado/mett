//! Working on the mailbox, as in mutt: marking messages deleted and
//! purging them, flags, saving and copying messages to other folders,
//! changing folders, and the folder browser.

use std::collections::{HashMap, HashSet};

use mett_core::{Error, FlagsUpdate, Folder, FolderId, MessageId, Page};

use super::composing::{Input, Question};
use super::{App, Effect, Screen, Target, effect_id};
use crate::action::Action;
use crate::config::Quad;
use crate::config::mailbox::{FolderPath, MailboxError, configured_path};
use crate::worker::{Request, RequestId};

/// A move, copy or purge waiting for the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Transfer {
    /// Messages moved to another folder (`s`).
    Save,
    /// Messages copied to another folder (`C`).
    Copy,
    /// Deleted messages moved to the trash or deleted permanently.
    Purge,
}

/// A change to the mailbox, made in its turn (see [`App::write`]).
// Few changes wait at a time, so their size doesn't matter.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Write {
    /// A message's flags.
    Flags(MessageId, FlagsUpdate),
    /// A move, copy or purge.
    Transfer(Transfer, Request),
}

/// A move, copy or purge of a message that failed, and where to.
type TransferFailure = (MessageId, Transfer, Option<FolderPath>);

/// How each message of a move, copy or purge fared.
pub(super) type Outcomes = Vec<(MessageId, mett_core::Result<()>)>;

/// What to do once the deleted messages have been purged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AfterPurge {
    /// Quit.
    Quit,
    /// Open another folder.
    Open(FolderPath),
}

/// The folder browser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Browser {
    /// The folders, in tree order, once listed.
    pub entries: Vec<BrowserEntry>,
    /// The selected entry.
    pub selected: usize,
    /// The request listing the folders, while it is pending.
    pub(super) listing: Option<RequestId>,
    /// What choosing a folder does.
    pub(super) purpose: Choose,
    /// Whether it lists the mailboxes (`mailboxes`), rather than every
    /// folder.
    pub mailboxes: bool,
}

impl Browser {
    /// Whether its folders are still being listed.
    pub fn listing(&self) -> bool {
        self.listing.is_some()
    }
}

/// A folder in the browser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserEntry {
    /// How deep it is in the tree; top-level folders are 0.
    pub depth: usize,
    /// What it is called.
    pub name: String,
    /// The folder, if it has been found.
    pub id: Option<FolderId>,
    /// Its unread and total counts, if known.
    pub counts: Option<(u32, u32)>,
    /// Whether it has new mail.
    pub new: bool,
    /// Its path from the top of the mailbox (`Inbox/Projects`), or for a
    /// mailbox, as configured (`=Inbox/Projects`), which mailbox questions
    /// take whatever `folder` is.
    pub path: String,
    /// The same path, by folder names: unlike the text, it can't be
    /// mistaken for a well-known folder (a folder named `@archive`).
    pub target: FolderPath,
}

/// What choosing a folder in the browser does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Choose {
    /// Opens it.
    Open,
    /// Answers the question it was opened from, which had this text typed
    /// so far.
    Answer(Question, String),
}

impl App {
    /// The selected message, if any.
    fn selected_id(&self) -> Option<MessageId> {
        self.messages.get(self.selected).map(|s| s.id.clone())
    }

    /// The message mailbox actions act on: the one the pager shows, or the
    /// one selected in the index. (A background listing may move the
    /// index's selection while the pager is open.)
    pub(super) fn acted_on(&self) -> Option<MessageId> {
        match self.screens.last() {
            Some(Screen::Pager(pager)) => Some(pager.id.clone()),
            _ => self.selected_id(),
        }
    }

    /// Whether the message is marked for deletion.
    pub fn is_deleted(&self, id: &MessageId) -> bool {
        self.deleted.contains(id)
    }

    /// How many messages are marked for deletion.
    pub fn deleted_count(&self) -> usize {
        self.deleted.len()
    }

    /// Handles the mailbox actions of the index and the pager. Returns
    /// `None` for other actions.
    pub(super) fn on_mailbox_action(&mut self, action: Action) -> Option<Vec<Effect>> {
        // A message being moved gets a new ID when the move is done:
        // nothing else may happen to it meanwhile. While it is moved or
        // copied, the message can't be moved, copied or marked again: one
        // of them would find it gone.
        let selected = self.acted_on();
        // A purge on its way can't be called off.
        let purging = selected
            .as_ref()
            .is_some_and(|id| self.transfers_of(id).any(|t| t == Transfer::Purge));
        if action == Action::UndeleteMessage && purging {
            self.fail("The message is already being purged.");
            return Some(Vec::new());
        }
        let busy = match action {
            Action::DeleteMessage
            | Action::UndeleteMessage
            | Action::SaveMessage
            | Action::CopyMessage => selected.as_ref().is_some_and(|id| self.transferring(id)),
            // A purge moves the message to the trash too. (A reply would
            // be made to the ID the message had.)
            Action::FlagMessage
            | Action::ToggleNew
            | Action::Reply
            | Action::GroupReply
            | Action::Forward => selected
                .as_ref()
                .is_some_and(|id| self.transfers_of(id).any(|t| t != Transfer::Copy)),
            _ => false,
        };
        if busy {
            self.fail("The message is still being moved, copied or purged.");
            return Some(Vec::new());
        }
        // A message written now would be lost when the other folder opens
        // and the screens are reset.
        let composing = matches!(
            action,
            Action::Mail | Action::Reply | Action::GroupReply | Action::Forward
        );
        if composing && self.switching.is_some() {
            self.fail("Opening folder; wait a moment.");
            return Some(Vec::new());
        }
        // Leaving the folder (opening another, or purging first): marks
        // made now would be lost or left behind.
        if matches!(action, Action::DeleteMessage | Action::UndeleteMessage) && self.leaving() {
            self.fail("Leaving the folder; wait a moment.");
            return Some(Vec::new());
        }
        let effects = match action {
            Action::DeleteMessage | Action::UndeleteMessage => {
                let Some(id) = selected else {
                    self.fail("No messages.");
                    return Some(Vec::new());
                };
                if action == Action::DeleteMessage {
                    self.deleted.insert(id);
                } else {
                    self.forget_failed_purge(&id);
                    self.deleted.remove(&id);
                }
                if self.resolve {
                    self.next_message()
                } else {
                    Vec::new()
                }
            }
            // In the index, a collapsed thread that shows as unread (mail
            // folded into it is) is read whole.
            Action::ToggleNew
                if self.screen() == &Screen::Index
                    && let Some(unread) = self.folded_thread_unread() =>
            {
                let mut effects = Vec::new();
                for id in unread {
                    effects.extend(self.update_flags(id, FlagsUpdate::read(true)));
                }
                effects
            }
            Action::FlagMessage | Action::ToggleNew => {
                let Some((id, flags)) =
                    selected.and_then(|id| self.summary(&id).map(|summary| (id, summary.flags)))
                else {
                    self.fail("No messages.");
                    return Some(Vec::new());
                };
                let update = if action == Action::FlagMessage {
                    FlagsUpdate::flagged(!flags.flagged)
                } else {
                    // Chosen while the message loads: showing it won't
                    // undo it.
                    if let Some(Screen::Pager(pager)) = self.screens.last_mut()
                        && pager.message.is_none()
                        && pager.id == id
                    {
                        pager.read_chosen = true;
                    }
                    FlagsUpdate::read(!flags.read)
                };
                self.update_flags(id, update)
            }
            Action::SyncMailbox => self.purge(None),
            Action::SaveMessage | Action::CopyMessage => {
                if self.messages.is_empty() {
                    self.fail("No messages.");
                    return Some(Vec::new());
                }
                let question = if action == Action::SaveMessage {
                    Question::SaveTo
                } else {
                    Question::CopyTo
                };
                // The answer is for this message, whatever the index shows
                // by then.
                self.question_message = selected;
                self.ask_mailbox(question, "");
                Vec::new()
            }
            Action::ChangeFolder => {
                // As in mutt, a mailbox with new mail is offered.
                let offered = self
                    .next_new_mailbox(None)
                    .map(|m| m.text.clone())
                    .unwrap_or_default();
                self.ask_mailbox(Question::OpenMailbox, &offered);
                Vec::new()
            }
            Action::BrowseMailboxes => self.open_browser(Choose::Open),
            _ => return None,
        };
        Some(effects)
    }

    /// Moves to the next message after marking one, as mutt does with
    /// `resolve`: in the index, the next entry; in the pager, the next
    /// message.
    fn next_message(&mut self) -> Vec<Effect> {
        // From the message acted on: the pager's may not be the one the
        // index has selected.
        if let Some(at) = self
            .acted_on()
            .and_then(|id| self.messages.iter().position(|m| m.id == id))
        {
            self.selected = at;
        }
        if self.selected + 1 >= self.messages.len() {
            // The next message may still be loading: move there once it
            // arrives, as `j` does.
            if self.more_to_list() {
                return self.wait_for(self.next_entry());
            }
            return Vec::new();
        }
        self.selected += 1;
        if matches!(self.screen(), Screen::Pager(_)) {
            self.screens.pop();
            return self.open_selected();
        }
        Vec::new()
    }

    /// Asks for a mailbox, offering the folder browser.
    pub(super) fn ask_mailbox(&mut self, question: Question, current: &str) {
        let prompt = match question {
            Question::SaveTo => "Save to mailbox ('?' for list): ",
            Question::CopyTo => "Copy to mailbox ('?' for list): ",
            _ => "Open mailbox ('?' for list): ",
        };
        self.ask(question, prompt, current);
    }

    /// The folder a typed mailbox names, with mutt's shortcuts: `^` is
    /// the open one, `-` (or `!!`) the one before, and `!`, `>` and `<`
    /// are `$spoolfile`, `$mbox` and `$record`.
    fn mailbox_path(&self, text: &str) -> Result<FolderPath, MailboxError> {
        match text.trim() {
            "^" => Ok(self.path.clone()),
            "-" | "!!" => self
                .previous
                .clone()
                .ok_or_else(|| MailboxError("no previous mailbox".into())),
            _ => configured_path(
                text,
                &self.mailbox_root,
                &self.account,
                &self.shortcuts.spoolfile,
                &self.shortcuts.mbox,
                &self.shortcuts.record,
            ),
        }
    }

    /// Acts on the mailbox the user typed.
    pub(super) fn answer_mailbox(&mut self, question: Question, text: String) -> Vec<Effect> {
        // Answered: a browser to open again after a sign-in is moot.
        self.unbrowsed = None;
        if text.trim().is_empty() {
            // Nothing chosen: the question is over.
            self.browsed = None;
            self.question_message = None;
            return Vec::new();
        }
        // A folder chosen in the browser, unchanged, is that folder.
        let browsed = self
            .browsed
            .take()
            .filter(|(path, _)| *path == text)
            .map(|(_, target)| target);
        let path = match browsed.map_or_else(|| self.mailbox_path(&text), Ok) {
            Ok(path) => path,
            Err(e) => {
                self.fail(e.to_string());
                return Vec::new();
            }
        };
        match question {
            Question::OpenMailbox => self.leave_folder(AfterPurge::Open(path)),
            Question::SaveTo | Question::CopyTo => {
                let Some(id) = self.question_message.take() else {
                    return Vec::new();
                };
                let copy = question == Question::CopyTo;
                let (transfer, status) = if copy {
                    (Transfer::Copy, "Copying message...")
                } else {
                    (Transfer::Save, "Saving message...")
                };
                self.status = status.into();
                let request = Request::Transfer {
                    ids: vec![id],
                    to: path,
                    copy,
                };
                self.write(Write::Transfer(transfer, request))
            }
            _ => Vec::new(),
        }
    }

    /// Quits or opens another folder, purging this one's deleted messages
    /// first if the user wants to.
    pub(super) fn leave_folder(&mut self, then: AfterPurge) -> Vec<Effect> {
        if self.deleted.is_empty() {
            return self.after_purge(then);
        }
        self.purge(Some(then))
    }

    /// Purges the deleted messages (mutt's `delete` quad-option), then
    /// carries on with `then`.
    fn purge(&mut self, then: Option<AfterPurge>) -> Vec<Effect> {
        if self.purging() {
            // Asked to quit or leave meanwhile: once that purge is done.
            if then.is_some() {
                self.then = then;
            }
            self.fail("Deleted messages are already being purged.");
            return Vec::new();
        }
        if self.deleted.is_empty() {
            if then.is_none() {
                self.status = "Mailbox is unchanged.".into();
            }
            return then.map_or_else(Vec::new, |then| self.after_purge(then));
        }
        self.then = then;
        let prompt = format!("Purge {}?", messages(self.deleted.len(), "deleted message"));
        match self.delete {
            Quad::Yes => self.answer_purge(true),
            Quad::No => self.answer_purge(false),
            quad => {
                self.input = Some(Input::YesNo {
                    prompt,
                    default: quad == Quad::AskYes,
                    question: Question::Purge,
                });
                Vec::new()
            }
        }
    }

    /// Acts on the answer to "Purge deleted messages?": purges them, or
    /// keeps them marked (and leaves anyway, as mutt does).
    pub(super) fn answer_purge(&mut self, purge: bool) -> Vec<Effect> {
        if !purge {
            // Nor later: a purge to retry after a sign-in is given up.
            self.give_up_purge_retries();
            let then = self.then.take();
            return then.map_or_else(Vec::new, |then| self.after_purge(then));
        }
        let Some(folder) = &self.folder else {
            self.then = None;
            return Vec::new();
        };
        let mut ids: Vec<MessageId> = self.deleted.iter().cloned().collect();
        ids.sort();
        // The worker finds the trash, and deletes for good in the trash
        // itself, as mutt does. Marked messages that earlier moves take
        // away are left out once they have gone (see
        // [`Self::follow_moves`]).
        let request = Request::Purge {
            ids,
            from: folder.id.clone(),
            trash: self.trash.clone(),
        };
        self.status = format!("Purging {}...", messages(self.deleted.len(), "message"));
        self.write(Write::Transfer(Transfer::Purge, request))
    }

    /// Makes a change to the mailbox once the ones before it are made.
    ///
    /// The worker runs requests side by side, and changes overtaking each
    /// other would go wrong: a flag change made after a move would find
    /// the message gone, and a purge could take away a message being
    /// copied. So they wait their turn.
    pub(super) fn write(&mut self, write: Write) -> Vec<Effect> {
        // Doing it again takes the place of a retry after a sign-in, which
        // would do it twice.
        if let Write::Transfer(transfer, request) = &write {
            let (ids, to) = (transfer_ids(request), destination(request));
            let folder_ids = &self.folder_ids;
            let mut again: Option<HashSet<&MessageId>> = None;
            for (failed, retry) in &mut self.failed_transfers {
                // Moved again, anywhere (or purged again): the earlier one
                // is moot.
                let same = *transfer != Transfer::Copy
                    || same_destination(folder_ids, destination(retry), to);
                if failed == transfer && same {
                    let again = again.get_or_insert_with(|| ids.iter().collect());
                    transfer_ids_mut(retry).retain(|id| !again.contains(id));
                }
            }
            self.prune_retries();
        }
        self.writes.push_back(write);
        self.write_next()
    }

    /// Sends the next change, unless one is on its way.
    pub(super) fn write_next(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        while self.writing.is_none() {
            let Some(write) = self.writes.pop_front() else {
                break;
            };
            let write = match write {
                Write::Flags(message, update) => {
                    let update = self.flags_to_send(&message, update);
                    // The user changed them back meanwhile: what failed
                    // before isn't wanted any more either.
                    if update == FlagsUpdate::default() {
                        if let Some(mailbox) = self.mailbox_flags.get(&message).copied() {
                            self.flags_settled(&message, mailbox, false);
                        }
                        self.save_failures.remove(&message);
                        continue;
                    }
                    Write::Flags(message, update)
                }
                // The marked messages all moved away: nothing to purge.
                Write::Transfer(Transfer::Purge, request) if transfer_ids(&request).is_empty() => {
                    self.status = "Mailbox is unchanged.".into();
                    if let Some(then) = self.then.take() {
                        effects.extend(self.after_purge(then));
                    }
                    continue;
                }
                write => write,
            };
            let request = match &write {
                Write::Flags(message, update) => Request::UpdateFlags(message.clone(), *update),
                Write::Transfer(_, request) => request.clone(),
            };
            let effect = self.request(request);
            self.writing = Some((effect_id(&effect), write));
            effects.push(effect);
        }
        effects
    }

    /// Whether a purge is waiting or on its way.
    fn purging(&self) -> bool {
        self.writes_made()
            .any(|write| matches!(write, Write::Transfer(Transfer::Purge, _)))
    }

    /// The changes waiting or on their way.
    fn writes_made(&self) -> impl Iterator<Item = &Write> {
        self.writing
            .iter()
            .map(|(_, write)| write)
            .chain(&self.writes)
    }

    /// Whether the message is being moved, copied or purged.
    pub(super) fn transferring(&self, id: &MessageId) -> bool {
        self.transfers_of(id).next().is_some()
    }

    /// The moves, copies and purges of a message, sent or waiting.
    fn transfers_of<'a>(&'a self, id: &'a MessageId) -> impl Iterator<Item = Transfer> + 'a {
        self.writes_made().filter_map(move |write| match write {
            Write::Transfer(transfer, request) if transfer_ids(request).contains(id) => {
                Some(*transfer)
            }
            _ => None,
        })
    }

    /// Sends again the moves, copies and purges that failed for want of a
    /// sign-in.
    pub(super) fn retry_transfers(&mut self) -> Vec<Effect> {
        let failed = std::mem::take(&mut self.failed_transfers);
        self.writes.extend(
            failed
                .into_iter()
                .map(|(transfer, request)| Write::Transfer(transfer, request)),
        );
        self.write_next()
    }

    /// Whether the user is leaving the folder: another is being opened, or
    /// the marked messages are being purged first.
    pub(super) fn leaving(&self) -> bool {
        self.switching.is_some() || (self.then.is_some() && self.purging())
    }

    /// Leaves an undeleted message out of a purge to retry after a
    /// sign-in. With nothing left to retry, the user stays.
    fn forget_failed_purge(&mut self, id: &MessageId) {
        self.transfer_failures
            .remove(&(id.clone(), Transfer::Purge, None));
        let mut emptied = false;
        for (transfer, retry) in &mut self.failed_transfers {
            if *transfer == Transfer::Purge {
                let ids = transfer_ids_mut(retry);
                let before = ids.len();
                ids.retain(|other| other != id);
                emptied |= before > 0 && ids.is_empty();
            }
        }
        self.prune_retries();
        if emptied && !self.purging() {
            self.then = None;
        }
    }

    /// Forgets the retries left with no messages.
    fn prune_retries(&mut self) {
        self.failed_transfers
            .retain(|(_, retry)| !transfer_ids(retry).is_empty());
    }

    /// Cancels a purge question: nothing is purged, now or by a retry,
    /// and the user stays.
    pub(super) fn cancel_purge(&mut self) {
        self.then = None;
        self.give_up_purge_retries();
    }

    /// Gives up a purge to retry after a sign-in: the user said not to.
    fn give_up_purge_retries(&mut self) {
        self.failed_transfers
            .retain(|(transfer, _)| *transfer != Transfer::Purge);
        self.transfer_failures
            .retain(|(_, transfer, _), _| *transfer != Transfer::Purge);
    }

    fn after_purge(&mut self, then: AfterPurge) -> Vec<Effect> {
        match then {
            AfterPurge::Quit => {
                self.quit = true;
                Vec::new()
            }
            AfterPurge::Open(path) => self.change_folder(path),
        }
    }

    /// Leaves the open folder for the one at `path`.
    ///
    /// The open folder stays until the new one is found, so that a bad
    /// name or a failure leaves the user where they were.
    pub(super) fn change_folder(&mut self, path: FolderPath) -> Vec<Effect> {
        // A recall on its way gives way to the folder the user wants.
        self.recall = None;
        self.unrecalled = false;
        if self.folder.is_none() {
            self.reset_folder();
            self.path = path;
            return self.open_folder();
        }
        self.unswitched = None;
        let request = self.request(Request::OpenFolder(path.clone()));
        self.switching = Some((effect_id(&request), path));
        self.status = "Opening folder...".into();
        vec![request]
    }

    /// Takes in the folder to change to, once found.
    pub(super) fn on_switched(&mut self, result: mett_core::Result<Folder>) -> Vec<Effect> {
        let Some((_, path)) = self.switching.take() else {
            return Vec::new();
        };
        match result {
            Ok(folder) => {
                // An earlier folder change waiting for a sign-in is moot.
                self.unswitched = None;
                self.see_index();
                self.reset_folder();
                self.previous = Some(std::mem::replace(&mut self.path, path));
                self.status = format!("Loading {}...", folder.name);
                self.folder = Some(folder);
                self.visited();
                let mut effects = self.list(Page::first(self.page_size));
                // The folder opened is watched instead.
                effects.extend(self.watch());
                effects
            }
            Err(e) => {
                // Without a sign-in, the sign-in window's Retry opens it.
                if matches!(e, Error::Auth(_)) {
                    self.unswitched = Some(path);
                }
                self.report(e);
                Vec::new()
            }
        }
    }

    /// Opens again the folder whose opening failed for want of a sign-in.
    pub(super) fn retry_switch(&mut self) -> Vec<Effect> {
        self.unswitched
            .take()
            .map_or_else(Vec::new, |path| self.change_folder(path))
    }

    /// Forgets the open folder's index. Moves, copies and saves that are
    /// on their way or failed are kept: they don't depend on it.
    fn reset_folder(&mut self) {
        self.screens.truncate(1);
        self.polling = None;
        self.known_before_refresh = None;
        self.tie_check = None;
        self.reselect = None;
        self.folder = None;
        self.messages.clear();
        self.hidden.clear();
        self.folded.clear();
        self.threads.clear();
        self.threaded_count = 0;
        self.chosen.clear();
        self.collapse_all = false;
        self.positions.clear();
        self.limit = None;
        self.limited.clear();
        self.pending_search = None;
        self.selected = 0;
        self.deleted.clear();
        self.listing = None;
        self.listing_page = None;
        self.unlisted = None;
        self.pending = None;
        self.unfetched = None;
        self.unfetched_search = None;
        self.relist_on_return = false;
        self.saved_flags.clear();
        self.removed_while_listing.clear();
    }

    /// Carries changes still to make, and those that failed, over to
    /// messages that moved (Graph gives them new IDs), so that making or
    /// retrying them still works.
    pub(super) fn follow_moves(&mut self, moved: &[(MessageId, MessageId)]) {
        let renamed: HashMap<&MessageId, &MessageId> =
            moved.iter().map(|(old, new)| (old, new)).collect();
        let rename = |id: &mut MessageId| {
            if let Some(new) = renamed.get(id) {
                *id = (*new).clone();
            }
        };
        let follow = |transfer: Transfer, request: &mut Request| {
            let ids = transfer_ids_mut(request);
            if transfer == Transfer::Purge {
                // Moved away, so not to be purged from here.
                ids.retain(|id| !renamed.contains_key(id));
            } else {
                ids.iter_mut().for_each(rename);
            }
        };
        for write in &mut self.writes {
            match write {
                Write::Flags(id, _) => rename(id),
                Write::Transfer(transfer, request) => follow(*transfer, request),
            }
        }
        for (transfer, request) in &mut self.failed_transfers {
            follow(*transfer, request);
        }
        self.prune_retries();
        // The failures of the messages that moved, found in one pass.
        let mut failures: HashMap<MessageId, Vec<TransferFailure>> = HashMap::new();
        for key in self.transfer_failures.keys() {
            if renamed.contains_key(&key.0) {
                failures.entry(key.0.clone()).or_default().push(key.clone());
            }
        }
        for (old, new) in moved {
            rekey(&mut self.save_failures, old, new);
            // Moved away, it is no longer to be purged from here.
            self.transfer_failures
                .remove(&(old.clone(), Transfer::Purge, None));
            for key in failures.remove(old).unwrap_or_default() {
                if let Some(failure) = self.transfer_failures.remove(&key) {
                    let (_, transfer, to) = key;
                    let key = (new.clone(), transfer, to);
                    // Moved on again, it takes them along.
                    if renamed.contains_key(new) {
                        failures.entry(new.clone()).or_default().push(key.clone());
                    }
                    self.transfer_failures.insert(key, failure);
                }
            }
            rekey(&mut self.mailbox_flags, old, new);
            rekey(&mut self.wanted_flags, old, new);
        }
    }

    /// Handles the outcome of a move, copy or purge: the folder could not
    /// be found, or how each message fared.
    ///
    /// `to` is where the messages went, if known: a move or copy into the
    /// open folder shows up by listing it again.
    pub(super) fn on_transferred(
        &mut self,
        id: RequestId,
        result: mett_core::Result<(Option<FolderId>, Outcomes)>,
    ) -> Vec<Effect> {
        let Some((_, Write::Transfer(transfer, request))) =
            self.writing.take_if(|(request, _)| *request == id)
        else {
            return Vec::new();
        };
        let mut effects = self.transferred(transfer, request, result);
        effects.extend(self.write_next());
        effects
    }

    fn transferred(
        &mut self,
        transfer: Transfer,
        request: Request,
        result: mett_core::Result<(Option<FolderId>, Outcomes)>,
    ) -> Vec<Effect> {
        let (to, results) = match result {
            Ok(result) => result,
            Err(e) => {
                // Nothing moved.
                let to = destination(&request);
                for id in transfer_ids(&request) {
                    self.transfer_failures
                        .insert((id.clone(), transfer, to.cloned()), e.to_string());
                }
                // A purge to retry after a sign-in still quits or leaves
                // once it is done; one that failed otherwise doesn't.
                if matches!(e, Error::Auth(_)) {
                    self.failed_transfers.push((transfer, request));
                } else if transfer == Transfer::Purge {
                    self.then = None;
                }
                self.report(e);
                return Vec::new();
            }
        };
        // Something arrived in the open folder: list it again (staying on
        // the same message).
        let into_open = to.is_some()
            && to.as_ref() == self.folder.as_ref().map(|f| &f.id)
            && results.iter().any(|(_, result)| result.is_ok());
        let mut effects = self.finish_transfer(transfer, request, results, to.clone());
        if into_open {
            // Not under the pager, whose message and position would no
            // longer match the index: once back in the index.
            if self.screen() == &Screen::Index {
                effects.extend(self.refresh());
            } else {
                self.relist_on_return = true;
            }
        }
        effects
    }

    fn finish_transfer(
        &mut self,
        transfer: Transfer,
        request: Request,
        results: Vec<(MessageId, mett_core::Result<()>)>,
        resolved: Option<FolderId>,
    ) -> Vec<Effect> {
        let total = results.len();
        let to = destination(&request).cloned();
        let mut done = HashSet::new();
        let mut failure: Option<Error> = None;
        let mut unauthorized = Vec::new();
        for (id, result) in results {
            match result {
                Ok(()) => {
                    done.insert(id);
                }
                // A message to purge that is gone already is as good as
                // purged.
                Err(Error::NotFound { .. }) if transfer == Transfer::Purge => {
                    done.insert(id);
                }
                Err(e) => {
                    self.transfer_failures
                        .insert((id.clone(), transfer, to.clone()), e.to_string());
                    if matches!(e, Error::Auth(_)) {
                        unauthorized.push(id);
                    }
                    failure = Some(e);
                }
            }
        }
        // Purged: flag changes waiting their turn have nothing to change.
        if transfer == Transfer::Purge {
            self.writes
                .retain(|write| !matches!(write, Write::Flags(id, _) if done.contains(id)));
            for id in &done {
                self.mailbox_flags.remove(id);
                self.wanted_flags.remove(id);
                // Nothing left to save, or to retry (see below).
                self.save_failures.remove(id);
            }
        }
        // Moved into the open folder, the messages come back with new IDs
        // when it is listed again; the old ones are gone.
        if transfer != Transfer::Copy {
            self.remove_messages(&done);
        }
        // Once to retry each: an earlier retry of the same thing to the
        // same folder (perhaps by another name) is this one.
        if let (Some(path), Some(folder)) = (&to, &resolved) {
            self.folder_ids.insert(path.clone(), folder.clone());
        }
        // Only the same operation to the same place succeeding clears a
        // failure: a copy doesn't make up for a failed move, nor a copy to
        // one folder for a copy to another. The same folder may go by
        // another name (`=Archive`, `=@archive`, the browser's).
        let same_place = |place: Option<&FolderPath>| {
            place == to.as_ref()
                || place
                    .zip(resolved.as_ref())
                    .is_some_and(|(path, folder)| self.folder_ids.get(path) == Some(folder))
        };
        self.transfer_failures.retain(|(id, failed, place), _| {
            !(done.contains(id) && *failed == transfer && same_place(place.as_ref()))
        });
        // Nor is it to be done again after a sign-in: purged, by anything;
        // moved or copied, by the same to the same place. Neither is what
        // fails for want of a sign-in once more, which is retried now.
        let folder_ids = &self.folder_ids;
        let unauthorized_ids: HashSet<&MessageId> = unauthorized.iter().collect();
        for (failed, retry) in &mut self.failed_transfers {
            if transfer == Transfer::Purge {
                transfer_ids_mut(retry).retain(|id| !done.contains(id));
            } else if *failed == transfer {
                let place = destination(retry);
                let again = same_destination(folder_ids, place, to.as_ref());
                let done_there = same_place(place);
                transfer_ids_mut(retry).retain(|id| {
                    !((again && unauthorized_ids.contains(id)) || (done_there && done.contains(id)))
                });
            }
        }
        self.prune_retries();
        // What failed for want of a sign-in can be retried from the sign-in
        // window.
        let unauthorized_count = unauthorized.len();
        if !unauthorized.is_empty() {
            self.failed_transfers
                .push((transfer, with_ids(request, unauthorized)));
        }
        if let Some(e) = failure {
            // Whatever failed stays where it was, still marked; don't go
            // on as if the purge had worked (unless all of it is to retry
            // after a sign-in).
            let failed = total - done.len();
            if transfer == Transfer::Purge && failed > unauthorized_count {
                self.then = None;
            }
            self.fail(format!(
                "{failed} of {} failed: {e}",
                messages(total, "message")
            ));
            return Vec::new();
        }
        self.status = match transfer {
            Transfer::Save => "Message saved.".into(),
            Transfer::Copy => "Message copied.".into(),
            Transfer::Purge => format!("{} purged.", messages(done.len(), "message")),
        };
        // Only a purge carries on with what was waiting for it.
        if transfer != Transfer::Purge {
            return Vec::new();
        }
        match self.then.take() {
            // Messages marked while it ran are purged too, before leaving.
            Some(then) if !self.deleted.is_empty() => self.purge(Some(then)),
            Some(then) => self.after_purge(then),
            None => Vec::new(),
        }
    }

    /// Forgets deletion marks of messages that a complete listing no
    /// longer has (they were moved or deleted elsewhere), so purging
    /// doesn't keep trying them.
    pub(super) fn forget_missing_marks(&mut self) {
        let listed: HashSet<&MessageId> = self
            .messages
            .iter()
            .chain(&self.hidden)
            .chain(&self.folded)
            .map(|m| &m.id)
            .collect();
        self.deleted.retain(|id| listed.contains(id));
    }

    /// Takes messages that are no longer in the folder out of the index,
    /// closing the pager if it shows one of them, even under another
    /// screen.
    pub(super) fn remove_messages(&mut self, gone: &HashSet<MessageId>) {
        if gone.is_empty() {
            return;
        }
        let selected = self.selected_id();
        if self.fetching_index() {
            self.removed_while_listing.extend(gone.iter().cloned());
        }
        self.screens
            .retain(|screen| !matches!(screen, Screen::Pager(pager) if gone.contains(&pager.id)));
        let before = self.messages[..self.selected.min(self.messages.len())]
            .iter()
            .filter(|m| gone.contains(&m.id))
            .count();
        // A pending move from a message that goes counts from the message
        // before it (or the top), not from the end it can no longer find.
        if let Some(Target::After(anchor, steps)) = &self.pending
            && gone.contains(anchor)
            && let Some(at) = self.entry_of(anchor)
        {
            let steps = *steps;
            self.pending = Some(
                match self.messages[..=at]
                    .iter()
                    .rev()
                    .find(|m| !gone.contains(&m.id))
                {
                    Some(kept) => Target::After(kept.id.clone(), steps),
                    None => Target::Entry(steps - 1),
                },
            );
        }
        // A selected collapsed thread whose first message goes stays
        // selected, by what is left of it.
        let left_of_thread = selected
            .as_ref()
            .filter(|id| gone.contains(*id))
            .and_then(|id| {
                self.folded
                    .iter()
                    .find(|m| {
                        !gone.contains(&m.id)
                            && self.thread_of(&m.id).is_some_and(|t| &t.root == id)
                    })
                    .map(|m| m.id.clone())
            });
        self.messages.retain(|m| !gone.contains(&m.id));
        self.hidden.retain(|m| !gone.contains(&m.id));
        self.folded.retain(|m| !gone.contains(&m.id));
        for id in gone {
            self.deleted.remove(id);
        }
        // Stay on the same message, or the one that took its place.
        self.selected = match selected {
            Some(id) if !gone.contains(&id) => self.selected - before,
            _ => self.selected.saturating_sub(before),
        }
        .min(self.messages.len().saturating_sub(1));
        // Their threads change. Without threads, what is left is still in
        // order.
        if self.threaded() {
            self.arrange();
        } else {
            debug_assert!(self.in_order(), "the index is in order");
        }
        if let Some(member) = left_of_thread {
            let root = self.thread_of(&member).map_or(member, |t| t.root.clone());
            if let Some(at) = self.messages.iter().position(|m| m.id == root) {
                self.selected = at;
            }
        }
    }

    /// Opens the folder browser.
    pub(super) fn open_browser(&mut self, purpose: Choose) -> Vec<Effect> {
        // This one takes the place of a browser to open again after a
        // sign-in.
        self.unbrowsed = None;
        // As in mutt, the mailboxes come first, if there are any; the
        // browser can switch to every folder.
        if !self.mailboxes.is_empty() {
            let open = self.mailboxes.iter().position(|m| m.path == self.path);
            return self.mailbox_browser(purpose, open.unwrap_or(0));
        }
        self.list_folders_in_browser(purpose)
    }

    /// Opens the browser on the mailboxes, at entry `selected`.
    fn mailbox_browser(&mut self, purpose: Choose, selected: usize) -> Vec<Effect> {
        let entries = self.mailbox_entries();
        self.screens.push(Screen::Browser(Browser {
            entries,
            selected,
            listing: None,
            purpose,
            mailboxes: true,
        }));
        // With fresh counts.
        self.check_mailboxes()
    }

    /// Opens the browser on every folder.
    fn list_folders_in_browser(&mut self, purpose: Choose) -> Vec<Effect> {
        let request = self.request(Request::ListFolders);
        self.screens.push(Screen::Browser(Browser {
            entries: Vec::new(),
            selected: 0,
            listing: Some(effect_id(&request)),
            purpose,
            mailboxes: false,
        }));
        vec![request]
    }

    /// Switches the browser between the mailboxes and every folder.
    fn toggle_mailboxes(&mut self) -> Vec<Effect> {
        let Some(Screen::Browser(browser)) = self.screens.last() else {
            return Vec::new();
        };
        let mailboxes = browser.mailboxes;
        if !mailboxes && self.mailboxes.is_empty() {
            self.fail("No mailboxes are defined.");
            return Vec::new();
        }
        let purpose = browser.purpose.clone();
        self.screens.pop();
        if mailboxes {
            self.list_folders_in_browser(purpose)
        } else {
            self.mailbox_browser(purpose, 0)
        }
    }

    /// Opens again the browser whose folders couldn't be listed for want of
    /// a sign-in.
    pub(super) fn retry_browser(&mut self) -> Vec<Effect> {
        match self.unbrowsed.take() {
            Some(purpose) if matches!(self.screen(), Screen::Index | Screen::Pager(_)) => {
                // The browser takes the keys, not the question it answers,
                // which comes back when it closes.
                self.input = None;
                self.open_browser(purpose)
            }
            _ => Vec::new(),
        }
    }

    /// The request listing the browser's folders, while it is pending.
    pub(super) fn browser_request(&self) -> Option<RequestId> {
        match self.screens.last() {
            Some(Screen::Browser(browser)) => browser.listing,
            _ => None,
        }
    }

    /// Fills the browser once the folders are listed.
    pub(super) fn on_folders_listed(
        &mut self,
        id: RequestId,
        result: mett_core::Result<Vec<Folder>>,
    ) -> Vec<Effect> {
        let Some(Screen::Browser(browser)) = self.screens.last_mut() else {
            return Vec::new();
        };
        if browser.listing != Some(id) {
            return Vec::new();
        }
        browser.listing = None;
        match result {
            Ok(folders) => {
                browser.entries = tree(folders);
                // Start on the open folder, if it is there.
                let open = self.folder.as_ref().map(|f| &f.id);
                browser.selected = browser
                    .entries
                    .iter()
                    .position(|e| e.id.as_ref() == open)
                    .unwrap_or(0);
            }
            Err(e) => {
                // Without a sign-in, the sign-in window's Retry opens the
                // browser again.
                if matches!(e, Error::Auth(_))
                    && let Some(Screen::Browser(browser)) = self.screens.last()
                {
                    self.unbrowsed = Some(browser.purpose.clone());
                }
                self.leave_browser(None);
                // Back at the question, which hides the status: say why
                // on its line.
                if let Some(Input::Line { prompt, .. }) = &mut self.input {
                    *prompt = format!("{e}. {prompt}");
                }
                self.report(e);
            }
        }
        Vec::new()
    }

    pub(super) fn on_browser_action(&mut self, action: Action) -> Vec<Effect> {
        let page = self.body_height.max(1);
        let Some(Screen::Browser(browser)) = self.screens.last_mut() else {
            return Vec::new();
        };
        if super::navigate(&mut browser.selected, action, browser.entries.len(), page) {
            return Vec::new();
        }
        match action {
            Action::DisplayMessage => {
                let Some(entry) = browser.entries.get(browser.selected) else {
                    return Vec::new();
                };
                let chosen = (entry.path.clone(), entry.target.clone());
                return self.leave_browser(Some(chosen));
            }
            Action::Exit | Action::Quit => return self.leave_browser(None),
            Action::ToggleMailboxes => return self.toggle_mailboxes(),
            _ => {}
        }
        Vec::new()
    }

    /// Closes the browser, with the chosen folder's path if one was chosen.
    fn leave_browser(&mut self, chosen: Option<(String, FolderPath)>) -> Vec<Effect> {
        let Some(Screen::Browser(browser)) = self.screens.pop() else {
            return Vec::new();
        };
        match (browser.purpose, chosen) {
            (Choose::Open, Some((_, target))) => self.leave_folder(AfterPurge::Open(target)),
            (Choose::Open, None) => Vec::new(),
            // Back to the question, with the folder filled in, or what was
            // typed before.
            (Choose::Answer(question, typed), chosen) => {
                let text = chosen.as_ref().map_or(typed.as_str(), |(path, _)| path);
                self.ask_mailbox(question, text);
                // If the user takes it as it is, it means this folder; a
                // browser left without choosing keeps the earlier choice.
                if chosen.is_some() {
                    self.browsed = chosen;
                }
                Vec::new()
            }
        }
    }
}

/// Whether `a` and `b` name the same folder: as written, or as found.
fn same_folder(folder_ids: &HashMap<FolderPath, FolderId>, a: &FolderPath, b: &FolderPath) -> bool {
    a == b
        || folder_ids
            .get(a)
            .is_some_and(|id| folder_ids.get(b) == Some(id))
}

/// The messages a move, copy or purge is for.
fn transfer_ids(request: &Request) -> &[MessageId] {
    match request {
        Request::Transfer { ids, .. } | Request::Purge { ids, .. } => ids,
        _ => &[],
    }
}

/// The messages a move, copy or purge is for, to change.
fn transfer_ids_mut(request: &mut Request) -> &mut Vec<MessageId> {
    match request {
        Request::Transfer { ids, .. } | Request::Purge { ids, .. } => ids,
        other => unreachable!("not a transfer: {other:?}"),
    }
}

/// Where a move or copy goes; a purge has no destination of its own.
fn destination(request: &Request) -> Option<&FolderPath> {
    match request {
        Request::Transfer { to, .. } => Some(to),
        _ => None,
    }
}

/// Whether two moves or copies go to the same folder.
fn same_destination(
    folder_ids: &HashMap<FolderPath, FolderId>,
    a: Option<&FolderPath>,
    b: Option<&FolderPath>,
) -> bool {
    a.zip(b).is_some_and(|(a, b)| same_folder(folder_ids, a, b))
}

/// Moves `map`'s entry for `old` to `new`.
fn rekey<V>(map: &mut HashMap<MessageId, V>, old: &MessageId, new: &MessageId) {
    if let Some(value) = map.remove(old) {
        map.insert(new.clone(), value);
    }
}

/// `request`, for `ids` only.
fn with_ids(request: Request, ids: Vec<MessageId>) -> Request {
    match request {
        Request::Transfer { to, copy, .. } => Request::Transfer { ids, to, copy },
        Request::Purge { from, trash, .. } => Request::Purge { ids, from, trash },
        other => other,
    }
}

/// `count` of `noun`, as in "1 message" or "2 messages".
fn messages(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// Orders `folders` as a tree, each folder followed by its subfolders, by
/// name, with the path the user would type for each.
fn tree(folders: Vec<Folder>) -> Vec<BrowserEntry> {
    let ids: HashSet<FolderId> = folders.iter().map(|f| f.id.clone()).collect();
    let mut children: HashMap<Option<FolderId>, Vec<Folder>> = HashMap::new();
    for folder in folders {
        // A folder whose parent wasn't listed is shown at the top.
        let parent = folder.parent.clone().filter(|p| ids.contains(p));
        children.entry(parent).or_default().push(folder);
    }
    for list in children.values_mut() {
        list.sort_by_cached_key(|f| f.name.to_lowercase());
    }
    let mut entries = Vec::new();
    let mut stack: Vec<(Vec<String>, Folder)> = children
        .remove(&None)
        .unwrap_or_default()
        .into_iter()
        .rev()
        .map(|f| (Vec::new(), f))
        .collect();
    while let Some((parents, folder)) = stack.pop() {
        let mut names = parents;
        names.push(folder.name.clone());
        if let Some(kids) = children.remove(&Some(folder.id.clone())) {
            for kid in kids.into_iter().rev() {
                stack.push((names.clone(), kid));
            }
        }
        entries.push(BrowserEntry {
            depth: names.len() - 1,
            name: folder.name,
            id: Some(folder.id),
            counts: Some((folder.unread_count, folder.total_count)),
            new: folder.unread_count > 0,
            path: names.join("/"),
            target: FolderPath::Named(names),
        });
    }
    entries
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta};
    use mett_core::{AccountId, Error, Message, WellKnownFolder, mock};
    use ratatui::crossterm::event::KeyCode;

    use super::*;
    use crate::app::Settings;
    use crate::app::testing::*;
    use crate::keymap::Menu;
    use crate::worker::Event;

    /// An app with Inbox messages m0, m1 and m2, newest first.
    fn app(settings: Settings) -> App {
        let inbox = FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
        let (mut app, effects) = App::open(80, 24, inbox, settings);
        let open = first_request(&effects);
        let folder = mett_core::mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox));
        let list = first_request(&respond(&mut app, open, Event::FolderOpened(Ok(folder))));
        let messages = (0..3)
            .map(|i| {
                let date = DateTime::UNIX_EPOCH + TimeDelta::minutes(10 - i);
                mock::message(&format!("m{i}"), "in", "s", date).summary
            })
            .collect();
        listed(&mut app, list, messages, None);
        app
    }

    /// An app that purges without asking.
    fn yes() -> App {
        app(Settings {
            delete: Quad::Yes,
            ..settings()
        })
    }

    /// Answers the transfer of `message` to the folder `to`: it is
    /// `new` there, or failed.
    fn transferred(
        app: &mut App,
        id: RequestId,
        to: &str,
        message: &str,
        result: mett_core::Result<&str>,
    ) -> Vec<Effect> {
        let moved = vec![(message.into(), result.map(MessageId::new))];
        respond(app, id, Event::Transferred(Ok((FolderId::new(to), moved))))
    }

    /// Answers the purge of `message`: done.
    fn purged(app: &mut App, id: RequestId, message: &str) -> Vec<Effect> {
        respond(app, id, Event::Purged(Ok(vec![(message.into(), Ok(()))])))
    }

    fn m0() -> Message {
        mock::message("m0", "in", "s", DateTime::UNIX_EPOCH)
    }

    /// Shows m0 in the pager; returns the effects of its delivery.
    fn show_m0(app: &mut App) -> Vec<Effect> {
        let (fetch, _) = sent(&act(app, Action::DisplayMessage));
        respond(app, fetch, Event::MessageFetched(Ok(m0())))
    }

    fn settings() -> Settings {
        Settings {
            account: AccountId::new("alice@contoso.com"),
            folder: "msgraph://alice@contoso.com/".into(),
            ..Settings::default()
        }
    }

    #[test]
    fn marks_and_purges_deleted_messages() {
        let mut app = app(settings());
        act(&mut app, Action::DeleteMessage);
        assert!(app.is_deleted(&"m0".into()));
        assert_eq!(app.selected, 1, "moved on");
        act(&mut app, Action::DeleteMessage);
        act(&mut app, Action::PreviousEntry);
        act(&mut app, Action::UndeleteMessage);
        assert_eq!(app.deleted_count(), 1);

        act(&mut app, Action::SyncMailbox);
        assert_eq!(prompt(&app), "Purge 1 deleted message?");
        let (id, request) = sent(&key(&mut app, KeyCode::Char('y')));
        // Without `trash`, purged messages are deleted for good.
        assert_eq!(
            request,
            Request::Purge {
                ids: vec!["m0".into()],
                from: FolderId::new("in"),
                trash: None,
            }
        );
        assert!(app.saving());
        // Only one purge at a time.
        act(&mut app, Action::SyncMailbox);
        assert!(app.input.is_none());
        assert_eq!(app.status, "Deleted messages are already being purged.");
        purged(&mut app, id, "m0");
        assert_eq!(ids(&app), ["m1", "m2"]);
        assert_eq!(app.status, "1 message purged.");
        // `u` moved on to m2, which stays selected.
        assert_eq!(ids(&app)[app.selected], "m2");
    }

    #[test]
    fn purging_moves_messages_to_the_trash() {
        let mut app = app(Settings {
            trash: Some(FolderPath::WellKnown(WellKnownFolder::DeletedItems, vec![])),
            delete: Quad::Yes,
            ..settings()
        });
        act(&mut app, Action::DeleteMessage);
        let (id, request) = sent(&act(&mut app, Action::SyncMailbox));
        assert!(matches!(request, Request::Purge { trash: Some(ref to), .. }
            if *to == FolderPath::WellKnown(WellKnownFolder::DeletedItems, vec![])));
        purged(&mut app, id, "m0");
        assert_eq!(ids(&app), ["m1", "m2"]);
    }

    #[test]
    fn quitting_offers_to_purge_first() {
        let mut app = app(settings());
        act(&mut app, Action::DeleteMessage);
        act(&mut app, Action::Quit);
        assert!(!app.quit);
        let (id, _) = sent(&key(&mut app, KeyCode::Enter));
        assert!(!app.quit, "not before the purge is done");
        respond(
            &mut app,
            id,
            Event::Purged(Ok(vec![("m0".into(), Err(offline()))])),
        );
        assert!(!app.quit, "the purge failed: stay");
        assert_eq!(app.status, "1 of 1 message failed: network error: offline");
        assert!(app.is_deleted(&"m0".into()));

        // Declining to purge quits straight away.
        act(&mut app, Action::Quit);
        key(&mut app, KeyCode::Char('n'));
        assert!(app.quit);
    }

    #[test]
    fn flags_and_toggles_new() {
        let mut app = app(settings());
        let (id, request) = sent(&act(&mut app, Action::FlagMessage));
        assert!(matches!(
            request,
            Request::UpdateFlags(_, update) if update == FlagsUpdate::flagged(true)
        ));
        assert!(app.messages[0].flags.flagged);
        respond(&mut app, id, Event::FlagsUpdated(Err(offline())));
        assert!(!app.messages[0].flags.flagged, "undone");

        let (_, request) = sent(&act(&mut app, Action::ToggleNew));
        assert!(matches!(
            request,
            Request::UpdateFlags(_, update) if update == FlagsUpdate::read(true)
        ));
        assert!(app.messages[0].flags.read);
    }

    #[test]
    fn saves_and_copies_to_other_folders() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        assert_eq!(prompt(&app), "Save to mailbox ('?' for list): ");
        let (id, request) = sent(&answer(&mut app, "=Archive/2026"));
        assert_eq!(
            request,
            Request::Transfer {
                ids: vec!["m0".into()],
                to: FolderPath::Named(vec!["Archive".into(), "2026".into()]),
                copy: false,
            }
        );
        transferred(&mut app, id, "arc", "m0", Ok("a0"));
        assert_eq!(ids(&app), ["m1", "m2"]);
        assert_eq!(app.status, "Message saved.");

        act(&mut app, Action::CopyMessage);
        let (id, request) = sent(&answer(&mut app, "=@archive"));
        assert!(matches!(request, Request::Transfer { copy: true, .. }));
        transferred(&mut app, id, "arc", "m1", Ok("c1"));
        assert_eq!(ids(&app), ["m1", "m2"], "copies stay");

        act(&mut app, Action::SaveMessage);
        assert!(answer(&mut app, "msgraph://bob@contoso.com/@inbox").is_empty());
        assert!(
            app.status.contains("can only use folders of"),
            "{}",
            app.status
        );
    }

    #[test]
    fn changes_folder() {
        let mut app = app(settings());
        act(&mut app, Action::ChangeFolder);
        let (id, request) = sent(&answer(&mut app, "=@sentitems"));
        assert_eq!(
            request,
            Request::OpenFolder(FolderPath::WellKnown(WellKnownFolder::SentItems, vec![]))
        );
        // The Inbox stays until Sent Items is found.
        assert_eq!(ids(&app), ["m0", "m1", "m2"]);
        let effects = respond(
            &mut app,
            id,
            Event::FolderOpened(Ok(mock::folder(
                "sent",
                "Sent Items",
                Some(WellKnownFolder::SentItems),
            ))),
        );
        assert!(matches!(sent(&effects).1, Request::ListMessages { .. }));
        assert!(app.messages.is_empty());
        assert_eq!(app.folder.as_ref().unwrap().name, "Sent Items");

        // A folder that isn't there leaves the user where they were.
        act(&mut app, Action::ChangeFolder);
        let (id, _) = sent(&answer(&mut app, "=Nowhere"));
        respond(
            &mut app,
            id,
            Event::FolderOpened(Err(Error::not_found("folder", "Nowhere"))),
        );
        assert_eq!(app.folder.as_ref().unwrap().name, "Sent Items");
        assert_eq!(app.status, "folder not found: Nowhere");
    }

    fn folders() -> Vec<Folder> {
        let child = |id: &str, name: &str, parent: &str| Folder {
            parent: Some(FolderId::new(parent)),
            ..mock::folder(id, name, None)
        };
        vec![
            mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox)),
            child("p", "Projects", "in"),
            child("m", "mett", "p"),
            mock::folder("arc", "Archive", None),
        ]
    }

    #[test]
    fn browses_folders_as_a_tree() {
        let mut app = app(settings());
        let (id, request) = sent(&act(&mut app, Action::BrowseMailboxes));
        assert_eq!(request, Request::ListFolders);
        assert_eq!(app.menu(), Menu::Browser);
        respond(&mut app, id, Event::FoldersListed(Ok(folders())));
        let Screen::Browser(browser) = app.screen() else {
            panic!("not in the browser");
        };
        let paths: Vec<_> = browser
            .entries
            .iter()
            .map(|e| (e.depth, e.path.as_str()))
            .collect();
        assert_eq!(
            paths,
            [
                (0, "Archive"),
                (0, "Inbox"),
                (1, "Inbox/Projects"),
                (2, "Inbox/Projects/mett"),
            ]
        );
        assert_eq!(browser.selected, 1, "starts on the open folder");
        act(&mut app, Action::NextEntry);
        let (_, request) = sent(&act(&mut app, Action::DisplayMessage));
        assert_eq!(
            request,
            Request::OpenFolder(FolderPath::Named(vec!["Inbox".into(), "Projects".into()]))
        );
        assert_eq!(app.menu(), Menu::Index);
    }

    #[test]
    fn the_browser_answers_a_mailbox_question() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        let (id, _) = sent(&key(&mut app, KeyCode::Char('?')));
        respond(&mut app, id, Event::FoldersListed(Ok(folders())));
        act(&mut app, Action::FirstEntry);
        act(&mut app, Action::DisplayMessage);
        assert_eq!(prompt(&app), "Save to mailbox ('?' for list): Archive");

        // Leaving the browser keeps what was typed.
        key(&mut app, KeyCode::Esc);
        act(&mut app, Action::CopyMessage);
        key(&mut app, KeyCode::Char('='));
        let (id, _) = sent(&key(&mut app, KeyCode::Tab));
        respond(&mut app, id, Event::FoldersListed(Ok(folders())));
        act(&mut app, Action::Exit);
        assert_eq!(prompt(&app), "Copy to mailbox ('?' for list): =");
    }

    #[test]
    fn browser_paths_work_whatever_folder_is() {
        // `folder` points below the mailbox root (or at ~/Mail): the
        // browser's paths still name the right folders.
        let mut app = app(Settings {
            folder: "msgraph://alice@contoso.com/Inbox".into(),
            ..settings()
        });
        let (id, _) = sent(&act(&mut app, Action::BrowseMailboxes));
        respond(&mut app, id, Event::FoldersListed(Ok(folders())));
        act(&mut app, Action::FirstEntry);
        let (_, request) = sent(&act(&mut app, Action::DisplayMessage));
        assert_eq!(
            request,
            Request::OpenFolder(FolderPath::Named(vec!["Archive".into()]))
        );
    }

    #[test]
    fn flag_changes_to_a_message_go_out_one_at_a_time() {
        let mut app = app(settings());
        let (first, _) = sent(&act(&mut app, Action::FlagMessage));
        // Toggled back before the first change is saved: it waits.
        assert!(act(&mut app, Action::FlagMessage).is_empty());
        assert!(!app.messages[0].flags.flagged);
        let effects = respond(&mut app, first, Event::FlagsUpdated(Ok(())));
        let (second, request) = sent(&effects);
        assert!(matches!(
            request,
            Request::UpdateFlags(
                _,
                FlagsUpdate {
                    flagged: Some(false),
                    ..
                }
            )
        ));
        // Flagged again while that goes out, and it fails: the mailbox
        // still has the flag from the first update, which is what the index
        // shows, so nothing more is sent.
        act(&mut app, Action::FlagMessage);
        let effects = respond(&mut app, second, Event::FlagsUpdated(Err(offline())));
        assert!(effects.is_empty(), "{effects:?}");
        assert!(app.messages[0].flags.flagged);
        assert!(!app.saving());

        // Toggled three times while the first is on its way, and the
        // first fails: what differs from the mailbox goes out.
        let (first, _) = sent(&act(&mut app, Action::ToggleNew));
        act(&mut app, Action::ToggleNew);
        act(&mut app, Action::ToggleNew);
        let effects = respond(&mut app, first, Event::FlagsUpdated(Err(offline())));
        assert!(matches!(
            sent(&effects).1,
            Request::UpdateFlags(_, update) if update == FlagsUpdate::read(true)
        ));
    }

    #[test]
    fn transfers_that_needed_a_sign_in_are_retried() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        let (id, request) = sent(&answer(&mut app, "=Archive"));
        signed_out(&mut app);
        respond(&mut app, id, Event::Transferred(Err(expired())));
        let effects = act(&mut app, Action::Retry);
        assert_eq!(sent(&effects).1, request);
    }

    #[test]
    fn a_move_waits_for_the_message_s_flag_update() {
        let mut app = app(settings());
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        act(&mut app, Action::SaveMessage);
        assert!(answer(&mut app, "=Archive").is_empty(), "waits");
        assert!(app.saving());
        let effects = respond(&mut app, flag, Event::FlagsUpdated(Ok(())));
        assert!(matches!(sent(&effects).1, Request::Transfer { .. }));
    }

    #[test]
    fn failed_moves_are_reported_on_exit() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        let (id, _) = sent(&answer(&mut app, "=Archive"));
        transferred(&mut app, id, "arc", "m0", Err(offline()));
        assert_eq!(app.save_failures(), ["network error: offline"]);
    }

    #[test]
    fn a_failed_flag_change_goes_out_again_with_the_next() {
        let mut app = app(settings());
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        act(&mut app, Action::ToggleNew);
        let effects = respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
        assert!(matches!(
            sent(&effects).1,
            Request::UpdateFlags(
                _,
                FlagsUpdate {
                    flagged: Some(true),
                    read: Some(true)
                }
            )
        ));
    }

    #[test]
    fn transfers_into_the_open_folder_list_it_again() {
        let mut app = app(settings());
        act(&mut app, Action::CopyMessage);
        let (id, _) = sent(&answer(&mut app, "=@inbox"));
        let effects = transferred(&mut app, id, "in", "m0", Ok("c0"));
        assert!(matches!(sent(&effects).1, Request::ListMessages { .. }));
    }

    #[test]
    fn failures_follow_messages_moved_on_again() {
        let mut app = app(settings());
        let archive = Some(FolderPath::Named(vec!["Archive".into()]));
        let other = Some(FolderPath::Named(vec!["Other".into()]));
        let failure = |id: &str, transfer, to: &Option<FolderPath>| {
            (
                (MessageId::new(id), transfer, to.clone()),
                format!("{id} failed"),
            )
        };
        app.transfer_failures = HashMap::from([
            failure("a", Transfer::Save, &archive),
            failure("a", Transfer::Purge, &None),
            failure("b", Transfer::Copy, &other),
            failure("b", Transfer::Purge, &None),
            failure("z", Transfer::Save, &archive),
        ]);
        let unsaved = ("offline".to_owned(), FlagsUpdate::read(true));
        app.save_failures.insert("a".into(), unsaved.clone());
        // a became b, and b (another message) became c: what failed for
        // a goes along to c, as what failed for b does.
        app.follow_moves(&[("a".into(), "b".into()), ("b".into(), "c".into())]);
        let mut failures: Vec<_> = app
            .transfer_failures
            .iter()
            .map(|((id, transfer, to), e)| format!("{id:?} {transfer:?} {to:?}: {e}"))
            .collect();
        failures.sort();
        let expected = [
            (failure("c", Transfer::Copy, &other).0, "b failed".into()),
            (failure("c", Transfer::Save, &archive).0, "a failed".into()),
            failure("z", Transfer::Save, &archive),
        ]
        .map(|((id, transfer, to), e)| format!("{id:?} {transfer:?} {to:?}: {e}"));
        assert_eq!(failures, expected);
        assert_eq!(
            app.save_failures,
            HashMap::from([(MessageId::new("c"), unsaved)])
        );
    }

    #[test]
    fn a_failed_flag_change_follows_the_move() {
        let mut app = app(settings());
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        act(&mut app, Action::SaveMessage);
        assert!(answer(&mut app, "=Archive").is_empty(), "waits its turn");
        let effects = respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
        let (moving, _) = sent(&effects);
        transferred(&mut app, moving, "archive", "m0", Ok("a0"));
        assert!(!app.saving());
        // Trying again changes the moved message.
        assert!(app.save_failures.contains_key(&MessageId::new("a0")));
        assert!(!app.save_failures.contains_key(&MessageId::new("m0")));
    }

    #[test]
    fn a_purge_waiting_for_flags_counts_as_in_progress() {
        let mut app = yes();
        sent(&act(&mut app, Action::FlagMessage));
        act(&mut app, Action::DeleteMessage);
        assert!(
            act(&mut app, Action::SyncMailbox).is_empty(),
            "waits for the flags"
        );
        act(&mut app, Action::SyncMailbox);
        assert_eq!(app.status, "Deleted messages are already being purged.");
    }

    #[test]
    fn messages_already_gone_count_as_purged() {
        let mut app = yes();
        act(&mut app, Action::DeleteMessage);
        let (id, _) = sent(&act(&mut app, Action::SyncMailbox));
        respond(
            &mut app,
            id,
            Event::Purged(Ok(vec![(
                "m0".into(),
                Err(Error::not_found("message", "m0")),
            )])),
        );
        assert_eq!(app.deleted_count(), 0);
        assert_eq!(ids(&app), ["m1", "m2"]);
    }

    #[test]
    fn a_copy_does_not_make_up_for_a_failed_move() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        let (id, _) = sent(&answer(&mut app, "=Archive"));
        transferred(&mut app, id, "arc", "m0", Err(offline()));
        act(&mut app, Action::CopyMessage);
        let (id, _) = sent(&answer(&mut app, "=Archive"));
        transferred(&mut app, id, "arc", "m0", Ok("c0"));
        assert_eq!(app.save_failures(), ["network error: offline"]);
    }

    #[test]
    fn quitting_retries_transfers_too() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        let (id, request) = sent(&answer(&mut app, "=Archive"));
        act(&mut app, Action::Quit);
        assert!(app.quit);
        signed_out(&mut app);
        respond(&mut app, id, Event::Transferred(Err(expired())));
        let effects = act(&mut app, Action::Retry);
        assert_eq!(sent(&effects).1, request);
    }

    #[test]
    fn a_copy_into_the_open_folder_waits_for_the_pager_to_close() {
        let mut app = app(settings());
        let (read, _) = sent(&show_m0(&mut app));
        respond(&mut app, read, Event::FlagsUpdated(Ok(())));
        act(&mut app, Action::CopyMessage);
        let (id, _) = sent(&answer(&mut app, "=@inbox"));
        let effects = transferred(&mut app, id, "in", "m0", Ok("c0"));
        assert!(effects.is_empty(), "not under the pager: {effects:?}");
        assert_eq!(ids(&app), ["m0", "m1", "m2"]);
        let effects = act(&mut app, Action::Exit);
        assert!(matches!(sent(&effects).1, Request::ListMessages { .. }));
    }

    #[test]
    fn a_failed_save_is_retried_after_changing_folder() {
        let mut app = app(settings());
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        act(&mut app, Action::ChangeFolder);
        answer(&mut app, "=Archive");
        signed_out(&mut app);
        respond(&mut app, flag, Event::FlagsUpdated(Err(expired())));
        let effects = act(&mut app, Action::Retry);
        assert!(
            effects.iter().any(|e| matches!(e,
                Effect::Request(_, Request::UpdateFlags(id, _)) if id.as_str() == "m0")),
            "{effects:?}"
        );
    }

    #[test]
    fn leaving_waits_if_the_purge_left_a_message_behind() {
        let mut app = yes();
        act(&mut app, Action::DeleteMessage);
        act(&mut app, Action::DeleteMessage);
        let (purge, _) = sent(&act(&mut app, Action::Quit));
        respond(
            &mut app,
            purge,
            Event::Purged(Ok(vec![
                ("m0".into(), Err(offline())),
                ("m1".into(), Ok(())),
            ])),
        );
        assert!(!app.quit, "m0 is still marked: stay");
        assert!(app.is_deleted(&"m0".into()));
        assert!(!app.leaving());
    }

    #[test]
    fn a_purge_after_a_failed_move_still_goes_ahead() {
        let mut app = yes();
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        act(&mut app, Action::DeleteMessage);
        act(&mut app, Action::PreviousEntry);
        act(&mut app, Action::SaveMessage);
        answer(&mut app, "=Archive");
        assert!(act(&mut app, Action::Quit).is_empty(), "waits its turn");
        assert!(app.leaving());
        let (moving, _) = sent(&respond(
            &mut app,
            flag,
            Event::FlagsUpdated(Err(offline())),
        ));
        // The move fails: m0 stays, marked, and is purged.
        let (purge, request) = sent(&respond(
            &mut app,
            moving,
            Event::Transferred(Err(offline())),
        ));
        assert!(
            matches!(request, Request::Purge { ref ids, .. } if ids == &[MessageId::new("m0")])
        );
        purged(&mut app, purge, "m0");
        assert!(app.quit);
    }

    #[test]
    fn a_purge_leaves_out_messages_moved_away_before_it() {
        let mut app = yes();
        act(&mut app, Action::DeleteMessage);
        act(&mut app, Action::PreviousEntry);
        act(&mut app, Action::SaveMessage);
        let (moving, _) = sent(&answer(&mut app, "=Archive"));
        assert!(act(&mut app, Action::Quit).is_empty(), "waits its turn");
        let effects = transferred(&mut app, moving, "archive", "m0", Ok("a0"));
        assert!(effects.is_empty(), "nothing left to purge: {effects:?}");
        assert_eq!(app.status, "Mailbox is unchanged.");
        assert!(app.quit);
    }

    #[test]
    fn a_purge_waits_for_a_copy() {
        let mut app = yes();
        act(&mut app, Action::DeleteMessage);
        act(&mut app, Action::PreviousEntry);
        act(&mut app, Action::CopyMessage);
        let (copy, _) = sent(&answer(&mut app, "=Archive"));
        assert!(
            act(&mut app, Action::SyncMailbox).is_empty(),
            "waits its turn"
        );
        let (_, request) = sent(&transferred(&mut app, copy, "archive", "m0", Ok("c0")));
        assert!(
            matches!(request, Request::Purge { ref ids, .. } if ids == &[MessageId::new("m0")])
        );
    }

    #[test]
    fn resolve_waits_for_the_next_page() {
        let inbox = FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
        let (mut app, effects) = App::open(80, 24, inbox, settings());
        let (open, _) = sent(&effects);
        let effects = respond(
            &mut app,
            open,
            Event::FolderOpened(Ok(mock::folder("in", "Inbox", None))),
        );
        // The listing, and the user's own addresses alongside.
        let Effect::Request(first, _) = effects[0] else {
            panic!("expected a request, got {effects:?}");
        };
        let message = |id: &str, minutes| {
            mock::message(
                id,
                "in",
                "s",
                DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes),
            )
            .summary
        };
        let effects = listed(&mut app, first, vec![message("m0", 9)], Some("c"));
        let (second, _) = sent(&effects);
        act(&mut app, Action::DeleteMessage);
        assert_eq!(app.selected, 0);
        listed(&mut app, second, vec![message("m1", 8)], None);
        assert_eq!(app.selected, 1, "moved on once it arrived");
    }

    #[test]
    fn the_browser_opens_again_after_a_sign_in() {
        let mut app = app(settings());
        let (id, _) = sent(&act(&mut app, Action::BrowseMailboxes));
        signed_out(&mut app);
        respond(&mut app, id, Event::FoldersListed(Err(expired())));
        assert_eq!(app.screen(), &Screen::Index);
        let (_, request) = sent(&act(&mut app, Action::Retry));
        assert_eq!(request, Request::ListFolders);
        assert_eq!(app.menu(), Menu::Browser);
    }

    #[test]
    fn a_message_being_moved_is_left_alone() {
        let mut app = app(settings());
        act(&mut app, Action::DeleteMessage);
        act(&mut app, Action::PreviousEntry);
        act(&mut app, Action::SaveMessage);
        let (moving, _) = sent(&answer(&mut app, "=Archive"));
        // Marked until the move succeeds, but not purged meanwhile.
        assert!(app.is_deleted(&"m0".into()));
        for action in [
            Action::DeleteMessage,
            Action::FlagMessage,
            Action::SaveMessage,
            Action::CopyMessage,
        ] {
            assert!(act(&mut app, action).is_empty());
            assert_eq!(
                app.status,
                "The message is still being moved, copied or purged."
            );
        }
        act(&mut app, Action::SyncMailbox);
        assert!(
            key(&mut app, KeyCode::Char('y')).is_empty(),
            "waits for the move"
        );
        // A move that fails keeps the mark, and the purge takes it.
        let effects = respond(&mut app, moving, Event::Transferred(Err(offline())));
        assert!(app.is_deleted(&"m0".into()));
        assert!(matches!(sent(&effects).1, Request::Purge { .. }));
    }

    #[test]
    fn a_folder_named_like_a_well_known_one_is_itself() {
        let mut app = app(settings());
        // Browse from the question.
        act(&mut app, Action::SaveMessage);
        let (id, _) = sent(&key(&mut app, KeyCode::Tab));
        respond(
            &mut app,
            id,
            Event::FoldersListed(Ok(vec![mock::folder("odd", "@archive", None)])),
        );
        act(&mut app, Action::DisplayMessage);
        let (_, request) = sent(&key(&mut app, KeyCode::Enter));
        assert!(matches!(request, Request::Transfer { ref to, .. }
            if *to == FolderPath::Named(vec!["@archive".into()])));
    }

    #[test]
    fn a_listing_error_shows_on_the_question() {
        let mut app = app(settings());
        act(&mut app, Action::CopyMessage);
        let (id, _) = sent(&key(&mut app, KeyCode::Tab));
        respond(&mut app, id, Event::FoldersListed(Err(offline())));
        assert_eq!(
            prompt(&app),
            "network error: offline. Copy to mailbox ('?' for list): "
        );
    }

    #[test]
    fn a_copy_into_the_open_folder_shows_once_the_browser_closes() {
        let mut app = app(settings());
        act(&mut app, Action::CopyMessage);
        let (copy, _) = sent(&answer(&mut app, "=@inbox"));
        sent(&act(&mut app, Action::BrowseMailboxes));
        transferred(&mut app, copy, "in", "m0", Ok("c0"));
        let effects = act(&mut app, Action::Exit);
        assert!(matches!(sent(&effects).1, Request::ListMessages { .. }));
    }

    #[test]
    fn the_last_flag_choice_survives_a_folder_change() {
        let mut app = app(settings());
        let (first, _) = sent(&act(&mut app, Action::FlagMessage));
        act(&mut app, Action::FlagMessage);
        act(&mut app, Action::ChangeFolder);
        let (open, _) = sent(&answer(&mut app, "=Archive"));
        respond(
            &mut app,
            open,
            Event::FolderOpened(Ok(mock::folder("arc", "Archive", None))),
        );
        // The first update lands; the user's last choice (unflagged) goes
        // out though m0 is no longer in the index.
        let effects = respond(&mut app, first, Event::FlagsUpdated(Ok(())));
        assert!(
            matches!(sent(&effects).1, Request::UpdateFlags(ref id, FlagsUpdate { flagged: Some(false), .. }) if id.as_str() == "m0"),
            "{effects:?}"
        );
    }

    #[test]
    fn an_unrelated_save_does_not_hide_a_failed_one() {
        let mut app = app(settings());
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
        let (read, _) = sent(&act(&mut app, Action::ToggleNew));
        respond(&mut app, read, Event::FlagsUpdated(Ok(())));
        assert_eq!(app.save_failures(), ["network error: offline"]);
    }

    #[test]
    fn a_message_being_copied_is_not_moved() {
        let mut app = app(settings());
        act(&mut app, Action::CopyMessage);
        sent(&answer(&mut app, "=Archive"));
        assert!(act(&mut app, Action::SaveMessage).is_empty());
        assert_eq!(
            app.status,
            "The message is still being moved, copied or purged."
        );
        // Flags can change while it is only copied (once it is).
        assert!(act(&mut app, Action::FlagMessage).is_empty());
        assert!(app.messages[0].flags.flagged);
    }

    #[test]
    fn a_folder_change_that_needed_a_sign_in_is_retried() {
        let mut app = app(settings());
        act(&mut app, Action::ChangeFolder);
        let (open, _) = sent(&answer(&mut app, "=Archive"));
        signed_out(&mut app);
        respond(&mut app, open, Event::FolderOpened(Err(expired())));
        let (_, request) = sent(&act(&mut app, Action::Retry));
        assert_eq!(
            request,
            Request::OpenFolder(FolderPath::Named(vec!["Archive".into()]))
        );
    }

    #[test]
    fn a_browser_reopened_on_retry_takes_the_keys() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        let (list, _) = sent(&key(&mut app, KeyCode::Tab));
        signed_out(&mut app);
        respond(&mut app, list, Event::FoldersListed(Err(expired())));
        act(&mut app, Action::Retry);
        assert_eq!(app.menu(), Menu::Browser);
        assert!(app.input.is_none(), "the question waits for the browser");
    }

    #[test]
    fn a_purge_on_its_way_cannot_be_undeleted() {
        let mut app = yes();
        act(&mut app, Action::DeleteMessage);
        act(&mut app, Action::PreviousEntry);
        sent(&act(&mut app, Action::SyncMailbox));
        act(&mut app, Action::UndeleteMessage);
        assert_eq!(app.status, "The message is already being purged.");
        assert!(app.is_deleted(&"m0".into()));
    }

    #[test]
    fn a_failed_flag_change_follows_its_message_when_moved() {
        let mut app = app(settings());
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        respond(&mut app, flag, Event::FlagsUpdated(Err(expired())));
        act(&mut app, Action::SaveMessage);
        let (save, _) = sent(&answer(&mut app, "=Archive"));
        transferred(&mut app, save, "arc", "m0", Ok("a0"));
        signed_out(&mut app);
        let effects = act(&mut app, Action::Retry);
        assert!(
            effects.iter().any(|e| matches!(e,
                Effect::Request(_, Request::UpdateFlags(id, _)) if id.as_str() == "a0")),
            "{effects:?}"
        );
    }

    #[test]
    fn a_cancelled_question_forgets_the_browsed_folder() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        let (id, _) = sent(&key(&mut app, KeyCode::Tab));
        respond(
            &mut app,
            id,
            Event::FoldersListed(Ok(vec![mock::folder("odd", "@archive", None)])),
        );
        act(&mut app, Action::DisplayMessage);
        key(&mut app, KeyCode::Esc);
        act(&mut app, Action::SaveMessage);
        let (_, request) = sent(&answer(&mut app, "@archive"));
        assert!(matches!(request, Request::Transfer { ref to, .. }
            if *to == FolderPath::WellKnown(WellKnownFolder::Archive, vec![])));
    }

    #[test]
    fn a_newer_folder_change_drops_an_older_retry() {
        let mut app = app(settings());
        act(&mut app, Action::ChangeFolder);
        let (a, _) = sent(&answer(&mut app, "=A"));
        respond(&mut app, a, Event::FolderOpened(Err(expired())));
        act(&mut app, Action::ChangeFolder);
        let (b, _) = sent(&answer(&mut app, "=B"));
        respond(
            &mut app,
            b,
            Event::FolderOpened(Ok(mock::folder("b", "B", None))),
        );
        signed_out(&mut app);
        let effects = act(&mut app, Action::Retry);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::OpenFolder(_)))),
            "{effects:?}"
        );
    }

    #[test]
    fn no_marks_while_opening_another_folder() {
        let mut app = app(settings());
        act(&mut app, Action::ChangeFolder);
        sent(&answer(&mut app, "=Archive"));
        act(&mut app, Action::DeleteMessage);
        assert_eq!(app.status, "Leaving the folder; wait a moment.");
        assert_eq!(app.deleted_count(), 0);
    }

    #[test]
    fn leaving_waits_for_moves_of_marked_messages() {
        let mut app = yes();
        act(&mut app, Action::DeleteMessage);
        act(&mut app, Action::PreviousEntry);
        act(&mut app, Action::SaveMessage);
        let (save, _) = sent(&answer(&mut app, "=Archive"));
        // m0 is marked and being moved: quitting waits for the move.
        assert!(act(&mut app, Action::Quit).is_empty());
        assert!(!app.quit);
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::DeleteMessage);
        assert_eq!(app.status, "Leaving the folder; wait a moment.");
        // The move fails: m0 stays, still marked, and is purged.
        let effects = respond(&mut app, save, Event::Transferred(Err(offline())));
        let (purge, request) = sent(&effects);
        assert!(
            matches!(request, Request::Purge { ref ids, .. } if ids == &[MessageId::new("m0")])
        );
        purged(&mut app, purge, "m0");
        assert!(app.quit);
    }

    #[test]
    fn a_message_being_purged_is_left_alone() {
        let mut app = yes();
        act(&mut app, Action::DeleteMessage);
        act(&mut app, Action::PreviousEntry);
        sent(&act(&mut app, Action::SyncMailbox));
        for action in [Action::SaveMessage, Action::FlagMessage] {
            assert!(act(&mut app, action).is_empty());
            assert_eq!(
                app.status,
                "The message is still being moved, copied or purged."
            );
        }
    }

    #[test]
    fn a_save_question_keeps_its_message() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        // The index moves on meanwhile (say, a relist).
        app.selected = 2;
        let (_, request) = sent(&answer(&mut app, "=Archive"));
        assert!(
            matches!(request, Request::Transfer { ref ids, .. } if ids == &[MessageId::new("m0")])
        );
    }

    #[test]
    fn a_relist_keeps_flag_changes_on_their_way() {
        let mut app = app(settings());
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        // Listed again before the flag is saved; the listing still has m0
        // unflagged.
        let (list, _) = sent(&app.relist());
        listed(&mut app, list, vec![m0().summary], None);
        assert!(app.messages[0].flags.flagged, "still the user's choice");
        respond(&mut app, flag, Event::FlagsUpdated(Ok(())));
        assert!(app.messages[0].flags.flagged);
    }

    #[test]
    fn a_listing_older_than_a_saved_flag_keeps_it() {
        let mut app = app(settings());
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        let (list, _) = sent(&app.relist());
        // Saved before the listing, which was taken earlier, comes back.
        respond(&mut app, flag, Event::FlagsUpdated(Ok(())));
        listed(&mut app, list, vec![m0().summary], None);
        assert!(app.messages[0].flags.flagged);
        // The next listing is newer: it is taken as it is.
        let (list, _) = sent(&app.relist());
        listed(&mut app, list, vec![m0().summary], None);
        assert!(!app.messages[0].flags.flagged);
    }

    #[test]
    fn failed_flag_changes_are_kept_together() {
        let mut app = app(settings());
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
        let (read, _) = sent(&act(&mut app, Action::ToggleNew));
        respond(&mut app, read, Event::FlagsUpdated(Err(offline())));
        let (_, failed) = &app.save_failures[&MessageId::new("m0")];
        assert_eq!(
            *failed,
            FlagsUpdate {
                read: Some(true),
                flagged: Some(true),
            }
        );
        // Saving one of them leaves the other to try again.
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        respond(&mut app, flag, Event::FlagsUpdated(Ok(())));
        let (_, failed) = &app.save_failures[&MessageId::new("m0")];
        assert_eq!(*failed, FlagsUpdate::read(true));
    }

    #[test]
    fn a_message_read_while_being_moved_is_marked_where_it_went() {
        let mut app = app(settings());
        let (fetch, _) = sent(&act(&mut app, Action::DisplayMessage));
        // Saved from the pager before the message arrives.
        act(&mut app, Action::SaveMessage);
        let (moving, _) = sent(&answer(&mut app, "=Archive"));
        let effects = respond(&mut app, fetch, Event::MessageFetched(Ok(m0())));
        assert!(effects.is_empty(), "the read mark waits its turn");
        let (_, request) = sent(&transferred(&mut app, moving, "archive", "m0", Ok("a0")));
        assert!(
            matches!(request, Request::UpdateFlags(ref id, _) if id.as_str() == "a0"),
            "{request:?}"
        );
    }

    #[test]
    fn a_pager_under_the_browser_closes_when_its_message_moves() {
        let mut app = app(settings());
        let (read, _) = sent(&show_m0(&mut app));
        respond(&mut app, read, Event::FlagsUpdated(Ok(())));
        act(&mut app, Action::SaveMessage);
        let (moving, _) = sent(&answer(&mut app, "=Archive"));
        sent(&act(&mut app, Action::BrowseMailboxes));
        transferred(&mut app, moving, "archive", "m0", Ok("a0"));
        assert_eq!(app.menu(), Menu::Browser);
        act(&mut app, Action::Exit);
        assert_eq!(app.screen(), &Screen::Index);
        assert_eq!(ids(&app), ["m1", "m2"]);
    }

    #[test]
    fn a_blank_mailbox_answer_forgets_the_browsed_folder() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        app.browsed = Some(("Archive".into(), FolderPath::Named(vec!["@archive".into()])));
        assert!(answer(&mut app, "").is_empty());
        assert!(app.browsed.is_none());
        assert!(app.question_message.is_none());
    }

    #[test]
    fn a_change_undone_meanwhile_is_not_a_failure() {
        let mut app = app(settings());
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        assert!(act(&mut app, Action::FlagMessage).is_empty(), "queued");
        let effects = respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
        assert!(effects.is_empty(), "nothing left to change");
        assert!(app.save_failures().is_empty());
        assert!(!app.saving());
    }

    #[test]
    fn a_purged_message_has_nothing_left_to_save() {
        let mut app = yes();
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
        app.selected = 0;
        act(&mut app, Action::DeleteMessage);
        let (purge, _) = sent(&act(&mut app, Action::SyncMailbox));
        purged(&mut app, purge, "m0");
        assert!(app.save_failures().is_empty());
    }

    #[test]
    fn copying_again_replaces_the_retry() {
        let mut app = app(settings());
        act(&mut app, Action::CopyMessage);
        let (id, _) = sent(&answer(&mut app, "=Archive"));
        signed_out(&mut app);
        respond(&mut app, id, Event::Transferred(Err(expired())));
        app.sign_in = None;
        act(&mut app, Action::CopyMessage);
        let (id, _) = sent(&answer(&mut app, "=Archive"));
        transferred(&mut app, id, "archive", "m0", Ok("c0"));
        let effects = act(&mut app, Action::Retry);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::Transfer { .. }))),
            "{effects:?}"
        );
    }

    #[test]
    fn moving_elsewhere_replaces_the_retry() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        let (id, _) = sent(&answer(&mut app, "=Archive"));
        signed_out(&mut app);
        respond(&mut app, id, Event::Transferred(Err(expired())));
        app.sign_in = None;
        act(&mut app, Action::SaveMessage);
        let (id, _) = sent(&answer(&mut app, "=Projects"));
        transferred(&mut app, id, "projects", "m0", Ok("p0"));
        let effects = act(&mut app, Action::Retry);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::Transfer { .. }))),
            "{effects:?}"
        );
    }

    #[test]
    fn no_replies_to_a_message_being_moved() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        sent(&answer(&mut app, "=Archive"));
        app.selected = 0;
        for action in [Action::Reply, Action::GroupReply, Action::Forward] {
            assert!(act(&mut app, action).is_empty());
            assert_eq!(
                app.status,
                "The message is still being moved, copied or purged."
            );
        }
    }

    #[test]
    fn a_save_that_works_the_second_time_is_not_a_failure() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        let (id, _) = sent(&answer(&mut app, "=Archive"));
        transferred(&mut app, id, "archive", "m0", Err(offline()));
        assert!(!app.save_failures().is_empty());
        app.selected = 0;
        act(&mut app, Action::SaveMessage);
        let (id, _) = sent(&answer(&mut app, "=Archive"));
        transferred(&mut app, id, "archive", "m0", Ok("a0"));
        assert!(app.save_failures().is_empty(), "{:?}", app.save_failures());
    }

    #[test]
    fn a_message_moved_into_the_open_folder_leaves_its_old_id() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        let (id, _) = sent(&answer(&mut app, "=@inbox"));
        let effects = transferred(&mut app, id, "in", "m0", Ok("n0"));
        assert!(!app.messages.iter().any(|m| m.id.as_str() == "m0"));
        assert!(
            matches!(sent(&effects).1, Request::ListMessages { .. }),
            "listed again"
        );
    }

    #[test]
    fn quitting_during_a_purge_quits_after_it() {
        let mut app = yes();
        act(&mut app, Action::DeleteMessage);
        let (purge, _) = sent(&act(&mut app, Action::SyncMailbox));
        assert!(act(&mut app, Action::Quit).is_empty(), "no second purge");
        assert!(!app.quit);
        purged(&mut app, purge, "m0");
        assert!(app.quit);
    }

    #[test]
    fn a_purge_retried_after_a_sign_in_still_quits() {
        let mut app = yes();
        act(&mut app, Action::DeleteMessage);
        let (purge, _) = sent(&act(&mut app, Action::Quit));
        signed_out(&mut app);
        respond(&mut app, purge, Event::Purged(Err(expired())));
        assert!(!app.quit);
        let (purge, _) = sent(&act(&mut app, Action::Retry));
        purged(&mut app, purge, "m0");
        assert!(app.quit);
    }

    #[test]
    fn an_undeleted_message_is_not_purged_on_retry() {
        let mut app = yes();
        act(&mut app, Action::DeleteMessage);
        act(&mut app, Action::PreviousEntry);
        let (purge, _) = sent(&act(&mut app, Action::SyncMailbox));
        signed_out(&mut app);
        respond(&mut app, purge, Event::Purged(Err(expired())));
        app.sign_in = None;
        act(&mut app, Action::UndeleteMessage);
        assert!(!app.is_deleted(&"m0".into()));
        let effects = act(&mut app, Action::Retry);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::Purge { .. }))),
            "{effects:?}"
        );
        assert!(app.save_failures().is_empty());
    }

    #[test]
    fn a_cancelled_question_forgets_its_browser() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        let (list, _) = sent(&key(&mut app, KeyCode::Tab));
        signed_out(&mut app);
        respond(&mut app, list, Event::FoldersListed(Err(expired())));
        app.sign_in = None;
        assert!(app.input.is_some(), "the question is back");
        key(&mut app, KeyCode::Esc);
        assert!(app.input.is_none());
        act(&mut app, Action::Retry);
        assert_ne!(app.menu(), Menu::Browser);
        assert!(app.input.is_none());
    }

    #[test]
    fn a_partly_undone_change_keeps_only_what_still_failed() {
        let mut app = app(settings());
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        act(&mut app, Action::FlagMessage);
        act(&mut app, Action::ToggleNew);
        let effects = respond(&mut app, flag, Event::FlagsUpdated(Err(offline())));
        let (read, request) = sent(&effects);
        assert!(matches!(
            request,
            Request::UpdateFlags(_, FlagsUpdate { flagged: None, .. })
        ));
        respond(&mut app, read, Event::FlagsUpdated(Ok(())));
        assert!(app.save_failures().is_empty(), "{:?}", app.save_failures());
    }

    #[test]
    fn a_listing_older_than_a_purge_does_not_bring_it_back() {
        let mut app = yes();
        act(&mut app, Action::DeleteMessage);
        app.selected = 0;
        act(&mut app, Action::CopyMessage);
        let (copy, _) = sent(&answer(&mut app, "=@inbox"));
        assert!(
            act(&mut app, Action::SyncMailbox).is_empty(),
            "behind the copy"
        );
        let effects = transferred(&mut app, copy, "in", "m0", Ok("c0"));
        let list = request_for(&effects, |r| matches!(r, Request::ListMessages { .. }))
            .expect("listed again");
        let purge = request_for(&effects, |r| matches!(r, Request::Purge { .. }))
            .expect("the purge goes next");
        purged(&mut app, purge, "m0");
        let messages = ["m0", "c0", "m1"]
            .map(|id| mock::message(id, "in", "s", DateTime::UNIX_EPOCH).summary)
            .to_vec();
        listed(&mut app, list, messages, None);
        assert!(!app.messages.iter().any(|m| m.id.as_str() == "m0"));
    }

    fn lists_folders(effects: &[Effect]) -> bool {
        effects
            .iter()
            .any(|e| matches!(e, Effect::Request(_, Request::ListFolders)))
    }

    #[test]
    fn an_off_screen_flag_retry_goes_out_once() {
        let mut app = app(settings());
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        signed_out(&mut app);
        respond(&mut app, flag, Event::FlagsUpdated(Err(expired())));
        // Its folder isn't open any more.
        app.messages.clear();
        let first = app.retry();
        assert!(matches!(sent(&first).1, Request::UpdateFlags(..)));
        assert!(
            !app.retry()
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::UpdateFlags(..)))),
            "already on its way"
        );
    }

    #[test]
    fn a_browser_asked_for_before_the_folder_opened_comes_back() {
        let (mut app, effects) = App::new(80, 23);
        let (open, _) = sent(&effects);
        let (list, _) = sent(&act(&mut app, Action::BrowseMailboxes));
        signed_out(&mut app);
        respond(&mut app, open, Event::FolderOpened(Err(expired())));
        respond(&mut app, list, Event::FoldersListed(Err(expired())));
        let effects = app.retry();
        assert!(lists_folders(&effects), "{effects:?}");
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::OpenFolder(_))))
        );
    }

    #[test]
    fn an_answered_question_forgets_its_browser() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        let (list, _) = sent(&key(&mut app, KeyCode::Tab));
        signed_out(&mut app);
        respond(&mut app, list, Event::FoldersListed(Err(expired())));
        app.sign_in = None;
        sent(&answer(&mut app, "=Archive"));
        assert!(!lists_folders(&app.retry()));
    }

    fn purge_failed_for_sign_in(app: &mut App) {
        act(app, Action::DeleteMessage);
        app.selected = 0;
        let (purge, _) = sent(&act(app, Action::SyncMailbox));
        signed_out(app);
        respond(app, purge, Event::Purged(Err(expired())));
        app.sign_in = None;
    }

    #[test]
    fn declining_a_purge_gives_up_its_retry() {
        let mut app = yes();
        purge_failed_for_sign_in(&mut app);
        app.delete = Quad::No;
        act(&mut app, Action::SyncMailbox);
        assert!(app.save_failures().is_empty());
        assert!(
            !app.retry()
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::Purge { .. })))
        );
    }

    #[test]
    fn moving_a_message_away_ends_its_purge_failure() {
        let mut app = yes();
        purge_failed_for_sign_in(&mut app);
        app.selected = 0;
        act(&mut app, Action::SaveMessage);
        let (id, _) = sent(&answer(&mut app, "=Archive"));
        transferred(&mut app, id, "archive", "m0", Ok("a0"));
        assert!(app.save_failures().is_empty(), "{:?}", app.save_failures());
    }

    #[test]
    fn cancelling_a_purge_gives_up_its_retry() {
        let mut app = yes();
        purge_failed_for_sign_in(&mut app);
        app.delete = Quad::AskYes;
        act(&mut app, Action::SyncMailbox);
        key(&mut app, KeyCode::Esc);
        assert!(app.save_failures().is_empty());
        assert!(
            !app.retry()
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::Purge { .. })))
        );
    }

    #[test]
    fn browsing_again_without_choosing_keeps_the_choice() {
        let mut app = app(settings());
        act(&mut app, Action::SaveMessage);
        let target = FolderPath::Named(vec!["@archive".into()]);
        app.browsed = Some(("=@archive".into(), target.clone()));
        // Browsing again, then leaving without a choice.
        app.input = None;
        app.open_browser(Choose::Answer(Question::SaveTo, "=@archive".into()));
        app.leave_browser(None);
        let (_, request) = sent(&key(&mut app, KeyCode::Enter));
        assert!(matches!(request, Request::Transfer { to, .. } if to == target));
    }

    fn flag_failed(app: &mut App) {
        let (flag, _) = sent(&act(app, Action::FlagMessage));
        respond(app, flag, Event::FlagsUpdated(Err(offline())));
    }

    #[test]
    fn an_off_screen_retry_leaves_other_flags_alone() {
        let mut app = app(settings());
        flag_failed(&mut app);
        let listed = app.messages.clone();
        app.messages.clear();
        let (retry, _) = sent(&app.retry());
        // Back in the folder before the retry is done; m0 is read there.
        app.messages = listed;
        app.messages[0].flags.read = true;
        app.messages[0].flags.flagged = false;
        respond(&mut app, retry, Event::FlagsUpdated(Ok(())));
        assert!(app.messages[0].flags.flagged);
        assert!(app.messages[0].flags.read, "untouched");
        assert!(app.save_failures().is_empty());
    }

    #[test]
    fn a_listing_showing_a_failed_change_made_settles_it() {
        let mut app = app(settings());
        flag_failed(&mut app);
        // The mailbox had it after all, as a new listing shows.
        app.messages[0].flags.flagged = true;
        let effects = app.retry();
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::UpdateFlags(..)))),
            "{effects:?}"
        );
        assert!(app.save_failures().is_empty());
    }

    #[test]
    fn a_save_before_a_failure_survives_an_older_listing() {
        let mut app = app(settings());
        let list = request_for(&app.relist(), |r| matches!(r, Request::ListMessages { .. }))
            .expect("listed again");
        app.messages = (0..3)
            .map(|i| mock::message(&format!("m{i}"), "in", "s", DateTime::UNIX_EPOCH).summary)
            .collect();
        let (flag, _) = sent(&act(&mut app, Action::FlagMessage));
        act(&mut app, Action::ToggleNew);
        let effects = respond(&mut app, flag, Event::FlagsUpdated(Ok(())));
        let (read, _) = sent(&effects);
        respond(&mut app, read, Event::FlagsUpdated(Err(offline())));
        // The listing taken before both changes arrives now, into an
        // index it fills afresh.
        app.messages.clear();
        listed(&mut app, list, vec![m0().summary], None);
        assert!(app.messages[0].flags.flagged, "the saved flag stays");
        assert!(
            !app.save_failures().is_empty(),
            "the read change still failed"
        );
    }

    fn switch_to(app: &mut App, text: &str, id: &str) {
        act(app, Action::ChangeFolder);
        let (open, _) = sent(&answer(app, text));
        respond(
            app,
            open,
            Event::FolderOpened(Ok(mock::folder(id, id, None))),
        );
    }

    #[test]
    fn no_writing_while_another_folder_opens() {
        let mut app = app(settings());
        act(&mut app, Action::ChangeFolder);
        sent(&answer(&mut app, "=Archive"));
        for action in [Action::Mail, Action::Reply, Action::Forward] {
            assert!(act(&mut app, action).is_empty());
            assert_eq!(app.status, "Opening folder; wait a moment.");
        }
    }

    #[test]
    fn mailbox_shortcuts_name_folders() {
        let mut app = app(Settings {
            shortcuts: crate::app::Shortcuts {
                spoolfile: "msgraph://alice@contoso.com/@inbox".into(),
                mbox: String::new(),
                record: "=@sentitems".into(),
            },
            ..settings()
        });
        let opened = app.path.clone();
        switch_to(&mut app, "=Archive", "arc");
        act(&mut app, Action::ChangeFolder);
        let (_, request) = sent(&answer(&mut app, "-"));
        assert_eq!(request, Request::OpenFolder(opened));
        app.switching = None;
        act(&mut app, Action::ChangeFolder);
        let (_, request) = sent(&answer(&mut app, "^"));
        assert_eq!(request, Request::OpenFolder(app.path.clone()));
        app.switching = None;
        act(&mut app, Action::ChangeFolder);
        let (_, request) = sent(&answer(&mut app, "!"));
        assert!(matches!(
            request,
            Request::OpenFolder(FolderPath::WellKnown(..))
        ));
        app.switching = None;
        act(&mut app, Action::ChangeFolder);
        let (_, request) = sent(&answer(&mut app, "<"));
        assert!(matches!(
            request,
            Request::OpenFolder(FolderPath::WellKnown(..))
        ));
        app.switching = None;
        act(&mut app, Action::ChangeFolder);
        assert!(answer(&mut app, ">").is_empty());
        assert_eq!(app.status, "mbox is not set");
    }

    #[test]
    fn a_change_during_an_off_screen_retry_keeps_the_retry() {
        let mut app = app(settings());
        flag_failed(&mut app);
        let listed = app.messages.clone();
        app.messages.clear();
        let (retry, _) = sent(&app.retry());
        // Back in the folder, which still shows m0 unflagged; the user
        // marks it read meanwhile.
        app.messages = listed;
        app.messages[0].flags.flagged = false;
        app.messages[0].flags.read = false;
        assert!(
            act(&mut app, Action::ToggleNew).is_empty(),
            "waits its turn"
        );
        assert!(app.messages[0].flags.flagged, "the retry shows");
        let effects = respond(&mut app, retry, Event::FlagsUpdated(Ok(())));
        let (_, request) = sent(&effects);
        let Request::UpdateFlags(_, update) = request else {
            panic!("expected a flag update, got {request:?}");
        };
        assert_eq!(update.flagged, None, "the retry isn't undone");
        assert_eq!(update.read, Some(true));
    }

    #[test]
    fn a_copy_elsewhere_doesnt_make_up_for_a_failed_one() {
        let mut app = app(settings());
        act(&mut app, Action::CopyMessage);
        let (id, _) = sent(&answer(&mut app, "=Archive"));
        transferred(&mut app, id, "archive", "m0", Err(offline()));
        app.selected = 0;
        act(&mut app, Action::CopyMessage);
        let (id, _) = sent(&answer(&mut app, "=Projects"));
        transferred(&mut app, id, "projects", "m0", Ok("p0"));
        assert_eq!(
            app.save_failures().len(),
            1,
            "the copy to Archive still failed"
        );
        // Copying to Archive again settles it.
        app.selected = 0;
        act(&mut app, Action::CopyMessage);
        let (id, _) = sent(&answer(&mut app, "=Archive"));
        transferred(&mut app, id, "archive", "m0", Ok("c0"));
        assert!(app.save_failures().is_empty());
    }

    #[test]
    fn a_copy_into_the_open_folder_keeps_the_selection() {
        let mut app = app(settings());
        app.selected = 1;
        act(&mut app, Action::CopyMessage);
        let (copy, _) = sent(&answer(&mut app, "=@inbox"));
        let effects = transferred(&mut app, copy, "in", "m1", Ok("c1"));
        let (list, _) = sent(&effects);
        let messages = ["c1", "m0", "m1", "m2"]
            .map(|id| mock::message(id, "in", "s", DateTime::UNIX_EPOCH).summary)
            .to_vec();
        listed(&mut app, list, messages, None);
        assert_eq!(app.messages[app.selected].id.as_str(), "m1");
    }

    #[test]
    fn a_failed_copy_into_the_open_folder_lists_nothing() {
        let mut app = app(settings());
        act(&mut app, Action::CopyMessage);
        let (copy, _) = sent(&answer(&mut app, "=@inbox"));
        let effects = transferred(&mut app, copy, "in", "m0", Err(offline()));
        assert!(effects.is_empty(), "{effects:?}");
    }

    #[test]
    fn an_off_screen_retry_survives_an_older_listing() {
        let mut app = app(settings());
        flag_failed(&mut app);
        app.messages.clear();
        let (retry, _) = sent(&app.retry());
        // The folder is listed again before the retry is done.
        let list = request_for(&app.relist(), |r| matches!(r, Request::ListMessages { .. }))
            .expect("listed again");
        respond(&mut app, retry, Event::FlagsUpdated(Ok(())));
        let mut m0 = m0().summary;
        m0.flags.read = true;
        listed(&mut app, list, vec![m0], None);
        assert!(app.messages[0].flags.flagged, "the retry's flag");
        assert!(app.messages[0].flags.read, "the rest as listed");
    }

    #[test]
    fn a_browser_opened_by_hand_replaces_the_one_to_retry() {
        let mut app = app(settings());
        let (list, _) = sent(&act(&mut app, Action::BrowseMailboxes));
        signed_out(&mut app);
        respond(&mut app, list, Event::FoldersListed(Err(expired())));
        app.sign_in = None;
        let (list, _) = sent(&act(&mut app, Action::BrowseMailboxes));
        respond(
            &mut app,
            list,
            Event::FoldersListed(Ok(vec![mock::folder("arc", "Archive", None)])),
        );
        act(&mut app, Action::Exit);
        assert!(!lists_folders(&app.retry()));
    }

    #[test]
    fn a_mark_made_during_a_purge_is_purged_before_quitting() {
        let mut app = yes();
        act(&mut app, Action::DeleteMessage);
        app.selected = 0;
        let (purge, _) = sent(&act(&mut app, Action::SyncMailbox));
        // m1 is marked while m0 is purged, then the user quits.
        app.selected = 1;
        act(&mut app, Action::DeleteMessage);
        act(&mut app, Action::Quit);
        let effects = purged(&mut app, purge, "m0");
        assert!(!app.quit, "m1 first");
        let (second, request) = sent(&effects);
        assert!(
            matches!(request, Request::Purge { ref ids, .. } if ids == &[MessageId::new("m1")])
        );
        purged(&mut app, second, "m1");
        assert!(app.quit);
    }

    #[test]
    fn unread_chosen_while_loading_stays_unread() {
        let mut app = app(settings());
        app.messages[0].flags.read = true;
        let (fetch, _) = sent(&act(&mut app, Action::DisplayMessage));
        let (toggle, _) = sent(&act(&mut app, Action::ToggleNew));
        let effects = respond(&mut app, fetch, Event::MessageFetched(Ok(m0())));
        assert!(effects.is_empty(), "not marked read again: {effects:?}");
        respond(&mut app, toggle, Event::FlagsUpdated(Ok(())));
        assert!(!app.messages[0].flags.read);
    }

    #[test]
    fn a_copy_to_the_same_folder_by_another_name_makes_up_for_a_failed_one() {
        let mut app = app(settings());
        act(&mut app, Action::CopyMessage);
        let (id, _) = sent(&answer(&mut app, "=Archive"));
        transferred(&mut app, id, "arc", "m0", Err(offline()));
        assert!(!app.save_failures().is_empty());
        app.selected = 0;
        act(&mut app, Action::CopyMessage);
        let (id, _) = sent(&answer(&mut app, "=@archive"));
        transferred(&mut app, id, "arc", "m0", Ok("c0"));
        assert!(app.save_failures().is_empty(), "{:?}", app.save_failures());
    }

    #[test]
    fn a_copy_to_the_same_folder_replaces_its_retry() {
        let mut app = app(settings());
        act(&mut app, Action::CopyMessage);
        let (id, _) = sent(&answer(&mut app, "=Archive"));
        signed_out(&mut app);
        // The folder was found, but copying needed a sign-in.
        transferred(&mut app, id, "arc", "m0", Err(expired()));
        app.sign_in = None;
        app.selected = 0;
        act(&mut app, Action::CopyMessage);
        let (id, _) = sent(&answer(&mut app, "=@archive"));
        transferred(&mut app, id, "arc", "m0", Ok("c0"));
        assert!(
            !app.retry()
                .iter()
                .any(|e| matches!(e, Effect::Request(_, Request::Transfer { .. })))
        );
    }

    #[test]
    fn copies_to_one_folder_by_two_names_are_retried_once() {
        let mut app = app(settings());
        signed_out(&mut app);
        for name in ["=Archive", "=@archive"] {
            app.sign_in = None;
            app.selected = 0;
            act(&mut app, Action::CopyMessage);
            let (id, _) = sent(&answer(&mut app, name));
            transferred(&mut app, id, "arc", "m0", Err(expired()));
        }
        // One retry is kept (changes go out one at a time, so look at what
        // is kept, not just what is sent first).
        let kept: Vec<_> = app
            .failed_transfers
            .iter()
            .flat_map(|(_, retry)| transfer_ids(retry).to_vec())
            .collect();
        assert_eq!(kept, [MessageId::new("m0")]);
        app.retry();
        assert_eq!(app.writes.len() + usize::from(app.writing.is_some()), 1);
    }

    #[test]
    fn the_pager_s_message_is_the_one_acted_on() {
        let mut app = app(Settings {
            resolve: false,
            ..settings()
        });
        show_m0(&mut app);
        // A background listing moves the index's selection meanwhile.
        app.selected = 1;
        act(&mut app, Action::DeleteMessage);
        assert!(app.is_deleted(&"m0".into()));
        assert!(!app.is_deleted(&"m1".into()));
    }

    #[test]
    fn marking_in_the_pager_goes_on_from_its_message() {
        let mut app = app(settings());
        show_m0(&mut app);
        // A background listing moves the index's selection meanwhile.
        app.selected = 2;
        let (_, request) = sent(&act(&mut app, Action::DeleteMessage));
        assert_eq!(
            request,
            Request::FetchMessage("m1".into()),
            "the one after m0"
        );
    }
}
