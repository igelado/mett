//! Requests that were throttled or failed, and retrying them.

use super::*;

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
        let failed = self.unfetched.take();
        let search = self.unfetched_search.take();
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
