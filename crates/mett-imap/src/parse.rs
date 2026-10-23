//! Reading mail: what the server sends, as mett's types.

use chrono::{DateTime, Utc};
use mail_parser::{
    Address as Addresses, HeaderValue, Message as Parsed, MessageParser, MessagePart, MimeHeaders,
};
use mett_core::{
    Address, Attachment, AttachmentId, AttachmentKind, Body, BodyKind, Error, Flags, FolderId,
    Message, MessageSummary, Result, ThreadPosition,
};

use crate::ids::Ref;

/// What a `FETCH` gave for one message.
#[derive(Clone, Debug)]
pub(crate) struct Fetched {
    pub uid: u32,
    pub flags: Flags,
    /// Whether the server marks it a draft (`\Draft`).
    pub draft: bool,
    /// When the server took the message in (`INTERNALDATE`).
    pub internal: Option<DateTime<Utc>>,
    /// The message's header, or all of it.
    pub data: Vec<u8>,
}

fn parse(data: &[u8]) -> Result<Parsed<'_>> {
    MessageParser::default()
        .parse(data)
        .ok_or_else(|| Error::Backend("the server sent a message that isn't one".into()))
}

fn addresses(list: Option<&Addresses<'_>>) -> Vec<Address> {
    list.into_iter()
        .flat_map(|list| list.iter())
        .filter_map(|a| {
            let email = a.address()?.to_owned();
            Some(Address {
                name: a.name().filter(|n| !n.is_empty()).map(str::to_owned),
                email,
            })
        })
        .collect()
}

/// A message identifier as the headers write it, with angle brackets.
fn bracketed(id: &str) -> String {
    format!("<{}>", id.trim_matches(['<', '>']))
}

fn ids(value: &HeaderValue<'_>) -> Vec<String> {
    match value {
        HeaderValue::Text(id) => vec![bracketed(id)],
        HeaderValue::TextList(ids) => ids.iter().map(|id| bracketed(id)).collect(),
        _ => Vec::new(),
    }
}

/// What threads a message: its own `Message-ID:` and the ones it
/// answers (`References:`, else `In-Reply-To:`), oldest first, all with
/// angle brackets.
pub(crate) fn threading(data: &[u8]) -> Result<(Option<String>, Vec<String>)> {
    let message = parse(data)?;
    let mut references = ids(message.references());
    if references.is_empty() {
        references = ids(message.in_reply_to());
    }
    Ok((message.message_id().map(bracketed), references))
}

fn utc(date: &mail_parser::DateTime) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(date.to_timestamp(), 0)
}

/// The index line of a message from its header.
pub(crate) fn summary(folder: &str, validity: u32, fetched: &Fetched) -> Result<MessageSummary> {
    let message = parse(&fetched.data)?;
    let at = Ref {
        folder: folder.to_owned(),
        validity,
        uid: fetched.uid,
    };
    let sent = message.date().and_then(utc);
    let (own, references) = threading(&fetched.data)?;
    let thread = (own.is_some() || !references.is_empty()).then(|| {
        let mut path: Vec<Vec<u8>> = references.iter().map(|r| r.as_bytes().to_vec()).collect();
        path.push(
            own.clone()
                .unwrap_or_else(|| at.id().to_string())
                .into_bytes(),
        );
        ThreadPosition {
            conversation: references
                .first()
                .or(own.as_ref())
                .cloned()
                .unwrap_or_default(),
            path,
        }
    });
    let multipart_mixed = message.content_type().is_some_and(|t| {
        t.ctype().eq_ignore_ascii_case("multipart")
            && matches!(t.subtype(), Some(s) if s.eq_ignore_ascii_case("mixed"))
    });
    Ok(MessageSummary {
        id: at.id(),
        folder: FolderId::new(folder),
        subject: message.subject().unwrap_or_default().to_owned(),
        from: addresses(message.from()).into_iter().next(),
        to: addresses(message.to()),
        // A message that says no date is dated when it came in.
        received: sent.or(fetched.internal).unwrap_or(DateTime::UNIX_EPOCH),
        flags: fetched.flags,
        has_attachments: multipart_mixed,
        draft: fetched.draft && sent.is_none(),
        delivered: fetched.internal,
        modified: None,
        thread,
    })
}

/// A whole message.
pub(crate) fn message(folder: &str, validity: u32, fetched: &Fetched) -> Result<Message> {
    let mut summary = summary(folder, validity, fetched)?;
    let parsed = parse(&fetched.data)?;
    summary.has_attachments = parsed.attachment_count() > 0;
    let text: Vec<_> = (0..parsed.text_body_count())
        .filter_map(|n| parsed.body_text(n))
        .collect();
    Ok(Message {
        summary,
        cc: addresses(parsed.cc()),
        bcc: addresses(parsed.bcc()),
        reply_to: addresses(parsed.reply_to()),
        internet_message_id: parsed.message_id().map(bracketed),
        body: Body {
            kind: BodyKind::Text,
            content: text.join("\n"),
        },
        invitation: None,
    })
}

fn content_type(part: &MessagePart<'_>) -> String {
    part.content_type()
        .map(|t| match t.subtype() {
            Some(sub) => format!("{}/{}", t.ctype(), sub),
            None => t.ctype().to_owned(),
        })
        .unwrap_or_else(|| "application/octet-stream".into())
}

fn name(part: &MessagePart<'_>, n: usize) -> String {
    part.attachment_name()
        .map(str::to_owned)
        .or_else(|| {
            part.message()
                .and_then(|m| m.subject())
                .map(|s| format!("{s}.eml"))
        })
        .unwrap_or_else(|| format!("attachment-{}", n + 1))
}

/// The attachments of a message.
pub(crate) fn attachments(data: &[u8]) -> Result<Vec<Attachment>> {
    let message = parse(data)?;
    Ok((0..message.attachment_count())
        .filter_map(|n| {
            let part = message.attachment(u32::try_from(n).ok()?)?;
            Some(Attachment {
                id: AttachmentId::new(n.to_string()),
                name: name(part, n),
                content_type: content_type(part),
                size: part.len() as u64,
                inline: part.content_id().is_some()
                    || part
                        .content_disposition()
                        .is_some_and(|d| d.ctype().eq_ignore_ascii_case("inline")),
                kind: if part.is_message() {
                    AttachmentKind::Item
                } else {
                    AttachmentKind::File
                },
            })
        })
        .collect())
}

/// An attachment's content: a file's bytes, or an attached message as MIME.
pub(crate) fn attachment(data: &[u8], id: &AttachmentId) -> Result<Vec<u8>> {
    let message = parse(data)?;
    let part = id
        .as_str()
        .parse::<u32>()
        .ok()
        .and_then(|n| message.attachment(n))
        .ok_or_else(|| Error::not_found("attachment", id))?;
    Ok(match part.message() {
        Some(nested) => nested.raw_message().to_vec(),
        None => part.contents().to_vec(),
    })
}

/// The attachments of a message as files to attach to another: those that
/// aren't shown in the body, except `skip`.
pub(crate) fn files(
    data: &[u8],
    skip: &[AttachmentId],
) -> Result<Vec<mett_core::OutgoingAttachment>> {
    let listed = attachments(data)?;
    listed
        .into_iter()
        .filter(|a| !a.inline && a.kind != AttachmentKind::Link && !skip.contains(&a.id))
        .map(|a| {
            Ok(mett_core::OutgoingAttachment {
                data: attachment(data, &a.id)?,
                content_type: if a.kind == AttachmentKind::Item {
                    "message/rfc822".into()
                } else {
                    a.content_type
                },
                name: a.name,
            })
        })
        .collect()
}
