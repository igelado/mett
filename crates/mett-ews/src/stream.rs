//! Streaming notifications: what `GetStreamingEvents` sends, one SOAP
//! envelope at a time, on a response that stays open.

use mett_core::{Error, FolderId};
use roxmltree::Node;

use crate::soap::{About, LocalName, check, child, parse, response_code, response_messages};

/// What one envelope of a `GetStreamingEvents` response says.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Streamed {
    /// Mail arrived, changed or went in these folders (maybe none: the
    /// server only says it is still there).
    Changed(Vec<FolderId>),
    /// The server closed the stream: watch again.
    Closed,
    /// The subscription is gone (expired, say): subscribe again.
    Unsubscribed,
}

/// The most the stream may hold of one envelope, when it is not complete.
const MAX_ENVELOPE: usize = 16 * 1024 * 1024;

const CLOSING: &[u8] = b"Envelope>";

/// The complete envelopes at the start of `buffer`, which they are taken
/// off; what is left is the start of one still on its way. `scanned` is
/// how much of `buffer` is known to hold no end of an envelope: the caller
/// starts it at 0 and passes it back with the same buffer, so bytes are not
/// searched again as the rest of an envelope comes. An envelope that is
/// not complete within [`MAX_ENVELOPE`] is an error, which
/// [`check_pending`] tells (after the complete ones are used).
pub(crate) fn take_envelopes(buffer: &mut Vec<u8>, scanned: &mut usize) -> Vec<Vec<u8>> {
    // Where each ends; the buffer is taken from once.
    let mut ends = Vec::new();
    let mut from = 0;
    while let Some(end) = envelope_end(buffer, from.max(*scanned)) {
        ends.push(end);
        from = end;
    }
    if ends.is_empty() {
        // A closing tag may start in the last bytes, and be completed by
        // the next chunk.
        *scanned = buffer.len().saturating_sub(CLOSING.len() - 1);
        return Vec::new();
    }
    let rest = buffer.split_off(from);
    let mut taken = std::mem::replace(buffer, rest);
    *scanned = buffer.len().saturating_sub(CLOSING.len() - 1);
    // Each envelope is split off the end, so every byte moves once.
    let mut envelopes: Vec<Vec<u8>> = Vec::with_capacity(ends.len());
    for start in std::iter::once(0).chain(ends.iter().copied()).rev().skip(1) {
        envelopes.push(taken.split_off(start));
    }
    envelopes.reverse();
    envelopes
}

/// Whether the start of an envelope left in `buffer` (after
/// [`take_envelopes`]) is more than [`MAX_ENVELOPE`] already.
pub(crate) fn check_pending(buffer: &[u8]) -> Result<(), Error> {
    if buffer.len() > MAX_ENVELOPE {
        return Err(Error::Backend(
            "EWS sent an envelope too large to read".into(),
        ));
    }
    Ok(())
}

/// Where the first envelope in `bytes` ends: just after its closing tag
/// (`</soap:Envelope>`, whatever its prefix), if it is there. The part
/// before `scanned` is known not to hold one.
fn envelope_end(bytes: &[u8], scanned: usize) -> Option<usize> {
    let mut from = scanned;
    while let Some(at) = find(&bytes[from..], CLOSING).map(|at| from + at) {
        let end = at + CLOSING.len();
        // The tag's start: `</`, then maybe a prefix and `:`.
        if let Some(open) = bytes[..at].iter().rposition(|&b| b == b'<') {
            let tag = &bytes[open + 1..at];
            let closing = tag.first() == Some(&b'/')
                && (tag.len() == 1
                    || (tag.last() == Some(&b':')
                        && tag[1..tag.len() - 1]
                            .iter()
                            .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')));
            if closing {
                return Some(end);
            }
        }
        from = end;
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Whether response code `code` says the subscription is gone.
pub(crate) fn subscription_gone(code: &str) -> bool {
    matches!(
        code,
        "ErrorInvalidSubscription" | "ErrorSubscriptionNotFound" | "ErrorExpiredSubscription"
    )
}

/// Reads one envelope of a `GetStreamingEvents` response.
pub(crate) fn read_envelope(envelope: &[u8]) -> Result<Streamed, Error> {
    // What separates envelopes (white space) comes before the next one,
    // whose XML declaration must come first.
    let text = std::str::from_utf8(envelope)
        .map_err(|_| Error::Backend("unexpected response from EWS: not UTF-8".into()))?
        .trim_start();
    let doc = parse(text)?;
    let about = About::MAILBOX;
    let mut changed: Vec<FolderId> = Vec::new();
    for message in response_messages(&doc, about)? {
        if subscription_gone(&response_code(message)) {
            return Ok(Streamed::Unsubscribed);
        }
        check(message, about)?;
        for notification in message
            .descendants()
            .filter(|n| n.has_tag_name_local("Notification"))
        {
            for event in notification.children().filter(Node::is_element) {
                // A status event only says the stream is alive.
                if !event.tag_name().name().ends_with("Event")
                    || event.has_tag_name_local("StatusEvent")
                {
                    continue;
                }
                // A folder's own events name it too.
                for folder in [
                    "ParentFolderId",
                    "OldParentFolderId",
                    "FolderId",
                    "OldFolderId",
                ]
                .into_iter()
                .filter_map(|name| child(event, name))
                .filter_map(|f| f.attribute("Id"))
                {
                    let folder = FolderId::new(folder);
                    if !changed.contains(&folder) {
                        changed.push(folder);
                    }
                }
            }
        }
        if child(message, "ConnectionStatus")
            .and_then(|s| s.text())
            .is_some_and(|s| s.trim() == "Closed")
            && changed.is_empty()
        {
            return Ok(Streamed::Closed);
        }
    }
    Ok(Streamed::Changed(changed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(body: &str) -> String {
        format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
             <s:Body xmlns:m=\"messages\" xmlns:t=\"types\">\
             <m:GetStreamingEventsResponse><m:ResponseMessages>{body}</m:ResponseMessages>\
             </m:GetStreamingEventsResponse></s:Body></s:Envelope>"
        )
    }

    fn message(class: &str, code: &str, rest: &str) -> String {
        format!(
            "<m:GetStreamingEventsResponseMessage ResponseClass=\"{class}\">\
             <m:ResponseCode>{code}</m:ResponseCode>{rest}</m:GetStreamingEventsResponseMessage>"
        )
    }

    #[test]
    fn splits_the_stream_into_envelopes() {
        let first = envelope(&message("Success", "NoError", ""));
        let second = envelope(&message("Success", "NoError", ""));
        let mut buffer = format!("{first}{second}").into_bytes();
        // Only part of a third has come.
        buffer.extend_from_slice(b"<s:Envelope xmlns:s=\"x\"><s:Bo");
        let mut scanned = 0;
        let envelopes = take_envelopes(&mut buffer, &mut scanned);
        assert_eq!(envelopes, [first.into_bytes(), second.into_bytes()]);
        assert_eq!(buffer, b"<s:Envelope xmlns:s=\"x\"><s:Bo");
        // An envelope without a prefix ends too, and text that only looks
        // like the tag doesn't.
        let mut buffer = b"<Envelope><Body>NotEnvelope></Body></Envelope>".to_vec();
        assert_eq!(take_envelopes(&mut buffer, &mut 0).len(), 1);
        assert!(buffer.is_empty());
    }

    #[test]
    fn an_envelope_that_comes_in_pieces_is_found_once_whole() {
        let whole = envelope(&message("Success", "NoError", "")).into_bytes();
        let mut buffer = Vec::new();
        let mut scanned = 0;
        let mut found = Vec::new();
        // The closing tag is split in every possible place.
        for piece in whole.chunks(7) {
            buffer.extend_from_slice(piece);
            found.extend(take_envelopes(&mut buffer, &mut scanned));
        }
        assert_eq!(found, [whole]);
        assert!(buffer.is_empty());
        assert_eq!(scanned, 0);
    }

    #[test]
    fn an_endless_envelope_is_an_error() {
        let mut buffer = vec![b'x'; MAX_ENVELOPE + 1];
        assert!(take_envelopes(&mut buffer, &mut 0).is_empty());
        assert!(matches!(check_pending(&buffer), Err(Error::Backend(_))));
    }

    #[test]
    fn envelopes_may_be_apart() {
        let one = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>{}",
            envelope(&message("Success", "NoError", ""))
        );
        let mut buffer = format!("{one}\r\n{one}").into_bytes();
        for envelope in take_envelopes(&mut buffer, &mut 0) {
            assert_eq!(read_envelope(&envelope).unwrap(), Streamed::Changed(vec![]));
        }
    }

    #[test]
    fn tells_which_folders_changed() {
        let events = "<m:Notifications><m:Notification><t:SubscriptionId>s</t:SubscriptionId>\
             <t:NewMailEvent><t:ItemId Id=\"i1\"/><t:ParentFolderId Id=\"inbox\"/></t:NewMailEvent>\
             <t:MovedEvent><t:ItemId Id=\"i2\"/><t:ParentFolderId Id=\"archive\"/>\
             <t:OldItemId Id=\"i0\"/><t:OldParentFolderId Id=\"inbox\"/></t:MovedEvent>\
             <t:StatusEvent><t:Watermark>w</t:Watermark></t:StatusEvent>\
             <t:ModifiedEvent><t:FolderId Id=\"work\"/><t:ParentFolderId Id=\"root\"/>\
             </t:ModifiedEvent>\
             </m:Notification></m:Notifications>";
        let streamed = read_envelope(envelope(&message("Success", "NoError", events)).as_bytes());
        assert_eq!(
            streamed.unwrap(),
            Streamed::Changed(vec![
                FolderId::new("inbox"),
                FolderId::new("archive"),
                FolderId::new("root"),
                FolderId::new("work"),
            ])
        );
        // Only alive.
        let alive = "<m:Notifications><m:Notification><t:StatusEvent/></m:Notification></m:Notifications>\
             <m:ConnectionStatus>OK</m:ConnectionStatus>";
        let streamed = read_envelope(envelope(&message("Success", "NoError", alive)).as_bytes());
        assert_eq!(streamed.unwrap(), Streamed::Changed(vec![]));
    }

    #[test]
    fn tells_when_to_watch_or_subscribe_again() {
        let closed = "<m:ConnectionStatus>Closed</m:ConnectionStatus>";
        let streamed = read_envelope(envelope(&message("Success", "NoError", closed)).as_bytes());
        assert_eq!(streamed.unwrap(), Streamed::Closed);
        let expired = message("Error", "ErrorSubscriptionNotFound", "");
        let streamed = read_envelope(envelope(&expired).as_bytes());
        assert_eq!(streamed.unwrap(), Streamed::Unsubscribed);
        let busy = message("Error", "ErrorServerBusy", "");
        assert!(matches!(
            read_envelope(envelope(&busy).as_bytes()),
            Err(Error::Throttled { .. })
        ));
    }
}
