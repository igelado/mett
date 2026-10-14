//! The background worker: the actor that owns all I/O (`docs/DESIGN.md`
//! §5.4).
//!
//! The UI sends [`Request`]s tagged with a [`RequestId`] and receives a
//! [`Response`] for each, so it can match results and drop stale ones. The
//! worker is generic over the [`MailStore`] port, so the UI is tested
//! against the in-memory store.

use std::sync::Arc;

use mett_core::FolderId;
use mett_core::{
    FlagsUpdate, Folder, MailStore, Message, MessageId, MessagePage, Page, Result, WellKnownFolder,
};
use tokio::sync::mpsc;

/// Identifies a request, so its response can be matched to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

/// Work for the worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Find the Inbox.
    OpenInbox,
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
    /// Outcome of [`Request::OpenInbox`].
    InboxOpened(Result<Folder>),
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
        Request::OpenInbox => {
            Event::InboxOpened(store.well_known_folder(WellKnownFolder::Inbox).await)
        }
        Request::ListMessages { folder, page } => {
            Event::MessagesListed(store.list_messages(&folder, page).await)
        }
        Request::FetchMessage(id) => Event::MessageFetched(store.fetch_message(&id).await),
        Request::UpdateFlags(id, update) => {
            Event::FlagsUpdated(store.update_flags(&id, update).await)
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use mett_core::mock::{self, MemoryStore};

    use super::*;

    #[tokio::test]
    async fn answers_each_request_with_its_id() {
        let store = MemoryStore::new()
            .with_folder(mock::folder("in", "Inbox", Some(WellKnownFolder::Inbox)))
            .with_message(mock::message("m1", "in", "hello", DateTime::UNIX_EPOCH));
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (response_tx, mut response_rx) = mpsc::unbounded_channel();
        tokio::spawn(run(Arc::new(store), request_rx, response_tx));

        request_tx.send((RequestId(1), Request::OpenInbox)).unwrap();
        let response = response_rx.recv().await.unwrap();
        assert_eq!(response.id, RequestId(1));
        assert!(matches!(response.event, Event::InboxOpened(Ok(f)) if f.name == "Inbox"));

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
