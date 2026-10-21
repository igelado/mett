//! In-memory adapters for tests.
//!
//! Enabled by the `mock` feature, so that other crates (the TUI, the worker)
//! can be tested against [`MemoryStore`] without a network connection.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use chrono::{DateTime, SecondsFormat, Utc};

use crate::attachment::{Attachment, AttachmentKind};
use crate::draft::{Draft, DraftOrigin};
use crate::error::{Error, Result};
use crate::folder::{Folder, WellKnownFolder};
use crate::ids::{AttachmentId, FolderId, MessageId, PageCursor, SyncState};
use crate::message::{Address, Body, BodyKind, Flags, FlagsUpdate, Message, MessageSummary};
use crate::ports::{FolderChanges, MailSender, MailStore, MessagePage, Page};

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
    /// Each message's attachments, with their content.
    attachments: HashMap<MessageId, Vec<(Attachment, Vec<u8>)>>,
    /// Whether folders can be synced ([`MemoryStore::with_sync`]).
    sync: bool,
    /// Sync states from before this many [`MemoryStore::forget_syncs`]
    /// are stale.
    sync_epoch: u64,
    /// How many times folders were synced.
    syncs: u64,
    /// The answers to invitations, as given.
    answers: Vec<(MessageId, crate::Answer)>,
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

    /// Adds a message, last changed when it is dated.
    pub fn with_message(self, message: Message) -> Self {
        self.deliver(message.clone(), message.summary.received);
        self
    }

    /// Adds a message, as if it arrived (or last changed) at `modified`.
    pub fn deliver(&self, mut message: Message, modified: DateTime<Utc>) {
        message.summary.modified = Some(modified);
        self.state().messages.push(message);
    }

    /// Attaches a file to the message `message`.
    pub fn with_attachment(
        self,
        message: &str,
        name: &str,
        content_type: &str,
        data: &[u8],
    ) -> Self {
        let mut state = self.state();
        let list = state
            .attachments
            .entry(MessageId::new(message))
            .or_default();
        list.push(file(
            format!("{message}-a{}", list.len()),
            name,
            content_type,
            data,
        ));
        if let Ok(message) = state.message_mut(&MessageId::new(message)) {
            message.summary.has_attachments = true;
        }
        drop(state);
        self
    }

    /// Lets folders be synced ([`MailStore::sync_folder`]).
    pub fn with_sync(self) -> Self {
        self.state().sync = true;
        self
    }

    /// The answers given to invitations, in order.
    pub fn answers(&self) -> Vec<(MessageId, crate::Answer)> {
        self.state().answers.clone()
    }

    /// Changes what a message says about a meeting, as its organizer or an
    /// answer from another client does.
    pub fn set_invitation(&self, id: &MessageId, invitation: Option<crate::Invitation>) {
        if let Ok(message) = self.state().message_mut(id) {
            message.invitation = invitation;
        }
    }

    /// Makes every sync state handed out so far stale.
    pub fn forget_syncs(&self) {
        self.state().sync_epoch += 1;
    }

    /// How many times folders were synced.
    pub fn syncs(&self) -> u64 {
        self.state().syncs
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
    fn well_known(&self, which: WellKnownFolder) -> Option<&Folder> {
        self.folders.iter().find(|f| f.well_known == Some(which))
    }

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

    /// The folders `which` picks, in order, with their counts (see
    /// [`State::with_counts`]), counting the messages once for them all.
    fn counted(&self, which: impl Fn(&Folder) -> bool) -> Vec<Folder> {
        let mut counts: HashMap<&FolderId, (u32, u32)> = HashMap::new();
        for m in &self.messages {
            let (total, unread) = counts.entry(&m.summary.folder).or_default();
            *total += 1;
            *unread += u32::from(!m.summary.flags.read);
        }
        self.folders
            .iter()
            .filter(|f| which(f))
            .map(|f| {
                let (total, unread) = counts.get(&f.id).copied().unwrap_or_default();
                Folder {
                    total_count: total,
                    unread_count: unread,
                    ..f.clone()
                }
            })
            .collect()
    }

    /// Fails if `draft` recalls a draft that is gone (sent or deleted), as
    /// Office365 does.
    fn check_recalled(&self, draft: &Draft) -> Result<()> {
        // Still in Drafts: one moved elsewhere isn't recalled any more.
        let drafts = self.well_known(WellKnownFolder::Drafts).map(|f| &f.id);
        match &draft.origin {
            Some(DraftOrigin::Recalled(id))
                if !self
                    .messages
                    .iter()
                    .any(|m| &m.summary.id == id && Some(&m.summary.folder) == drafts) =>
            {
                Err(Error::Backend(
                    "the postponed message is no longer in Drafts".into(),
                ))
            }
            _ => Ok(()),
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
            .well_known(which)
            .ok_or_else(|| Error::not_found("folder", which))?
            .id
            .clone();
        let id = self.fresh_id(&MessageId::new(which.as_str()));
        // A forward carries the original's attachments along, as Graph's
        // createForward does.
        // So does a recalled draft its own.
        let mut attachments: Vec<(Attachment, Vec<u8>)> = match &draft.origin {
            Some(DraftOrigin::Forward(original)) => {
                self.attachments.get(original).cloned().unwrap_or_default()
            }
            Some(DraftOrigin::Recalled(original)) => {
                let mut kept = self.attachments.get(original).cloned().unwrap_or_default();
                kept.retain(|(a, _)| !draft.drop_attachments.contains(&a.id));
                kept
            }
            _ => Vec::new(),
        };
        let forwarded = matches!(&draft.origin, Some(DraftOrigin::Forward(original))
            if self.messages.iter().any(|m| &m.summary.id == original && m.summary.has_attachments));
        for f in &draft.attachments {
            attachments.push(file(
                format!("{id}-a{}", attachments.len()),
                &f.name,
                &f.content_type,
                &f.data,
            ));
        }
        let has_attachments = forwarded || !attachments.is_empty();
        if !attachments.is_empty() {
            self.attachments.insert(id.clone(), attachments);
        }
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
                draft: which == WellKnownFolder::Drafts,
                delivered: Some(Utc::now()),
                modified: Some(Utc::now()),
                thread: None,
            },
            cc: draft.cc.clone(),
            bcc: draft.bcc.clone(),
            reply_to: Vec::new(),
            internet_message_id: None,
            body: Body {
                kind: BodyKind::Text,
                content: draft.body.clone(),
            },
            invitation: None,
        });
        Ok(id)
    }

    /// Removes message `id` and its attachments, and returns whether it was
    /// there.
    fn remove(&mut self, id: &MessageId) -> bool {
        let before = self.messages.len();
        self.messages.retain(|m| &m.summary.id != id);
        let removed = self.messages.len() != before;
        if removed {
            self.attachments.remove(id);
        }
        removed
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
        Ok(self.state().counted(|_| true))
    }

    async fn child_folders(&self, parent: Option<&FolderId>) -> Result<Vec<Folder>> {
        Ok(self.state().counted(|f| f.parent.as_ref() == parent))
    }

    /// The senders and recipients of the stored mail.
    async fn search_people(&self, text: &str) -> Result<Vec<Address>> {
        let text = text.to_lowercase();
        let state = self.state();
        let mut found: Vec<Address> = Vec::new();
        let people = state.messages.iter().flat_map(|m| {
            m.summary
                .from
                .iter()
                .chain(&m.summary.to)
                .chain(&m.cc)
                .chain(&m.bcc)
        });
        for person in people {
            let matches = person.email.to_lowercase().contains(&text)
                || person
                    .name
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().contains(&text));
            if matches
                && !found
                    .iter()
                    .any(|f| f.email.eq_ignore_ascii_case(&person.email))
            {
                found.push(person.clone());
            }
        }
        Ok(found)
    }

    async fn folder(&self, id: &FolderId) -> Result<Folder> {
        let state = self.state();
        let folder = state.folder(id)?;
        Ok(state.with_counts(folder))
    }

    async fn recent_unread(
        &self,
        folder: &FolderId,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<(MessageId, DateTime<Utc>)>> {
        let state = self.state();
        state.folder(folder)?;
        let mut recent: Vec<_> = state
            .messages
            .iter()
            .filter(|m| &m.summary.folder == folder && !m.summary.flags.read)
            .filter_map(|m| Some((m.summary.id.clone(), m.summary.modified?)))
            .filter(|(_, modified)| since.is_none_or(|since| *modified >= since))
            .collect();
        recent.sort_by_key(|(_, modified)| Reverse(*modified));
        recent.truncate(crate::ports::RECENT_UNREAD);
        Ok(recent)
    }

    async fn well_known_folder(&self, which: WellKnownFolder) -> Result<Folder> {
        let state = self.state();
        state
            .well_known(which)
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
            .map(|m| &m.summary)
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
            .then(|| messages.get(end - 1).map(|m| encode_cursor(m)))
            .flatten();
        let messages = messages
            .get(start..end)
            .unwrap_or_default()
            .iter()
            .map(|m| (*m).clone())
            .collect();

        Ok(MessagePage { messages, next })
    }

    /// Every change matching is on the one page.
    async fn changed_messages(
        &self,
        folder: &FolderId,
        since: DateTime<Utc>,
        _page: Page,
    ) -> Result<MessagePage> {
        let state = self.state();
        state.folder(folder)?;
        let mut changed: Vec<_> = state
            .messages
            .iter()
            .filter(|m| &m.summary.folder == folder)
            .filter_map(|m| {
                let modified = m.summary.modified?;
                (modified >= since).then(|| (modified, m.summary.clone()))
            })
            .collect();
        changed.sort_by_key(|(modified, _)| Reverse(*modified));
        Ok(MessagePage {
            messages: changed.into_iter().map(|(_, m)| m).collect(),
            next: None,
        })
    }

    async fn fetch_message(&self, id: &MessageId) -> Result<Message> {
        self.state().message_mut(id).map(|m| m.clone())
    }

    async fn list_attachments(&self, message: &MessageId) -> Result<Vec<Attachment>> {
        let mut state = self.state();
        state.message_mut(message)?;
        Ok(state
            .attachments
            .get(message)
            .map(|list| list.iter().map(|(a, _)| a.clone()).collect())
            .unwrap_or_default())
    }

    async fn fetch_attachment(
        &self,
        message: &MessageId,
        attachment: &AttachmentId,
    ) -> Result<Vec<u8>> {
        let state = self.state();
        state
            .attachments
            .get(message)
            .and_then(|list| list.iter().find(|(a, _)| &a.id == attachment))
            .map(|(_, data)| data.clone())
            .ok_or_else(|| Error::not_found("attachment", attachment))
    }

    async fn update_flags(&self, id: &MessageId, update: FlagsUpdate) -> Result<()> {
        let mut state = self.state();
        let summary = &mut state.message_mut(id)?.summary;
        update.apply(&mut summary.flags);
        summary.modified = Some(Utc::now());
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
        message.summary.modified = Some(Utc::now());
        if let Some(attachments) = state.attachments.remove(id) {
            state.attachments.insert(new_id.clone(), attachments);
        }
        Ok(new_id)
    }

    async fn copy_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        let mut state = self.state();
        state.folder(to)?;
        let mut copy = state.message_mut(id)?.clone();
        copy.summary.id = state.fresh_id(id);
        copy.summary.folder = to.clone();
        copy.summary.modified = Some(Utc::now());
        if let Some(attachments) = state.attachments.get(id).cloned() {
            state
                .attachments
                .insert(copy.summary.id.clone(), attachments);
        }
        let new_id = copy.summary.id.clone();
        state.messages.push(copy);
        Ok(new_id)
    }

    async fn delete_message(&self, id: &MessageId) -> Result<()> {
        if !self.state().remove(id) {
            return Err(Error::not_found("message", id));
        }
        Ok(())
    }

    async fn answer_invitation(
        &self,
        id: &MessageId,
        invitation: &crate::Invitation,
        answer: &crate::Answer,
    ) -> Result<()> {
        if !invitation.can_answer() {
            return Err(Error::Backend("the meeting can't be answered".into()));
        }
        let mut state = self.state();
        let message = state.message_mut(id)?;
        if let Some(invitation) = &mut message.invitation {
            invitation.reply = answer.rsvp.reply();
        }
        state.answers.push((id.clone(), answer.clone()));
        Ok(())
    }

    fn can_sync(&self) -> bool {
        self.state().sync
    }

    /// The state is a snapshot of the folder: its epoch, then each
    /// message's ID and a hash of its summary. What differs from it now
    /// changed.
    async fn sync_folder(
        &self,
        folder: &FolderId,
        state: Option<&SyncState>,
    ) -> Result<FolderChanges> {
        use std::hash::{Hash, Hasher};

        let mut store = self.state();
        if !store.sync {
            return Err(Error::Backend("this mailbox can't sync folders".into()));
        }
        store.folder(folder)?;
        store.syncs += 1;
        // A state is only good for its own folder, from this epoch.
        let epoch = format!("{} {folder}", store.sync_epoch);
        let mut before: HashMap<&str, &str> = HashMap::new();
        if let Some(state) = state {
            let mut lines = state.as_str().lines();
            if lines.next() != Some(epoch.as_str()) {
                return Err(Error::StaleSync);
            }
            for line in lines {
                let (id, hash) = line.split_once('\t').ok_or(Error::StaleSync)?;
                before.insert(id, hash);
            }
        }
        let hash = |summary: &MessageSummary| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            format!("{summary:?}").hash(&mut hasher);
            hasher.finish().to_string()
        };
        let now: Vec<(&MessageSummary, String)> = store
            .messages
            .iter()
            .filter(|m| &m.summary.folder == folder)
            .map(|m| (&m.summary, hash(&m.summary)))
            .collect();
        let changed = now
            .iter()
            .filter(|(m, hash)| before.get(m.id.as_str()) != Some(&hash.as_str()))
            .map(|(m, _)| (*m).clone())
            .collect();
        let removed = before
            .keys()
            .filter(|id| !now.iter().any(|(m, _)| m.id.as_str() == **id))
            .map(|id| MessageId::new(*id))
            .collect();
        let mut snapshot = epoch;
        for (m, hash) in &now {
            snapshot.push_str(&format!("\n{}\t{hash}", m.id));
        }
        Ok(FolderChanges {
            changed,
            removed,
            state: SyncState::new(snapshot),
            more: false,
        })
    }

    async fn save_draft(&self, draft: &Draft) -> Result<MessageId> {
        let mut state = self.state();
        state.check_recalled(draft)?;
        let id = state.store_draft(draft, WellKnownFolder::Drafts)?;
        // A recalled draft is postponed again as itself.
        match &draft.origin {
            Some(DraftOrigin::Recalled(old)) if state.remove(old) => {
                let old = old.clone();
                for message in state.messages.iter_mut().filter(|m| m.summary.id == id) {
                    message.summary.id = old.clone();
                }
                if let Some(attachments) = state.attachments.remove(&id) {
                    state.attachments.insert(old.clone(), attachments);
                }
                Ok(old)
            }
            _ => Ok(id),
        }
    }
}

impl MailSender for MemoryStore {
    async fn send(&self, draft: &Draft) -> Result<()> {
        if !draft.has_recipients() {
            return Err(Error::Backend("the message has no recipients".into()));
        }
        let mut state = self.state();
        state.check_recalled(draft)?;
        // A recalled draft is what is sent: it leaves Drafts, and a copy
        // is always kept, as Office365 does with drafts it sends.
        let recalled = match &draft.origin {
            Some(DraftOrigin::Recalled(id)) => Some(id.clone()),
            _ => None,
        };
        if draft.save_copy || recalled.is_some() {
            state.store_draft(draft, WellKnownFolder::SentItems)?;
        }
        if let Some(id) = recalled {
            state.remove(&id);
        }
        state.sent.push(draft.clone());
        Ok(())
    }
}

/// A file attachment `id` with its content.
fn file(id: String, name: &str, content_type: &str, data: &[u8]) -> (Attachment, Vec<u8>) {
    let attachment = Attachment {
        id: AttachmentId::new(id),
        name: name.to_owned(),
        content_type: content_type.to_owned(),
        size: data.len() as u64,
        inline: false,
        kind: AttachmentKind::File,
    };
    (attachment, data.to_vec())
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
            draft: false,
            delivered: Some(received),
            modified: None,
            thread: None,
        },
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        internet_message_id: None,
        body: Body {
            kind: BodyKind::Text,
            content: format!("Body of {subject}"),
        },
        invitation: None,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes)
    }

    #[tokio::test]
    async fn syncs_tell_what_changed_since() {
        let store = store().with_sync();
        let inbox = FolderId::new("in");
        let all = store.sync_folder(&inbox, None).await.unwrap();
        assert_eq!(all.changed.len(), 3);
        assert!(all.removed.is_empty() && !all.more);
        let none = store.sync_folder(&inbox, Some(&all.state)).await.unwrap();
        assert!(none.changed.is_empty() && none.removed.is_empty());

        store
            .update_flags(&"m1".into(), FlagsUpdate::read(true))
            .await
            .unwrap();
        store
            .move_message(&"m2".into(), &FolderId::new("arc"))
            .await
            .unwrap();
        let some = store.sync_folder(&inbox, Some(&none.state)).await.unwrap();
        let changed: Vec<&str> = some.changed.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(changed, ["m1"]);
        assert_eq!(some.removed, [MessageId::new("m2")]);

        // Another folder's state is no good here.
        assert!(matches!(
            store
                .sync_folder(&FolderId::new("arc"), Some(&some.state))
                .await,
            Err(Error::StaleSync)
        ));
        store.forget_syncs();
        assert!(matches!(
            store.sync_folder(&inbox, Some(&some.state)).await,
            Err(Error::StaleSync)
        ));
        assert_eq!(store.syncs(), 5);
    }

    fn store() -> MemoryStore {
        MemoryStore::new()
            .with_folder(folder("in", "Inbox", Some(WellKnownFolder::Inbox)))
            .with_folder(folder("arc", "Archive", Some(WellKnownFolder::Archive)))
            .with_message(message("m1", "in", "first", at(1)))
            .with_message(message("m2", "in", "second", at(2)))
            .with_message(message("m3", "in", "third", at(3)))
    }

    /// [`store`], with Drafts and Sent Items.
    fn store_with_drafts() -> MemoryStore {
        store()
            .with_folder(folder("drafts", "Drafts", Some(WellKnownFolder::Drafts)))
            .with_folder(folder("sent", "Sent", Some(WellKnownFolder::SentItems)))
    }

    async fn has_attachments(store: &MemoryStore, id: &MessageId) -> bool {
        store
            .fetch_message(id)
            .await
            .unwrap()
            .summary
            .has_attachments
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
        let store = store_with_drafts();
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
    async fn a_recalled_draft_is_postponed_and_sent_as_itself() {
        let store = store_with_drafts();
        let draft = Draft {
            to: vec![crate::Address::new("bob@x.org")],
            subject: "hi".into(),
            attachments: vec![crate::OutgoingAttachment {
                name: "a.txt".into(),
                content_type: "text/plain".into(),
                data: b"a".to_vec(),
            }],
            ..Draft::default()
        };
        let id = store.save_draft(&draft).await.unwrap();
        assert!(has_attachments(&store, &id).await);
        let recalled = Draft {
            attachments: Vec::new(),
            subject: "hi again".into(),
            origin: Some(DraftOrigin::Recalled(id.clone())),
            ..draft
        };
        assert_eq!(store.save_draft(&recalled).await.unwrap(), id);
        let drafts = FolderId::new("drafts");
        let listed = store.list_messages(&drafts, Page::first(10)).await.unwrap();
        assert_eq!(subjects(&listed), ["hi again"]);
        // Without the attachments it had.
        let bare = Draft {
            drop_attachments: vec![crate::AttachmentId::new(format!("{id}-a0"))],
            ..recalled.clone()
        };
        assert_eq!(store.save_draft(&bare).await.unwrap(), id);
        assert!(!has_attachments(&store, &id).await);
        store.send(&recalled).await.unwrap();
        let listed = store.list_messages(&drafts, Page::first(10)).await.unwrap();
        assert!(listed.messages.is_empty());
        let sent = store
            .list_messages(&FolderId::new("sent"), Page::first(10))
            .await
            .unwrap();
        assert_eq!(subjects(&sent), ["hi again"]);
        // Gone: not sent again, nor made again.
        assert!(store.send(&recalled).await.is_err());
        assert!(store.save_draft(&recalled).await.is_err());
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
        assert!(has_attachments(&store, &id).await);
        let reply = Draft {
            origin: Some(DraftOrigin::Reply(MessageId::new("m4"))),
            ..forward
        };
        let id = store.save_draft(&reply).await.unwrap();
        assert!(!has_attachments(&store, &id).await);
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
            .update_flags(&id, FlagsUpdate::read(true))
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
            .update_flags(&MessageId::new("m1"), FlagsUpdate::read(true))
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
    async fn lists_the_folders_below_one_as_the_whole_tree_does() {
        let mut sub = folder("sub", "Sub", None);
        sub.parent = Some(FolderId::new("in"));
        let store = store()
            .with_folder(sub)
            .with_message(message("s1", "sub", "below", at(4)));
        let all = store.list_folders().await.unwrap();
        for parent in [None, Some(FolderId::new("in")), Some(FolderId::new("sub"))] {
            let below: Vec<_> = all.iter().filter(|f| f.parent == parent).cloned().collect();
            assert_eq!(
                store.child_folders(parent.as_ref()).await.unwrap(),
                below,
                "below {parent:?}"
            );
        }
        let message_folder = store.message_folder(&MessageId::new("s1")).await;
        assert_eq!(message_folder.unwrap(), FolderId::new("sub"));
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
    async fn lists_changes_since_a_time() {
        let store = store();
        let inbox = FolderId::new("in");
        let late = message("late", "in", "late", at(-60));
        store.deliver(late, at(50));
        let changed = store
            .changed_messages(&inbox, at(40), Page::first(10))
            .await
            .unwrap();
        assert_eq!(subjects(&changed), ["late"]);
        store
            .update_flags(&MessageId::new("m1"), FlagsUpdate::read(true))
            .await
            .unwrap();
        let changed = store
            .changed_messages(&inbox, at(40), Page::first(10))
            .await
            .unwrap();
        assert_eq!(changed.messages[0].id, MessageId::new("m1"));
    }

    #[tokio::test]
    async fn keeps_attachments_with_their_messages() {
        let store = store().with_attachment("m1", "a.txt", "text/plain", b"hello");
        let list = store.list_attachments(&MessageId::new("m1")).await.unwrap();
        assert_eq!(list[0].name, "a.txt");
        assert_eq!(list[0].size, 5);
        let moved = store
            .move_message(&MessageId::new("m1"), &FolderId::new("arc"))
            .await
            .unwrap();
        let data = store.fetch_attachment(&moved, &list[0].id).await.unwrap();
        assert_eq!(data, b"hello");
        assert!(
            store
                .list_attachments(&MessageId::new("m2"))
                .await
                .unwrap()
                .is_empty()
        );
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
