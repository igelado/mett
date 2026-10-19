//! SOAP for Exchange Web Services: writing requests and reading their
//! responses.
//!
//! Requests are written as text: they are small, and their shape is
//! fixed. Responses are read with `roxmltree`, by local name, so the
//! namespace prefixes Exchange picks don't matter.

use std::time::Duration;

use mett_core::Error;
use roxmltree::{Document, Node};

/// The EWS schema version mett asks for: Exchange 2013 SP1, which Exchange
/// Online and every supported Exchange Server take, and which has the
/// follow-up flag (`item:Flag`).
const SERVER_VERSION: &str = "Exchange2013_SP1";

/// A request `body` (the operation's element, with the `m:` and `t:`
/// prefixes) in a SOAP envelope.
pub(crate) fn envelope(body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <soap:Envelope xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\" \
         xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\">\
         <soap:Header><t:RequestServerVersion Version=\"{SERVER_VERSION}\"/></soap:Header>\
         <soap:Body>{body}</soap:Body></soap:Envelope>"
    )
}

/// `text` escaped for XML text and attribute values. Characters XML 1.0
/// can't hold at all (most control characters) are left out.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if c < ' ' || matches!(c, '\u{FFFE}' | '\u{FFFF}') => {}
            c => out.push(c),
        }
    }
    out
}

/// The object a request concerns, named in a "not found" error.
#[derive(Clone, Copy)]
pub(crate) struct About<'a> {
    pub kind: &'static str,
    pub id: &'a str,
}

impl<'a> About<'a> {
    pub const MAILBOX: About<'static> = About {
        kind: "mailbox",
        id: "me",
    };

    pub fn folder(id: &'a str) -> Self {
        Self { kind: "folder", id }
    }

    pub fn message(id: &'a str) -> Self {
        Self {
            kind: "message",
            id,
        }
    }

    pub fn attachment(id: &'a str) -> Self {
        Self {
            kind: "attachment",
            id,
        }
    }
}

/// Parses a response body.
pub(crate) fn parse(xml: &str) -> Result<Document<'_>, Error> {
    Document::parse(xml).map_err(|e| Error::Backend(format!("unexpected response from EWS: {e}")))
}

/// The response messages of a parsed response (one per object the request
/// named, in order), or the SOAP fault it holds.
pub(crate) fn response_messages<'a, 'i>(
    doc: &'a Document<'i>,
    about: About<'_>,
) -> Result<Vec<Node<'a, 'i>>, Error> {
    let root = doc.root_element();
    if let Some(fault) = root.descendants().find(|n| n.has_tag_name_local("Fault")) {
        return Err(fault_error(fault, about));
    }
    let messages = root
        .descendants()
        .find(|n| n.has_tag_name_local("ResponseMessages"))
        .ok_or_else(|| {
            Error::Backend("unexpected response from EWS: no response messages".into())
        })?;
    Ok(messages.children().filter(Node::is_element).collect())
}

/// The single response message of a response about one object, checked.
pub(crate) fn only_message<'a, 'i>(
    doc: &'a Document<'i>,
    about: About<'_>,
) -> Result<Node<'a, 'i>, Error> {
    let message = response_messages(doc, about)?
        .into_iter()
        .next()
        .ok_or_else(|| Error::Backend("unexpected response from EWS: no response".into()))?;
    check(message, about)?;
    Ok(message)
}

/// The error a response message reports, if it failed. Warnings (such as
/// a name that resolves to several people) are successes.
pub(crate) fn check(message: Node<'_, '_>, about: About<'_>) -> Result<(), Error> {
    if message.attribute("ResponseClass") != Some("Error") {
        return Ok(());
    }
    let code = text(message, "ResponseCode").unwrap_or_default();
    let detail = text(message, "MessageText").unwrap_or_default();
    Err(error_for(&code, &detail, back_off(message), about))
}

/// The response code of a response message (`NoError` when it succeeded).
pub(crate) fn response_code(message: Node<'_, '_>) -> String {
    text(message, "ResponseCode").unwrap_or_else(|| "NoError".into())
}

/// Maps an EWS response code onto a port error.
pub(crate) fn error_for(
    code: &str,
    detail: &str,
    retry_after: Option<Duration>,
    about: About<'_>,
) -> Error {
    let message = || {
        if detail.is_empty() {
            code.to_owned()
        } else {
            format!("{code}: {detail}")
        }
    };
    match code {
        "ErrorItemNotFound"
        | "ErrorFolderNotFound"
        | "ErrorAttachmentNotFound"
        | "ErrorInvalidAttachmentId"
        | "ErrorCannotOpenFileAttachment" => Error::NotFound {
            kind: about.kind,
            id: about.id.to_owned(),
        },
        "ErrorServerBusy" | "ErrorExceededConnectionCount" | "ErrorTooManyObjectsOpened" => {
            Error::Throttled { retry_after }
        }
        "ErrorAccessDenied"
        | "ErrorInvalidSecurityToken"
        | "ErrorNoPermission"
        | "ErrorImpersonationDenied"
        | "ErrorNonExistentMailbox"
        | "ErrorMailboxConfiguration" => Error::Auth(message()),
        "ErrorTimeoutExpired"
        | "ErrorInternalServerTransientError"
        | "ErrorMailboxStoreUnavailable"
        | "ErrorMailboxMoveInProgress"
        | "ErrorConnectionFailed"
        | "ErrorBatchProcessingStopped" => Error::Network(message()),
        _ => Error::Backend(message()),
    }
}

/// The error the SOAP fault in a failed response reports, if it says which
/// (with an Exchange response code). Without one, only the HTTP status
/// tells.
pub(crate) fn coded_fault(doc: &Document<'_>, about: About<'_>) -> Option<Error> {
    let fault = doc.descendants().find(|n| n.has_tag_name_local("Fault"))?;
    text(fault, "ResponseCode")?;
    Some(fault_error(fault, about))
}

/// The error a SOAP fault reports.
fn fault_error(fault: Node<'_, '_>, about: About<'_>) -> Error {
    let detail = text(fault, "faultstring").unwrap_or_default();
    match text(fault, "ResponseCode") {
        Some(code) => error_for(&code, &detail, back_off(fault), about),
        None => Error::Backend(format!("EWS failed: {detail}")),
    }
}

/// How long the server asked the client to wait, from a throttling error's
/// `BackOffMilliseconds`.
fn back_off(node: Node<'_, '_>) -> Option<Duration> {
    node.descendants()
        .find(|n| {
            n.has_tag_name_local("Value") && n.attribute("Name") == Some("BackOffMilliseconds")
        })
        .and_then(|n| n.text())
        .and_then(|ms| ms.trim().parse().ok())
        .map(Duration::from_millis)
}

/// Lookups by local name, whatever the namespace.
pub(crate) trait LocalName {
    /// Whether this is an element named `name`.
    fn has_tag_name_local(&self, name: &str) -> bool;
}

impl LocalName for Node<'_, '_> {
    fn has_tag_name_local(&self, name: &str) -> bool {
        self.is_element() && self.tag_name().name() == name
    }
}

/// The first child element of `node` named `name`.
pub(crate) fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|n| n.has_tag_name_local(name))
}

/// The child elements of `node` named `name`.
pub(crate) fn children<'a, 'i: 'a>(
    node: Node<'a, 'i>,
    name: &'a str,
) -> impl Iterator<Item = Node<'a, 'i>> + 'a {
    node.children().filter(move |n| n.has_tag_name_local(name))
}

/// The first element below `node` named `name`.
pub(crate) fn descendant<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.descendants().find(|n| n.has_tag_name_local(name))
}

/// The text of the first element below `node` named `name`.
pub(crate) fn text(node: Node<'_, '_>, name: &str) -> Option<String> {
    descendant(node, name).map(|n| n.text().unwrap_or_default().to_owned())
}

/// The text of `node`'s child element `name`.
pub(crate) fn child_text(node: Node<'_, '_>, name: &str) -> Option<String> {
    child(node, name).map(|n| n.text().unwrap_or_default().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_and_drops_what_xml_cannot_hold() {
        assert_eq!(
            escape("a<b>&\"c'\u{1}\td\n"),
            "a&lt;b&gt;&amp;&quot;c&apos;\td\n"
        );
    }

    #[test]
    fn envelopes_ask_for_the_server_version() {
        let xml = envelope("<m:GetFolder/>");
        let doc = Document::parse(&xml).unwrap();
        let version = doc
            .descendants()
            .find(|n| n.has_tag_name_local("RequestServerVersion"))
            .unwrap();
        assert_eq!(version.attribute("Version"), Some(SERVER_VERSION));
        assert!(doc.descendants().any(|n| n.has_tag_name_local("GetFolder")));
    }

    const NOT_FOUND: &str = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
        <s:Body><m:GetItemResponse xmlns:m="http://schemas.microsoft.com/exchange/services/2006/messages">
        <m:ResponseMessages><m:GetItemResponseMessage ResponseClass="Error">
        <m:MessageText>The specified object was not found in the store.</m:MessageText>
        <m:ResponseCode>ErrorItemNotFound</m:ResponseCode>
        </m:GetItemResponseMessage></m:ResponseMessages></m:GetItemResponse></s:Body></s:Envelope>"#;

    #[test]
    fn failed_messages_map_to_port_errors() {
        let doc = Document::parse(NOT_FOUND).unwrap();
        let e = only_message(&doc, About::message("m1")).unwrap_err();
        assert!(matches!(e, Error::NotFound { kind: "message", ref id } if id == "m1"));
    }

    #[test]
    fn busy_servers_say_how_long_to_wait() {
        let fault = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
            <s:Body><s:Fault><faultcode>a:ErrorServerBusy</faultcode>
            <faultstring>The server cannot service this request right now.</faultstring>
            <detail><e:ResponseCode xmlns:e="http://schemas.microsoft.com/exchange/services/2006/errors">ErrorServerBusy</e:ResponseCode>
            <e:MessageXml xmlns:e="http://schemas.microsoft.com/exchange/services/2006/errors">
            <t:Value xmlns:t="http://schemas.microsoft.com/exchange/services/2006/types" Name="BackOffMilliseconds">2500</t:Value>
            </e:MessageXml></detail></s:Fault></s:Body></s:Envelope>"#;
        let doc = Document::parse(fault).unwrap();
        let e = response_messages(&doc, About::MAILBOX).unwrap_err();
        assert!(matches!(
            e,
            Error::Throttled { retry_after: Some(d) } if d == Duration::from_millis(2500)
        ));
    }

    #[test]
    fn only_faults_with_a_code_say_what_failed() {
        let fault = |detail: &str| {
            format!(
                r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
                <s:Fault><faultstring>No.</faultstring><detail>{detail}</detail></s:Fault>
                </s:Body></s:Envelope>"#
            )
        };
        let coded = fault("<e:ResponseCode xmlns:e=\"errors\">ErrorQuotaExceeded</e:ResponseCode>");
        let doc = Document::parse(&coded).unwrap();
        assert!(matches!(
            coded_fault(&doc, About::MAILBOX),
            Some(Error::Backend(m)) if m.contains("ErrorQuotaExceeded")
        ));
        let bare = fault("");
        assert!(coded_fault(&Document::parse(&bare).unwrap(), About::MAILBOX).is_none());
    }

    #[test]
    fn warnings_are_successes() {
        let xml = NOT_FOUND.replace("\"Error\"", "\"Warning\"");
        let doc = Document::parse(&xml).unwrap();
        assert!(only_message(&doc, About::MAILBOX).is_ok());
    }
}
