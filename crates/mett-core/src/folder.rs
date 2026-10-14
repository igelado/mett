//! Mail folders.

use std::fmt;
use std::str::FromStr;

use crate::ids::FolderId;

/// A mail folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    /// The folder's identifier.
    pub id: FolderId,
    /// The parent folder, or `None` for a top-level folder.
    pub parent: Option<FolderId>,
    /// The display name. It may be localised, so don't match on it; use
    /// [`Folder::well_known`] instead.
    pub name: String,
    /// The folder's well-known role, if it has one.
    pub well_known: Option<WellKnownFolder>,
    /// Number of unread items. Backends may count items other than
    /// messages (Office365 counts every Outlook item in the folder).
    pub unread_count: u32,
    /// Total number of items, counted as for [`Folder::unread_count`].
    pub total_count: u32,
}

/// A folder with a standard role, independent of the mailbox's language.
///
/// The names match Microsoft Graph's well-known folder names, which mett
/// exposes in `mettrc` as `@name` (see `docs/CONFIG-OFFICE365.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WellKnownFolder {
    /// `inbox`
    Inbox,
    /// `drafts`
    Drafts,
    /// `sentitems`
    SentItems,
    /// `deleteditems`
    DeletedItems,
    /// `junkemail`
    JunkEmail,
    /// `archive`
    Archive,
    /// `outbox`
    Outbox,
}

impl WellKnownFolder {
    /// All well-known folders.
    pub const ALL: [Self; 7] = [
        Self::Inbox,
        Self::Drafts,
        Self::SentItems,
        Self::DeletedItems,
        Self::JunkEmail,
        Self::Archive,
        Self::Outbox,
    ];

    /// The well-known name, as used by Microsoft Graph and in `mettrc`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Drafts => "drafts",
            Self::SentItems => "sentitems",
            Self::DeletedItems => "deleteditems",
            Self::JunkEmail => "junkemail",
            Self::Archive => "archive",
            Self::Outbox => "outbox",
        }
    }
}

impl fmt::Display for WellKnownFolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when parsing an unknown well-known folder name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown well-known folder: {0}")]
pub struct UnknownWellKnownFolder(pub String);

impl FromStr for WellKnownFolder {
    type Err = UnknownWellKnownFolder;

    /// Parses a well-known name, ignoring ASCII case.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|w| w.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| UnknownWellKnownFolder(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_names_round_trip() {
        for w in WellKnownFolder::ALL {
            assert_eq!(w.as_str().parse::<WellKnownFolder>(), Ok(w));
        }
    }

    #[test]
    fn well_known_parse_ignores_case() {
        assert_eq!("SentItems".parse(), Ok(WellKnownFolder::SentItems));
    }

    #[test]
    fn well_known_parse_rejects_unknown() {
        assert_eq!(
            "spam".parse::<WellKnownFolder>(),
            Err(UnknownWellKnownFolder("spam".into()))
        );
    }
}
