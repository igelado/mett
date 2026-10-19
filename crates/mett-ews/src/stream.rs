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

/// The complete envelopes at the start of `buffer`, which they are taken
/// off; what is left is the start of one still on its way.
pub(crate) fn take_envelopes(buffer: &mut Vec<u8>) -> Vec<Vec<u8>> {
    let mut envelopes = Vec::new();
    let mut from = 0;
    while let Some(end) = envelope_end(&buffer[from..]) {
        envelopes.push(buffer[from..from + end].to_vec());
        from += end;
    }
    buffer.drain(..from);
    envelopes
}

/// Where the first envelope in `bytes` ends: just after its closing tag
/// (`</soap:Envelope>`, whatever its prefix), if it is there.
fn envelope_end(bytes: &[u8]) -> Option<usize> {
    const NAME: &[u8] = b"Envelope>";
    let mut from = 0;
    while let Some(at) = find(&bytes[from..], NAME).map(|at| from + at) {
        let end = at + NAME.len();
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
        let envelopes = take_envelopes(&mut buffer);
        assert_eq!(envelopes, [first.into_bytes(), second.into_bytes()]);
        assert_eq!(buffer, b"<s:Envelope xmlns:s=\"x\"><s:Bo");
        // An envelope without a prefix ends too, and text that only looks
        // like the tag doesn't.
        let mut buffer = b"<Envelope><Body>NotEnvelope></Body></Envelope>".to_vec();
        assert_eq!(take_envelopes(&mut buffer).len(), 1);
        assert!(buffer.is_empty());
    }

    #[test]
    fn envelopes_may_be_apart() {
        let one = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>{}",
            envelope(&message("Success", "NoError", ""))
        );
        let mut buffer = format!("{one}\r\n{one}").into_bytes();
        for envelope in take_envelopes(&mut buffer) {
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
