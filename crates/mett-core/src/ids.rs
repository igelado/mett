//! Strongly typed identifiers.
//!
//! Each kind of identifier is a distinct newtype so that, for example, a
//! folder ID can never be passed where a message ID is expected. The values
//! are opaque: they are whatever the backend uses (for Microsoft Graph,
//! base64-like strings).

use std::fmt;

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Wraps a backend identifier.
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            /// Returns the identifier as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(id: &str) -> Self {
                Self::new(id)
            }
        }

        impl From<String> for $name {
            fn from(id: String) -> Self {
                Self(id)
            }
        }
    };
}

string_id!(
    /// An account, identified by its user principal name (for example
    /// `alice@contoso.com`).
    AccountId
);
string_id!(
    /// A mail folder.
    FolderId
);
string_id!(
    /// A message.
    MessageId
);
string_id!(
    /// An opaque cursor to the next page of a listing.
    PageCursor
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_display() {
        let id = MessageId::new("AAMkAG=");
        assert_eq!(id.as_str(), "AAMkAG=");
        assert_eq!(id.to_string(), "AAMkAG=");
        assert_eq!(MessageId::from("AAMkAG="), id);
        assert_eq!(FolderId::from(String::from("inbox")).as_str(), "inbox");
    }
}
