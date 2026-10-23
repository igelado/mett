//! Identifiers: what a message or folder is called to mett.
//!
//! A folder is its name on the server, as text. A message is its folder, the
//! folder's `UIDVALIDITY` and its `UID` there: `<validity>.<uid>.<folder>`.
//! The UID changes when the message moves, and a folder that the server
//! made anew (another `UIDVALIDITY`) has none of the messages that went
//! by its name before.

use mett_core::{Error, FolderId, MessageId, Result};

/// Where a message is on the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ref {
    /// The folder's name.
    pub folder: String,
    /// The folder's `UIDVALIDITY`.
    pub validity: u32,
    /// The message's `UID`.
    pub uid: u32,
}

impl Ref {
    pub fn id(&self) -> MessageId {
        MessageId::new(format!("{}.{}.{}", self.validity, self.uid, self.folder))
    }

    pub fn parse(id: &MessageId) -> Result<Self> {
        let mut parts = id.as_str().splitn(3, '.');
        let (Some(validity), Some(uid), Some(folder)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(Error::not_found("message", id));
        };
        match (validity.parse(), uid.parse()) {
            (Ok(validity), Ok(uid)) => Ok(Self {
                folder: folder.to_owned(),
                validity,
                uid,
            }),
            _ => Err(Error::not_found("message", id)),
        }
    }

    pub fn folder_id(&self) -> FolderId {
        FolderId::new(&self.folder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_id_keeps_dots_in_the_folder_name() {
        let at = Ref {
            folder: "INBOX.Work.2026".into(),
            validity: 7,
            uid: 42,
        };
        assert_eq!(at.id().as_str(), "7.42.INBOX.Work.2026");
        assert_eq!(Ref::parse(&at.id()).unwrap(), at);
    }

    #[test]
    fn rubbish_is_not_a_message() {
        for id in ["", "1", "1.2", "x.2.INBOX", "1.y.INBOX"] {
            assert!(matches!(
                Ref::parse(&MessageId::new(id)),
                Err(Error::NotFound { .. })
            ));
        }
    }
}
