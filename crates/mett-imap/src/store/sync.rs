//! Syncing a folder: what changed since a sync state, which the cache keeps
//! a folder's index up to date with (`MailStore::sync_folder`).
//!
//! IMAP has no "changes since" call, so the state holds what it takes to
//! tell: the folder's `UIDVALIDITY`, the `UID` its next message would have,
//! its highest modification sequence (`CONDSTORE`: the messages whose flags
//! changed since are asked for) and the `UID`s it had (the ones that are
//! gone now are those missing). A sync of many messages is made in batches:
//! the state then also holds the messages not yet taken in.

use futures_util::TryStreamExt;
use mett_core::{Error, FolderChanges, MessageId, SyncState};

use super::{BATCH_SIZE, Fail, Live, Ref, Res, SUMMARY, Selected, fetch, parse, search, select};
use crate::parse::Fetched;
use crate::uidset::UidSet;

/// What a state says (see the module).
#[derive(Clone, Debug, PartialEq, Eq)]
struct State {
    validity: u32,
    /// The `UID` the folder's next message had.
    next: u32,
    /// The folder's highest modification sequence.
    modseq: u64,
    /// The `UID`s the folder had.
    set: UidSet,
    /// Those of them whose summaries haven't been taken in yet.
    pending: UidSet,
    /// Whether this is the first sync, whose messages are as old as they
    /// are (not changed now).
    initial: bool,
}

/// A state's first word: what the rest is.
const VERSION: &str = "imap1";

impl State {
    fn encode(&self) -> SyncState {
        SyncState::new(format!(
            "{VERSION}/{}/{}/{}/{}/{}/{}",
            self.validity,
            self.next,
            self.modseq,
            self.set,
            self.pending,
            u8::from(self.initial)
        ))
    }

    fn decode(state: &SyncState) -> Option<Self> {
        let mut parts = state.as_str().split('/');
        if parts.next()? != VERSION {
            return None;
        }
        let state = Self {
            validity: parts.next()?.parse().ok()?,
            next: parts.next()?.parse().ok()?,
            modseq: parts.next()?.parse().ok()?,
            set: UidSet::parse(parts.next()?)?,
            pending: UidSet::parse(parts.next()?)?,
            initial: parts.next()? == "1",
        };
        parts.next().is_none().then_some(state)
    }
}

/// What changed in `folder` since `state`, a batch at a time.
pub(super) async fn sync_folder(
    live: &mut Live,
    folder: &str,
    state: Option<&SyncState>,
) -> Res<FolderChanges> {
    let selected = select(live, folder).await?;
    let stale = || Fail::Core(Error::StaleSync);
    // Without modification sequences, a change of flags would go unseen: the
    // folder is left to the ordinary listing.
    let Some(modseq) = selected.modseq.filter(|_| live.caps.condstore) else {
        return Err(Error::CannotSync.into());
    };
    let (state, removed) = match state {
        // Everything is a change; what the cache has that isn't is gone
        // once the sync is complete.
        None => {
            let all = UidSet::of(search(live, "ALL").await?);
            let state = State {
                validity: selected.validity,
                next: selected.next,
                modseq,
                set: all.clone(),
                pending: all,
                initial: true,
            };
            (state, Vec::new())
        }
        Some(state) => {
            let state = State::decode(state)
                .filter(|state| state.validity == selected.validity)
                .ok_or_else(stale)?;
            if state.pending.is_empty() {
                advance(live, &selected, modseq, state).await?
            } else {
                (state, Vec::new())
            }
        }
    };
    batch(live, folder, state, removed).await
}

/// `state` moved on to how `selected` stands: what came in, changed and went
/// since.
async fn advance(
    live: &mut Live,
    selected: &Selected,
    modseq: u64,
    old: State,
) -> Res<(State, Vec<u32>)> {
    // Nothing happened, by what the folder says of itself: no new message,
    // no change of flags, none gone.
    if selected.next == old.next
        && modseq == old.modseq
        && selected.exists as usize == old.set.len()
    {
        return Ok((old, Vec::new()));
    }
    let came: Vec<u32> = search(live, &format!("UID {}:*", old.next))
        .await?
        .into_iter()
        .filter(|uid| *uid >= old.next)
        .collect();
    let (set, gone, came) = if selected.exists as usize == old.set.len() + came.len() {
        (old.set.with(came.iter().copied()), Vec::new(), came)
    } else {
        // Some are gone (and perhaps more came): the folder says which are
        // there, and what was there tells which are not.
        let all = UidSet::of(search(live, "ALL").await?);
        let gone = old.set.minus(&all);
        let came = all.minus(&old.set);
        (all, gone, came)
    };
    let mut changed = came;
    if modseq > old.modseq && selected.exists > 0 {
        let flagged: Vec<_> = live
            .session
            .uid_fetch("1:*", format!("(UID) (CHANGEDSINCE {})", old.modseq))
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        changed.extend(flagged.iter().filter_map(|f| f.uid));
    }
    let pending = UidSet::of(changed).without(gone.iter().copied());
    let state = State {
        validity: old.validity,
        next: selected.next,
        modseq,
        set,
        pending,
        initial: false,
    };
    Ok((state, gone))
}

/// The next of the messages `state` has to take in, and where that leaves
/// the sync.
async fn batch(
    live: &mut Live,
    folder: &str,
    mut state: State,
    gone: Vec<u32>,
) -> Res<FolderChanges> {
    let first = state.initial;
    let (uids, rest) = state.pending.split_newest(BATCH_SIZE);
    state.pending = rest;
    let by_arrival = !live.caps.sort;
    let found: Vec<Fetched> = fetch(live, &uids, SUMMARY, false).await?;
    let now = chrono::Utc::now();
    let mut changed = Vec::with_capacity(found.len());
    for fetched in &found {
        let mut summary = parse::summary(folder, state.validity, fetched, by_arrival)?;
        // What came in or changed just now did so by now (the first sync's
        // are as old as they are): that is what the new-mail checks go by.
        if !first {
            summary.modified = Some(now);
        }
        changed.push(summary);
    }
    let removed: Vec<MessageId> = gone
        .into_iter()
        .map(|uid| {
            Ref {
                folder: folder.to_owned(),
                validity: state.validity,
                uid,
            }
            .id()
        })
        .collect();
    Ok(FolderChanges {
        changed,
        removed,
        more: !state.pending.is_empty(),
        state: state.encode(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_is_written_and_read_back() {
        let state = State {
            validity: 7,
            next: 40,
            modseq: 1234,
            set: UidSet::of([1, 2, 3, 9]),
            pending: UidSet::of([9]),
            initial: true,
        };
        assert_eq!(State::decode(&state.encode()), Some(state));
        for bad in [
            "",
            "imap2/1/2/3//",
            "imap1/1/2/3/1:3/0",
            "imap1/x/2/3//",
            "imap1/1/2/3/5,1/",
        ] {
            assert_eq!(State::decode(&SyncState::new(bad)), None, "{bad}");
        }
    }
}
