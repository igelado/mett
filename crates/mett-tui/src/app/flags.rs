//! Flag bookkeeping: changing flags in the index at once, and tracking
//! which changes the server has saved.

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
                if *id == message_id && !self.mailbox_flags.contains_key(&message_id) =>
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
        self.mailbox_flags
            .entry(message_id.clone())
            .or_insert(before);
        self.wanted_flags.insert(message_id.clone(), flags);
        // Changing back what failed to change gives up on it.
        if let Some((_, failed)) = self.save_failures.get_mut(&message_id) {
            if failed
                .read
                .is_some_and(|read| update.read.is_some_and(|new| new != read))
            {
                failed.read = None;
            }
            if failed
                .flagged
                .is_some_and(|flagged| update.flagged.is_some_and(|new| new != flagged))
            {
                failed.flagged = None;
            }
            if *failed == FlagsUpdate::default() {
                self.save_failures.remove(&message_id);
            }
        }
        // A change still waiting its turn takes this one along.
        let queued = self.writes.iter_mut().find_map(|write| match write {
            Write::Flags(id, queued) if *id == message_id => Some(queued),
            _ => None,
        });
        if let Some(queued) = queued {
            *queued = merged(*queued, update);
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
            self.wanted_flags.get(message),
            self.mailbox_flags.get(message),
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
                    let saved = self.saved_flags.get(&message).copied().unwrap_or_default();
                    self.saved_flags
                        .insert(message.clone(), merged(saved, update));
                }
                self.forget_saved(&message, update);
            }
            Err(e) => {
                let failed = self
                    .save_failures
                    .get(&message)
                    .map_or(update, |(_, before)| merged(*before, update));
                self.save_failures.insert(message, (e.to_string(), failed));
                self.report(e);
            }
        }
        Vec::new()
    }

    /// Forgets that saving what `update` has saved now failed before for
    /// `message`.
    pub(super) fn forget_saved(&mut self, message: &MessageId, update: FlagsUpdate) {
        if let Some((_, failed)) = self.save_failures.get_mut(message) {
            if update.read.is_some() {
                failed.read = None;
            }
            if update.flagged.is_some() {
                failed.flagged = None;
            }
            if *failed == FlagsUpdate::default() {
                self.save_failures.remove(message);
            }
        }
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
        let Some(mut mailbox) = self.mailbox_flags.get(&message).copied() else {
            return self.on_unknown_flags_updated(message, update, result);
        };
        let saved = result.is_ok();
        match result {
            Ok(()) => {
                update.apply(&mut mailbox);
                // Whatever failed before of what was saved now is done.
                self.forget_saved(&message, update);
            }
            Err(e) => {
                // What the user has changed back meanwhile isn't wanted any
                // more; the rest is kept with what failed before, to try
                // again together.
                let update = match self.wanted_flags.get(&message) {
                    Some(wanted) => FlagsUpdate {
                        read: update.read.filter(|read| *read == wanted.read),
                        flagged: update.flagged.filter(|flagged| *flagged == wanted.flagged),
                    },
                    None => update,
                };
                let failed = self
                    .save_failures
                    .get(&message)
                    .map_or(update, |(_, before)| merged(*before, update));
                if failed != FlagsUpdate::default() {
                    self.save_failures
                        .insert(message.clone(), (e.to_string(), failed));
                }
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
                self.saved_flags.insert(message.clone(), whole(mailbox));
            }
            self.mailbox_flags.insert(message, mailbox);
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
        self.mailbox_flags.remove(message);
        self.wanted_flags.remove(message);
        if let Some(summary) = self.summary_mut(message) {
            summary.flags = mailbox;
        }
        // Pages of the listing on its way may predate the save (or an
        // earlier one, which a failure since doesn't undo).
        if self.fetching_index() && (saved || self.saved_flags.contains_key(message)) {
            self.saved_flags.insert(message.clone(), whole(mailbox));
        }
    }

    /// Shows freshly listed messages with the flags the user asked for,
    /// while those changes are on their way, or that were saved while the
    /// folder was listed: the listing may predate them.
    pub(super) fn overlay_wanted_flags(&self, messages: &mut [MessageSummary]) {
        for message in messages {
            if let Some(flags) = self.wanted_flags.get(&message.id) {
                message.flags = *flags;
            } else if let Some(saved) = self.saved_flags.get(&message.id) {
                saved.apply(&mut message.flags);
            }
        }
    }
}

/// All of `flags`, as an update.
fn whole(flags: mett_core::Flags) -> FlagsUpdate {
    FlagsUpdate {
        read: Some(flags.read),
        flagged: Some(flags.flagged),
    }
}

/// `earlier` with what `later` changes too.
fn merged(earlier: FlagsUpdate, later: FlagsUpdate) -> FlagsUpdate {
    FlagsUpdate {
        read: later.read.or(earlier.read),
        flagged: later.flagged.or(earlier.flagged),
    }
}
