use super::*;

/// A composed message, as backends tell its attempts apart: what it answers
/// or recalls, and its [`Draft::attempt`].
pub(super) type DraftKey = (Option<DraftOrigin>, u64);

/// `draft`'s [`DraftKey`].
pub(super) fn key(draft: &Draft) -> DraftKey {
    (draft.origin.clone(), draft.attempt)
}

/// A file mett attached (or may have: the answer can be lost).
#[derive(Clone, Debug)]
pub(super) struct Upload {
    /// Its attachment's ID, once EWS said.
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
    /// before it.
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
    /// attaching again, even those whose answer was lost.
    pub(super) base: Option<Vec<String>>,
}

impl<A: Authenticator> EwsMailStore<A> {
    /// The uploads of each attempt.
    fn uploads(&self) -> MutexGuard<'_, HashMap<DraftKey, Vec<Upload>>> {
        self.uploads.lock().expect("lock")
    }

    /// The server drafts kept.
    pub(super) fn kept(&self) -> MutexGuard<'_, HashMap<DraftKey, Kept>> {
        self.kept.lock().expect("lock")
    }
}

impl<A: Authenticator + 'static> EwsMailStore<A> {
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
        // content, and keeps its attachments (they are the base). One gone
        // from Drafts was sent or deleted elsewhere; sending it again from
        // a new draft could send it twice.
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
        let (item, about) = match &draft.origin {
            Some(origin) => {
                let (original, element) = match origin {
                    DraftOrigin::Reply(id) => (id, "ReplyToItem"),
                    DraftOrigin::Forward(id) => (id, "ForwardItem"),
                    DraftOrigin::Recalled(_) => unreachable!("updated above"),
                };
                // Exchange Online answers to a version of the original:
                // without its change key, it refuses
                // (`ErrorChangeKeyRequiredForWriteOperations`).
                let change_key = self.change_key(original).await?;
                (
                    format!(
                        "<t:{element}><t:ReferenceItemId Id=\"{}\" ChangeKey=\"{}\"/>\
                         </t:{element}>",
                        escape(original.as_str()),
                        escape(&change_key)
                    ),
                    About::message(original.as_str()),
                )
            }
            None => (message_xml(draft), About::MAILBOX),
        };
        let body = create_item("SaveOnly", Some("drafts"), &item);
        let created = match self.call_with(&body, about, unsent, Duration::ZERO).await {
            Ok(xml) => created_id(&xml),
            Err(Error::Network(e)) => Err(Error::Network(format!(
                "{e}; a draft of it may be left in Drafts"
            ))),
            Err(e) => Err(e),
        }?;
        // Replace what Exchange filled in (recipients, the quoted original)
        // with what the user wrote.
        let updated = match draft.origin {
            Some(_) => self.update_draft(&created, draft).await,
            None => Ok(()),
        };
        match updated {
            Ok(()) => self.attach_all(draft, created, None, true).await,
            Err(e) => Err(self.give_up(draft, created, None, e).await),
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
        // Nothing of mett's there, and nothing to put on: done.
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
            if let Err(e) = self.remove_attachment(attachment.id.as_str()).await {
                return Err(self.give_up(draft, id, Some(base), e).await);
            }
            // Gone: no more an upload of mett's to take off.
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

    /// The base for [`Self::attach_all`] of the recalled `draft`, which has
    /// the attachments `present`: all of them, but those of its own the
    /// user took off and mett's uploads of earlier attempts, which come
    /// off. Others, even ones added elsewhere since, stay.
    ///
    /// An upload whose answer was lost has no known ID, and an attachment
    /// of its name and type may be it, or one added elsewhere: it stays, as
    /// mett can't tell. If the file is still to go, it is attached again
    /// (it may then go twice). If the user took it off (or put another file
    /// in its place), sending it could send what they didn't want: that
    /// fails, for them to look in Outlook.
    fn recalled_base(&self, draft: &Draft, present: &[Attachment]) -> Result<Vec<String>> {
        let uploads = self.uploads().get(&key(draft)).cloned().unwrap_or_default();
        let known = |a: &Attachment| {
            uploads
                .iter()
                .any(|u| u.id.as_deref() == Some(a.id.as_str()))
        };
        let digests: Vec<[u8; 32]> = if uploads.iter().any(|u| u.id.is_none()) {
            draft.attachments.iter().map(|f| digest(&f.data)).collect()
        } else {
            Vec::new()
        };
        for upload in uploads.iter().filter(|u| u.id.is_none()) {
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

    /// Gives up on the server draft `id` after `e`: kept for the next
    /// attempt if `e` leaves its state unknown (or it can't be deleted),
    /// deleted otherwise.
    pub(super) async fn give_up(
        &self,
        draft: &Draft,
        id: MessageId,
        base: Option<Vec<String>>,
        e: Error,
    ) -> Error {
        match e {
            Error::Network(_) => self.keep(draft, id, base),
            _ => {
                if !self.discard(draft, &id).await {
                    self.keep(draft, id, base);
                }
            }
        }
        e
    }

    /// Attaches `file` to the server draft `message`. Returns the
    /// attachment's ID, if EWS said.
    async fn attach(
        &self,
        message: &MessageId,
        file: &OutgoingAttachment,
    ) -> Result<Option<String>> {
        let about = About::message(message.as_str());
        let body = format!(
            "<m:CreateAttachment><m:ParentItemId Id=\"{}\"/><m:Attachments>{}</m:Attachments>\
             </m:CreateAttachment>",
            escape(message.as_str()),
            file_attachment_xml(file)
        );
        let xml = self.call(&body, about).await?;
        let doc = parse(&xml)?;
        let response = only_message(&doc, about)?;
        Ok(descendant(response, "AttachmentId")
            .and_then(|a| a.attribute("Id"))
            .map(str::to_owned))
    }

    /// Takes an attachment off a server draft. One already gone is as good
    /// as taken off.
    async fn remove_attachment(&self, attachment: &str) -> Result<()> {
        let about = About::attachment(attachment);
        let body = format!(
            "<m:DeleteAttachment><m:AttachmentIds><t:AttachmentId Id=\"{}\"/></m:AttachmentIds>\
             </m:DeleteAttachment>",
            escape(attachment)
        );
        let outcome = match self.call(&body, about).await {
            Ok(xml) => only_message(&parse(&xml)?, about).map(drop),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) | Err(Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Changes item `id` with `updates` (`SetItemField`s and
    /// `DeleteItemField`s).
    pub(super) async fn update_item(&self, id: &MessageId, updates: &str) -> Result<()> {
        let about = About::message(id.as_str());
        let body = format!(
            "<m:UpdateItem MessageDisposition=\"SaveOnly\" ConflictResolution=\"AlwaysOverwrite\" \
             SuppressReadReceipts=\"true\"><m:ItemChanges><t:ItemChange>{}\
             <t:Updates>{updates}</t:Updates></t:ItemChange></m:ItemChanges></m:UpdateItem>",
            item_id_xml(id.as_str())
        );
        let xml = self.call(&body, about).await?;
        only_message(&parse(&xml)?, about).map(drop)
    }

    /// Gives a server draft `draft`'s content.
    async fn update_draft(&self, id: &MessageId, draft: &Draft) -> Result<()> {
        let mut updates = set_field(
            "item:Subject",
            &format!("<t:Subject>{}</t:Subject>", escape(&draft.subject)),
        );
        updates.push_str(&set_field(
            "item:Body",
            &format!("<t:Body BodyType=\"Text\">{}</t:Body>", escape(&draft.body)),
        ));
        for (uri, element, list) in [
            ("message:ToRecipients", "ToRecipients", &draft.to),
            ("message:CcRecipients", "CcRecipients", &draft.cc),
            ("message:BccRecipients", "BccRecipients", &draft.bcc),
        ] {
            if list.is_empty() {
                updates.push_str(&format!(
                    "<t:DeleteItemField>{}</t:DeleteItemField>",
                    field(uri)
                ));
            } else {
                updates.push_str(&set_field(
                    uri,
                    &format!("<t:{element}>{}</t:{element}>", mailboxes(list)),
                ));
            }
        }
        if let Some(from) = &draft.from {
            updates.push_str(&set_field(
                "message:From",
                &format!("<t:From>{}</t:From>", mailboxes(std::slice::from_ref(from))),
            ));
        }
        self.update_item(id, &updates).await
    }

    /// The current change key of item `id`, which sending it, and
    /// replying to or forwarding it, take.
    pub(super) async fn change_key(&self, id: &MessageId) -> Result<String> {
        let about = About::message(id.as_str());
        let xml = self.get_items(&[id.to_string()], &[], false, about).await?;
        let doc = parse(&xml)?;
        let message = only_message(&doc, about)?;
        first_item(message)
            .and_then(|item| child(item, "ItemId"))
            .and_then(|item| item.attribute("ChangeKey"))
            .map(str::to_owned)
            .ok_or_else(|| unexpected("GetItem", "no ChangeKey"))
    }

    /// Sends the server draft `id`, as of `change_key`, keeping a copy in
    /// Sent Items if `save_copy`.
    pub(super) async fn send_draft(
        &self,
        id: &MessageId,
        change_key: &str,
        save_copy: bool,
    ) -> Result<()> {
        let about = About::message(id.as_str());
        let saved = if save_copy {
            "<m:SavedItemFolderId><t:DistinguishedFolderId Id=\"sentitems\"/></m:SavedItemFolderId>"
        } else {
            ""
        };
        let body = format!(
            "<m:SendItem SaveItemToFolder=\"{save_copy}\"><m:ItemIds>\
             <t:ItemId Id=\"{}\" ChangeKey=\"{}\"/></m:ItemIds>{saved}</m:SendItem>",
            escape(id.as_str()),
            escape(change_key)
        );
        let xml = self.hand_over(&body, about).await?;
        only_message(&parse(&xml)?, about).map(drop)
    }

    /// Forgets the uploads of `draft`, which is done with: sent, postponed,
    /// or given up.
    pub(super) fn forget_uploads(&self, draft: &Draft) {
        self.uploads().remove(&key(draft));
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

    /// Deletes a server draft of `draft` that could not be completed, so it
    /// doesn't linger in Drafts, and returns whether it is gone. A recalled
    /// postponed message is never deleted: it is the user's.
    pub(super) async fn discard(&self, draft: &Draft, id: &MessageId) -> bool {
        !is_recalled(draft, id) && self.delete_draft(id).await.is_ok()
    }

    /// Deletes a server draft for good. One that is gone already is as
    /// good as deleted.
    pub(super) async fn delete_draft(&self, id: &MessageId) -> Result<()> {
        match self.delete_item(id).await {
            Ok(()) | Err(Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
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

/// Whether `draft` goes through a server draft: it answers a message, has
/// attachments, which Exchange takes only on a saved message, or is sent
/// from a given address without a copy, which Exchange doesn't send in one
/// request for a delegate. A failed attempt to send such a message may leave
/// its draft behind.
pub fn needs_server_draft(draft: &Draft) -> bool {
    draft.origin.is_some()
        || !draft.attachments.is_empty()
        || (draft.from.is_some() && !draft.save_copy)
}
