//! Attachments: those of a message, and files attached to one being
//! written.

use std::fmt;

use crate::ids::AttachmentId;

/// What an attachment is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentKind {
    /// A file.
    File,
    /// A message (or other mailbox item) attached whole: its content is
    /// the item as MIME.
    Item,
    /// A link to a file kept elsewhere (such as OneDrive), with no content
    /// of its own.
    Link,
}

/// An attachment of a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// The attachment's identifier, within its message.
    pub id: AttachmentId,
    /// Its file name.
    pub name: String,
    /// Its MIME type, such as `text/plain`.
    pub content_type: String,
    /// Its size in bytes, as the backend counts it.
    pub size: u64,
    /// Whether it is shown in the body (such as an image in HTML) rather
    /// than attached for its own sake.
    pub inline: bool,
    /// What it is.
    pub kind: AttachmentKind,
}

/// A file attached to a message being written.
#[derive(Clone, PartialEq, Eq)]
pub struct OutgoingAttachment {
    /// The file name the recipients see.
    pub name: String,
    /// Its MIME type.
    pub content_type: String,
    /// Its content.
    pub data: Vec<u8>,
}

impl fmt::Debug for OutgoingAttachment {
    /// Leaves the content out: it may be large.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutgoingAttachment")
            .field("name", &self.name)
            .field("content_type", &self.content_type)
            .field("size", &self.data.len())
            .finish()
    }
}

/// The MIME type of a file, going by its name's extension, as mutt's
/// `mime.types` lookup does for the common ones.
pub fn content_type_for(name: &str) -> &'static str {
    let extension = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "txt" | "text" | "log" | "md" | "rs" | "c" | "h" | "py" | "sh" | "toml" | "yaml"
        | "yml" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "tar" => "application/x-tar",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ics" => "text/calendar",
        "eml" => "message/rfc822",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_types_by_extension() {
        assert_eq!(content_type_for("notes.TXT"), "text/plain");
        assert_eq!(content_type_for("report.pdf"), "application/pdf");
        assert_eq!(content_type_for("no-extension"), "application/octet-stream");
    }

    #[test]
    fn debug_leaves_out_the_content() {
        let attachment = OutgoingAttachment {
            name: "a.bin".into(),
            content_type: "application/octet-stream".into(),
            data: vec![1, 2, 3],
        };
        assert_eq!(
            format!("{attachment:?}"),
            r#"OutgoingAttachment { name: "a.bin", content_type: "application/octet-stream", size: 3 }"#
        );
    }
}
