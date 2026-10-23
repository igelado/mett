//! The state machine behind sending and postponing through a server draft.
//!
//! Backends that make a message in the server's Drafts folder, put its
//! attachments on it one by one, and then send (or leave) it, all face the
//! same problem: any request may fail with its answer lost, leaving it
//! unknown whether the change was made. [`DraftSession`] holds what was
//! learned about each composed message across attempts, and the algorithms
//! that act on it; a [`DraftBackend`] supplies the few requests that depend
//! on the transport (Graph's REST, EWS's SOAP).

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Mutex, MutexGuard};

use sha2::{Digest, Sha256};

use crate::attachment::{Attachment, OutgoingAttachment};
use crate::draft::{Draft, DraftOrigin};
use crate::error::{Error, Result};
use crate::ids::MessageId;

/// The requests a mailbox's transport makes for a [`DraftSession`].
///
/// An `Err(Error::Network(_))` means the answer was lost: the change may
/// have been made. Any other error means the server refused, and nothing was
/// changed.
pub trait DraftBackend: Send + Sync {
    /// Creates a server draft with `draft`'s content (but not its
    /// attachments), which replies to or forwards the original if `draft`
    /// does, and returns its ID. A reply's or forward's content is only
    /// what the server filled in: [`Self::update_draft`] follows.
    ///
    /// A lost answer may still have made a draft, which mett can't tell
    /// apart from others: the error says so (see [`may_be_left`]). Failing
    /// before the request is made leaves nothing.
    fn create_draft(&self, draft: &Draft) -> impl Future<Output = Result<MessageId>> + Send;

    /// Gives the server draft `id` `draft`'s content. [`Error::NotFound`]
    /// says it is gone.
    fn update_draft(
        &self,
        id: &MessageId,
        draft: &Draft,
    ) -> impl Future<Output = Result<()>> + Send;

    /// The attachments the server draft `id` has.
    fn list_attachments(
        &self,
        id: &MessageId,
    ) -> impl Future<Output = Result<Vec<Attachment>>> + Send;

    /// Attaches `file` to the server draft `id`, and returns the
    /// attachment's ID, if the server said.
    fn attach(
        &self,
        id: &MessageId,
        file: &OutgoingAttachment,
    ) -> impl Future<Output = Result<Option<String>>> + Send;

    /// Takes `attachment` off the server draft `id`. One already gone is as
    /// good as taken off. (Some servers need only the attachment's ID.)
    fn remove_attachment(
        &self,
        id: &MessageId,
        attachment: &str,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Deletes the server draft `id` for good. One that is gone already is
    /// as good as deleted.
    fn delete_draft(&self, id: &MessageId) -> impl Future<Output = Result<()>> + Send;
}

/// What a composed message answers or recalls, and its [`Draft::attempt`]:
/// how the attempts to send or postpone one message are told apart.
type DraftKey = (Option<DraftOrigin>, u64);

/// `draft`'s [`DraftKey`].
fn key(draft: &Draft) -> DraftKey {
    (draft.origin.clone(), draft.attempt)
}

/// A file mett uploaded (or may have: the answer can be lost).
#[derive(Clone, Debug)]
struct Upload {
    /// Its attachment's ID, once the server said.
    id: Option<String>,
    /// Without an ID, it is known by its name and type.
    name: String,
    content_type: String,
    /// What it held, to tell a file put in its place since.
    digest: [u8; 32],
    /// The attachments the draft had before it was made: one whose answer
    /// was lost may be any other of its name and type, however many
    /// attempts later.
    prior: Vec<String>,
}

/// A file's contents, to tell two apart.
fn digest(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

impl Upload {
    /// Whether `attachment` is this upload. One whose answer was lost is
    /// matched by name and type, among attachments the draft didn't have
    /// before it. (The size can't be compared: a server may count more than
    /// the file's bytes.)
    fn is(&self, attachment: &Attachment) -> bool {
        match &self.id {
            Some(uploaded) => *uploaded == attachment.id.as_str(),
            None => {
                !self.prior.contains(&attachment.id.to_string())
                    && self.name == attachment.name
                    && self
                        .content_type
                        .eq_ignore_ascii_case(&attachment.content_type)
            }
        }
    }
}

/// A server draft kept after an attempt whose outcome is unknown.
#[derive(Clone, Debug)]
struct Kept {
    id: MessageId,
    /// The attachments the draft had before mett attached any (what a
    /// forward carries along), once known: the others come off before
    /// attaching again, even those whose upload's answer was lost.
    base: Option<Vec<String>>,
}

/// What mett knows of the server drafts it made or recalled, and the
/// algorithms that act on it. A mailbox adapter holds one, and passes its
/// [`DraftBackend`] to each call.
#[derive(Debug, Default)]
pub struct DraftSession {
    /// Server drafts kept after a send or update whose outcome is unknown,
    /// by what they answer and the composed message they hold
    /// ([`Draft::attempt`]): sending or postponing that message again
    /// updates the draft rather than making another.
    kept: Mutex<HashMap<DraftKey, Kept>>,
    /// The files mett uploaded to recalled drafts, by the same key, until
    /// they are known to be gone or the message is done with: a retry
    /// takes them off (to put on again what is still to go), and leaves
    /// alone what was added elsewhere.
    uploads: Mutex<HashMap<DraftKey, Vec<Upload>>>,
}

impl DraftSession {
    /// A session that knows of no drafts.
    pub fn new() -> Self {
        Self::default()
    }

    /// The uploads of each attempt.
    fn uploads(&self) -> MutexGuard<'_, HashMap<DraftKey, Vec<Upload>>> {
        self.uploads.lock().expect("lock")
    }

    /// The server drafts kept.
    fn kept(&self) -> MutexGuard<'_, HashMap<DraftKey, Kept>> {
        self.kept.lock().expect("lock")
    }

    /// Whether a server draft is kept for `draft`.
    pub fn has_kept(&self, draft: &Draft) -> bool {
        self.kept().contains_key(&key(draft))
    }

    /// Remembers a server draft kept after an attempt whose outcome is
    /// unknown, with the attachments it added (`base`, as
    /// [`Self::server_draft`] returned it).
    pub fn keep(&self, draft: &Draft, id: MessageId, base: Option<Vec<String>>) {
        self.kept().insert(key(draft), Kept { id, base });
    }

    /// Forgets the uploads of `draft`, which is done with: sent, postponed,
    /// or given up.
    pub fn forget_uploads(&self, draft: &Draft) {
        self.uploads().remove(&key(draft));
    }

    /// Deletes a server draft of `draft` that could not be completed, so it
    /// doesn't linger in Drafts, and returns whether it is gone. Failing to
    /// is not worth reporting over the error that caused it. A recalled
    /// postponed message is never deleted: it is the user's.
    pub async fn discard<B: DraftBackend>(
        &self,
        backend: &B,
        draft: &Draft,
        id: &MessageId,
    ) -> bool {
        !is_recalled(draft, id) && backend.delete_draft(id).await.is_ok()
    }

    /// Deletes the server draft kept for `draft`, if any. It is kept still
    /// if that fails.
    pub async fn discard_kept<B: DraftBackend>(&self, backend: &B, draft: &Draft) -> Result<()> {
        let Some(Kept { id, base }) = self.kept().remove(&key(draft)) else {
            return Ok(());
        };
        // A recalled postponed message stays in Drafts (with what the
        // attempts put on it).
        if is_recalled(draft, &id) {
            self.forget_uploads(draft);
            return Ok(());
        }
        let deleted = backend.delete_draft(&id).await;
        if deleted.is_err() {
            self.keep(draft, id, base);
        }
        deleted
    }

    /// Settles the server draft `id` after the request to send it, whose
    /// outcome was `sent`, and returns `sent` (a lost answer says the
    /// message may have gone out). `base` is what [`Self::server_draft`]
    /// returned with `id`.
    pub async fn after_send<B: DraftBackend, T>(
        &self,
        backend: &B,
        draft: &Draft,
        id: MessageId,
        base: Option<Vec<String>>,
        sent: Result<T>,
    ) -> Result<T> {
        match &sent {
            // The response was lost or the server failed: the message may
            // have gone out anyway. Keep the draft, so nothing is lost if
            // it didn't, and say so; sending again reuses it.
            Err(Error::Network(e)) => {
                let e = Error::Network(format!("{e}; if it wasn't sent, it is in Drafts"));
                self.keep(draft, id, base);
                return Err(e);
            }
            // The server refused it: it wasn't sent. A draft that can't be
            // deleted either is kept, for the next attempt.
            Err(_) => {
                if !self.discard(backend, draft, &id).await {
                    self.keep(draft, id, base);
                }
            }
            Ok(_) => self.forget_uploads(draft),
        }
        sent
    }

    /// Makes a server draft with `draft`'s content and attachments, which
    /// replies to or forwards the original if it does (or reuses what an
    /// earlier attempt left), and returns its ID, with what to keep of it
    /// should sending it fail (pass both to [`Self::after_send`]).
    pub async fn server_draft<B: DraftBackend>(
        &self,
        backend: &B,
        draft: &Draft,
    ) -> Result<(MessageId, Option<Vec<String>>)> {
        // A draft kept after an earlier attempt gets the new content (and
        // its attachments again); if it is gone (it was sent after all, or
        // deleted), a new one is made.
        let kept = self.kept().remove(&key(draft));
        if let Some(Kept { id, base }) = kept {
            match backend.update_draft(&id, draft).await {
                Ok(()) => return self.attach_all(backend, draft, id, base, false).await,
                Err(Error::NotFound { .. }) => {}
                Err(e) => {
                    self.keep(draft, id, base);
                    return Err(e);
                }
            }
        }
        // A recalled postponed message is the draft: it gets the new
        // content, and keeps its attachments (they are the base). One gone
        // from Drafts was sent or deleted elsewhere; sending it again from
        // a new draft could send it twice.
        if let Some(DraftOrigin::Recalled(id)) = &draft.origin {
            return match backend.update_draft(id, draft).await {
                // Those the user took off come off.
                Ok(()) => {
                    self.attach_all(backend, draft, id.clone(), None, false)
                        .await
                }
                Err(Error::NotFound { .. }) => Err(Error::Backend(
                    "the postponed message is no longer in Drafts (it was sent or deleted \
                     elsewhere)"
                        .into(),
                )),
                Err(e) => Err(e),
            };
        }
        let id = backend.create_draft(draft).await?;
        // Replace what the server filled in (recipients, a quoted body) with
        // what the user wrote.
        let updated = match draft.origin {
            Some(_) => backend.update_draft(&id, draft).await,
            None => Ok(()),
        };
        match updated {
            Ok(()) => self.attach_all(backend, draft, id, None, true).await,
            Err(e) => Err(self.give_up(backend, draft, id, None, e).await),
        }
    }

    /// Gives the server draft `id` `draft`'s attachments: what it has
    /// beyond `base` (an earlier attempt's) comes off, then `draft`'s go
    /// on. Without `base`, mett has attached nothing to it yet, and what it
    /// has now is its base (a `fresh` new message has none).
    async fn attach_all<B: DraftBackend>(
        &self,
        backend: &B,
        draft: &Draft,
        id: MessageId,
        base: Option<Vec<String>>,
        fresh: bool,
    ) -> Result<(MessageId, Option<Vec<String>>)> {
        let recalled = is_recalled(draft, &id);
        let key = key(draft);
        // A recalled draft has what the user took off, and mett's uploads
        // of earlier attempts, to take off.
        let to_take_off = recalled
            && (!draft.drop_attachments.is_empty()
                || self
                    .uploads()
                    .get(&key)
                    .is_some_and(|uploads| !uploads.is_empty()));
        // Nothing of mett's there, and nothing to put on: done. (What an
        // earlier attempt attached comes off even if nothing is to go on
        // now.)
        if base.is_none() && draft.attachments.is_empty() && !to_take_off {
            return Ok((id, base));
        }
        let present = match &base {
            None if fresh && draft.origin.is_none() => Vec::new(),
            _ => match backend.list_attachments(&id).await {
                Ok(present) => present,
                Err(e) => return Err(self.give_up(backend, draft, id, base, e).await),
            },
        };
        // A recalled draft's base is what it has (from this one listing,
        // so that nothing added elsewhere meanwhile comes off), but what
        // is to come off.
        let base = if recalled {
            match self.recalled_base(draft, &present) {
                Ok(recalled) => recalled,
                Err(e) => return Err(self.give_up(backend, draft, id, base, e).await),
            }
        } else {
            base.unwrap_or_else(|| present.iter().map(|a| a.id.to_string()).collect())
        };
        for attachment in present.iter().filter(|a| !base.contains(&a.id.to_string())) {
            if let Err(e) = backend.remove_attachment(&id, attachment.id.as_str()).await {
                return Err(self.give_up(backend, draft, id, Some(base), e).await);
            }
            // Gone: no more an upload of mett's to take off (one known by
            // its ID: an upload whose answer was lost is never matched by
            // name here, so it still stops a send it may be part of).
            if recalled
                && let Some(uploads) = self.uploads().get_mut(&key)
                && let Some(at) = uploads
                    .iter()
                    .position(|u| u.id.as_deref() == Some(attachment.id.as_str()))
            {
                uploads.remove(at);
            }
        }
        for file in &draft.attachments {
            // Remembered before it is sent: the answer may be lost.
            if recalled {
                self.uploads().entry(key.clone()).or_default().push(Upload {
                    id: None,
                    name: file.name.clone(),
                    content_type: file.content_type.clone(),
                    digest: digest(&file.data),
                    prior: present.iter().map(|a| a.id.to_string()).collect(),
                });
            }
            match backend.attach(&id, file).await {
                Ok(uploaded) => {
                    if recalled
                        && let Some(upload) = self
                            .uploads()
                            .get_mut(&key)
                            .and_then(|uploads| uploads.last_mut())
                    {
                        upload.id = uploaded;
                    }
                }
                Err(e) => {
                    // Refused: nothing was attached. Only a lost answer
                    // leaves it unknown.
                    if recalled
                        && !matches!(e, Error::Network(_))
                        && let Some(uploads) = self.uploads().get_mut(&key)
                    {
                        uploads.pop();
                    }
                    return Err(self.give_up(backend, draft, id, Some(base), e).await);
                }
            }
        }
        Ok((id, Some(base)))
    }

    /// Gives up on the server draft `id` after `e`: kept for the next
    /// attempt if `e` leaves its state unknown (or it can't be deleted),
    /// deleted otherwise. Returns `e`.
    pub async fn give_up<B: DraftBackend>(
        &self,
        backend: &B,
        draft: &Draft,
        id: MessageId,
        base: Option<Vec<String>>,
        e: Error,
    ) -> Error {
        match e {
            // The change may have been made: keep the draft, for the next
            // attempt to update.
            Error::Network(_) => self.keep(draft, id, base),
            // One that can't be deleted (access was denied, say) is kept
            // for the next attempt, rather than left behind.
            _ => {
                if !self.discard(backend, draft, &id).await {
                    self.keep(draft, id, base);
                }
            }
        }
        e
    }

    /// The base for [`Self::attach_all`] of the recalled `draft`, which
    /// has the attachments `present`: all of them, but those of its own the
    /// user took off and mett's uploads of earlier attempts, which come
    /// off. Others, even ones added elsewhere since, stay.
    ///
    /// An upload whose answer was lost has no known ID, and an attachment
    /// of its name and type may be it, or one added elsewhere (it may never
    /// have been made): it stays, as mett can't tell. If the file is still
    /// to go, it is attached again (it may then go twice). If the user took
    /// it off (or put another file in its place), sending it could send
    /// what they didn't want: that fails, for them to look in Outlook.
    fn recalled_base(&self, draft: &Draft, present: &[Attachment]) -> Result<Vec<String>> {
        let uploads = self.uploads().get(&key(draft)).cloned().unwrap_or_default();
        let known = |a: &Attachment| {
            uploads
                .iter()
                .any(|u| u.id.as_deref() == Some(a.id.as_str()))
        };
        // Each file's digest, worked out once, if any upload was lost.
        let digests: Vec<[u8; 32]> = if uploads.iter().any(|u| u.id.is_none()) {
            draft.attachments.iter().map(|f| digest(&f.data)).collect()
        } else {
            Vec::new()
        };
        for upload in uploads.iter().filter(|u| u.id.is_none()) {
            // Taken off, or put in its place by a file of other contents
            // (or fewer of the same than lost uploads): the one uploaded
            // may not be to go.
            let same = |name: &str, content_type: &str, digest: &[u8; 32]| {
                name == upload.name
                    && content_type == upload.content_type
                    && *digest == upload.digest
            };
            let wanted = draft
                .attachments
                .iter()
                .zip(&digests)
                .filter(|(f, digest)| same(&f.name, &f.content_type, digest))
                .count();
            // (A copy may then go twice, as with any lost upload; more of
            // them lost than are wanted can't be told apart.)
            let lost = uploads
                .iter()
                .filter(|u| u.id.is_none() && same(&u.name, &u.content_type, &u.digest))
                .count();
            let still_wanted = wanted >= lost;
            let unsure = present.iter().any(|a| !known(a) && upload.is(a));
            if !still_wanted && unsure {
                return Err(Error::Backend(format!(
                    "can't tell whether {} (taken off) is still on the message: \
                     check it in Drafts in Outlook",
                    upload.name
                )));
            }
        }
        Ok(present
            .iter()
            .filter(|a| !draft.drop_attachments.contains(&a.id) && !known(a))
            .map(|a| a.id.to_string())
            .collect())
    }
}

/// Whether `id` is the postponed message `draft` recalls.
pub fn is_recalled(draft: &Draft, id: &MessageId) -> bool {
    matches!(&draft.origin, Some(DraftOrigin::Recalled(recalled)) if recalled == id)
}

/// A network failure before a message was handed over: it wasn't sent.
pub fn unsent(e: Error) -> Error {
    match e {
        Error::Network(e) => Error::NotSent(format!("network error: {e}")),
        e => e,
    }
}

/// `e` as [`DraftBackend::create_draft`] reports it: a lost answer may
/// have made a draft anyway.
pub fn may_be_left(e: Error) -> Error {
    match e {
        Error::Network(e) => Error::Network(format!("{e}; a draft of it may be left in Drafts")),
        e => e,
    }
}

#[cfg(test)]
mod tests;
