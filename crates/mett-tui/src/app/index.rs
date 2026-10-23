//! The state of the open folder's index: its messages, how they are
//! threaded and limited, and the listing that fills it.

use std::collections::{HashMap, HashSet};

use mett_core::{MessageId, MessageSummary, Page};

use super::{Limit, Target, Threaded};
use crate::pattern::Pattern;
use crate::worker::RequestId;

/// What the app knows of the open folder's index. It belongs to the folder:
/// [`IndexState::reset`] forgets all of it when another folder opens, so a
/// new field is reset with the rest.
#[derive(Debug, Default)]
pub(crate) struct IndexState {
    /// The index, newest first.
    pub(crate) messages: Vec<MessageSummary>,
    /// The selected index entry.
    pub(crate) selected: usize,
    /// The pattern the index is limited to (`l`), if any.
    pub(crate) limit: Option<Limit>,
    /// The messages that the limit hides, newest first.
    pub(super) hidden: Vec<MessageSummary>,
    /// The messages folded into collapsed threads.
    pub(super) folded: Vec<MessageSummary>,
    /// Where each message is in its thread, when the index shows threads.
    pub(super) threads: HashMap<MessageId, Threaded>,
    /// How many messages the threads were last built from: while a large
    /// folder is listed, they are built again only as it doubles.
    pub(super) threaded_count: usize,
    /// Whether building the threads again would give what they were last
    /// built as (see [`crate::sort::Arranged::settled`]).
    pub(super) settled: bool,
    /// The threads (by key) the user collapsed (`true`) or opened
    /// (`false`) themselves. The others are open, or with `collapse_all`
    /// collapsed unless they hold unread mail (as mutt's
    /// `collapse_unread=no`).
    pub(super) chosen: HashMap<std::sync::Arc<str>, bool>,
    /// `Esc V` collapsed every thread, including those listed since.
    pub(super) collapse_all: bool,
    /// Whether the limit showed each message it was applied to, kept
    /// across listings so that they show the same messages.
    pub(super) limited: HashMap<MessageId, bool>,
    /// Where each message was in the folder's listing, to restore the
    /// order when the limit is lifted.
    pub(super) positions: HashMap<MessageId, usize>,
    /// A message was listed twice since the index was last empty: it may
    /// show it twice.
    pub(super) listed_twice: bool,
    /// A search that found nothing yet, waiting for more of the folder,
    /// and its pattern.
    pub(super) pending_search: Option<(String, Pattern)>,
    /// Messages marked for deletion, purged by `$` or when leaving.
    pub(super) deleted: HashSet<MessageId>,
    /// Where the user asked to move while more messages were loading; the
    /// selection moves there once they arrive.
    pub(super) pending: Option<Target>,
    /// The request whose messages the index is waiting for, if any.
    pub(super) listing: Option<RequestId>,
    /// The page that `listing` asked for.
    pub(super) listing_page: Option<Page>,
    /// A page that failed to load, so the index is incomplete. Loading it
    /// is retried when the user reaches the end of the index.
    pub(super) unlisted: Option<Page>,
}

impl IndexState {
    /// Forgets everything: another folder is opening.
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Forgets the listed messages, and what is kept of their place, to
    /// list the folder again from the start. The limit, the user's choices
    /// of threads and the messages marked for deletion stay.
    pub(super) fn forget_listing(&mut self) {
        self.messages.clear();
        self.hidden.clear();
        self.folded.clear();
        self.threads.clear();
        self.threaded_count = 0;
        self.positions.clear();
        self.selected = 0;
        self.pending = None;
        self.unlisted = None;
    }
}
