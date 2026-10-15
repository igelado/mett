//! The background worker: the actor that owns all I/O (`docs/DESIGN.md`
//! §5.4).
//!
//! The UI sends [`Request`]s tagged with a [`RequestId`] and receives a
//! [`Response`] for each, so it can match results and drop stale ones. The
//! worker is generic over the [`MailStore`] and [`MailSender`] ports, so
//! the UI is tested against the in-memory store.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use mett_core::{
    Attachment, AttachmentId, Draft, Error, FlagsUpdate, Folder, FolderId, MailSender, MailStore,
    Message, MessageId, MessagePage, Page, Result,
};
use tokio::sync::mpsc;

use crate::config::mailbox::FolderPath;

/// Identifies a request, so its response can be matched to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

/// Work for the worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Find a folder by its path in the mailbox.
    OpenFolder(FolderPath),
    /// List a page of messages in a folder.
    ListMessages {
        /// The folder.
        folder: FolderId,
        /// Which page.
        page: Page,
    },
    /// Check a folder for new mail: list its newest messages, and those
    /// that changed or arrived since a time.
    PollFolder {
        /// The folder.
        folder: FolderId,
        /// How many of each to list.
        page: Page,
        /// Changes since when.
        since: DateTime<Utc>,
    },
    /// Fetch a full message.
    FetchMessage(MessageId),
    /// Change a message's flags.
    UpdateFlags(MessageId, FlagsUpdate),
    /// List a message's attachments.
    ListAttachments(MessageId),
    /// Fetch an attachment's content.
    FetchAttachment(MessageId, AttachmentId),
    /// List every folder in the mailbox.
    ListFolders,
    /// Move or copy messages to the folder at a path.
    Transfer {
        /// The messages.
        ids: Vec<MessageId>,
        /// Where they go.
        to: FolderPath,
        /// Copy them rather than move them.
        copy: bool,
    },
    /// Purge messages marked for deletion from the folder `from`: move
    /// them to the trash folder, or delete them for good without one, or
    /// in the trash itself (as mutt does).
    Purge {
        /// The messages.
        ids: Vec<MessageId>,
        /// The folder they are in.
        from: FolderId,
        /// The trash folder, if any.
        trash: Option<FolderPath>,
    },
    /// Send a message.
    Send(Draft),
    /// Save a message in the drafts folder.
    SaveDraft(Draft),
    /// Delete what failed attempts to send or save a message the user gave
    /// up on left behind.
    DiscardKept(Draft),
    /// Find the mailbox owner's own addresses.
    OwnAddresses,
    /// Read the counts of the folders at these paths, and their unread
    /// mail that changed since a time (if one is given), to tell which have
    /// new mail: by ID, where the folder was found before, or else by
    /// path.
    CheckMailboxes(Vec<(FolderPath, Option<FolderId>, Option<DateTime<Utc>>)>),
    /// Find which of `ids` are no longer in `folder` (deleted, or moved
    /// away).
    StillThere {
        /// The folder.
        folder: FolderId,
        /// The messages.
        ids: Vec<MessageId>,
    },
}

/// The outcome of a [`Request`].
// Events are handled one at a time, so their size doesn't matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Event {
    /// Outcome of [`Request::OpenFolder`].
    FolderOpened(Result<Folder>),
    /// Outcome of [`Request::ListMessages`].
    MessagesListed(Result<MessagePage>),
    /// Outcome of [`Request::PollFolder`].
    FolderPolled(Result<Polled>),
    /// Outcome of [`Request::FetchMessage`].
    MessageFetched(Result<Message>),
    /// Outcome of [`Request::UpdateFlags`].
    FlagsUpdated(Result<()>),
    /// Outcome of [`Request::ListAttachments`].
    AttachmentsListed(Result<Vec<Attachment>>),
    /// Outcome of [`Request::FetchAttachment`].
    AttachmentFetched(Result<Vec<u8>>),
    /// Outcome of [`Request::ListFolders`].
    FoldersListed(Result<Vec<Folder>>),
    /// Outcome of [`Request::Transfer`]: the folder could not be found, or
    /// the folder and, for each message, its new ID (or the copy's) or why
    /// it failed.
    Transferred(Result<(FolderId, Moved)>),
    /// Outcome of [`Request::Purge`]: the trash folder could not be found,
    /// or for each message, whether it was purged.
    Purged(Result<Vec<(MessageId, Result<()>)>>),
    /// Outcome of [`Request::Send`], and whether a draft is kept for the
    /// next attempt (see [`MailSender::keeps_draft`]).
    Sent(Result<()>, bool),
    /// Outcome of [`Request::SaveDraft`], and whether a draft is kept.
    DraftSaved(Result<MessageId>, bool),
    /// Outcome of [`Request::DiscardKept`].
    KeptDiscarded(Result<()>),
    /// Outcome of [`Request::OwnAddresses`].
    OwnAddresses(Result<Vec<String>>),
    /// Outcome of [`Request::CheckMailboxes`]: each folder, or why it
    /// couldn't be read.
    MailboxesChecked(Vec<(FolderPath, Result<Checked>)>),
    /// Outcome of [`Request::StillThere`]: the messages gone.
    StillThere(Result<Vec<MessageId>>),
}

/// A mailbox, as checked for new mail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    /// The folder, with its counts.
    pub folder: Folder,
    /// Its unread messages that arrived or changed last, and when.
    pub recent: Vec<(MessageId, DateTime<Utc>)>,
}

/// What a check of a folder for new mail found.
#[derive(Debug)]
pub struct Polled {
    /// The newest messages.
    pub newest: MessagePage,
    /// The messages that changed or arrived, most recently first.
    pub changed: MessagePage,
}

/// How each message of a move or copy fared: its new ID (or the copy's),
/// or why it failed.
pub type Moved = Vec<(MessageId, Result<MessageId>)>;

/// An [`Event`] for the request with `id`.
#[derive(Debug)]
pub struct Response {
    /// The request this answers.
    pub id: RequestId,
    /// What happened.
    pub event: Event,
}

/// Serves requests until the request channel closes. Each request runs as
/// its own task, so a slow fetch doesn't hold up the others.
pub async fn run<S: MailStore + MailSender + 'static>(
    store: Arc<S>,
    mut requests: mpsc::UnboundedReceiver<(RequestId, Request)>,
    responses: mpsc::UnboundedSender<Response>,
) {
    while let Some((id, request)) = requests.recv().await {
        let store = Arc::clone(&store);
        let responses = responses.clone();
        tokio::spawn(async move {
            let event = serve(&*store, request).await;
            // The UI may have quit; nothing to do then.
            let _ = responses.send(Response { id, event });
        });
    }
}

async fn serve<S: MailStore + MailSender>(store: &S, request: Request) -> Event {
    match request {
        Request::OpenFolder(path) => Event::FolderOpened(resolve(store, &path).await),
        Request::ListMessages { folder, page } => {
            Event::MessagesListed(store.list_messages(&folder, page).await)
        }
        Request::PollFolder {
            folder,
            page,
            since,
        } => {
            let (newest, changed) = tokio::join!(
                store.list_messages(&folder, page.clone()),
                changes(store, &folder, since, page),
            );
            Event::FolderPolled(newest.and_then(|newest| {
                Ok(Polled {
                    newest,
                    changed: changed?,
                })
            }))
        }
        Request::FetchMessage(id) => Event::MessageFetched(store.fetch_message(&id).await),
        Request::ListAttachments(id) => Event::AttachmentsListed(store.list_attachments(&id).await),
        Request::FetchAttachment(id, attachment) => {
            Event::AttachmentFetched(store.fetch_attachment(&id, &attachment).await)
        }
        Request::UpdateFlags(id, update) => {
            Event::FlagsUpdated(store.update_flags(&id, update).await)
        }
        Request::ListFolders => Event::FoldersListed(store.list_folders().await),
        Request::Transfer { ids, to, copy } => {
            Event::Transferred(transfer(store, &ids, &to, copy).await)
        }
        Request::Purge { ids, from, trash } => {
            Event::Purged(purge(store, &ids, &from, trash.as_ref()).await)
        }
        Request::Send(draft) => {
            let sent = store.send(&draft).await;
            Event::Sent(sent, store.keeps_draft(&draft))
        }
        Request::SaveDraft(draft) => {
            let saved = store.save_draft(&draft).await;
            Event::DraftSaved(saved, store.keeps_draft(&draft))
        }
        Request::DiscardKept(draft) => Event::KeptDiscarded(store.discard_kept(&draft).await),
        Request::OwnAddresses => Event::OwnAddresses(store.own_addresses().await),
        Request::StillThere { folder, ids } => {
            let mut gone = Vec::new();
            let mut failed = None;
            for id in ids {
                match store.fetch_message(&id).await {
                    Err(Error::NotFound { .. }) => gone.push(id),
                    Ok(message) if message.summary.folder != folder => gone.push(id),
                    Ok(_) => {}
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            Event::StillThere(failed.map_or(Ok(gone), Err))
        }
        Request::CheckMailboxes(mailboxes) => {
            Event::MailboxesChecked(check_mailboxes(store, mailboxes).await)
        }
    }
}

/// How many mailboxes are checked at once: a slow one doesn't hold up the
/// others, nor do many at once flood Graph.
const CHECKS_AT_ONCE: usize = 4;

/// Checks `mailboxes` (see [`Request::CheckMailboxes`]), a few at a time.
async fn check_mailboxes<S: MailStore>(
    store: &S,
    mailboxes: Vec<(FolderPath, Option<FolderId>, Option<DateTime<Utc>>)>,
) -> Vec<(FolderPath, Result<Checked>)> {
    // The folders, by the IDs known. A folder deleted (or moved away)
    // since is looked for by its path again; so is one renamed or moved
    // since (another may be at its path now).
    let known = join_bounded(
        mailboxes
            .iter()
            .map(|(path, id, _)| async move {
                let id = id.as_ref()?;
                match store.folder(id).await {
                    Err(Error::NotFound { .. }) => None,
                    Ok(folder) => match at_path(store, path, &folder).await {
                        Ok(true) => Some(Ok(folder)),
                        Ok(false) => None,
                        Err(e) => Some(Err(e)),
                    },
                    Err(e) => Some(Err(e)),
                }
            })
            .collect(),
        CHECKS_AT_ONCE,
    )
    .await;
    // The others, by their paths: the folder tree is listed once for all.
    let mut listed = None;
    let mut found = Vec::new();
    for ((path, _, _), known) in mailboxes.iter().zip(known) {
        found.push(match known {
            Some(found) => found,
            None => resolve_listed(store, path, &mut listed).await,
        });
    }
    join_bounded(
        mailboxes
            .into_iter()
            .zip(found)
            .map(|((path, id, since), found)| async move {
                let checked = match found {
                    Ok(folder) => {
                        // Changes since what was seen of the folder asked
                        // about, not of another now at its path.
                        let since = since.filter(|_| id.as_ref() == Some(&folder.id));
                        store
                            .recent_unread(&folder.id, since)
                            .await
                            .map(|recent| Checked { folder, recent })
                    }
                    Err(e) => Err(e),
                };
                (path, checked)
            })
            .collect(),
        CHECKS_AT_ONCE,
    )
    .await
}

/// Runs `futures`, at most `limit` at a time, and gives their outputs in
/// order.
async fn join_bounded<F: std::future::Future>(futures: Vec<F>, limit: usize) -> Vec<F::Output> {
    let mut futures: Vec<std::pin::Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        // Futures do nothing until polled: polling the first `limit`
        // unfinished ones runs only those.
        let mut running = 0;
        for (future, output) in futures.iter_mut().zip(outputs.iter_mut()) {
            if output.is_some() {
                continue;
            }
            if running == limit {
                break;
            }
            match future.as_mut().poll(cx) {
                std::task::Poll::Ready(done) => *output = Some(done),
                std::task::Poll::Pending => running += 1,
            }
        }
        if running == 0 {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    outputs
        .into_iter()
        .map(|output| output.expect("every future finished"))
        .collect()
}

/// How many pages of changes a check reads before listing the whole
/// folder again is cheaper.
const CHANGE_PAGES: usize = 10;

/// The messages of `folder` changed since `since`: up to
/// [`CHANGE_PAGES`] pages, with a cursor if there are more.
async fn changes<S: MailStore>(
    store: &S,
    folder: &FolderId,
    since: DateTime<Utc>,
    page: Page,
) -> Result<MessagePage> {
    let size = page.size();
    let mut changed = store.changed_messages(folder, since, page).await?;
    for _ in 1..CHANGE_PAGES {
        let Some(cursor) = changed.next.take() else {
            break;
        };
        let more = store
            .changed_messages(folder, since, Page::after(cursor, size))
            .await?;
        changed.messages.extend(more.messages);
        changed.next = more.next;
    }
    Ok(changed)
}

/// Purges `ids` from the folder `from` (see [`Request::Purge`]), one at a
/// time, so that a failure only affects its own message.
async fn purge<S: MailStore>(
    store: &S,
    ids: &[MessageId],
    from: &FolderId,
    trash: Option<&FolderPath>,
) -> Result<Vec<(MessageId, Result<()>)>> {
    // The trash may be named differently from how the open folder was
    // opened (`=Deleted Items` and `@deleteditems`): compare the folders.
    let trash = match trash {
        Some(path) => Some(resolve(store, path).await?).filter(|t| &t.id != from),
        None => None,
    };
    let mut results = Vec::new();
    for id in ids {
        let result = match &trash {
            Some(trash) => store.move_message(id, &trash.id).await.map(drop),
            None => store.delete_message(id).await,
        };
        results.push((id.clone(), result));
    }
    Ok(results)
}

/// Moves or copies `ids` to the folder at `to`, one at a time, so that a
/// failure only affects its own message.
async fn transfer<S: MailStore>(
    store: &S,
    ids: &[MessageId],
    to: &FolderPath,
    copy: bool,
) -> Result<(FolderId, Moved)> {
    let folder = resolve(store, to).await?;
    let mut results = Vec::new();
    for id in ids {
        let result = if copy {
            store.copy_message(id, &folder.id).await
        } else {
            store.move_message(id, &folder.id).await
        };
        results.push((id.clone(), result));
    }
    Ok((folder.id, results))
}

/// Whether `folder` is still at `path`: its name and its parents' match
/// the path's, up to the top of the mailbox or the well-known folder the
/// path starts from.
async fn at_path<S: MailStore>(store: &S, path: &FolderPath, folder: &Folder) -> Result<bool> {
    let (names, start) = match path {
        FolderPath::Root => return Ok(true),
        FolderPath::WellKnown(which, rest) => (rest.as_slice(), Some(*which)),
        FolderPath::Named(names) => (names.as_slice(), None),
    };
    let mut current = folder.clone();
    for (i, name) in names.iter().enumerate().rev() {
        if !FolderPath::Named(vec![name.clone()]).names(&current.name) {
            return Ok(false);
        }
        if i > 0 {
            let Some(parent) = current.parent.clone() else {
                return Ok(false);
            };
            current = store.folder(&parent).await?;
        }
    }
    match start {
        // A well-known folder keeps its ID.
        Some(_) if names.is_empty() => Ok(true),
        Some(which) => {
            let start = store.well_known_folder(which).await?;
            Ok(current.parent.as_ref() == Some(&start.id))
        }
        None => Ok(current.parent.is_none()),
    }
}

/// Finds the folder at `path`: a well-known folder, possibly followed by
/// names of folders below it, or names from the top of the mailbox.
async fn resolve<S: MailStore>(store: &S, path: &FolderPath) -> Result<Folder> {
    resolve_listed(store, path, &mut None).await
}

/// [`resolve`], listing the folders (if it must) into `listed`, or using
/// those listed there already: checking several mailboxes lists them once.
async fn resolve_listed<S: MailStore>(
    store: &S,
    path: &FolderPath,
    listed: &mut Option<Vec<Folder>>,
) -> Result<Folder> {
    match path {
        FolderPath::Root => Err(Error::Backend(
            "the mailbox itself isn't a folder; choose one, such as @inbox".into(),
        )),
        FolderPath::WellKnown(which, rest) => {
            let start = store.well_known_folder(*which).await?;
            if rest.is_empty() {
                return Ok(start);
            }
            descend(store, Some(start), rest, listed).await
        }
        FolderPath::Named(names) => descend(store, None, names, listed).await,
    }
}

/// Walks down `names` from `start` (or the top of the mailbox). Names match
/// exactly, or else ignoring case (in any script), as Office365 folder
/// names do.
async fn descend<S: MailStore>(
    store: &S,
    start: Option<Folder>,
    names: &[String],
    listed: &mut Option<Vec<Folder>>,
) -> Result<Folder> {
    if listed.is_none() {
        *listed = Some(store.list_folders().await?);
    }
    let folders = listed.as_deref().unwrap_or_default();
    let mut current = start;
    for name in names {
        let parent = current.as_ref().map(|f| &f.id);
        let children = || folders.iter().filter(|f| f.parent.as_ref() == parent);
        let found = children()
            .find(|f| &f.name == name)
            .or_else(|| {
                let name = name.to_lowercase();
                children().find(|f| f.name.to_lowercase() == name)
            })
            .ok_or_else(|| Error::NotFound {
                kind: "folder",
                id: names.join("/"),
            })?;
        current = Some(found.clone());
    }
    current.ok_or_else(|| Error::not_found("folder", names.join("/")))
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use mett_core::WellKnownFolder;
    use mett_core::mock::{self, MemoryStore};

    use super::*;

    fn tree() -> MemoryStore {
        let child = |id: &str, name: &str, parent: &str| {
            let mut folder = mock::folder(id, name, None);
            folder.parent = Some(FolderId::new(parent));
            folder
        };
        MemoryStore::new()
            .with_folder(mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox)))
            .with_folder(child("proj", "Projects", "in"))
            .with_folder(child("mett", "mett", "proj"))
            .with_folder(child("team", "équipe", "in"))
            .with_folder(mock::folder("arc", "Archive", None))
    }

    #[tokio::test]
    async fn tells_which_messages_left_a_folder() {
        let store = MemoryStore::new()
            .with_folder(mock::folder("in", "Inbox", None))
            .with_message(mock::message("m1", "in", "Hi", DateTime::UNIX_EPOCH));
        let event = serve(
            &store,
            Request::StillThere {
                folder: FolderId::new("in"),
                ids: vec![MessageId::new("m1"), MessageId::new("m2")],
            },
        )
        .await;
        let Event::StillThere(Ok(gone)) = event else {
            panic!("expected the messages gone, got {event:?}");
        };
        assert_eq!(gone, [MessageId::new("m2")]);
    }
    #[tokio::test]
    async fn runs_a_few_at_a_time_in_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (running, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let futures = (0..10)
            .map(|i| {
                let (running, most) = (&running, &most);
                async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    i
                }
            })
            .collect();
        let done = join_bounded(futures, 3).await;
        assert_eq!(done, (0..10).collect::<Vec<_>>());
        assert_eq!(most.load(Ordering::SeqCst), 3);
    }
    #[tokio::test]
    async fn resolves_folder_paths() {
        let store = tree();
        let inbox = || FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
        assert_eq!(
            resolve(&store, &inbox()).await.unwrap().id,
            FolderId::new("in")
        );
        let nested = FolderPath::WellKnown(
            WellKnownFolder::Inbox,
            vec!["projects".into(), "mett".into()],
        );
        assert_eq!(
            resolve(&store, &nested).await.unwrap().id,
            FolderId::new("mett")
        );
        let accented = FolderPath::WellKnown(WellKnownFolder::Inbox, vec!["ÉQUIPE".into()]);
        assert_eq!(
            resolve(&store, &accented).await.unwrap().id,
            FolderId::new("team")
        );
        let named = FolderPath::Named(vec!["Archive".into()]);
        assert_eq!(
            resolve(&store, &named).await.unwrap().id,
            FolderId::new("arc")
        );
        // A name is looked up only below its parent.
        let wrong = FolderPath::Named(vec!["Projects".into()]);
        assert!(matches!(
            resolve(&store, &wrong).await,
            Err(Error::NotFound { kind: "folder", .. })
        ));
        assert!(resolve(&store, &FolderPath::Root).await.is_err());
    }

    #[tokio::test]
    async fn checks_mailboxes_by_id_or_path() {
        let store = tree().with_message(mock::message("m1", "arc", "one", DateTime::UNIX_EPOCH));
        let archive = FolderPath::Named(vec!["Archive".into()]);
        let inbox = FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
        let missing = FolderPath::Named(vec!["nowhere".into()]);
        let Event::MailboxesChecked(checked) = serve(
            &store,
            Request::CheckMailboxes(vec![
                (archive.clone(), Some(FolderId::new("arc")), None),
                // A folder that is gone is looked for by path again.
                (inbox.clone(), Some(FolderId::new("gone")), None),
                (missing.clone(), None, None),
            ]),
        )
        .await
        else {
            panic!("expected MailboxesChecked");
        };
        let found: Vec<_> = checked
            .iter()
            .map(|(path, checked)| {
                let folder = checked.as_ref().ok().map(|c| &c.folder);
                (path, folder.map(|f| (f.id.as_str(), f.unread_count)))
            })
            .collect();
        assert_eq!(found[0], (&archive, Some(("arc", 1))));
        assert_eq!(found[1], (&inbox, Some(("in", 0))));
        assert!(matches!(checked[2].1, Err(Error::NotFound { .. })));
        // Renamed: the folder now called Archive is the one checked.
        let renamed = MemoryStore::new()
            .with_folder(mock::folder("old", "Elsewhere", None))
            .with_folder(mock::folder("arc", "Archive", None));
        let Event::MailboxesChecked(checked) = serve(
            &renamed,
            Request::CheckMailboxes(vec![(archive, Some(FolderId::new("old")), None)]),
        )
        .await
        else {
            panic!("expected MailboxesChecked");
        };
        assert_eq!(
            checked[0].1.as_ref().unwrap().folder.id,
            FolderId::new("arc")
        );
        // Moved to another parent: the folder now at the path is checked.
        let child = |id: &str, name: &str, parent: &str| {
            let mut folder = mock::folder(id, name, None);
            folder.parent = Some(FolderId::new(parent));
            folder
        };
        let moved = tree()
            .with_folder(child("shared-old", "Shared", "arc"))
            .with_folder(child("shared-new", "Shared", "proj"));
        let path = FolderPath::WellKnown(
            WellKnownFolder::Inbox,
            vec!["Projects".into(), "Shared".into()],
        );
        let Event::MailboxesChecked(checked) = serve(
            &moved,
            Request::CheckMailboxes(vec![
                (path.clone(), Some(FolderId::new("shared-old")), None),
                (path, Some(FolderId::new("shared-new")), None),
            ]),
        )
        .await
        else {
            panic!("expected MailboxesChecked");
        };
        assert_eq!(
            checked[0].1.as_ref().unwrap().folder.id,
            FolderId::new("shared-new")
        );
        assert_eq!(
            checked[1].1.as_ref().unwrap().folder.id,
            FolderId::new("shared-new")
        );
    }

    #[tokio::test]
    async fn moves_and_copies_messages_and_lists_folders() {
        let store = tree()
            .with_message(mock::message("m1", "in", "one", DateTime::UNIX_EPOCH))
            .with_message(mock::message("m2", "in", "two", DateTime::UNIX_EPOCH));
        let archive = FolderPath::Named(vec!["archive".into()]);
        let ids = vec![MessageId::new("m1"), MessageId::new("nope")];
        let Event::Transferred(Ok((_, results))) = serve(
            &store,
            Request::Transfer {
                ids,
                to: archive.clone(),
                copy: false,
            },
        )
        .await
        else {
            panic!("expected Transferred");
        };
        assert!(results[0].1.is_ok());
        assert!(matches!(results[1].1, Err(Error::NotFound { .. })));
        let missing = FolderPath::Named(vec!["nowhere".into()]);
        let event = serve(
            &store,
            Request::Transfer {
                ids: vec![MessageId::new("m2")],
                to: missing,
                copy: true,
            },
        )
        .await;
        assert!(matches!(event, Event::Transferred(Err(_))));

        let Event::FoldersListed(Ok(folders)) = serve(&store, Request::ListFolders).await else {
            panic!("expected FoldersListed");
        };
        assert_eq!(folders.len(), 5);
    }

    #[tokio::test]
    async fn purges_to_the_trash_or_for_good() {
        let store = tree()
            .with_message(mock::message("m1", "in", "one", DateTime::UNIX_EPOCH))
            .with_message(mock::message("a1", "arc", "old", DateTime::UNIX_EPOCH));
        let count = |folder: &'static str| {
            let store = &store;
            async move {
                store
                    .list_messages(&FolderId::new(folder), Page::first(10))
                    .await
                    .unwrap()
                    .messages
                    .len()
            }
        };
        let trash = Some(FolderPath::Named(vec!["Archive".into()]));
        let purge = |id: &str, from: &str| Request::Purge {
            ids: vec![MessageId::new(id)],
            from: FolderId::new(from),
            trash: trash.clone(),
        };
        let Event::Purged(Ok(results)) = serve(&store, purge("m1", "in")).await else {
            panic!("expected Purged");
        };
        assert!(results[0].1.is_ok());
        assert_eq!((count("in").await, count("arc").await), (0, 2), "moved");
        // In the trash itself, however it is named, purging deletes.
        let Event::Purged(Ok(results)) = serve(&store, purge("a1", "arc")).await else {
            panic!("expected Purged");
        };
        assert!(results[0].1.is_ok());
        assert_eq!(count("arc").await, 1, "deleted, not moved");
        let missing = Request::Purge {
            ids: vec![],
            from: FolderId::new("in"),
            trash: Some(FolderPath::Named(vec!["nowhere".into()])),
        };
        assert!(matches!(
            serve(&store, missing).await,
            Event::Purged(Err(_))
        ));
    }

    #[tokio::test]
    async fn answers_each_request_with_its_id() {
        let store = MemoryStore::new()
            .with_folder(mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox)))
            .with_message(mock::message("m1", "in", "hello", DateTime::UNIX_EPOCH));
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (response_tx, mut response_rx) = mpsc::unbounded_channel();
        tokio::spawn(run(Arc::new(store), request_rx, response_tx));

        let inbox = FolderPath::WellKnown(WellKnownFolder::Inbox, vec![]);
        request_tx
            .send((RequestId(1), Request::OpenFolder(inbox)))
            .unwrap();
        let response = response_rx.recv().await.unwrap();
        assert_eq!(response.id, RequestId(1));
        assert!(matches!(response.event, Event::FolderOpened(Ok(f)) if f.name == "Inbox"));

        request_tx
            .send((RequestId(2), Request::FetchMessage("m1".into())))
            .unwrap();
        let response = response_rx.recv().await.unwrap();
        assert_eq!(response.id, RequestId(2));
        assert!(
            matches!(response.event, Event::MessageFetched(Ok(m)) if m.summary.subject == "hello")
        );
    }
}
