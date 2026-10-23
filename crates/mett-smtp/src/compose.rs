//! Writing a draft as a MIME message.

use lettre::message::header::{ContentDisposition, ContentType};
use lettre::message::{Mailbox, Message, MessageBuilder, MultiPart, SinglePart};
use mett_core::{Address, Draft, Error, Result};

/// What threads a reply under its original: the headers of the message
/// answered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Threading {
    /// The original's `Message-ID:`, with its angle brackets.
    pub in_reply_to: Option<String>,
    /// The original's `References:` (its `Message-ID:`s, in order), with
    /// its own `Message-ID:` last, if the caller added it.
    pub references: Vec<String>,
}

/// A draft, written as a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Composed {
    /// The message's `Message-ID:`, with its angle brackets.
    pub message_id: String,
    /// The sender's address, which the server is given as the envelope's.
    pub sender: String,
    /// Every recipient's address (`To:`, `Cc:` and `Bcc:`), which the
    /// server is given as the envelope's.
    pub recipients: Vec<String>,
    /// The message as it goes out: without `Bcc:`.
    pub sent: Vec<u8>,
    /// The message as the sender keeps it (in Sent): with `Bcc:`.
    pub kept: Vec<u8>,
}

/// Writes `draft` as a message from `from` (unless the draft says
/// another sender), threaded as `threading` says.
///
/// Fails with [`Error::NotSent`] if an address isn't one, or there is
/// no recipient.
pub fn compose(draft: &Draft, from: &Address, threading: &Threading) -> Result<Composed> {
    if !draft.has_recipients() {
        return Err(Error::NotSent("the message has no recipients".into()));
    }
    let from = draft.from.as_ref().unwrap_or(from);
    // One message, so that its boundary and date are the same in both.
    let kept = build(draft, from, threading, true)?;
    let message_id = kept
        .headers()
        .get_raw("Message-ID")
        .map(str::to_owned)
        .ok_or_else(|| Error::NotSent("the message has no identifier".into()))?;
    let kept = kept.formatted();
    let recipients = draft
        .to
        .iter()
        .chain(&draft.cc)
        .chain(&draft.bcc)
        .map(|a| a.email.clone())
        .collect();
    Ok(Composed {
        message_id,
        sender: from.email.clone(),
        recipients,
        sent: without_bcc(&kept),
        kept,
    })
}

/// The message, with its `Bcc:` if `keep_bcc`.
fn build(draft: &Draft, from: &Address, threading: &Threading, keep_bcc: bool) -> Result<Message> {
    let builder = headers(draft, from, threading, keep_bcc)?.message_id(None);
    finish(builder, draft)
}

/// `message` without its `Bcc:` header (and the lines it is folded on).
fn without_bcc(message: &[u8]) -> Vec<u8> {
    let end = message
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(message.len(), |at| at + 2);
    let mut out = Vec::with_capacity(message.len());
    let mut skipping = false;
    for line in message[..end].split_inclusive(|b| *b == b'\n') {
        if line.first().is_some_and(|b| *b == b' ' || *b == b'\t') {
            // A continuation goes with the header before it.
            if skipping {
                continue;
            }
        } else {
            skipping = line.len() >= 4 && line[..4].eq_ignore_ascii_case(b"bcc:");
            if skipping {
                continue;
            }
        }
        out.extend_from_slice(line);
    }
    out.extend_from_slice(&message[end..]);
    out
}

/// The message's headers.
fn headers(
    draft: &Draft,
    from: &Address,
    threading: &Threading,
    keep_bcc: bool,
) -> Result<MessageBuilder> {
    let mut builder = Message::builder().from(mailbox(from)?);
    for to in &draft.to {
        builder = builder.to(mailbox(to)?);
    }
    for cc in &draft.cc {
        builder = builder.cc(mailbox(cc)?);
    }
    for bcc in &draft.bcc {
        builder = builder.bcc(mailbox(bcc)?);
    }
    if keep_bcc {
        builder = builder.keep_bcc();
    }
    builder = builder.subject(draft.subject.clone());
    if let Some(id) = &threading.in_reply_to {
        builder = builder.in_reply_to(id.clone());
    }
    if !threading.references.is_empty() {
        builder = builder.references(threading.references.join(" "));
    }
    Ok(builder)
}

/// `builder` with the body and attachments of `draft`.
fn finish(builder: MessageBuilder, draft: &Draft) -> Result<Message> {
    let text = SinglePart::plain(draft.body.clone());
    let built = if draft.attachments.is_empty() {
        builder.singlepart(text)
    } else {
        let mut parts = MultiPart::mixed().singlepart(text);
        for file in &draft.attachments {
            let content_type = ContentType::parse(&file.content_type)
                .or_else(|_| ContentType::parse("application/octet-stream"))
                .map_err(|e| Error::NotSent(format!("{}: {e}", file.name)))?;
            parts = parts.singlepart(
                SinglePart::builder()
                    .header(content_type)
                    .header(ContentDisposition::attachment(&file.name))
                    .body(file.data.clone()),
            );
        }
        builder.multipart(parts)
    };
    built.map_err(|e| Error::NotSent(e.to_string()))
}

fn mailbox(address: &Address) -> Result<Mailbox> {
    let email = address
        .email
        .parse()
        .map_err(|e| Error::NotSent(format!("{}: {e}", address.email)))?;
    Ok(Mailbox::new(
        address.name.clone().filter(|n| !n.is_empty()),
        email,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Draft {
        Draft {
            to: vec![Address::new("alice@example.org")],
            bcc: vec![Address::new("secret@example.org")],
            subject: "Café ☕".into(),
            body: "Bonjour\n".into(),
            ..Draft::default()
        }
    }

    #[test]
    fn the_copy_kept_shows_the_blind_copy_and_is_the_message_sent() {
        let composed = compose(
            &draft(),
            &Address::new("me@example.com"),
            &Threading::default(),
        )
        .unwrap();
        let sent = String::from_utf8(composed.sent).unwrap();
        let kept = String::from_utf8(composed.kept).unwrap();
        assert!(!sent.contains("secret@example.org"));
        assert!(kept.contains("Bcc: secret@example.org"));
        let id = format!("Message-ID: {}", composed.message_id);
        assert!(sent.contains(&id) && kept.contains(&id));
        assert_eq!(composed.sender, "me@example.com");
        assert_eq!(
            composed.recipients,
            ["alice@example.org", "secret@example.org"]
        );
    }

    #[test]
    fn writes_non_ascii_text_in_mime() {
        let composed = compose(
            &draft(),
            &Address::new("me@example.com"),
            &Threading::default(),
        )
        .unwrap();
        let sent = String::from_utf8(composed.sent).unwrap();
        assert!(sent.contains("Subject: =?utf-8?"));
        assert!(sent.contains("Content-Type: text/plain; charset=utf-8"));
    }

    #[test]
    fn the_draft_names_its_own_sender() {
        let mut draft = draft();
        draft.from = Some(Address::new("alias@example.com").with_name("Me"));
        let composed = compose(
            &draft,
            &Address::new("me@example.com"),
            &Threading::default(),
        )
        .unwrap();
        assert_eq!(composed.sender, "alias@example.com");
        assert!(
            String::from_utf8(composed.sent)
                .unwrap()
                .contains("From: Me <alias@example.com>")
        );
    }

    #[test]
    fn a_bad_address_is_not_sent() {
        let mut draft = draft();
        draft.to = vec![Address::new("not an address")];
        let error = compose(
            &draft,
            &Address::new("me@example.com"),
            &Threading::default(),
        )
        .unwrap_err();
        assert!(matches!(error, Error::NotSent(_)));
    }

    #[test]
    fn the_copy_differs_from_what_is_sent_by_the_blind_copy_alone() {
        let mut draft = draft();
        draft.attachments = vec![mett_core::OutgoingAttachment {
            name: "a.txt".into(),
            content_type: "text/plain".into(),
            data: b"a".to_vec(),
        }];
        // Enough blind copies for their header to be folded.
        draft.bcc = (0..8)
            .map(|n| Address::new(format!("someone-with-a-long-name-{n}@example.org")))
            .collect();
        let composed = compose(
            &draft,
            &Address::new("me@example.com"),
            &Threading::default(),
        )
        .unwrap();
        let kept = String::from_utf8(composed.kept).unwrap();
        let sent = String::from_utf8(composed.sent).unwrap();
        let (kept_headers, kept_body) = kept.split_once("\r\n\r\n").unwrap();
        let (sent_headers, sent_body) = sent.split_once("\r\n\r\n").unwrap();
        assert_eq!(kept_body, sent_body);
        assert!(kept_headers.contains("Bcc: "));
        assert!(!sent_headers.contains("Bcc") && !sent_headers.contains("someone-with"));
        let mut without = Vec::new();
        let mut in_bcc = false;
        for line in kept_headers.split("\r\n") {
            if !line.starts_with([' ', '\t']) {
                in_bcc = line.starts_with("Bcc:");
            }
            if !in_bcc {
                without.push(line);
            }
        }
        assert_eq!(sent_headers.split("\r\n").collect::<Vec<_>>(), without);
    }
}
