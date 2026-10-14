//! In-memory adapters for tests.
//!
//! Enabled by the `mock` feature, so that other crates (the TUI, the worker)
//! can be tested against [`MemoryStore`] without a network connection.

use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, Utc};

use crate::error::{Error, Result};
use crate::folder::{Folder, WellKnownFolder};
use crate::ids::{FolderId, MessageId, PageCursor};
use crate::message::{Body, BodyKind, Flags, FlagsUpdate, Message, MessageSummary};
use crate::ports::{MailStore, MessagePage, Page};

/// A [`MailStore`] that keeps everything in memory.
#[derive(Debug, Default)]
pub struct MemoryStore {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    folders: Vec<Folder>,
    messages: Vec<Message>,
    moves: u64,
}

impl MemoryStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a folder.
    pub fn with_folder(self, folder: Folder) -> Self {
        self.state().folders.push(folder);
        self
    }

    /// Adds a message.
    pub fn with_message(self, message: Message) -> Self {
        self.state().messages.push(message);
        self
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock can only come from a failing test;
        // keep using the data rather than cascading the panic.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl State {
    fn folder(&self, id: &FolderId) -> Result<&Folder> {
        self.folders
            .iter()
            .find(|f| &f.id == id)
            .ok_or_else(|| Error::not_found("folder", id))
    }

    fn message_mut(&mut self, id: &MessageId) -> Result<&mut Message> {
        self.messages
            .iter_mut()
            .find(|m| &m.summary.id == id)
            .ok_or_else(|| Error::not_found("message", id))
    }
}

impl MailStore for MemoryStore {
    async fn list_folders(&self) -> Result<Vec<Folder>> {
        Ok(self.state().folders.clone())
    }

    async fn well_known_folder(&self, which: WellKnownFolder) -> Result<Folder> {
        self.state()
            .folders
            .iter()
            .find(|f| f.well_known == Some(which))
            .cloned()
            .ok_or_else(|| Error::not_found("folder", which))
    }

    async fn list_messages(&self, folder: &FolderId, page: Page) -> Result<MessagePage> {
        let state = self.state();
        state.folder(folder)?;

        let mut messages: Vec<_> = state
            .messages
            .iter()
            .filter(|m| &m.summary.folder == folder)
            .map(|m| m.summary.clone())
            .collect();
        messages.sort_by_key(|m| std::cmp::Reverse(m.received));

        // The cursor is the offset of the next page.
        let start = match &page.cursor {
            None => 0,
            Some(cursor) => cursor
                .as_str()
                .parse::<usize>()
                .map_err(|_| Error::Backend(format!("invalid page cursor: {cursor}")))?,
        };
        let end = start.saturating_add(page.size as usize).min(messages.len());
        let next = (end < messages.len()).then(|| PageCursor::new(end.to_string()));
        let messages = messages.get(start..end).unwrap_or_default().to_vec();

        Ok(MessagePage { messages, next })
    }

    async fn fetch_message(&self, id: &MessageId) -> Result<Message> {
        self.state().message_mut(id).map(|m| m.clone())
    }

    async fn update_flags(&self, id: &MessageId, update: FlagsUpdate) -> Result<()> {
        update.apply(&mut self.state().message_mut(id)?.summary.flags);
        Ok(())
    }

    async fn move_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        let mut state = self.state();
        state.folder(to)?;
        state.moves += 1;
        let new_id = MessageId::new(format!("{id}~{}", state.moves));
        let message = state.message_mut(id)?;
        message.summary.id = new_id.clone();
        message.summary.folder = to.clone();
        Ok(new_id)
    }

    async fn delete_message(&self, id: &MessageId) -> Result<()> {
        let mut state = self.state();
        let before = state.messages.len();
        state.messages.retain(|m| &m.summary.id != id);
        if state.messages.len() == before {
            return Err(Error::not_found("message", id));
        }
        Ok(())
    }
}

/// Creates an empty folder for tests.
pub fn folder(id: &str, name: &str, well_known: Option<WellKnownFolder>) -> Folder {
    Folder {
        id: FolderId::new(id),
        parent: None,
        name: name.to_owned(),
        well_known,
        unread_count: 0,
        total_count: 0,
    }
}

/// Creates an unread plain-text message for tests.
pub fn message(id: &str, folder: &str, subject: &str, received: DateTime<Utc>) -> Message {
    Message {
        summary: MessageSummary {
            id: MessageId::new(id),
            folder: FolderId::new(folder),
            subject: subject.to_owned(),
            from: None,
            to: Vec::new(),
            received,
            flags: Flags::default(),
            has_attachments: false,
        },
        cc: Vec::new(),
        internet_message_id: None,
        body: Body {
            kind: BodyKind::Text,
            content: format!("Body of {subject}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes)
    }

    fn store() -> MemoryStore {
        MemoryStore::new()
            .with_folder(folder("in", "Inbox", Some(WellKnownFolder::Inbox)))
            .with_folder(folder("arc", "Archive", Some(WellKnownFolder::Archive)))
            .with_message(message("m1", "in", "first", at(1)))
            .with_message(message("m2", "in", "second", at(2)))
            .with_message(message("m3", "in", "third", at(3)))
    }

    fn subjects(page: &MessagePage) -> Vec<&str> {
        page.messages.iter().map(|m| m.subject.as_str()).collect()
    }

    #[tokio::test]
    async fn lists_messages_newest_first_in_pages() {
        let store = store();
        let inbox = FolderId::new("in");

        let first = store.list_messages(&inbox, Page::first(2)).await.unwrap();
        assert_eq!(subjects(&first), ["third", "second"]);

        let cursor = first.next.expect("a second page");
        let second = store
            .list_messages(&inbox, Page::after(cursor, 2))
            .await
            .unwrap();
        assert_eq!(subjects(&second), ["first"]);
        assert_eq!(second.next, None);
    }

    #[tokio::test]
    async fn finds_well_known_folders() {
        let store = store();
        let inbox = store
            .well_known_folder(WellKnownFolder::Inbox)
            .await
            .unwrap();
        assert_eq!(inbox.id, FolderId::new("in"));
        assert!(matches!(
            store.well_known_folder(WellKnownFolder::Drafts).await,
            Err(Error::NotFound { kind: "folder", .. })
        ));
    }

    #[tokio::test]
    async fn listing_an_unknown_folder_fails() {
        let result = store()
            .list_messages(&FolderId::new("nope"), Page::first(10))
            .await;
        assert!(matches!(
            result,
            Err(Error::NotFound { kind: "folder", .. })
        ));
    }

    #[tokio::test]
    async fn updates_flags() {
        let store = store();
        let id = MessageId::new("m1");
        store
            .update_flags(
                &id,
                FlagsUpdate {
                    read: Some(true),
                    flagged: None,
                },
            )
            .await
            .unwrap();
        let flags = store.fetch_message(&id).await.unwrap().summary.flags;
        assert!(flags.read);
        assert!(!flags.flagged);
    }

    #[tokio::test]
    async fn moving_assigns_a_new_id() {
        let store = store();
        let archive = FolderId::new("arc");
        let new_id = store
            .move_message(&MessageId::new("m1"), &archive)
            .await
            .unwrap();

        assert_ne!(new_id, MessageId::new("m1"));
        assert!(store.fetch_message(&MessageId::new("m1")).await.is_err());
        let moved = store.fetch_message(&new_id).await.unwrap();
        assert_eq!(moved.summary.folder, archive);
    }

    #[tokio::test]
    async fn deletes_messages() {
        let store = store();
        let id = MessageId::new("m2");
        store.delete_message(&id).await.unwrap();
        assert!(store.fetch_message(&id).await.is_err());
        assert!(matches!(
            store.delete_message(&id).await,
            Err(Error::NotFound {
                kind: "message",
                ..
            })
        ));
    }
}
