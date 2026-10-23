use mett_core::{DraftBackend, may_be_left};

use super::*;

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

impl<A: Authenticator> DraftBackend for GraphMailStore<A> {
    async fn create_draft(&self, draft: &Draft) -> Result<MessageId> {
        let (request, about) = match &draft.origin {
            Some(origin) => {
                let (original, action) = match origin {
                    DraftOrigin::Reply(id) => (id, "createReply"),
                    DraftOrigin::Forward(id) => (id, "createForward"),
                    DraftOrigin::Recalled(_) => unreachable!("a recalled draft is updated"),
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
        let created: IdOnly = self
            .json_with(request, about, unsent)
            .await
            .map_err(may_be_left)?;
        Ok(MessageId::new(created.id))
    }

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

    async fn list_attachments(&self, id: &MessageId) -> Result<Vec<Attachment>> {
        MailStore::list_attachments(self, id).await
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

    /// Deletes a server draft for good where Graph can (DELETE would only
    /// move it to Deleted Items); elsewhere it goes to Deleted Items. One
    /// that is gone already is as good as deleted.
    async fn delete_draft(&self, id: &MessageId) -> Result<()> {
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
