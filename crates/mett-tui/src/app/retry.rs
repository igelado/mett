//! Requests that were throttled or failed, and retrying them.

use super::*;

/// What failed for want of a sign-in, to do again when the user signs in
/// and retries.
#[derive(Debug)]
pub(super) enum Retry {
    /// Opening the message that failed to open, with the pager search that
    /// was waiting for it.
    Open {
        id: MessageId,
        search: Option<String>,
    },
    /// Changing to the folder that couldn't be found.
    Switch(FolderPath),
    /// Opening again the browser whose folders couldn't be listed.
    Browse(mailbox::Choose),
    /// Starting again the recall of a postponed message.
    Recall,
    /// Fetching again an attachment, and what for.
    Attach {
        message: MessageId,
        attachment: mett_core::Attachment,
        to: attachments::Use,
    },
    /// Starting again a reply or forward whose message couldn't be fetched.
    ComposeFetch { id: MessageId, kind: Kind },
    /// Sending again a move, copy or purge.
    Transfer(Transfer, Request),
}

/// What a [`Retry`] is, apart from what it carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RetryKind {
    Open,
    Switch,
    Browse,
    Recall,
    Attach,
    ComposeFetch,
    Transfer,
}

impl Retry {
    fn kind(&self) -> RetryKind {
        match self {
            Self::Open { .. } => RetryKind::Open,
            Self::Switch(_) => RetryKind::Switch,
            Self::Browse(_) => RetryKind::Browse,
            Self::Recall => RetryKind::Recall,
            Self::Attach { .. } => RetryKind::Attach,
            Self::ComposeFetch { .. } => RetryKind::ComposeFetch,
            Self::Transfer(..) => RetryKind::Transfer,
        }
    }
}

/// Everything waiting for a retry. Each kind but transfers keeps only the
/// last one pushed, which takes the place of the one before. [`App::retry`]
/// takes them in an order of its own, not the order they were pushed in.
#[derive(Debug, Default)]
pub(super) struct Retries(Vec<Retry>);

impl Retries {
    /// Keeps `retry` for the next retry, in place of an earlier one of its
    /// kind (unless it's a transfer, of which there may be many).
    pub(super) fn push(&mut self, retry: Retry) {
        let kind = retry.kind();
        if kind != RetryKind::Transfer {
            self.forget(kind);
        }
        self.0.push(retry);
    }

    /// Takes the retry of `kind`, if there is one.
    pub(super) fn take(&mut self, kind: RetryKind) -> Option<Retry> {
        let at = self.0.iter().position(|r| r.kind() == kind)?;
        Some(self.0.remove(at))
    }

    /// Drops what is kept of `kind`: it no longer holds.
    pub(super) fn forget(&mut self, kind: RetryKind) {
        self.0.retain(|r| r.kind() != kind);
    }

    /// Drops what the user gave up on by dismissing the sign-in window: a
    /// reply or forward, and a recall. (The rest is kept: it is work already
    /// asked for, which a later retry still does.)
    pub(super) fn dismiss(&mut self) {
        self.forget(RetryKind::ComposeFetch);
        self.forget(RetryKind::Recall);
    }

    /// Takes every transfer.
    pub(super) fn take_transfers(&mut self) -> Vec<(Transfer, Request)> {
        let (transfers, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.0)
            .into_iter()
            .partition(|r| r.kind() == RetryKind::Transfer);
        self.0 = rest;
        transfers
            .into_iter()
            .filter_map(|r| match r {
                Retry::Transfer(transfer, request) => Some((transfer, request)),
                _ => None,
            })
            .collect()
    }

    /// The transfers, to change in place.
    pub(super) fn transfers_mut(&mut self) -> impl Iterator<Item = (&mut Transfer, &mut Request)> {
        self.0.iter_mut().filter_map(|r| match r {
            Retry::Transfer(transfer, request) => Some((transfer, request)),
            _ => None,
        })
    }

    /// The transfers.
    #[cfg(test)]
    pub(super) fn transfers(&self) -> impl Iterator<Item = (&Transfer, &Request)> {
        self.0.iter().filter_map(|r| match r {
            Retry::Transfer(transfer, request) => Some((transfer, request)),
            _ => None,
        })
    }

    /// Keeps the transfers `keep` says to, and everything else.
    pub(super) fn retain_transfers(&mut self, mut keep: impl FnMut(Transfer, &Request) -> bool) {
        self.0.retain(|r| match r {
            Retry::Transfer(transfer, request) => keep(*transfer, request),
            _ => true,
        });
    }
}

impl App {
    /// The server throttled request `id`, which is tried again after
    /// `wait`, for the `attempt`th time.
    pub(super) fn on_throttled(
        &mut self,
        id: RequestId,
        wait: Duration,
        attempt: u32,
    ) -> Vec<Effect> {
        // Whole seconds, so that the countdown ends when the wait does.
        let wait = Duration::from_secs(wait.as_secs() + u64::from(wait.subsec_nanos() > 0));
        if self.given_up.contains(&id) {
            return Vec::new();
        }
        let counting = !self.throttled.is_empty();
        // Calls of one request may wait at once: the longest wait counts.
        let entry = self.throttled.entry(id).or_insert((wait, attempt));
        *entry = (entry.0.max(wait), entry.1.max(attempt));
        if counting {
            return Vec::new();
        }
        self.throttle_timer += 1;
        vec![Effect::Later(SECOND, Timer::Throttled(self.throttle_timer))]
    }

    /// A second of the wait for the server went by: the countdown goes on
    /// while a request still waits.
    pub(super) fn count_down_throttled(&mut self, timer: u64) -> Vec<Effect> {
        if timer != self.throttle_timer {
            return Vec::new();
        }
        for (left, _) in self.throttled.values_mut() {
            *left = left.saturating_sub(SECOND);
        }
        self.throttled.retain(|_, (left, _)| !left.is_zero());
        if self.throttled.is_empty() {
            return Vec::new();
        }
        vec![Effect::Later(SECOND, Timer::Throttled(timer))]
    }

    /// What the message line says while requests wait for a throttling
    /// server: the longest wait, and the highest retry.
    #[cfg(test)]
    pub fn throttled_notice(&self) -> Option<String> {
        self.throttled_notices().map(|[long, _]| long)
    }

    /// [`App::throttled_notice`], and a shorter one for a narrow line.
    pub fn throttled_notices(&self) -> Option<[String; 2]> {
        let left = self.throttled.values().map(|(left, _)| *left).max()?;
        let attempt = self.throttled.values().map(|(_, attempt)| *attempt).max()?;
        let seconds = left.as_secs();
        Some([
            format!(
                "Throttled by the server: retry {attempt}/{} in {seconds}s",
                crate::worker::THROTTLED_RETRIES,
            ),
            format!("Throttled: {seconds}s"),
        ])
    }

    /// Retries once the last of the requests that were pending when the
    /// user asked to retry has reported back.
    pub(super) fn retry_if_requested(&mut self, id: RequestId) -> Vec<Effect> {
        if self.retry_after.remove(&id) && self.retry_after.is_empty() {
            self.retry()
        } else {
            Vec::new()
        }
    }

    /// Retries what failed for want of a sign-in: opening the folder,
    /// listing the rest of it, opening the selected message and saving
    /// changes.
    pub(super) fn retry(&mut self) -> Vec<Effect> {
        // Signed in again: checking for new mail can go on.
        self.poll_paused = false;
        self.watch_paused = false;
        self.poll_epoch += 1;
        if self.quit {
            let mut effects = self.retry_saves();
            effects.extend(self.retry_discards());
            effects.extend(self.retry_transfers());
            return effects;
        }
        // Everything that failed is retried, not just the first thing.
        // Saves and moves don't depend on the open folder.
        let mut effects = self.retry_delivery();
        effects.extend(self.retry_discards());
        effects.extend(self.retry_compose_fetch());
        effects.extend(self.retry_recall());
        effects.extend(self.retry_saves());
        effects.extend(self.retry_transfers());
        effects.extend(self.retry_switch());
        effects.extend(self.retry_attachments());
        // The folders are listed whether or not one is open.
        effects.extend(self.retry_browser());
        // Watching goes on, and the checks it asked for.
        effects.extend(self.watch());
        effects.extend(self.recheck());
        if self.folder.is_none() {
            if self.opening.is_none() {
                effects.extend(self.open_folder());
            }
            return effects;
        }
        effects.extend(self.retry_listing());
        // Open the message that failed to open, if it is still the one
        // selected. (Opening another message forgets the failure.) The
        // screen, not the menu: another sign-in window may be over it.
        let (failed, search) = match self.retries.take(RetryKind::Open) {
            Some(Retry::Open { id, search }) => (Some(id), search),
            _ => (None, None),
        };
        let selected = self.messages.get(self.selected).map(|s| &s.id);
        if failed.is_some() && failed.as_ref() == selected && self.screen() == &Screen::Index {
            effects.extend(self.open_selected());
            if let Some(Screen::Pager(pager)) = self.screens.last_mut() {
                pager.pending_search = search;
            }
        }
        effects
    }

    /// Makes again the changes that failed to save, which the index undid.
    /// (Their failures are cleared once that succeeds.)
    pub(super) fn retry_saves(&mut self) -> Vec<Effect> {
        let mut unsaved: Vec<(MessageId, FlagsUpdate)> = self
            .save_failures
            .iter()
            .map(|(id, (_, update))| (id.clone(), *update))
            .collect();
        unsaved.sort_by(|a, b| a.0.cmp(&b.0));
        // What the index shows of each, and whether a change of its flags
        // is on its way, in one pass each. (Retrying one message changes
        // only its own.)
        let mut shown = HashMap::new();
        for summary in self.folder_messages() {
            if self.save_failures.contains_key(&summary.id) {
                shown.entry(summary.id.clone()).or_insert(summary.flags);
            }
        }
        let pending: HashSet<MessageId> = self
            .writing
            .iter()
            .map(|(_, write)| write)
            .chain(&self.writes)
            .filter_map(|write| match write {
                Write::Flags(id, _) if self.save_failures.contains_key(id) => Some(id.clone()),
                _ => None,
            })
            .collect();
        unsaved
            .into_iter()
            .flat_map(|(message, update)| {
                // Its folder may not be open any more: save it all the
                // same, once (a retry may come again before it is done).
                if let Some(&before) = shown.get(&message) {
                    // The mailbox may have it after all (the response was
                    // lost): a fresh listing shows it, and that settles it.
                    let mut flags = before;
                    update.apply(&mut flags);
                    if flags == before && !pending.contains(&message) {
                        self.save_failures.remove(&message);
                        return Vec::new();
                    }
                    self.update_flags(message, update)
                } else if pending.contains(&message) {
                    Vec::new()
                } else {
                    self.write(Write::Flags(message, update))
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod retries_tests {
    use super::*;

    fn named(name: &str) -> FolderPath {
        FolderPath::Named(vec![name.to_owned()])
    }

    #[test]
    fn the_last_retry_of_a_kind_takes_the_place_of_the_one_before() {
        let mut retries = Retries::default();
        retries.push(Retry::Switch(named("a")));
        retries.push(Retry::Recall);
        retries.push(Retry::Switch(named("b")));
        assert!(matches!(
            retries.take(RetryKind::Switch),
            Some(Retry::Switch(path)) if path == named("b")
        ));
        assert!(retries.take(RetryKind::Switch).is_none(), "taken once");
        assert!(retries.take(RetryKind::Recall).is_some());
    }

    #[test]
    fn dismissing_gives_up_only_what_the_user_didnt_ask_to_retry() {
        let mut retries = Retries::default();
        retries.push(Retry::Recall);
        retries.push(Retry::ComposeFetch {
            id: MessageId::new("m"),
            kind: Kind::Reply,
        });
        retries.push(Retry::Switch(named("a")));
        retries.dismiss();
        assert!(retries.take(RetryKind::Recall).is_none());
        assert!(retries.take(RetryKind::ComposeFetch).is_none());
        assert!(retries.take(RetryKind::Switch).is_some());
    }
}
