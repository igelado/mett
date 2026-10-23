//! The state of the checks for new mail and of watching the mailbox for
//! changes (see `newmail.rs`).

use std::collections::HashSet;

use mett_core::{FolderId, MessageId};

use super::newmail::Polling;
use crate::worker::RequestId;

/// What the open folder's check for new mail has under way. It belongs to
/// the folder: another folder's [`Poll::reset`] forgets it.
#[derive(Debug, Default)]
pub(super) struct OpenPoll {
    /// The open folder's check for new mail, on its way.
    pub(super) polling: Option<Polling>,
    /// The messages in the index before a check had it listed again, to
    /// tell what arrived.
    pub(super) known_before_refresh: Option<HashSet<MessageId>>,
    /// The request asking whether messages missing from a check's page,
    /// as old as its last, are still in the folder (on the next page).
    pub(super) tie_check: Option<(RequestId, FolderId)>,
}

/// The checks for new mail and the watching of the mailbox, which go on
/// across folders but for what is in `open`.
#[derive(Debug)]
pub(super) struct Poll {
    /// What belongs to the open folder.
    pub(super) open: OpenPoll,
    /// Whether the timer for the next check for new mail is set.
    pub(super) armed: bool,
    /// Checks for new mail wait for the user to sign in again.
    pub(super) paused: bool,
    /// Since when the next check asks for changes.
    pub(super) since: Option<chrono::DateTime<chrono::Utc>>,
    /// Which timer for checks counts.
    pub(super) timer: u64,
    /// The request reading the mailboxes' counts.
    pub(super) checking: Option<RequestId>,
    /// How many times the user retried after a failed sign-in: background
    /// checks made before the last retry don't hold off checking.
    pub(super) epoch: u64,
    /// When the folder was last listed from the start.
    pub(super) listed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The request watching the folders for changes, while one is, and
    /// which folders it watches.
    pub(super) watching: Option<RequestId>,
    pub(super) watched: Vec<FolderId>,
    /// The request ending what the mailbox keeps for watching, while it
    /// is under way.
    pub(super) unwatching: Option<RequestId>,
    /// Ending it failed: it is tried again.
    pub(super) unwatch_failed: bool,
    /// The `epoch` the watch started in.
    pub(super) watch_epoch: u64,
    /// Watching is held off until the user signs in again: its sign-in
    /// failed. (Only watching: the mailbox may just not let mett watch.)
    pub(super) watch_paused: bool,
    /// What a watch said changed and is still to be checked (a check under
    /// way when it said may predate the change): the open folder, and the
    /// folders of `mailboxes` (the open one among them, for its counts).
    pub(super) recheck_folder: bool,
    pub(super) recheck_mailboxes: Vec<FolderId>,
    /// Whether the mailbox can tell when folders change (until it says it
    /// can't).
    pub(super) can_watch: bool,
    /// A retry of watching is set, after it failed.
    pub(super) watch_retry: bool,
}

impl Default for Poll {
    fn default() -> Self {
        Self {
            open: OpenPoll::default(),
            armed: false,
            paused: false,
            since: None,
            timer: 0,
            checking: None,
            epoch: 0,
            listed_at: None,
            watching: None,
            watched: Vec::new(),
            unwatching: None,
            unwatch_failed: false,
            watch_epoch: 0,
            watch_paused: false,
            recheck_folder: false,
            recheck_mailboxes: Vec::new(),
            can_watch: true,
            watch_retry: false,
        }
    }
}

impl Poll {
    /// Forgets what belongs to the open folder, as another folder opens.
    /// The timers, the watch and the state of signing in carry on.
    pub(super) fn reset(&mut self) {
        self.open = OpenPoll::default();
    }
}
