//! Messages and their parts.

use std::fmt;

use chrono::{DateTime, Utc};

use crate::ids::{FolderId, MessageId};

/// An e-mail address with an optional display name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    /// The display name, for example `Alice Example`.
    pub name: Option<String>,
    /// The address itself, for example `alice@contoso.com`.
    pub email: String,
}

impl Address {
    /// Creates an address without a display name.
    pub fn new(email: impl Into<String>) -> Self {
        Self {
            name: None,
            email: email.into(),
        }
    }

    /// Sets the display name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

impl fmt::Display for Address {
    /// Formats as `Name <email>`, or just `email` without a name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) if !name.is_empty() => write!(f, "{name} <{}>", self.email),
            _ => f.write_str(&self.email),
        }
    }
}

/// Message state flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    /// The message has been read.
    pub read: bool,
    /// The message is flagged for follow-up.
    pub flagged: bool,
}

/// A change to a message's flags. `None` leaves a flag unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlagsUpdate {
    /// New value of [`Flags::read`].
    pub read: Option<bool>,
    /// New value of [`Flags::flagged`].
    pub flagged: Option<bool>,
}

impl FlagsUpdate {
    /// Applies the update to `flags`.
    pub fn apply(self, flags: &mut Flags) {
        if let Some(read) = self.read {
            flags.read = read;
        }
        if let Some(flagged) = self.flagged {
            flags.flagged = flagged;
        }
    }
}

/// What a message index line needs: everything except the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageSummary {
    /// The message's identifier.
    pub id: MessageId,
    /// The folder that holds the message.
    pub folder: FolderId,
    /// The subject line.
    pub subject: String,
    /// The sender, if known.
    pub from: Option<Address>,
    /// The `To:` recipients.
    pub to: Vec<Address>,
    /// The message's date, which the index shows and sorts by: when it was
    /// sent (mutt's default index date), or created for an unsent draft.
    pub received: DateTime<Utc>,
    /// Read and flagged state.
    pub flags: Flags,
    /// Whether the message has attachments.
    pub has_attachments: bool,
}

/// The format of a message body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    /// Plain text.
    Text,
    /// HTML, to be rendered for the terminal.
    Html,
}

/// A message body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Body {
    /// The format of [`Body::content`].
    pub kind: BodyKind,
    /// The body content.
    pub content: String,
}

/// A full message, as shown in the pager.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// The fields shown in the index.
    pub summary: MessageSummary,
    /// The `Cc:` recipients.
    pub cc: Vec<Address>,
    /// The `Message-ID:` header, if present.
    pub internet_message_id: Option<String>,
    /// The body.
    pub body: Body,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_display() {
        assert_eq!(Address::new("a@b.c").to_string(), "a@b.c");
        assert_eq!(
            Address::new("a@b.c").with_name("Alice").to_string(),
            "Alice <a@b.c>"
        );
        assert_eq!(Address::new("a@b.c").with_name("").to_string(), "a@b.c");
    }

    #[test]
    fn flags_update_changes_only_given_flags() {
        let mut flags = Flags {
            read: false,
            flagged: true,
        };
        FlagsUpdate {
            read: Some(true),
            flagged: None,
        }
        .apply(&mut flags);
        assert_eq!(
            flags,
            Flags {
                read: true,
                flagged: true
            }
        );
    }
}
