//! Flag bookkeeping: changing flags in the index at once, and tracking
//! which changes the server has saved.

use std::collections::HashMap;

use super::*;

impl App {
    /// Marks a message read, updating the index straight away.
    pub(super) fn mark_read(&mut self, message_id: MessageId) -> Vec<Effect> {
        let update = FlagsUpdate::read(true);
        self.update_flags(message_id, update)
    }

    /// Changes a message's flags, updating the index straight away. Nothing
    /// is sent if they already are as asked.
    pub(super) fn update_flags(
        &mut self,
        message_id: MessageId,
        update: FlagsUpdate,
    ) -> Vec<Effect> {
        // A change on its way for a message mett had no flags for (sent
        // while its folder was closed) is what the user wants too: the
        // listing shown may predate it.
        let in_flight = match &self.writing {
            Some((_, Write::Flags(id, sent)))
                if *id == message_id && !self.flag_sync.mailbox.contains_key(&message_id) =>
            {
                Some(*sent)
            }
            _ => None,
        };
        // The message may be hidden by the limit, when retrying.
        let Some(summary) = self.summary_mut(&message_id) else {
            return Vec::new();
        };
        let before = summary.flags;
        let mut flags = before;
        if let Some(sent) = in_flight {
            sent.apply(&mut flags);
        }
        update.apply(&mut flags);
        if flags == before {
            return Vec::new();
        }
        summary.flags = flags;
        // What the mailbox has, as far as mett knows, while changes are
        // on their way.
        self.flag_sync
            .mailbox
            .entry(message_id.clone())
            .or_insert(before);
        self.flag_sync.wanted.insert(message_id.clone(), flags);
        // Changing back what failed to change gives up on it.
        self.flag_sync.changed_back(&message_id, update);
        // A change still waiting its turn takes this one along.
        let queued = self.writes.iter_mut().find_map(|write| match write {
            Write::Flags(id, queued) if *id == message_id => Some(queued),
            _ => None,
        });
        if let Some(queued) = queued {
            *queued = queued.merged(update);
            return Vec::new();
        }
        self.write(Write::Flags(message_id, update))
    }

    /// What to send of a flag change, now that its turn has come: what the
    /// user wants and the mailbox doesn't have (which may be nothing, or
    /// include an earlier change that failed), or `update` for a message
    /// mett knows nothing more about.
    pub(super) fn flags_to_send(&self, message: &MessageId, update: FlagsUpdate) -> FlagsUpdate {
        match (
            self.flag_sync.wanted.get(message),
            self.flag_sync.mailbox.get(message),
        ) {
            (Some(wanted), Some(mailbox)) => FlagsUpdate {
                read: (wanted.read != mailbox.read).then_some(wanted.read),
                flagged: (wanted.flagged != mailbox.flagged).then_some(wanted.flagged),
            },
            _ => update,
        }
    }

    /// Handles the outcome of a flag change to a message whose other flags
    /// mett doesn't know: the index, if it shows the message now, takes
    /// just the fields that were saved.
    pub(super) fn on_unknown_flags_updated(
        &mut self,
        message: MessageId,
        update: FlagsUpdate,
        result: mett_core::Result<()>,
    ) -> Vec<Effect> {
        match result {
            Ok(()) => {
                if let Some(summary) = self.summary_mut(&message) {
                    update.apply(&mut summary.flags);
                }
                // Pages of a listing on their way may predate it.
                if self.fetching_index() {
                    let saved = self
                        .flag_sync
                        .saved
                        .get(&message)
                        .copied()
                        .unwrap_or_default();
                    self.flag_sync
                        .saved
                        .insert(message.clone(), saved.merged(update));
                }
                self.flag_sync.forget_saved(&message, update);
            }
            Err(e) => {
                self.flag_sync.fail(&message, e.to_string(), update);
                self.report(e);
            }
        }
        Vec::new()
    }

    /// Handles the outcome of a flag change.
    pub(super) fn on_flags_updated(
        &mut self,
        message: MessageId,
        update: FlagsUpdate,
        result: mett_core::Result<()>,
    ) -> Vec<Effect> {
        if result.is_ok() {
            self.changed_here(&message);
        }
        // Sent for a message mett has no flags for (its folder wasn't
        // open): only the fields it changed are known.
        let Some(mut mailbox) = self.flag_sync.mailbox.get(&message).copied() else {
            return self.on_unknown_flags_updated(message, update, result);
        };
        let saved = result.is_ok();
        match result {
            Ok(()) => {
                update.apply(&mut mailbox);
                // Whatever failed before of what was saved now is done.
                self.flag_sync.forget_saved(&message, update);
            }
            Err(e) => {
                // What the user has changed back meanwhile isn't wanted any
                // more; the rest is kept with what failed before, to try
                // again together.
                let update = match self.flag_sync.wanted.get(&message) {
                    Some(wanted) => FlagsUpdate {
                        read: update.read.filter(|read| *read == wanted.read),
                        flagged: update.flagged.filter(|flagged| *flagged == wanted.flagged),
                    },
                    None => update,
                };
                self.flag_sync.fail(&message, e.to_string(), update);
                self.report(e);
            }
        }
        let queued = self
            .writes
            .iter()
            .any(|write| matches!(write, Write::Flags(id, _) if *id == message));
        if queued {
            // The next change sends whatever still differs, which
            // includes what just failed. Pages of the listing on its way
            // may predate what was saved.
            if saved && self.fetching_index() {
                self.flag_sync
                    .saved
                    .insert(message.clone(), FlagsUpdate::whole(mailbox));
            }
            self.flag_sync.mailbox.insert(message, mailbox);
        } else {
            self.flags_settled(&message, mailbox, saved);
        }
        Vec::new()
    }

    /// Forgets the flag bookkeeping of a message whose changes are done,
    /// and shows what the mailbox has: after a failure, so the index is
    /// right and doing it again tries again; after a save, in case a
    /// listing taken before it showed the old flags.
    pub(super) fn flags_settled(
        &mut self,
        message: &MessageId,
        mailbox: mett_core::Flags,
        saved: bool,
    ) {
        self.flag_sync.mailbox.remove(message);
        self.flag_sync.wanted.remove(message);
        if let Some(summary) = self.summary_mut(message) {
            summary.flags = mailbox;
        }
        // Pages of the listing on its way may predate the save (or an
        // earlier one, which a failure since doesn't undo).
        if self.fetching_index() && (saved || self.flag_sync.saved.contains_key(message)) {
            self.flag_sync
                .saved
                .insert(message.clone(), FlagsUpdate::whole(mailbox));
        }
    }

    /// Shows freshly listed messages with the flags the user asked for,
    /// while those changes are on their way, or that were saved while the
    /// folder was listed: the listing may predate them.
    pub(super) fn overlay_wanted_flags(&self, messages: &mut [MessageSummary]) {
        for message in messages {
            if let Some(flags) = self.flag_sync.wanted.get(&message.id) {
                message.flags = *flags;
            } else if let Some(saved) = self.flag_sync.saved.get(&message.id) {
                saved.apply(&mut message.flags);
            }
        }
    }
}

/// What the app does with [`FlagsUpdate`]s.
pub(super) trait FlagsUpdateExt: Sized {
    /// All of `flags`, as an update.
    fn whole(flags: mett_core::Flags) -> Self;

    /// `self` with what `later` changes too.
    fn merged(self, later: Self) -> Self;

    /// `self` without the fields that `saved` changes.
    fn without(self, saved: Self) -> Self;

    /// `self` without the fields that `later` changes to something else:
    /// the user changed them back.
    fn undone_by(self, later: Self) -> Self;

    /// Whether it changes nothing.
    fn is_empty(&self) -> bool;
}

impl FlagsUpdateExt for FlagsUpdate {
    fn whole(flags: mett_core::Flags) -> Self {
        Self {
            read: Some(flags.read),
            flagged: Some(flags.flagged),
        }
    }

    fn merged(self, later: Self) -> Self {
        Self {
            read: later.read.or(self.read),
            flagged: later.flagged.or(self.flagged),
        }
    }

    fn without(self, saved: Self) -> Self {
        Self {
            read: self.read.filter(|_| saved.read.is_none()),
            flagged: self.flagged.filter(|_| saved.flagged.is_none()),
        }
    }

    fn undone_by(self, later: Self) -> Self {
        Self {
            read: self
                .read
                .filter(|read| later.read.is_none_or(|new| new == *read)),
            flagged: self
                .flagged
                .filter(|flagged| later.flagged.is_none_or(|new| new == *flagged)),
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// How the flag changes the user made stand with the mailbox.
#[derive(Debug, Default)]
pub(super) struct FlagSync {
    /// The mailbox's flags, as far as mett knows, for messages whose flag
    /// changes are on their way, so a failure can be undone in the index.
    pub(super) mailbox: HashMap<MessageId, mett_core::Flags>,
    /// The flags the user last asked for, for messages whose updates are
    /// on their way.
    pub(super) wanted: HashMap<MessageId, mett_core::Flags>,
    /// Flags saved while the folder was being listed, as far as known:
    /// pages asked for before may still have the old ones. Forgotten once
    /// the listing is done.
    pub(super) saved: HashMap<MessageId, FlagsUpdate>,
    /// Changes that failed to save, by message, kept apart from the status
    /// line so that later messages can't hide them. A later successful save
    /// of the same message clears its entry.
    pub(super) failures: HashMap<MessageId, (String, FlagsUpdate)>,
}

impl FlagSync {
    /// Forgets what is only known for the listing that is done, or that
    /// another folder replaces. Changes on their way and failed ones stay:
    /// they don't depend on the folder.
    pub(super) fn forget_listing(&mut self) {
        self.saved.clear();
    }

    /// Takes `update` out of what failed for `message`: it is saved now,
    /// or the user changed it back. A failure with nothing left is gone.
    fn clear_failed(
        &mut self,
        message: &MessageId,
        clear: impl FnOnce(FlagsUpdate) -> FlagsUpdate,
    ) {
        if let Some((_, failed)) = self.failures.get_mut(message) {
            *failed = clear(*failed);
            if failed.is_empty() {
                self.failures.remove(message);
            }
        }
    }

    /// Changing back what failed to change gives up on it.
    pub(super) fn changed_back(&mut self, message: &MessageId, update: FlagsUpdate) {
        self.clear_failed(message, |failed| failed.undone_by(update));
    }

    /// Forgets that saving what `update` has saved now failed before for
    /// `message`.
    pub(super) fn forget_saved(&mut self, message: &MessageId, update: FlagsUpdate) {
        self.clear_failed(message, |failed| failed.without(update));
    }

    /// Notes that saving `update` failed with `error`, with what failed
    /// before for `message` (if `update` isn't empty).
    pub(super) fn fail(&mut self, message: &MessageId, error: String, update: FlagsUpdate) {
        let failed = self
            .failures
            .get(message)
            .map_or(update, |(_, before)| before.merged(update));
        if !failed.is_empty() {
            self.failures.insert(message.clone(), (error, failed));
        }
    }
}
