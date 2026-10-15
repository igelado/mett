//! In-memory adapters for tests.
//!
//! Enabled by the `mock` feature, so that other crates (the TUI, the worker)
//! can be tested against [`MemoryStore`] without a network connection.

use std::cmp::Reverse;
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, SecondsFormat, Utc};

use crate::draft::{Draft, DraftOrigin};
use crate::error::{Error, Result};
use crate::folder::{Folder, WellKnownFolder};
use crate::ids::{FolderId, MessageId, PageCursor};
use crate::message::{Body, BodyKind, Flags, FlagsUpdate, Message, MessageSummary};
use crate::ports::{MailSender, MailStore, MessagePage, Page};

/// A [`MailStore`] and [`MailSender`] that keeps everything in memory.
/// Sent drafts are recorded (see [`MemoryStore::sent`]), and copies and
/// saved drafts land in the well-known sent-items and drafts folders.
#[derive(Debug, Default)]
pub struct MemoryStore {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    folders: Vec<Folder>,
    messages: Vec<Message>,
    moves: u64,
    sent: Vec<Draft>,
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

    /// The drafts sent so far, oldest first.
    pub fn sent(&self) -> Vec<Draft> {
        self.state().sent.clone()
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

    /// Returns `folder` with its counts derived from the stored messages, so
    /// they always agree with [`MailStore::list_messages`].
    fn with_counts(&self, folder: &Folder) -> Folder {
        let in_folder = self
            .messages
            .iter()
            .filter(|m| m.summary.folder == folder.id);
        let (total, unread) = in_folder.fold((0, 0), |(total, unread), m| {
            (total + 1, unread + u32::from(!m.summary.flags.read))
        });
        Folder {
            total_count: total,
            unread_count: unread,
            ..folder.clone()
        }
    }

    /// Returns an ID derived from `id` that no stored message uses.
    fn fresh_id(&mut self, id: &MessageId) -> MessageId {
        loop {
            self.moves += 1;
            let candidate = MessageId::new(format!("{id}~{}", self.moves));
            if !self.messages.iter().any(|m| m.summary.id == candidate) {
                return candidate;
            }
        }
    }

    /// Stores `draft` as a message in the well-known folder `which`.
    fn store_draft(&mut self, draft: &Draft, which: WellKnownFolder) -> Result<MessageId> {
        let folder = self
            .folders
            .iter()
            .find(|f| f.well_known == Some(which))
            .ok_or_else(|| Error::not_found("folder", which))?
            .id
            .clone();
        let id = self.fresh_id(&MessageId::new(which.as_str()));
        // A forward carries the original's attachments along, as Graph's
        // createForward does.
        let has_attachments = match &draft.origin {
            Some(DraftOrigin::Forward(original)) => self
                .messages
                .iter()
                .any(|m| &m.summary.id == original && m.summary.has_attachments),
            _ => false,
        };
        self.messages.push(Message {
            summary: MessageSummary {
                id: id.clone(),
                folder,
                subject: draft.subject.clone(),
                from: draft.from.clone(),
                to: draft.to.clone(),
                received: Utc::now(),
                flags: Flags {
                    read: true,
                    flagged: false,
                },
                has_attachments,
            },
            cc: draft.cc.clone(),
            bcc: draft.bcc.clone(),
            reply_to: Vec::new(),
            internet_message_id: None,
            body: Body {
                kind: BodyKind::Text,
                content: draft.body.clone(),
            },
        });
        Ok(id)
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
        let state = self.state();
        Ok(state.folders.iter().map(|f| state.with_counts(f)).collect())
    }

    async fn folder(&self, id: &FolderId) -> Result<Folder> {
        let state = self.state();
        let folder = state.folder(id)?;
        Ok(state.with_counts(folder))
    }

    async fn well_known_folder(&self, which: WellKnownFolder) -> Result<Folder> {
        let state = self.state();
        state
            .folders
            .iter()
            .find(|f| f.well_known == Some(which))
            .map(|f| state.with_counts(f))
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
        messages.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));

        // The cursor holds the sort key of the last message returned, so the
        // next page resumes after it even if messages were moved or deleted
        // in between. An offset would skip messages in that case.
        let start = match page.cursor() {
            None => 0,
            Some(cursor) => {
                let (received, id) = decode_cursor(cursor)?;
                let after = (Reverse(received), &id);
                messages.partition_point(|m| sort_key(m) <= after)
            }
        };
        let end = start
            .saturating_add(page.size() as usize)
            .min(messages.len());
        let next = (end < messages.len())
            .then(|| messages.get(end - 1).map(encode_cursor))
            .flatten();
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
        state.message_mut(id)?;
        let new_id = state.fresh_id(id);
        let message = state.message_mut(id)?;
        message.summary.id = new_id.clone();
        message.summary.folder = to.clone();
        Ok(new_id)
    }

    async fn copy_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        let mut state = self.state();
        state.folder(to)?;
        let mut copy = state.message_mut(id)?.clone();
        copy.summary.id = state.fresh_id(id);
        copy.summary.folder = to.clone();
        let new_id = copy.summary.id.clone();
        state.messages.push(copy);
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

    async fn save_draft(&self, draft: &Draft) -> Result<MessageId> {
        self.state().store_draft(draft, WellKnownFolder::Drafts)
    }
}

impl MailSender for MemoryStore {
    async fn send(&self, draft: &Draft) -> Result<()> {
        if !draft.has_recipients() {
            return Err(Error::Backend("the message has no recipients".into()));
        }
        let mut state = self.state();
        if draft.save_copy {
            state.store_draft(draft, WellKnownFolder::SentItems)?;
        }
        state.sent.push(draft.clone());
        Ok(())
    }
}

/// Listing order: newest first, with ties broken by ID.
fn sort_key(m: &MessageSummary) -> (Reverse<DateTime<Utc>>, &MessageId) {
    (Reverse(m.received), &m.id)
}

fn encode_cursor(m: &MessageSummary) -> PageCursor {
    let received = m.received.to_rfc3339_opts(SecondsFormat::Nanos, true);
    PageCursor::new(format!("{received}|{}", m.id))
}

fn decode_cursor(cursor: &PageCursor) -> Result<(DateTime<Utc>, MessageId)> {
    let invalid = || Error::Backend(format!("invalid page cursor: {cursor}"));
    let (received, id) = cursor.as_str().split_once('|').ok_or_else(invalid)?;
    let received = DateTime::parse_from_rfc3339(received).map_err(|_| invalid())?;
    Ok((received.with_timezone(&Utc), MessageId::new(id)))
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
        bcc: Vec::new(),
        reply_to: Vec::new(),
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
    async fn copies_messages() {
        let store = store();
        let copy = store
            .copy_message(&MessageId::new("m1"), &FolderId::new("arc"))
            .await
            .unwrap();
        assert_ne!(copy, MessageId::new("m1"));
        let archive = store
            .list_messages(&FolderId::new("arc"), Page::first(10))
            .await
            .unwrap();
        assert_eq!(subjects(&archive), ["first"]);
        let inbox = store
            .list_messages(&FolderId::new("in"), Page::first(10))
            .await
            .unwrap();
        assert_eq!(inbox.messages.len(), 3, "the original stays");
    }

    #[tokio::test]
    async fn sends_and_saves_drafts() {
        let store = store()
            .with_folder(folder("drafts", "Drafts", Some(WellKnownFolder::Drafts)))
            .with_folder(folder("sent", "Sent", Some(WellKnownFolder::SentItems)));
        let draft = Draft {
            to: vec![crate::Address::new("bob@x.org")],
            subject: "hi".into(),
            save_copy: true,
            ..Draft::default()
        };
        store.send(&draft).await.unwrap();
        assert_eq!(store.sent(), std::slice::from_ref(&draft));
        let sent = store
            .list_messages(&FolderId::new("sent"), Page::first(10))
            .await
            .unwrap();
        assert_eq!(subjects(&sent), ["hi"]);

        let id = store.save_draft(&draft).await.unwrap();
        assert_eq!(
            store.fetch_message(&id).await.unwrap().summary.folder,
            FolderId::new("drafts")
        );
        assert!(store.send(&Draft::default()).await.is_err());
    }

    #[tokio::test]
    async fn a_forward_keeps_the_original_s_attachments() {
        let mut original = message("m4", "in", "report", at(4));
        original.summary.has_attachments = true;
        let store = store().with_message(original).with_folder(folder(
            "drafts",
            "Drafts",
            Some(WellKnownFolder::Drafts),
        ));
        let forward = Draft {
            to: vec![crate::Address::new("bob@x.org")],
            subject: "Fwd: report".into(),
            origin: Some(DraftOrigin::Forward(MessageId::new("m4"))),
            ..Draft::default()
        };
        let id = store.save_draft(&forward).await.unwrap();
        assert!(
            store
                .fetch_message(&id)
                .await
                .unwrap()
                .summary
                .has_attachments
        );
        let reply = Draft {
            origin: Some(DraftOrigin::Reply(MessageId::new("m4"))),
            ..forward
        };
        let id = store.save_draft(&reply).await.unwrap();
        assert!(
            !store
                .fetch_message(&id)
                .await
                .unwrap()
                .summary
                .has_attachments
        );
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
    async fn pagination_survives_deleting_seen_messages() {
        let store = store();
        let inbox = FolderId::new("in");

        let first = store.list_messages(&inbox, Page::first(2)).await.unwrap();
        assert_eq!(subjects(&first), ["third", "second"]);
        store.delete_message(&MessageId::new("m3")).await.unwrap();

        let cursor = first.next.expect("a second page");
        let second = store
            .list_messages(&inbox, Page::after(cursor, 2))
            .await
            .unwrap();
        assert_eq!(subjects(&second), ["first"]);
    }

    #[tokio::test]
    async fn rejects_malformed_cursors() {
        let result = store()
            .list_messages(&FolderId::new("in"), Page::after("junk".into(), 2))
            .await;
        assert!(matches!(result, Err(Error::Backend(_))));
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
    async fn folder_counts_follow_the_messages() {
        let store = store();
        let counts = |f: Folder| (f.total_count, f.unread_count);
        let inbox = || store.well_known_folder(WellKnownFolder::Inbox);
        assert_eq!(counts(inbox().await.unwrap()), (3, 3));

        store
            .update_flags(
                &MessageId::new("m1"),
                FlagsUpdate {
                    read: Some(true),
                    flagged: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(counts(inbox().await.unwrap()), (3, 2));

        store
            .move_message(&MessageId::new("m2"), &FolderId::new("arc"))
            .await
            .unwrap();
        store.delete_message(&MessageId::new("m3")).await.unwrap();
        let folders = store.list_folders().await.unwrap();
        let totals: Vec<_> = folders.into_iter().map(counts).collect();
        assert_eq!(totals, [(1, 0), (1, 1)]);
    }

    #[tokio::test]
    async fn moved_ids_never_collide() {
        let store = store().with_message(message("m1~1", "in", "decoy", at(4)));
        let new_id = store
            .move_message(&MessageId::new("m1"), &FolderId::new("arc"))
            .await
            .unwrap();
        assert_ne!(new_id, MessageId::new("m1~1"));
        let decoy = store.fetch_message(&MessageId::new("m1~1")).await.unwrap();
        assert_eq!(decoy.summary.subject, "decoy");
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
