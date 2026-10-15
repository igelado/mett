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

/// Why an address list could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("bad address {0:?}")]
pub struct AddressError(pub String);

impl Address {
    /// Parses a comma-separated list of addresses, as typed in a `To:`
    /// prompt: `alice@contoso.com`, `Alice <alice@contoso.com>` or
    /// `"Example, Alice" <alice@contoso.com>`. Empty entries are skipped.
    pub fn parse_list(text: &str) -> Result<Vec<Address>, AddressError> {
        split_list(text)
            .into_iter()
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(Address::parse)
            .collect()
    }

    /// Parses one address (see [`Address::parse_list`]).
    pub fn parse(entry: &str) -> Result<Address, AddressError> {
        let bad = || AddressError(entry.to_owned());
        let entry = entry.trim();
        let (name, email) = match entry.rfind('<') {
            Some(open) => {
                let email = entry[open + 1..].strip_suffix('>').ok_or_else(bad)?;
                let name = entry[..open].trim();
                let name = match name.strip_prefix('"') {
                    Some(quoted) => unquote(quoted.strip_suffix('"').ok_or_else(bad)?),
                    // An unquoted name can't hold brackets: `A <a@x, B <b@y>`
                    // is a mistake, not B alone.
                    None if name.contains(['<', '>']) => return Err(bad()),
                    None => name.to_owned(),
                };
                (Some(name).filter(|n| !n.is_empty()), email.trim())
            }
            None => (None, entry),
        };
        let valid = email.split_once('@').is_some_and(|(user, domain)| {
            !user.is_empty()
                && !domain.is_empty()
                && !domain.contains('@')
                && !email.contains(|c: char| c.is_whitespace() || "<>\",;".contains(c))
        });
        if !valid {
            return Err(bad());
        }
        Ok(Address {
            name,
            email: email.to_owned(),
        })
    }

    /// Formats addresses as a comma-separated list that
    /// [`Address::parse_list`] reads back, quoting names where needed.
    pub fn format_list(addresses: &[Address]) -> String {
        addresses
            .iter()
            .map(|a| match a.name.as_deref().filter(|n| !n.is_empty()) {
                Some(name) if name.contains(['"', ',', ';', '<', '>', '@', '\\']) => {
                    let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
                    format!("\"{escaped}\" <{}>", a.email)
                }
                _ => a.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Splits an address list at commas outside quotes and angle brackets.
fn split_list(text: &str) -> Vec<&str> {
    let mut entries = Vec::new();
    let (mut start, mut quoted, mut angle, mut escaped) = (0, false, false, false);
    for (i, c) in text.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' if !angle => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' if !quoted && !angle => {
                entries.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    entries.push(&text[start..]);
    entries
}

/// Undoes backslash escapes in a quoted name.
fn unquote(quoted: &str) -> String {
    let mut out = String::new();
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            c => out.push(c),
        }
    }
    out
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
    /// Whether it has no sent date, as an unsent draft: it is dated when
    /// it was created, and comes apart from the rest when listed by date.
    pub draft: bool,
    /// When it reached the mailbox, if the backend says (`received` is
    /// when it was sent).
    pub delivered: Option<DateTime<Utc>>,
    /// When it last changed (arrived, was moved, read, flagged), if the
    /// backend says.
    pub modified: Option<DateTime<Utc>>,
    /// Where it is in its conversation, for threading, if the backend
    /// knows.
    pub thread: Option<ThreadPosition>,
}

/// Where a message is in its conversation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThreadPosition {
    /// The conversation it belongs to.
    pub conversation: String,
    /// Its path in the conversation's tree, from the start of the
    /// conversation: a message's parent is the message whose path is this
    /// one without its last step. Empty if only the conversation is known.
    pub path: Vec<Vec<u8>>,
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
    /// The `Bcc:` recipients, which only the sender's copy has.
    pub bcc: Vec<Address>,
    /// The `Reply-To:` addresses, if the sender asked for replies to go
    /// elsewhere.
    pub reply_to: Vec<Address>,
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
    fn parses_address_lists() {
        let list = Address::parse_list(
            r#"alice@contoso.com, Bob Smith <bob@fabrikam.com>,, "Doe, Jane \"JD\"" <jane@x.org>"#,
        )
        .unwrap();
        assert_eq!(
            list,
            [
                Address::new("alice@contoso.com"),
                Address::new("bob@fabrikam.com").with_name("Bob Smith"),
                Address::new("jane@x.org").with_name(r#"Doe, Jane "JD""#),
            ]
        );
        assert_eq!(Address::parse_list("  ").unwrap(), []);
        assert_eq!(
            Address::parse("<carol@x.org>").unwrap(),
            Address::new("carol@x.org")
        );
        for bad in [
            "alice",
            "a@b@c",
            "Bob <bob@x",
            "x y@z",
            "\"Bob <b@x>",
            "@x",
            "a@",
            "Alice <a@x.org, Bob <b@y.org>",
            "Bob <b@y.org>>",
        ] {
            assert!(Address::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn formatted_lists_parse_back() {
        let list = vec![
            Address::new("alice@contoso.com"),
            Address::new("jane@x.org").with_name(r#"Doe, Jane "JD" \o/"#),
            Address::new("bob@x.org").with_name("Bob"),
        ];
        let text = Address::format_list(&list);
        assert_eq!(Address::parse_list(&text).unwrap(), list);
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
