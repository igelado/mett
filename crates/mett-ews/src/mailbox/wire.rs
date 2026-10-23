use super::*;

/// The folder fields mett reads.
const FOLDER_FIELDS: &[&str] = &[
    "folder:DisplayName",
    "folder:ParentFolderId",
    "folder:TotalCount",
    "folder:UnreadCount",
    "folder:ChildFolderCount",
    "folder:FolderClass",
];

/// The message fields the index needs.
pub(super) const SUMMARY_FIELDS: &[&str] = &[
    "item:ParentFolderId",
    "item:Subject",
    "message:From",
    "message:ToRecipients",
    "item:DateTimeSent",
    "item:DateTimeCreated",
    "item:DateTimeReceived",
    "item:IsDraft",
    "message:IsRead",
    "item:Flag",
    "item:HasAttachments",
    "item:LastModifiedTime",
    "item:ConversationId",
    "message:ConversationIndex",
];

/// The fields only a full message needs, besides its body.
pub(super) const MESSAGE_FIELDS: &[&str] = &[
    "message:CcRecipients",
    "message:BccRecipients",
    "message:ReplyTo",
    "message:InternetMessageId",
    "item:Body",
];

/// The response message of a `ResolveNames`, or `None` if it found no one.
pub(super) fn only_message_or_none<'a, 'i>(
    doc: &'a roxmltree::Document<'i>,
) -> Result<Option<Node<'a, 'i>>> {
    let message = response_messages(doc, About::MAILBOX)?
        .into_iter()
        .next()
        .ok_or_else(|| unexpected("ResolveNames", "no response"))?;
    if response_code(message) == "ErrorNameResolutionNoResults" {
        return Ok(None);
    }
    check(message, About::MAILBOX)?;
    Ok(Some(message))
}

/// An unexpected response to `operation`.
pub(super) fn unexpected(operation: &str, what: &str) -> Error {
    Error::Backend(format!(
        "unexpected response from EWS to {operation}: {what}"
    ))
}

/// A `FieldURI` for `uri`, such as `item:Subject`.
pub(super) fn field(uri: &str) -> String {
    format!("<t:FieldURI FieldURI=\"{uri}\"/>")
}

/// A `SetItemField` setting `uri` to the message `content` holds.
pub(super) fn set_field(uri: &str, content: &str) -> String {
    format!(
        "<t:SetItemField>{}<t:Message>{content}</t:Message></t:SetItemField>",
        field(uri)
    )
}

/// An `ItemShape` with `fields`, and the body as `body_type` if given.
pub(super) fn item_shape(fields: &[&str], body_type: Option<&str>) -> String {
    let fields: String = fields.iter().map(|uri| field(uri)).collect();
    let body_type = body_type
        .map(|t| format!("<t:BodyType>{t}</t:BodyType>"))
        .unwrap_or_default();
    let fields = if fields.is_empty() {
        String::new()
    } else {
        format!("<t:AdditionalProperties>{fields}</t:AdditionalProperties>")
    };
    format!("<m:ItemShape><t:BaseShape>IdOnly</t:BaseShape>{body_type}{fields}</m:ItemShape>")
}

/// The `FolderShape` with the folder fields mett reads.
pub(super) fn folder_shape() -> String {
    let fields: String = FOLDER_FIELDS.iter().map(|uri| field(uri)).collect();
    format!(
        "<m:FolderShape><t:BaseShape>IdOnly</t:BaseShape>\
         <t:AdditionalProperties>{fields}</t:AdditionalProperties></m:FolderShape>"
    )
}

/// A `FolderId` element.
pub(super) fn folder_id(id: &FolderId) -> String {
    format!("<t:FolderId Id=\"{}\"/>", escape(id.as_str()))
}

/// An `ItemId` element.
pub(super) fn item_id_xml(id: &str) -> String {
    format!("<t:ItemId Id=\"{}\"/>", escape(id))
}

/// A restriction to items modified at or after `since`.
pub(super) fn modified_since(since: DateTime<Utc>) -> String {
    format!(
        "<t:IsGreaterThanOrEqualTo>{}<t:FieldURIOrConstant><t:Constant Value=\"{}\"/>\
         </t:FieldURIOrConstant></t:IsGreaterThanOrEqualTo>",
        field("item:LastModifiedTime"),
        since.to_rfc3339_opts(SecondsFormat::Secs, true)
    )
}

/// A `CreateItem` of `items` with `disposition`, saved in the
/// distinguished folder `saved` if given.
pub(super) fn create_item(disposition: &str, saved: Option<&str>, items: &str) -> String {
    let saved = saved
        .map(|name| {
            format!(
                "<m:SavedItemFolderId><t:DistinguishedFolderId Id=\"{name}\"/></m:SavedItemFolderId>"
            )
        })
        .unwrap_or_default();
    format!(
        "<m:CreateItem MessageDisposition=\"{disposition}\">{saved}<m:Items>{items}</m:Items>\
         </m:CreateItem>"
    )
}

/// The ID of the item a `CreateItem` made.
pub(super) fn created_id(xml: &str) -> Result<MessageId> {
    let doc = parse(xml)?;
    let message = only_message(&doc, About::MAILBOX)?;
    first_item(message)
        .and_then(item_id)
        .map(MessageId::new)
        .ok_or_else(|| unexpected("CreateItem", "no item ID"))
}

/// `Mailbox` elements for `addresses`.
pub(super) fn mailboxes(addresses: &[Address]) -> String {
    addresses
        .iter()
        .map(|a| {
            let name = a
                .name
                .as_deref()
                .filter(|n| !n.is_empty())
                .map(|n| format!("<t:Name>{}</t:Name>", escape(n)))
                .unwrap_or_default();
            format!(
                "<t:Mailbox>{name}<t:EmailAddress>{}</t:EmailAddress></t:Mailbox>",
                escape(&a.email)
            )
        })
        .collect()
}

/// A `FileAttachment` element for `file`.
pub(super) fn file_attachment_xml(file: &OutgoingAttachment) -> String {
    use base64::Engine;

    format!(
        "<t:FileAttachment><t:Name>{}</t:Name><t:ContentType>{}</t:ContentType>\
         <t:Content>{}</t:Content></t:FileAttachment>",
        escape(&file.name),
        escape(&file.content_type),
        base64::engine::general_purpose::STANDARD.encode(&file.data)
    )
}

/// A `Message` element with `draft`'s content, but its attachments (which
/// Exchange takes only on a saved message). The elements follow the order
/// the schema requires.
pub(super) fn message_xml(draft: &Draft) -> String {
    let mut xml = format!(
        "<t:Message><t:Subject>{}</t:Subject><t:Body BodyType=\"Text\">{}</t:Body>",
        escape(&draft.subject),
        escape(&draft.body)
    );
    for (element, list) in [
        ("ToRecipients", &draft.to),
        ("CcRecipients", &draft.cc),
        ("BccRecipients", &draft.bcc),
    ] {
        if !list.is_empty() {
            xml.push_str(&format!("<t:{element}>{}</t:{element}>", mailboxes(list)));
        }
    }
    if let Some(from) = &draft.from {
        xml.push_str(&format!(
            "<t:From>{}</t:From>",
            mailboxes(std::slice::from_ref(from))
        ));
    }
    xml.push_str("</t:Message>");
    xml
}

/// The offset of the page after one at `offset` with `found` entries, in
/// a `RootFolder` that says whether it has more.
pub(super) fn next_offset(root: Node<'_, '_>, offset: u32, found: usize) -> Option<u32> {
    if root.attribute("IncludesLastItemInRange") != Some("false") || found == 0 {
        return None;
    }
    let after = offset.saturating_add(u32::try_from(found).unwrap_or(u32::MAX));
    let next = root
        .attribute("IndexedPagingOffset")
        .and_then(|o| o.parse().ok())
        .unwrap_or(after);
    // Always make progress.
    Some(next.max(after))
}

/// A page cursor: where the next page starts, in which listing (`query`)
/// of which folder.
pub(super) fn cursor(offset: u32, query: &str, folder: &FolderId) -> PageCursor {
    PageCursor::new(format!("{offset} {query} {folder}"))
}

/// The offset in `cursor`, which must be for `query` of `folder`.
pub(super) fn parse_cursor(cursor: &PageCursor, folder: &FolderId, query: &str) -> Result<u32> {
    let bad = || {
        Error::Backend(format!(
            "page cursor is not for this listing of folder {folder}"
        ))
    };
    let (offset, rest) = cursor.as_str().split_once(' ').ok_or_else(bad)?;
    let (cursor_query, cursor_folder) = rest.split_once(' ').ok_or_else(bad)?;
    if cursor_query != query || cursor_folder != folder.as_str() {
        return Err(bad());
    }
    offset.parse().map_err(|_| bad())
}

/// The first item of a response message's `Items`.
pub(super) fn first_item<'a, 'i>(message: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
    child(message, "Items").and_then(|items| items.children().find(Node::is_element))
}

/// An item's ID.
pub(super) fn item_id(item: Node<'_, '_>) -> Option<String> {
    child(item, "ItemId")
        .and_then(|id| id.attribute("Id"))
        .map(str::to_owned)
}

/// An item found by `FindItem`.
pub(super) struct FoundItem {
    pub(super) id: String,
    /// Its `LastModifiedTime`, if asked for.
    pub(super) modified: Option<String>,
}

/// A folder, as EWS gives it.
#[derive(Debug)]
pub(super) struct EwsFolder {
    pub(super) id: String,
    pub(super) parent: Option<String>,
    pub(super) name: String,
    /// The kind of folder: `Folder` for mail folders (and generic ones),
    /// or `CalendarFolder`, `SearchFolder` and so on.
    pub(super) element: String,
    pub(super) class: Option<String>,
    pub(super) unread: u32,
    pub(super) total: u32,
}

impl EwsFolder {
    pub(super) fn parse(node: Node<'_, '_>) -> Option<Self> {
        let number = |name: &str| {
            child_text(node, name)
                .and_then(|n| n.trim().parse().ok())
                .unwrap_or(0)
        };
        Some(Self {
            id: child(node, "FolderId")?.attribute("Id")?.to_owned(),
            parent: child(node, "ParentFolderId")
                .and_then(|p| p.attribute("Id"))
                .map(str::to_owned),
            name: child_text(node, "DisplayName").unwrap_or_default(),
            element: node.tag_name().name().to_owned(),
            class: child_text(node, "FolderClass"),
            unread: number("UnreadCount"),
            total: number("TotalCount"),
        })
    }

    /// Whether it holds mail: a plain folder of mail (`IPF.Note`) or of no
    /// class.
    pub(super) fn is_mail(&self) -> bool {
        self.element == "Folder"
            && self.class.as_deref().is_none_or(|class| {
                class.is_empty() || class == "IPF.Note" || class.starts_with("IPF.Note.")
            })
    }
}

/// A person `ResolveNames` found.
pub(super) struct Resolved {
    pub(super) name: Option<String>,
    /// Their address, as the mailbox gives it.
    pub(super) email: Option<String>,
    /// Every SMTP address known for them: the mailbox's, and with full
    /// contact data, the contact's.
    pub(super) addresses: Vec<String>,
}

impl Resolved {
    pub(super) fn parse(resolution: Node<'_, '_>) -> Self {
        let mailbox = child(resolution, "Mailbox");
        let routing = mailbox.and_then(|m| child_text(m, "RoutingType"));
        let email = mailbox
            .and_then(|m| child_text(m, "EmailAddress"))
            .filter(|_| {
                routing
                    .as_deref()
                    .is_none_or(|r| r.eq_ignore_ascii_case("SMTP"))
            });
        let name = mailbox.and_then(|m| child_text(m, "Name"));
        let mut addresses: Vec<String> = email.iter().cloned().collect();
        if let Some(entries) = child(resolution, "Contact").and_then(|c| child(c, "EmailAddresses"))
        {
            for entry in children(entries, "Entry") {
                let value = entry.text().unwrap_or_default().trim();
                let address = match value.split_once(':') {
                    Some((kind, address)) if kind.eq_ignore_ascii_case("smtp") => address,
                    Some(_) => continue,
                    None => value,
                };
                addresses.push(address.to_owned());
            }
        }
        Self {
            name,
            email,
            addresses,
        }
    }
}

/// An attachment, from an item's `Attachments`.
pub(super) fn attachment(node: Node<'_, '_>) -> Option<Attachment> {
    let kind = match node.tag_name().name() {
        "FileAttachment" => AttachmentKind::File,
        "ItemAttachment" => AttachmentKind::Item,
        "ReferenceAttachment" => AttachmentKind::Link,
        _ => return None,
    };
    let id = child(node, "AttachmentId")?.attribute("Id")?.to_owned();
    Some(Attachment {
        id: AttachmentId::new(id),
        name: child_text(node, "Name").unwrap_or_default(),
        // An attached item comes as MIME.
        content_type: child_text(node, "ContentType")
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| match kind {
                AttachmentKind::Item => "message/rfc822".into(),
                _ => "application/octet-stream".into(),
            }),
        size: child_text(node, "Size")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0),
        inline: child_text(node, "IsInline").as_deref() == Some("true"),
        kind,
    })
}

/// `text` decoded from base64, ignoring whitespace.
pub(super) fn decode_base64(text: &str) -> Result<Vec<u8>> {
    use base64::Engine;

    /// Characters decoded at a time (a multiple of 4).
    const BLOCK: usize = 4096;

    let invalid = |e: &dyn std::fmt::Display| {
        Error::Backend(format!("invalid attachment content from EWS: {e}"))
    };
    let engine = base64::engine::general_purpose::STANDARD;
    let mut decoded = Vec::with_capacity(text.len() / 4 * 3);
    let mut block = [0u8; BLOCK];
    let mut filled = 0;
    // Padding ends the data: it may be in the last block only.
    let mut padded = false;
    for c in text.chars().filter(|c| !c.is_whitespace()) {
        if padded {
            return Err(invalid(&"data after padding"));
        }
        block[filled] = u8::try_from(c).map_err(|_| invalid(&"not base64"))?;
        filled += 1;
        if filled == BLOCK {
            engine
                .decode_vec(&block[..], &mut decoded)
                .map_err(|e| invalid(&e))?;
            padded = block.contains(&b'=');
            filled = 0;
        }
    }
    engine
        .decode_vec(&block[..filled], &mut decoded)
        .map_err(|e| invalid(&e))?;
    Ok(decoded)
}

/// The addresses of a recipients element (such as `ToRecipients`) of
/// `item`.
fn recipients(item: Node<'_, '_>, element: &str) -> Vec<Address> {
    child(item, element)
        .into_iter()
        .flat_map(|list| children(list, "Mailbox"))
        .map(address)
        .collect()
}

/// A `Mailbox` element as an address.
pub(crate) fn address(mailbox: Node<'_, '_>) -> Address {
    Address {
        name: child_text(mailbox, "Name").filter(|n| !n.is_empty()),
        email: child_text(mailbox, "EmailAddress").unwrap_or_default(),
    }
}

/// An EWS timestamp, if it parses.
pub(super) fn parse_time(time: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(time.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// An item's summary.
pub(super) fn summary(item: Node<'_, '_>) -> Result<MessageSummary> {
    let id = item_id(item).ok_or_else(|| unexpected("GetItem", "an item without an ID"))?;
    let time = |name: &str| child_text(item, name).as_deref().and_then(parse_time);
    let is_draft = child_text(item, "IsDraft").as_deref() == Some("true");
    // The date is when the message was sent, as in mutt's index, or for an
    // unsent draft, when it was created. It matches the listing's sort
    // order.
    let sent = time("DateTimeSent").filter(|_| !is_draft);
    let received = sent
        .or_else(|| time("DateTimeCreated"))
        .ok_or_else(|| Error::Backend("message has no timestamp".into()))?;
    let conversation = child(item, "ConversationId")
        .and_then(|c| c.attribute("Id"))
        .map(str::to_owned);
    Ok(MessageSummary {
        id: MessageId::new(id),
        folder: FolderId::new(
            child(item, "ParentFolderId")
                .and_then(|f| f.attribute("Id"))
                .unwrap_or_default(),
        ),
        subject: child_text(item, "Subject").unwrap_or_default(),
        from: child(item, "From")
            .and_then(|f| child(f, "Mailbox"))
            .map(address),
        to: recipients(item, "ToRecipients"),
        received,
        flags: Flags {
            // Items that aren't messages have no read state: they count as
            // read.
            read: child_text(item, "IsRead").is_none_or(|r| r == "true"),
            flagged: text(item, "FlagStatus").as_deref() == Some("Flagged"),
        },
        has_attachments: child_text(item, "HasAttachments").as_deref() == Some("true"),
        // Without a sent date, it is listed with the drafts.
        draft: sent.is_none(),
        delivered: time("DateTimeReceived"),
        modified: time("LastModifiedTime"),
        thread: conversation.map(|conversation| ThreadPosition {
            conversation,
            path: child_text(item, "ConversationIndex")
                .as_deref()
                .map(thread_path)
                .unwrap_or_default(),
        }),
    })
}

/// A full message.
pub(super) fn full_message(item: Node<'_, '_>) -> Result<Message> {
    let body = child(item, "Body");
    Ok(Message {
        summary: summary(item)?,
        cc: recipients(item, "CcRecipients"),
        bcc: recipients(item, "BccRecipients"),
        reply_to: recipients(item, "ReplyTo"),
        internet_message_id: child_text(item, "InternetMessageId"),
        body: Body {
            kind: match body.and_then(|b| b.attribute("BodyType")) {
                Some(t) if t.eq_ignore_ascii_case("HTML") => BodyKind::Html,
                _ => BodyKind::Text,
            },
            content: body.and_then(|b| b.text()).unwrap_or_default().to_owned(),
        },
        invitation: invitation(item),
    })
}

/// A message's path in its conversation, from Outlook's conversation
/// index: the 22-byte header (the conversation's first message), then a
/// 5-byte block for each reply. An index that doesn't decode, or doesn't
/// have that shape, gives none.
fn thread_path(index: &str) -> Vec<Vec<u8>> {
    use base64::Engine;

    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(index.trim()) else {
        return Vec::new();
    };
    if bytes.len() < 22 || (bytes.len() - 22) % 5 != 0 {
        return Vec::new();
    }
    let (header, replies) = bytes.split_at(22);
    std::iter::once(header.to_vec())
        .chain(replies.chunks(5).map(<[u8]>::to_vec))
        .collect()
}
