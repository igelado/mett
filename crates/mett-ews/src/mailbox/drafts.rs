use mett_core::{DraftBackend, may_be_left};

use super::*;

impl<A: Authenticator + 'static> DraftBackend for EwsMailStore<A> {
    async fn create_draft(&self, draft: &Draft) -> Result<MessageId> {
        let (item, about) = match &draft.origin {
            Some(origin) => {
                let (original, element) = match origin {
                    DraftOrigin::Reply(id) => (id, "ReplyToItem"),
                    DraftOrigin::Forward(id) => (id, "ForwardItem"),
                    DraftOrigin::Recalled(_) => unreachable!("a recalled draft is updated"),
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
        match self.call_with(&body, about, unsent, Duration::ZERO).await {
            Ok(xml) => created_id(&xml),
            Err(e) => Err(may_be_left(e)),
        }
    }

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

    async fn list_attachments(&self, id: &MessageId) -> Result<Vec<Attachment>> {
        MailStore::list_attachments(self, id).await
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

    /// Takes an attachment off a server draft (EWS names it by its ID
    /// alone). One already gone is as good as taken off.
    async fn remove_attachment(&self, _message: &MessageId, attachment: &str) -> Result<()> {
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

    /// Deletes a server draft for good. One that is gone already is as
    /// good as deleted.
    async fn delete_draft(&self, id: &MessageId) -> Result<()> {
        match self.delete_item(id).await {
            Ok(()) | Err(Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl<A: Authenticator + 'static> EwsMailStore<A> {
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
