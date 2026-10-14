//! The sample mailbox behind `mett --demo`, until mett can open an Office365
//! mailbox.

use chrono::{TimeDelta, Utc};
use mett_core::mock::{self, MemoryStore};
use mett_core::{Address, Body, BodyKind, WellKnownFolder};

const PEOPLE: &[(&str, &str)] = &[
    ("Alice Example", "alice@contoso.com"),
    ("Bob Builder", "bob@fabrikam.com"),
    ("Carol Cloud", "carol@contoso.com"),
    ("Dave Devops", "dave@northwind.example"),
];

const SUBJECTS: &[&str] = &[
    "Welcome to mett",
    "Weekly sync notes",
    "Re: Build failures on main",
    "Lunch on Friday?",
    "Quarterly planning draft",
    "Re: Re: Office move",
    "Your expense report was approved",
    "Design review: pager layout",
];

/// A sample mailbox: an Inbox with messages of various ages and flags, one
/// of them HTML, and an empty Archive.
pub fn mailbox() -> MemoryStore {
    let now = Utc::now();
    let mut store = MemoryStore::new()
        .with_folder(mock::folder("inbox", "Inbox", Some(WellKnownFolder::Inbox)))
        .with_folder(mock::folder(
            "archive",
            "Archive",
            Some(WellKnownFolder::Archive),
        ));

    for i in 0..40 {
        let (name, email) = PEOPLE[i % PEOPLE.len()];
        let subject = SUBJECTS[i % SUBJECTS.len()];
        let received = now - TimeDelta::hours(7 * i as i64);
        let mut message = mock::message(&format!("demo-{i}"), "inbox", subject, received);
        message.summary.from = Some(Address::new(email).with_name(name));
        message.summary.to = vec![Address::new("you@contoso.com").with_name("You")];
        message.summary.flags.read = i > 3;
        message.summary.flags.flagged = i % 9 == 2;
        message.summary.has_attachments = i % 5 == 4;
        message.body = if i == 0 {
            Body {
                kind: BodyKind::Html,
                content: "<html><body><p>Hi,</p><p>This is <b>mett</b>'s demo mailbox. \
                          Use <i>j</i> and <i>k</i> to move, <i>Enter</i> to read and \
                          <i>q</i> to go back or quit.</p><p>Enjoy!</p></body></html>"
                    .into(),
            }
        } else {
            Body {
                kind: BodyKind::Text,
                content: format!(
                    "Hello,\n\nThis is sample message {i}: \"{subject}\".\n\n\
                     {}\n-- \n{name}\n",
                    "Lorem ipsum dolor sit amet, consectetur adipiscing elit.\n".repeat(i % 7 * 6)
                ),
            }
        };
        store = store.with_message(message);
    }
    store
}
