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

/// The demo user, to whom the sample mail is addressed.
pub const YOU: &str = "you@contoso.com";

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

/// How to write messages in the demo: as the user the sample mail is
/// addressed to, so that replies leave them out.
pub fn settings() -> crate::app::Settings {
    crate::app::Settings {
        compose: crate::compose::Settings {
            me: vec![YOU.to_owned()],
            ..crate::compose::Settings::default()
        },
        ..crate::app::Settings::default()
    }
}

/// A sample mailbox: an Inbox with messages of various ages and flags, one
/// of them HTML, an empty Archive, and Drafts and Sent Items for what is
/// written in the demo.
pub fn mailbox() -> MemoryStore {
    let now = Utc::now();
    let mut store = MemoryStore::new()
        .with_folder(mock::folder("inbox", "Inbox", Some(WellKnownFolder::Inbox)))
        .with_folder(mock::folder(
            "archive",
            "Archive",
            Some(WellKnownFolder::Archive),
        ))
        // Where sent and postponed messages go.
        .with_folder(mock::folder(
            "drafts",
            "Drafts",
            Some(WellKnownFolder::Drafts),
        ))
        .with_folder(mock::folder(
            "sent",
            "Sent Items",
            Some(WellKnownFolder::SentItems),
        ));

    const COUNT: usize = 40;
    for i in 0..COUNT {
        let (name, email) = PEOPLE[i % PEOPLE.len()];
        // Messages with the same subject are a conversation: the oldest
        // starts it, and the others answer it.
        let topic = i % SUBJECTS.len();
        let oldest = (COUNT - 1 - topic) / SUBJECTS.len() * SUBJECTS.len() + topic;
        // (The welcome message stands alone.)
        let reply = i != oldest && i != 0;
        let subject = if reply && !SUBJECTS[topic].starts_with("Re:") {
            format!("Re: {}", SUBJECTS[topic])
        } else {
            SUBJECTS[topic].to_owned()
        };
        let received = now - TimeDelta::hours(7 * i as i64);
        let mut message = mock::message(&format!("demo-{i}"), "inbox", &subject, received);
        let start = vec![u8::try_from(topic).unwrap_or(u8::MAX)];
        message.summary.thread = (i != 0).then(|| mett_core::ThreadPosition {
            conversation: SUBJECTS[topic].to_owned(),
            path: if reply {
                vec![start, vec![u8::try_from(i).unwrap_or(u8::MAX)]]
            } else {
                vec![start]
            },
        });
        message.summary.from = Some(Address::new(email).with_name(name));
        message.summary.to = vec![Address::new(YOU).with_name("You")];
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
        // Two invitations to answer: a meeting, and a weekly one.
        if matches!(i, 1 | 2) {
            message.invitation = Some(demo_invitation(i == 2, now, name, email));
            message.summary.subject = format!(
                "Invitation: {}",
                if i == 2 { "Team sync" } else { "Design review" }
            );
        }
        let attached = message.summary.has_attachments;
        store = store.with_message(message);
        // Something to look at with `v`.
        if attached {
            let id = format!("demo-{i}");
            store = store
                .with_attachment(
                    &id,
                    "notes.txt",
                    "text/plain",
                    format!("Notes for sample message {i}.\n\nA text attachment opens in mett.\n")
                        .as_bytes(),
                )
                .with_attachment(
                    &id,
                    "figures.csv",
                    "text/csv",
                    b"month,count\nJan,3\nFeb,5\n",
                );
        }
    }
    store
}

/// An invitation to a meeting tomorrow at this time (weekly if `weekly`),
/// from `name` at `email`.
fn demo_invitation(
    weekly: bool,
    now: chrono::DateTime<chrono::Utc>,
    name: &str,
    email: &str,
) -> mett_core::Invitation {
    use mett_core::{Invitation, InvitationKind, Recurrence, RecurrenceUnit, Reply};
    let start = now + TimeDelta::days(1);
    let mut recurrence = Recurrence::new(RecurrenceUnit::Week, 1);
    recurrence.weekdays = vec![chrono::Datelike::weekday(&start)];
    Invitation {
        kind: InvitationKind::Request,
        title: None,
        start: Some(start),
        end: Some(start + TimeDelta::hours(1)),
        all_day: false,
        location: Some("Room 4A".into()),
        organizer: Some(Address::new(email).with_name(name)),
        attendees: vec![Address::new(YOU).with_name("You")],
        recurrence: weekly.then_some(recurrence),
        reply: Reply::None,
        out_of_date: false,
        response_requested: true,
        in_series: false,
        event: Some("demo-event".into()),
    }
}

#[cfg(test)]
mod tests {
    use mett_core::{FolderId, MailStore, Page};

    use super::*;

    #[tokio::test]
    async fn group_replies_leave_the_demo_user_out() {
        let store = mailbox();
        let page = store
            .list_messages(&FolderId::new("inbox"), Page::first(1))
            .await
            .unwrap();
        let message = store.fetch_message(&page.messages[0].id).await.unwrap();
        let (to, cc) = crate::compose::reply_recipients(&message, true, true, &settings().compose);
        assert!(
            !to.iter().chain(&cc).any(|a| a.email == YOU),
            "{to:?} {cc:?}"
        );
    }
}
