//! Where each folder had got to when it was last asked about new mail.
//!
//! IMAP can't be asked what arrived since a time: a message copied or moved
//! into a folder keeps the date it came in with, and a flag that another
//! client changes dates nothing. So the adapter remembers, for each folder
//! and each asker, the `UID` the next message would have (everything from
//! there on is new) and, where the server has `CONDSTORE`, the folder's
//! highest modification sequence (everything changed after it is changed).

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use chrono::{DateTime, Utc};

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
    /// How it stood the time before: a scan compares with that, so that
    /// what the last scan found is found again by this one. A check that is
    /// made of more than one request (the worker lists the newest page as it
    /// asks for the changes) may fail after this has noted a scan, and its
    /// result is then thrown away: what it found must not be lost with it.
    /// What is found twice is taken in once, as the app does.
    lag: Option<Floor>,
    /// What that scan was compared with, for the pages of the same scan that
    /// come after the first.
    scanning: Option<Floor>,
}

/// The marks of every folder, and the unread mail found since them that is
/// still unread: a check reports it until it is read, as the user may not
/// have visited the folder yet when the next check comes.
#[derive(Default)]
pub(crate) struct Marks {
    marks: Mutex<HashMap<(Watch, String), Mark>>,
    unread: Mutex<HashMap<String, Found>>,
}

/// The unread mail found new in a folder (of this `UIDVALIDITY`), and when.
type Found = (u32, HashMap<u32, DateTime<Utc>>);

impl Marks {
    /// What to compare the folder with: how it stood the time before the last
    /// it was asked about (the first page of a scan), or what the scan under
    /// way compares with (the pages after it). `None` if it was never asked
    /// about.
    pub fn floor(&self, watch: Watch, folder: &str, validity: u32, first: bool) -> Option<Floor> {
        let marks = self.marks.lock().expect("no panic holds the marks");
        let mark = marks.get(&(watch, folder.to_owned()))?;
        if mark.validity != validity {
            return None;
        }
        if first {
            Some(mark.lag.unwrap_or(mark.now))
        } else {
            mark.scanning
        }
    }

    /// The unread mail found new in the folder, with when it was found: what
    /// `found` adds (at `now`) to what was found before, less what is no
    /// longer in `unseen`. A folder made anew (`validity`) has none from
    /// before.
    pub fn new_unread(
        &self,
        folder: &str,
        validity: u32,
        found: impl IntoIterator<Item = u32>,
        now: DateTime<Utc>,
        unseen: &BTreeSet<u32>,
    ) -> Vec<(u32, DateTime<Utc>)> {
        let mut all = self.unread.lock().expect("no panic holds the unread");
        let entry = all
            .entry(folder.to_owned())
            .or_insert_with(|| (validity, HashMap::new()));
        if entry.0 != validity {
            *entry = (validity, HashMap::new());
        }
        for uid in found {
            entry.1.entry(uid).or_insert(now);
        }
        entry.1.retain(|uid, _| unseen.contains(uid));
        entry.1.iter().map(|(uid, at)| (*uid, *at)).collect()
    }

    /// Notes how the folder stands when it is first listed, for both
    /// askers, unless they have marks already (the polls list it again): what
    /// comes in between the listing and the first poll is new, whatever its
    /// date says.
    pub fn start(&self, folder: &str, validity: u32, now: Floor) {
        let mut marks = self.marks.lock().expect("no panic holds the marks");
        for watch in [Watch::Changes, Watch::Unread] {
            let key = (watch, folder.to_owned());
            if marks.get(&key).is_none_or(|mark| mark.validity != validity) {
                marks.insert(
                    key,
                    Mark {
                        validity,
                        now,
                        lag: None,
                        scanning: None,
                    },
                );
            }
        }
    }

    /// Takes back the mark a scan moved on, because a later page of it
    /// failed: the next scan compares with what this one did.
    pub fn rollback(&self, watch: Watch, folder: &str) {
        let mut marks = self.marks.lock().expect("no panic holds the marks");
        let key = (watch, folder.to_owned());
        let restored = marks.get_mut(&key).map(|mark| {
            mark.lag = None;
            mark.scanning.inspect(|floor| mark.now = *floor).is_some()
        });
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
        let mut marks = self.marks.lock().expect("no panic holds the marks");
        let key = (watch, folder.to_owned());
        let before = marks
            .get(&key)
            .filter(|mark| mark.validity == validity)
            .map(|mark| mark.now);
        marks.insert(
            key,
            Mark {
                validity,
                now,
                lag: before,
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
    }

    #[test]
    fn a_scan_starts_from_where_the_one_before_the_last_got() {
        let marks = Marks::default();
        marks.commit(Watch::Changes, "INBOX", 1, floor(10), None);
        marks.commit(Watch::Changes, "INBOX", 1, floor(20), Some(floor(10)));
        // What the last scan found is found again by the next (the check it
        // was part of may have failed).
        assert_eq!(
            marks.floor(Watch::Changes, "INBOX", 1, true),
            Some(floor(10))
        );
        marks.commit(Watch::Changes, "INBOX", 1, floor(30), Some(floor(10)));
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
    fn unread_mail_found_stays_found_until_it_is_read() {
        let marks = Marks::default();
        let (t0, t1) = (
            DateTime::UNIX_EPOCH,
            DateTime::UNIX_EPOCH + chrono::TimeDelta::minutes(5),
        );
        let unseen: BTreeSet<u32> = [3, 4].into();
        let mut found = marks.new_unread("INBOX", 1, [3], t0, &unseen);
        assert_eq!(found, [(3, t0)]);
        // The next check finds nothing new, and still has it, as first found.
        found = marks.new_unread("INBOX", 1, [], t1, &unseen);
        assert_eq!(found, [(3, t0)]);
        // Read since, it is gone.
        found = marks.new_unread("INBOX", 1, [], t1, &[4].into());
        assert!(found.is_empty());
        // A folder made anew forgets.
        marks.new_unread("INBOX", 1, [4], t1, &[4].into());
        assert!(marks.new_unread("INBOX", 2, [], t1, &[4].into()).is_empty());
    }

    #[test]
    fn a_folder_made_anew_has_no_mark() {
        let marks = Marks::default();
        marks.commit(Watch::Changes, "INBOX", 1, floor(10), None);
        assert_eq!(marks.floor(Watch::Changes, "INBOX", 2, true), None);
    }
}
