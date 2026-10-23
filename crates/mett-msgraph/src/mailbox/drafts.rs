use super::*;

/// A composed message, as backends tell its attempts apart: what it answers
/// or recalls, and its [`Draft::attempt`].
pub(super) type DraftKey = (Option<DraftOrigin>, u64);

/// `draft`'s [`DraftKey`].
pub(super) fn key(draft: &Draft) -> DraftKey {
    (draft.origin.clone(), draft.attempt)
}

/// A file mett uploaded (or may have: the answer can be lost).
#[derive(Clone, Debug)]
pub(super) struct Upload {
    /// Its attachment's ID, once Graph said.
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
    use sha2::{Digest, Sha256};
    Sha256::digest(data).into()
}

impl Upload {
    /// Whether `attachment` is this upload. One whose answer was lost is
    /// matched by name and type, among attachments the draft didn't have
    /// before it: Graph's `size` counts more than the file's bytes, so it
    /// can't be compared.
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
pub(super) struct Kept {
    pub(super) id: MessageId,
    /// The attachments the draft had before mett attached any (what a
    /// forward carries along), once known: the others come off before
    /// attaching again, even those whose upload's answer was lost.
    pub(super) base: Option<Vec<String>>,
}

/// A new message whose request (with its attachments in base64) comes to
/// at most this size is sent or saved in one; Graph takes requests of up
/// to 4 MB, and this leaves room for the rest of the request.
const INLINE_REQUEST: usize = 3_500_000;

/// Attachments up to this size are added in one request; larger ones go up
/// in pieces through an upload session, as Graph requires.
const SMALL_ATTACHMENT: usize = 3_000_000;

/// The size of each piece of an upload: a multiple of 320 KiB, as Graph
/// requires.
pub(super) const UPLOAD_CHUNK: usize = 10 * 320 * 1024;

impl<A: Authenticator> GraphMailStore<A> {
    /// The uploads of each attempt.
    fn uploads(&self) -> MutexGuard<'_, HashMap<DraftKey, Vec<Upload>>> {
        self.uploads.lock().expect("lock")
    }

    /// The server drafts kept.
    pub(super) fn kept(&self) -> MutexGuard<'_, HashMap<DraftKey, Kept>> {
        self.kept.lock().expect("lock")
    }
}

impl<A: Authenticator> GraphMailStore<A> {
    /// Creates a server draft with `draft`'s content and attachments, which
    /// replies to or forwards the original if it does, and returns its ID,
    /// with what to keep of it should sending it fail.
    pub(super) async fn server_draft(
        &self,
        draft: &Draft,
    ) -> Result<(MessageId, Option<Vec<String>>)> {
        // A draft kept after an earlier attempt gets the new content (and
        // its attachments again); if it is gone (it was sent after all, or
        // deleted), a new one is made.
        let kept = self.kept().remove(&key(draft));
        if let Some(Kept { id, base }) = kept {
            match self.update_draft(&id, draft).await {
                Ok(()) => return self.attach_all(draft, id, base, false).await,
                Err(Error::NotFound { .. }) => {}
                Err(e) => {
                    self.keep(draft, id, base);
                    return Err(e);
                }
            }
        }
        // A recalled postponed message is the draft: it gets the new
        // content, and keeps its attachments (they are the base). One
        // gone from Drafts was sent or deleted elsewhere; sending it again
        // from a new draft could send it twice.
        if let Some(DraftOrigin::Recalled(id)) = &draft.origin {
            return match self.update_draft(id, draft).await {
                // Those the user took off come off.
                Ok(()) => self.attach_all(draft, id.clone(), None, false).await,
                Err(Error::NotFound { .. }) => Err(Error::Backend(
                    "the postponed message is no longer in Drafts (it was sent or deleted \
                     elsewhere)"
                        .into(),
                )),
                Err(e) => Err(e),
            };
        }
        // A lost response may still have made a draft, which mett can't
        // tell apart from others: say so. Failing before the request is
        // made leaves nothing.
        let (request, about) = match &draft.origin {
            Some(origin) => {
                let (original, action) = match origin {
                    DraftOrigin::Reply(id) => (id, "createReply"),
                    DraftOrigin::Forward(id) => (id, "createForward"),
                    DraftOrigin::Recalled(_) => unreachable!("updated above"),
                };
                let url = self.url(&["me", "messages", original.as_str(), action]);
                (
                    self.request(Method::POST, url),
                    About::message(original.as_str()),
                )
            }
            None => {
                let url = self.url(&["me", "messages"]);
                (
                    self.request(Method::POST, url)
                        .json(&message_json(draft, false)),
                    About::MAILBOX,
                )
            }
        };
        let created: IdOnly =
            self.json_with(request, about, unsent)
                .await
                .map_err(|e| match e {
                    Error::Network(e) => {
                        Error::Network(format!("{e}; a draft of it may be left in Drafts"))
                    }
                    e => e,
                })?;
        let id = MessageId::new(created.id);
        // Replace what Graph filled in (recipients, a quoted HTML body) with
        // what the user wrote.
        let updated = match draft.origin {
            Some(_) => self.update_draft(&id, draft).await,
            None => Ok(()),
        };
        match updated {
            Ok(()) => self.attach_all(draft, id, None, true).await,
            Err(e) => Err(self.give_up(draft, id, None, e).await),
        }
    }

    /// Gives the server draft `id` `draft`'s attachments: what it has
    /// beyond `base` (an earlier attempt's) comes off, then `draft`'s go
    /// on. Without `base`, mett has attached nothing to it yet, and what it
    /// has now is its base (a `fresh` new message has none).
    async fn attach_all(
        &self,
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
            _ => match self.list_attachments(&id).await {
                Ok(present) => present,
                Err(e) => return Err(self.give_up(draft, id, base, e).await),
            },
        };
        // A recalled draft's base is what it has (from this one listing,
        // so that nothing added elsewhere meanwhile comes off), but what
        // is to come off.
        let base = if recalled {
            match self.recalled_base(draft, &present) {
                Ok(recalled) => recalled,
                Err(e) => return Err(self.give_up(draft, id, base, e).await),
            }
        } else {
            base.unwrap_or_else(|| present.iter().map(|a| a.id.to_string()).collect())
        };
        for attachment in present.iter().filter(|a| !base.contains(&a.id.to_string())) {
            if let Err(e) = self.remove_attachment(&id, attachment.id.as_str()).await {
                return Err(self.give_up(draft, id, Some(base), e).await);
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
            match self.attach(&id, file).await {
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
                    return Err(self.give_up(draft, id, Some(base), e).await);
                }
            }
        }
        Ok((id, Some(base)))
    }

    /// Gives up on the server draft `id` after `e`: kept for the next
    /// attempt if `e` leaves its state unknown (or it can't be deleted),
    /// deleted otherwise.
    async fn give_up(
        &self,
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
                if !self.discard(draft, &id).await {
                    self.keep(draft, id, base);
                }
            }
        }
        e
    }

    /// Attaches `file` to the server draft `message`.
    /// Returns the attachment's ID, if Graph said.
    async fn attach(
        &self,
        message: &MessageId,
        file: &OutgoingAttachment,
    ) -> Result<Option<String>> {
        if file.data.len() < SMALL_ATTACHMENT {
            let url = self.url(&["me", "messages", message.as_str(), "attachments"]);
            let response = self
                .send(
                    self.request(Method::POST, url)
                        .json(&file_attachment_json(file)),
                    About::message(message.as_str()),
                )
                .await?;
            return Ok(response
                .json::<IdOnly>()
                .await
                .ok()
                .map(|created| created.id));
        }
        let url = self.url(&[
            "me",
            "messages",
            message.as_str(),
            "attachments",
            "createUploadSession",
        ]);
        let body = json!({
            "AttachmentItem": {
                "attachmentType": "file",
                "name": file.name,
                "size": file.data.len(),
                "contentType": file.content_type,
            }
        });
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Session {
            upload_url: String,
        }
        let session: Session = self
            .json(
                self.request(Method::POST, url).json(&body),
                About::message(message.as_str()),
            )
            .await?;
        // The upload URL carries its own authorisation: the access token
        // never goes there.
        let upload = Url::parse(&session.upload_url)
            .map_err(|e| Error::Backend(format!("bad upload URL from Graph: {e}")))?;
        // HTTPS, unless it is the configured endpoint's own (a local proxy).
        if upload.scheme() != "https" && upload.origin() != self.base.origin() {
            return Err(Error::Backend(
                "Graph offered an upload URL without HTTPS".into(),
            ));
        }
        let total = file.data.len();
        let mut id = None;
        for (i, chunk) in file.data.chunks(UPLOAD_CHUNK).enumerate() {
            let start = i * UPLOAD_CHUNK;
            let end = start + chunk.len() - 1;
            let response = self
                .http
                .put(upload.clone())
                .timeout(upload_timeout(chunk.len()))
                .header("Content-Range", format!("bytes {start}-{end}/{total}"))
                // Graph requires it of every piece.
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", chunk.len())
                .body(chunk.to_vec())
                .send()
                .await
                .map_err(|e| Error::Network(e.to_string()))?;
            let response = Self::checked(response, About::message(message.as_str())).await?;
            // The last answer says where the attachment is:
            // `.../Attachments('ID')`.
            id = response
                .headers()
                .get("Location")
                .and_then(|location| location.to_str().ok())
                .and_then(|location| location.rsplit_once("Attachments('"))
                .and_then(|(_, rest)| rest.split_once("')"))
                .map(|(id, _)| percent_decoded(id));
        }
        Ok(id)
    }

    /// Takes an attachment off a server draft. One already gone is as good
    /// as taken off.
    async fn remove_attachment(&self, message: &MessageId, attachment: &str) -> Result<()> {
        let url = self.url(&[
            "me",
            "messages",
            message.as_str(),
            "attachments",
            attachment,
        ]);
        match self
            .send(
                self.request(Method::DELETE, url),
                About::message(message.as_str()),
            )
            .await
        {
            Ok(_) | Err(Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The base for [`Self::attach_all`] of the recalled `draft`, which
    /// has the attachments `present`: all of them, but those of its own the user took off and
    /// mett's uploads of earlier attempts, which come off. Others, even
    /// ones added elsewhere since, stay.
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

    /// Forgets the uploads of `draft`, which is done with: sent, postponed,
    /// or given up.
    pub(super) fn forget_uploads(&self, draft: &Draft) {
        self.uploads().remove(&key(draft));
    }

    /// Gives a server draft `draft`'s content.
    async fn update_draft(&self, id: &MessageId, draft: &Draft) -> Result<()> {
        let url = self.url(&["me", "messages", id.as_str()]);
        self.send(
            self.request(Method::PATCH, url)
                .json(&message_json(draft, false)),
            About::message(id.as_str()),
        )
        .await
        .map(drop)
    }

    /// Whether a server draft is kept for `draft`.
    pub(super) fn has_kept(&self, draft: &Draft) -> bool {
        self.kept().contains_key(&key(draft))
    }

    /// Remembers a server draft kept after an attempt whose outcome is
    /// unknown, with the attachments it added.
    pub(super) fn keep(&self, draft: &Draft, id: MessageId, base: Option<Vec<String>>) {
        self.kept().insert(key(draft), Kept { id, base });
    }

    /// Deletes a server draft of `draft` that could not be completed, so
    /// it doesn't linger in Drafts, and returns whether it is gone. Failing
    /// to is not worth reporting over the error that caused it. A recalled
    /// postponed message is never deleted: it is the user's.
    pub(super) async fn discard(&self, draft: &Draft, id: &MessageId) -> bool {
        !is_recalled(draft, id) && self.delete_draft(id).await.is_ok()
    }

    /// Deletes a server draft for good where Graph can (DELETE would only
    /// move it to Deleted Items); elsewhere it goes to Deleted Items. One
    /// that is gone already is as good as deleted.
    pub(super) async fn delete_draft(&self, id: &MessageId) -> Result<()> {
        let request = if self.cloud == Cloud::Global {
            let url = self.url(&["me", "messages", id.as_str(), "permanentDelete"]);
            self.request(Method::POST, url)
        } else {
            self.request(Method::DELETE, self.url(&["me", "messages", id.as_str()]))
        };
        match self.send(request, About::message(id.as_str())).await {
            Ok(_) | Err(Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// `text` with its `%XX` escapes decoded (as a `Location` header may have
/// them in an ID).
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| char::from(b).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).copied().and_then(hex),
            bytes.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(high), Some(low)) => {
                out.push(u8::try_from(high * 16 + low).expect("two hex digits"));
                i += 3;
            }
            (b, ..) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether `id` is the postponed message `draft` recalls.
pub(super) fn is_recalled(draft: &Draft, id: &MessageId) -> bool {
    matches!(&draft.origin, Some(DraftOrigin::Recalled(recalled)) if recalled == id)
}

/// A network failure before a message was handed over: it wasn't sent.
pub(super) fn unsent(e: Error) -> Error {
    match e {
        Error::Network(e) => Error::NotSent(format!("network error: {e}")),
        e => e,
    }
}

/// Whether `draft` goes through a server draft: it answers a message, or
/// its attachments are too large to go with it in one request. Such a
/// message, once sent, is always kept in Sent Items, and a failed attempt
/// may leave its draft behind.
pub fn needs_server_draft(draft: &Draft) -> bool {
    if draft.origin.is_some() {
        return true;
    }
    if draft.attachments.is_empty() {
        return false;
    }
    // Base64 takes 4 bytes for every 3: too large already, without
    // encoding it all to find out.
    let encoded: usize = draft
        .attachments
        .iter()
        .map(|a| a.data.len().div_ceil(3) * 4)
        .sum();
    if encoded + draft.body.len() > INLINE_REQUEST {
        return true;
    }
    // Otherwise, the request as sent: escaping may make the text much
    // larger.
    send_mail_size(draft).map_or(true, |size| size > INLINE_REQUEST)
}
