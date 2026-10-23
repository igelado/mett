use super::*;

/// The size of the `sendMail` request for `draft` ([`send_mail_json`]),
/// worked out without encoding its attachments: base64 needs no escaping
/// in JSON, so the request is as long as one with every file's
/// `contentBytes` empty, and their base64 besides.
pub(super) fn send_mail_size(draft: &Draft) -> serde_json::Result<usize> {
    /// Counts what is written to it.
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, &send_mail_json_with(draft, |_| String::new()))?;
    let encoded: usize = draft
        .attachments
        .iter()
        .map(|a| a.data.len().div_ceil(3) * 4)
        .sum();
    Ok(counter.0 + encoded)
}

/// A `sendMail` request for `draft`, which goes with its attachments.
pub(super) fn send_mail_json(draft: &Draft) -> serde_json::Value {
    send_mail_json_with(draft, in_base64)
}

/// [`send_mail_json`], with `content` for each file's `contentBytes`.
fn send_mail_json_with(draft: &Draft, content: fn(&[u8]) -> String) -> serde_json::Value {
    json!({
        "message": message_json_with(draft, Some(content)),
        "saveToSentItems": draft.save_copy,
    })
}

/// `data` in base64, as Graph takes a file's content.
fn in_base64(data: &[u8]) -> String {
    use base64::Engine;

    base64::engine::general_purpose::STANDARD.encode(data)
}

/// A Graph `fileAttachment` resource for `file`.
pub(super) fn file_attachment_json(file: &OutgoingAttachment) -> serde_json::Value {
    file_attachment_json_with(file, in_base64)
}

/// [`file_attachment_json`], with `content` for its `contentBytes`.
fn file_attachment_json_with(
    file: &OutgoingAttachment,
    content: fn(&[u8]) -> String,
) -> serde_json::Value {
    json!({
        "@odata.type": "#microsoft.graph.fileAttachment",
        "name": file.name,
        "contentType": file.content_type,
        "contentBytes": content(&file.data),
    })
}

/// A Graph `message` resource with `draft`'s content, and its attachments
/// if `attachments`.
pub(super) fn message_json(draft: &Draft, attachments: bool) -> serde_json::Value {
    message_json_with(draft, attachments.then_some(in_base64))
}

/// [`message_json`], with its attachments if given `content` for their
/// `contentBytes`.
fn message_json_with(draft: &Draft, content: Option<fn(&[u8]) -> String>) -> serde_json::Value {
    let recipients = |list: &[Address]| -> Vec<serde_json::Value> {
        list.iter()
            .map(|a| {
                let mut address = json!({ "address": a.email });
                if let Some(name) = a.name.as_deref().filter(|n| !n.is_empty()) {
                    address["name"] = json!(name);
                }
                json!({ "emailAddress": address })
            })
            .collect()
    };
    let mut message = json!({
        "subject": draft.subject,
        "body": { "contentType": "Text", "content": draft.body },
        "toRecipients": recipients(&draft.to),
        "ccRecipients": recipients(&draft.cc),
        "bccRecipients": recipients(&draft.bcc),
    });
    if let Some(from) = &draft.from {
        message["from"] = recipients(std::slice::from_ref(from)).remove(0);
    }
    if let Some(content) = content
        && !draft.attachments.is_empty()
    {
        message["attachments"] = draft
            .attachments
            .iter()
            .map(|file| file_attachment_json_with(file, content))
            .collect();
    }
    message
}

/// A page of a delta query: messages (or `@removed` ones), and the link to
/// go on from.
#[derive(Deserialize)]
pub(super) struct Delta {
    pub(super) value: Vec<serde_json::Value>,
    #[serde(rename = "@odata.nextLink")]
    pub(super) next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    pub(super) delta_link: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct Collection<T> {
    pub(super) value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    pub(super) next_link: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ErrorResponse {
    pub(super) error: ErrorBody,
}

#[derive(Deserialize)]
pub(super) struct ErrorBody {
    pub(super) code: String,
    pub(super) message: String,
}

/// The signed-in user's addresses.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Me {
    pub(super) mail: Option<String>,
    pub(super) user_principal_name: Option<String>,
    #[serde(default)]
    pub(super) proxy_addresses: Vec<String>,
}

#[derive(Deserialize)]
pub(super) struct IdOnly {
    pub(super) id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GraphAttachment {
    #[serde(rename = "@odata.type")]
    odata_type: Option<String>,
    id: String,
    name: Option<String>,
    content_type: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    is_inline: bool,
    /// The attached item, of an item attachment.
    item: Option<AttachedItem>,
}

#[derive(Deserialize)]
struct AttachedItem {
    #[serde(rename = "@odata.type")]
    odata_type: Option<String>,
}

impl GraphAttachment {
    pub(super) fn into_attachment(self) -> Attachment {
        let kind = match self.odata_type.as_deref() {
            Some("#microsoft.graph.itemAttachment") => AttachmentKind::Item,
            Some("#microsoft.graph.referenceAttachment") => AttachmentKind::Link,
            _ => AttachmentKind::File,
        };
        Attachment {
            id: AttachmentId::new(self.id),
            name: self.name.unwrap_or_default(),
            // An item comes as MIME (a message), iCalendar (an event) or a
            // vCard (a contact).
            content_type: self.content_type.unwrap_or_else(|| {
                let item = self.item.and_then(|i| i.odata_type);
                match (kind, item.as_deref()) {
                    (AttachmentKind::Item, Some("#microsoft.graph.event")) => "text/calendar",
                    (AttachmentKind::Item, Some("#microsoft.graph.contact")) => "text/vcard",
                    (AttachmentKind::Item, Some("#microsoft.graph.message") | None) => {
                        "message/rfc822"
                    }
                    _ => "application/octet-stream",
                }
                .into()
            }),
            size: self.size,
            inline: self.is_inline,
            kind,
        }
    }
}

#[derive(Deserialize)]
pub(super) struct BatchResponse {
    pub(super) responses: Vec<BatchItem>,
}

#[derive(Deserialize)]
pub(super) struct BatchItem {
    pub(super) id: String,
    pub(super) status: u16,
    #[serde(default)]
    headers: HashMap<String, String>,
    pub(super) body: Option<MaybeId>,
}

impl BatchItem {
    /// The item's `Retry-After`, in seconds, if it has one.
    pub(super) fn retry_after(&self) -> Option<Duration> {
        mett_core::retry_after(
            self.headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
                .map(|(_, value)| value.as_str()),
        )
    }
}

impl From<GraphFolder> for Folder {
    /// The folder, not yet labelled by [`Roles::folder`] with its role.
    fn from(folder: GraphFolder) -> Self {
        Self {
            id: FolderId::new(folder.id),
            parent: folder.parent_folder_id.map(FolderId::new),
            name: folder.display_name,
            well_known: None,
            unread_count: folder.unread_item_count,
            total_count: folder.total_item_count,
        }
    }
}

#[derive(Deserialize)]
pub(super) struct MaybeId {
    pub(super) id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MoveRequest<'a> {
    pub(super) destination_id: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GraphFolder {
    pub(super) id: String,
    pub(super) display_name: String,
    pub(super) parent_folder_id: Option<String>,
    #[serde(default)]
    pub(super) child_folder_count: u32,
    #[serde(default)]
    pub(super) unread_item_count: u32,
    #[serde(default)]
    pub(super) total_item_count: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GraphMessage {
    id: String,
    parent_folder_id: String,
    subject: Option<String>,
    from: Option<Recipient>,
    #[serde(default)]
    to_recipients: Vec<Recipient>,
    #[serde(default)]
    cc_recipients: Vec<Recipient>,
    #[serde(default)]
    bcc_recipients: Vec<Recipient>,
    #[serde(default)]
    reply_to: Vec<Recipient>,
    /// Missing, or a placeholder, for unsent drafts.
    sent_date_time: Option<String>,
    #[serde(default)]
    is_draft: bool,
    created_date_time: Option<String>,
    last_modified_date_time: Option<String>,
    received_date_time: Option<String>,
    #[serde(default)]
    is_read: bool,
    flag: Option<FollowupFlag>,
    #[serde(default)]
    has_attachments: bool,
    internet_message_id: Option<String>,
    body: Option<ItemBody>,
    conversation_id: Option<String>,
    /// Base64: a 22-byte header, then 5 bytes for each reply down the
    /// conversation.
    conversation_index: Option<String>,
    #[serde(rename = "@odata.type")]
    odata_type: Option<String>,
    /// What a meeting message says (all missing for other messages).
    #[serde(flatten)]
    meeting: MeetingMessage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recipient {
    email_address: EmailAddress,
}

#[derive(Deserialize)]
pub(crate) struct EmailAddress {
    pub(crate) name: Option<String>,
    pub(crate) address: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FollowupFlag {
    flag_status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemBody {
    content_type: String,
    content: String,
}

impl Recipient {
    fn into_address(self) -> Address {
        Address {
            name: self.email_address.name.filter(|n| !n.is_empty()),
            email: self.email_address.address.unwrap_or_default(),
        }
    }
}

impl GraphMessage {
    pub(super) fn into_summary(self) -> Result<MessageSummary> {
        self.split().map(|(summary, _)| summary)
    }

    pub(super) fn into_message(self) -> Result<Message> {
        let (summary, rest) = self.split()?;
        Ok(Message {
            summary,
            cc: addresses(rest.cc),
            bcc: addresses(rest.bcc),
            reply_to: addresses(rest.reply_to),
            internet_message_id: rest.internet_message_id,
            invitation: rest.invitation,
            body: rest.body.map_or(
                Body {
                    kind: BodyKind::Text,
                    content: String::new(),
                },
                |b| Body {
                    kind: if b.content_type.eq_ignore_ascii_case("html") {
                        BodyKind::Html
                    } else {
                        BodyKind::Text
                    },
                    content: b.content,
                },
            ),
        })
    }

    fn split(self) -> Result<(MessageSummary, Rest)> {
        // The date is when the message was sent, as in mutt's index, or for
        // an unsent draft, when it was created. It matches the listing's
        // sort order.
        // Graph may give drafts a placeholder sent date (such as year 1),
        // so drafts always use their creation date.
        let sent = self.sent_date_time.filter(|_| !self.is_draft);
        let unsent = sent.is_none();
        let timestamp = sent
            .or(self.created_date_time)
            .ok_or_else(|| Error::Backend("message has no timestamp".into()))?;
        let received = DateTime::parse_from_rfc3339(&timestamp)
            .map_err(|e| Error::Backend(format!("invalid message timestamp {timestamp:?}: {e}")))?
            .with_timezone(&Utc);
        let summary = MessageSummary {
            id: MessageId::new(self.id),
            folder: FolderId::new(self.parent_folder_id),
            subject: self.subject.unwrap_or_default(),
            from: self.from.map(Recipient::into_address),
            to: addresses(self.to_recipients),
            received,
            flags: Flags {
                read: self.is_read,
                flagged: self.flag.is_some_and(|f| f.flag_status == "flagged"),
            },
            has_attachments: self.has_attachments,
            // Without a sent date, it is listed with the drafts, whatever
            // Graph calls it.
            draft: unsent,
            delivered: parse_time(self.received_date_time.as_deref()),
            modified: parse_time(self.last_modified_date_time.as_deref()),
            thread: self.conversation_id.map(|conversation| ThreadPosition {
                conversation,
                path: self
                    .conversation_index
                    .as_deref()
                    .map(mett_core::thread_path)
                    .unwrap_or_default(),
            }),
        };
        let rest = Rest {
            invitation: self.meeting.invitation(self.odata_type.as_deref()),
            cc: self.cc_recipients,
            bcc: self.bcc_recipients,
            reply_to: self.reply_to,
            internet_message_id: self.internet_message_id,
            body: self.body,
        };
        Ok((summary, rest))
    }
}

/// Graph's recipients as addresses.
fn addresses(recipients: Vec<Recipient>) -> Vec<Address> {
    recipients
        .into_iter()
        .map(Recipient::into_address)
        .collect()
}

/// A Graph timestamp, if there is one and it parses.
fn parse_time(time: Option<&str>) -> Option<DateTime<Utc>> {
    time.and_then(mett_core::parse_time)
}

/// The parts of a [`GraphMessage`] that only a full [`Message`] needs.
struct Rest {
    invitation: Option<Invitation>,
    cc: Vec<Recipient>,
    bcc: Vec<Recipient>,
    reply_to: Vec<Recipient>,
    internet_message_id: Option<String>,
    body: Option<ItemBody>,
}
