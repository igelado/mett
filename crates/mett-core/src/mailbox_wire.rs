//! What the mailbox adapters (Graph, EWS) share in reading a mailbox: the
//! timestamps and conversation indexes they get, the page limit, and the
//! bookkeeping of folder roles.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::{Folder, FolderId, WellKnownFolder};

/// The well-known name of the root of the mail folders, which top-level
/// folders hang from.
pub const ROOT: &str = "msgfolderroot";

/// The most items or folders a mailbox backend returns per request.
pub const MAX_PAGE_SIZE: u32 = 1000;

/// A backend's RFC 3339 timestamp (padding around it, as in XML text,
/// ignored), if it parses.
pub fn parse_time(time: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(time.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// A message's path in its conversation, from Outlook's conversation
/// index: the 22-byte header (the conversation's first message), then a
/// 5-byte block for each reply. An index that doesn't decode, or doesn't
/// have that shape, gives none. Padding around the index (as in XML text)
/// is ignored.
pub fn thread_path(index: &str) -> Vec<Vec<u8>> {
    use base64::Engine;

    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(index.trim()) else {
        return Vec::new();
    };
    if bytes.len() < 22 || (bytes.len() - 22) % 5 != 0 {
        return Vec::new();
    }
    let (header, replies) = bytes.split_at(22);
    std::iter::once(header.to_vec())
        .chain(replies.chunks(5).map(<[u8]>::to_vec))
        .collect()
}

/// Which folders play special roles. Folder IDs don't change, so this is
/// looked up once.
#[derive(Debug, Default)]
pub struct Roles {
    /// The root that top-level folders hang from.
    pub root: Option<FolderId>,
    /// Well-known folders by ID.
    pub well_known: HashMap<FolderId, WellKnownFolder>,
}

impl Roles {
    /// The role of the folder `id`, if it is a well-known one.
    pub fn label(&self, id: &FolderId) -> Option<WellKnownFolder> {
        self.well_known.get(id).copied()
    }

    /// A folder with these details, labelled with its role. Top-level
    /// folders hang from the root; the port says they have no parent.
    pub fn folder(&self, folder: impl Into<Folder>) -> Folder {
        let folder = folder.into();
        Folder {
            well_known: self.label(&folder.id),
            parent: folder.parent.filter(|p| Some(p) != self.root.as_ref()),
            ..folder
        }
    }
}

/// The well-known folders a backend can name, by the name it knows them
/// by: `name` gives it, or none for those it can't name.
pub fn nameable(
    name: fn(WellKnownFolder) -> Option<&'static str>,
) -> impl Iterator<Item = (WellKnownFolder, &'static str)> {
    WellKnownFolder::ALL
        .into_iter()
        .filter_map(move |which| name(which).map(|name| (which, name)))
}

#[cfg(test)]
mod tests {
    use base64::Engine;

    use super::*;

    #[test]
    fn reads_rfc3339_times() {
        let time = parse_time(" 2026-03-05T09:30:00+01:00\n").unwrap();
        assert_eq!(time.to_rfc3339(), "2026-03-05T08:30:00+00:00");
        assert!(parse_time("2026-03-05").is_none());
        assert!(parse_time("").is_none());
    }

    #[test]
    fn splits_a_conversation_index_into_its_path() {
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let mut bytes = vec![1u8; 22];
        assert_eq!(thread_path(&encode(&bytes)), [vec![1u8; 22]]);
        bytes.extend([2; 5]);
        bytes.extend([3; 5]);
        let path = thread_path(&format!(" {}\n", encode(&bytes)));
        assert_eq!(path, [vec![1; 22], vec![2; 5], vec![3; 5]]);
        // Not the shape of an index.
        assert!(thread_path(&encode(&[1; 21])).is_empty());
        assert!(thread_path(&encode(&[1; 24])).is_empty());
        assert!(thread_path("not base64!").is_empty());
    }

    fn folder(id: &str, parent: Option<&str>) -> Folder {
        Folder {
            id: FolderId::new(id),
            parent: parent.map(FolderId::new),
            name: id.into(),
            well_known: None,
            unread_count: 1,
            total_count: 2,
        }
    }

    #[test]
    fn labels_folders_by_role_and_hides_the_root() {
        let roles = Roles {
            root: Some(FolderId::new("root")),
            well_known: HashMap::from([(FolderId::new("in"), WellKnownFolder::Inbox)]),
        };
        let inbox = roles.folder(folder("in", Some("root")));
        assert_eq!(inbox.well_known, Some(WellKnownFolder::Inbox));
        assert_eq!(inbox.parent, None);
        assert_eq!((inbox.unread_count, inbox.total_count), (1, 2));
        let other = roles.folder(folder("sub", Some("in")));
        assert_eq!(other.well_known, None);
        assert_eq!(other.parent, Some(FolderId::new("in")));
        assert_eq!(
            roles.label(&FolderId::new("in")),
            Some(WellKnownFolder::Inbox)
        );
        // Without a known root, no parent is hidden.
        let unknown = Roles::default().folder(folder("x", Some("root")));
        assert_eq!(unknown.parent, Some(FolderId::new("root")));
    }

    #[test]
    fn names_the_well_known_folders_a_backend_can() {
        let all: Vec<_> = nameable(|w| Some(w.as_str())).collect();
        assert_eq!(all.len(), WellKnownFolder::ALL.len());
        let fewer: Vec<_> =
            nameable(|w| (w != WellKnownFolder::Archive).then(|| w.as_str())).collect();
        assert_eq!(fewer.len(), all.len() - 1);
        assert!(fewer.iter().all(|(w, _)| *w != WellKnownFolder::Archive));
    }
}
