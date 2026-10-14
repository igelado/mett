//! The background worker: the actor that owns all I/O (`docs/DESIGN.md`
//! §5.4).
//!
//! The UI sends [`Request`]s tagged with a [`RequestId`] and receives a
//! [`Response`] for each, so it can match results and drop stale ones. The
//! worker is generic over the [`MailStore`] port, so the UI is tested
//! against the in-memory store.

use std::sync::Arc;

use mett_core::{
    Error, FlagsUpdate, Folder, FolderId, MailStore, Message, MessageId, MessagePage, Page, Result,
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
    /// Fetch a full message.
    FetchMessage(MessageId),
    /// Change a message's flags.
    UpdateFlags(MessageId, FlagsUpdate),
}

/// The outcome of a [`Request`].
#[derive(Debug)]
pub enum Event {
    /// Outcome of [`Request::OpenFolder`].
    FolderOpened(Result<Folder>),
    /// Outcome of [`Request::ListMessages`].
    MessagesListed(Result<MessagePage>),
    /// Outcome of [`Request::FetchMessage`].
    MessageFetched(Result<Message>),
    /// Outcome of [`Request::UpdateFlags`].
    FlagsUpdated(Result<()>),
}

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
pub async fn run<S: MailStore + 'static>(
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

async fn serve<S: MailStore>(store: &S, request: Request) -> Event {
    match request {
        Request::OpenFolder(path) => Event::FolderOpened(resolve(store, &path).await),
        Request::ListMessages { folder, page } => {
            Event::MessagesListed(store.list_messages(&folder, page).await)
        }
        Request::FetchMessage(id) => Event::MessageFetched(store.fetch_message(&id).await),
        Request::UpdateFlags(id, update) => {
            Event::FlagsUpdated(store.update_flags(&id, update).await)
        }
    }
}

/// Finds the folder at `path`: a well-known folder, possibly followed by
/// names of folders below it, or names from the top of the mailbox.
async fn resolve<S: MailStore>(store: &S, path: &FolderPath) -> Result<Folder> {
    match path {
        FolderPath::Root => Err(Error::Backend(
            "the mailbox itself isn't a folder; choose one, such as @inbox".into(),
        )),
        FolderPath::WellKnown(which, rest) => {
            let start = store.well_known_folder(*which).await?;
            if rest.is_empty() {
                return Ok(start);
            }
            descend(store, Some(start), rest).await
        }
        FolderPath::Named(names) => descend(store, None, names).await,
    }
}

/// Walks down `names` from `start` (or the top of the mailbox). Names match
/// exactly, or else ignoring case (in any script), as Office365 folder
/// names do.
async fn descend<S: MailStore>(
    store: &S,
    start: Option<Folder>,
    names: &[String],
) -> Result<Folder> {
    let folders = store.list_folders().await?;
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
