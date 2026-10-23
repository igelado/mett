//! Where each folder had got to when it was last asked about new mail.
//!
//! IMAP can't be asked what arrived since a time: a message copied or moved
//! into a folder keeps the date it came in with, and a flag that another
//! client changes dates nothing. So the adapter remembers, for each folder
//! and each asker, the `UID` the next message would have (everything from
//! there on is new) and, where the server has `CONDSTORE`, the folder's
//! highest modification sequence (everything changed after it is changed).

use std::collections::HashMap;
use std::sync::Mutex;

/// What asks about new mail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Watch {
    /// `MailStore::changed_messages`.
    Changes,
    /// `MailStore::recent_unread`.
    Unread,
}

/// How a folder stood: everything after this is new.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Floor {
    /// The `UID` the next message had.
    pub next: u32,
    /// The highest modification sequence, if the server has them.
    pub modseq: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
struct Mark {
    /// The folder's `UIDVALIDITY`: a mark of another is no mark.
    validity: u32,
    /// How the folder stood when last asked about.
    now: Floor,
    /// What that was compared with, for the pages of the same scan that
    /// come after the first.
    scanning: Option<Floor>,
}

/// The marks of every folder.
#[derive(Default)]
pub(crate) struct Marks(Mutex<HashMap<(Watch, String), Mark>>);

impl Marks {
    /// What to compare the folder with: how it stood when last asked (the
    /// first page of a scan), or what the scan under way compares with (the
    /// pages after it). `None` if it was never asked about.
    pub fn floor(&self, watch: Watch, folder: &str, validity: u32, first: bool) -> Option<Floor> {
        let marks = self.0.lock().expect("no panic holds the marks");
        let mark = marks.get(&(watch, folder.to_owned()))?;
        if mark.validity != validity {
            return None;
        }
        if first { Some(mark.now) } else { mark.scanning }
    }

    /// Notes how the folder stands when it is first listed, for both
    /// askers, unless they have marks already (the polls list it again): what
    /// comes in between the listing and the first poll is new, whatever its
    /// date says.
    pub fn start(&self, folder: &str, validity: u32, now: Floor) {
        let mut marks = self.0.lock().expect("no panic holds the marks");
        for watch in [Watch::Changes, Watch::Unread] {
            let key = (watch, folder.to_owned());
            if marks.get(&key).is_none_or(|mark| mark.validity != validity) {
                marks.insert(
                    key,
                    Mark {
                        validity,
                        now,
                        scanning: None,
                    },
                );
            }
        }
    }

    /// Takes back the mark a scan moved on, because a later page of it
    /// failed: the next scan compares with what this one did.
    pub fn rollback(&self, watch: Watch, folder: &str) {
        let mut marks = self.0.lock().expect("no panic holds the marks");
        let key = (watch, folder.to_owned());
        let restored = marks
            .get_mut(&key)
            .map(|mark| mark.scanning.inspect(|floor| mark.now = *floor).is_some());
        // A scan that began with no mark leaves none.
        if restored == Some(false) {
            marks.remove(&key);
        }
    }

    /// Notes that the folder stands as `now`, after a scan that compared
    /// it with `floor`. A scan that failed notes nothing.
    pub fn commit(
        &self,
        watch: Watch,
        folder: &str,
        validity: u32,
        now: Floor,
        floor: Option<Floor>,
    ) {
        let mut marks = self.0.lock().expect("no panic holds the marks");
        marks.insert(
            (watch, folder.to_owned()),
            Mark {
                validity,
                now,
                scanning: floor,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn floor(next: u32) -> Floor {
        Floor { next, modseq: None }
    }

    #[test]
    fn a_folder_asked_about_for_the_first_time_has_no_floor() {
        let marks = Marks::default();
        assert_eq!(marks.floor(Watch::Changes, "INBOX", 1, true), None);
    }

    #[test]
    fn the_pages_of_a_scan_share_the_floor_of_its_first() {
        let marks = Marks::default();
        marks.commit(Watch::Changes, "INBOX", 1, floor(10), None);
        // The next scan starts from where the last one got.
        assert_eq!(
            marks.floor(Watch::Changes, "INBOX", 1, true),
            Some(floor(10))
        );
        marks.commit(Watch::Changes, "INBOX", 1, floor(20), Some(floor(10)));
        // Its later pages compare with what its first did.
        assert_eq!(
            marks.floor(Watch::Changes, "INBOX", 1, false),
            Some(floor(10))
        );
        assert_eq!(
            marks.floor(Watch::Changes, "INBOX", 1, true),
            Some(floor(20))
        );
    }

    #[test]
    fn askers_and_folders_have_marks_of_their_own() {
        let marks = Marks::default();
        marks.commit(Watch::Changes, "INBOX", 1, floor(10), None);
        assert_eq!(marks.floor(Watch::Unread, "INBOX", 1, true), None);
        assert_eq!(marks.floor(Watch::Changes, "Sent", 1, true), None);
    }

    #[test]
    fn listing_a_folder_marks_it_once() {
        let marks = Marks::default();
        marks.start("INBOX", 1, floor(10));
        assert_eq!(
            marks.floor(Watch::Changes, "INBOX", 1, true),
            Some(floor(10))
        );
        assert_eq!(
            marks.floor(Watch::Unread, "INBOX", 1, true),
            Some(floor(10))
        );
        // The polls list it again: that doesn't move the mark.
        marks.start("INBOX", 1, floor(20));
        assert_eq!(
            marks.floor(Watch::Changes, "INBOX", 1, true),
            Some(floor(10))
        );
        // A folder made anew is marked anew.
        marks.start("INBOX", 2, floor(5));
        assert_eq!(
            marks.floor(Watch::Changes, "INBOX", 2, true),
            Some(floor(5))
        );
    }

    #[test]
    fn a_scan_that_failed_gives_its_mark_back() {
        let marks = Marks::default();
        marks.commit(Watch::Changes, "INBOX", 1, floor(10), None);
        marks.commit(Watch::Changes, "INBOX", 1, floor(20), Some(floor(10)));
        marks.rollback(Watch::Changes, "INBOX");
        assert_eq!(
            marks.floor(Watch::Changes, "INBOX", 1, true),
            Some(floor(10))
        );
        // One that began with no mark leaves none.
        marks.commit(Watch::Unread, "INBOX", 1, floor(20), None);
        marks.rollback(Watch::Unread, "INBOX");
        assert_eq!(marks.floor(Watch::Unread, "INBOX", 1, true), None);
    }

    #[test]
    fn a_folder_made_anew_has_no_mark() {
        let marks = Marks::default();
        marks.commit(Watch::Changes, "INBOX", 1, floor(10), None);
        assert_eq!(marks.floor(Watch::Changes, "INBOX", 2, true), None);
    }
}
