//! The requests a server throttled, and the countdown to trying them again.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use super::SECOND;
use crate::worker::RequestId;

/// How many requests given up on are remembered (see [`Throttle::given_up`]).
pub(super) const GIVEN_UP_KEPT: usize = 16;

/// What the app knows of the requests the server throttled. It doesn't
/// belong to a folder: the requests wait whichever folder is open.
#[derive(Debug, Default)]
pub(super) struct Throttle {
    /// The requests the server throttled, by how long until each is tried
    /// again (counted down every second) and which retry that is. A
    /// request's answer, or giving it up, takes it out.
    pub(super) waiting: HashMap<RequestId, (Duration, u32)>,
    /// The countdown's timer, which goes while a request waits.
    pub(super) timer: u64,
    /// The last requests given up on, whose waits that the worker told of
    /// before it stopped them are ignored. Those come right after giving
    /// up, so only the last few are kept.
    pub(super) given_up: VecDeque<RequestId>,
}

impl Throttle {
    /// Notes that request `id` waits `wait` (in whole seconds) for the
    /// `attempt`th time; returns the timer to set, if the countdown isn't
    /// going yet.
    pub(super) fn throttled(&mut self, id: RequestId, wait: Duration, attempt: u32) -> Option<u64> {
        if self.given_up.contains(&id) {
            return None;
        }
        let counting = !self.waiting.is_empty();
        // Calls of one request may wait at once: the longest wait counts.
        let entry = self.waiting.entry(id).or_insert((wait, attempt));
        *entry = (entry.0.max(wait), entry.1.max(attempt));
        if counting {
            return None;
        }
        self.timer += 1;
        Some(self.timer)
    }

    /// A second went by for the countdown of `timer`; returns whether it
    /// goes on (a request still waits).
    pub(super) fn count_down(&mut self, timer: u64) -> bool {
        if timer != self.timer {
            return false;
        }
        for (left, _) in self.waiting.values_mut() {
            *left = left.saturating_sub(SECOND);
        }
        self.waiting.retain(|_, (left, _)| !left.is_zero());
        !self.waiting.is_empty()
    }

    /// Request `id` was answered: it doesn't wait.
    pub(super) fn answered(&mut self, id: RequestId) {
        self.waiting.remove(&id);
    }

    /// Request `id` is given up on: it doesn't wait, and a wait the worker
    /// told of before it stopped it is ignored.
    pub(super) fn give_up(&mut self, id: RequestId) {
        self.waiting.remove(&id);
        if self.given_up.len() == GIVEN_UP_KEPT {
            self.given_up.pop_front();
        }
        self.given_up.push_back(id);
    }

    /// The longest wait left, and the highest retry.
    pub(super) fn longest(&self) -> Option<(Duration, u32)> {
        let left = self.waiting.values().map(|(left, _)| *left).max()?;
        let attempt = self.waiting.values().map(|(_, attempt)| *attempt).max()?;
        Some((left, attempt))
    }
}
