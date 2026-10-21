//! An [`Invitation`], as stored with a message's body.

use chrono::{DateTime, Datelike, NaiveDate, Utc, Weekday};
use mett_core::{Address, Invitation, InvitationKind, Recurrence, RecurrenceUnit, Reply};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(crate) struct StoredInvitation {
    kind: u8,
    title: Option<String>,
    /// Microseconds since the epoch.
    start: Option<i64>,
    end: Option<i64>,
    all_day: bool,
    location: Option<String>,
    organizer: Option<(Option<String>, String)>,
    attendees: Vec<(Option<String>, String)>,
    recurrence: Option<StoredRecurrence>,
    reply: u8,
    out_of_date: bool,
    response_requested: bool,
    /// Missing in what was stored before it was known.
    #[serde(default)]
    in_series: bool,
    event: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct StoredRecurrence {
    unit: u8,
    every: u32,
    /// Days from Monday (0) to Sunday (6).
    weekdays: Vec<u8>,
    day_of_month: Option<u32>,
    month: Option<u32>,
    nth: Option<i8>,
    /// Days since the Common Era.
    until: Option<i32>,
    count: Option<u32>,
}

const KINDS: [InvitationKind; 6] = [
    InvitationKind::Request,
    InvitationKind::Update,
    InvitationKind::Cancellation,
    InvitationKind::Accepted,
    InvitationKind::Tentative,
    InvitationKind::Declined,
];

const REPLIES: [Reply; 5] = [
    Reply::None,
    Reply::Organizer,
    Reply::Accepted,
    Reply::Tentative,
    Reply::Declined,
];

const UNITS: [RecurrenceUnit; 4] = [
    RecurrenceUnit::Day,
    RecurrenceUnit::Week,
    RecurrenceUnit::Month,
    RecurrenceUnit::Year,
];

/// The position of `value` in `all`, which is how it is stored.
fn index<T: PartialEq>(all: &[T], value: &T) -> u8 {
    all.iter()
        .position(|v| v == value)
        .and_then(|i| u8::try_from(i).ok())
        .unwrap_or_default()
}

fn address(a: &Address) -> (Option<String>, String) {
    (a.name.clone(), a.email.clone())
}

fn unaddress((name, email): (Option<String>, String)) -> Address {
    Address { name, email }
}

fn micros(time: Option<DateTime<Utc>>) -> Option<i64> {
    time.map(|t| t.timestamp_micros())
}

impl From<&Invitation> for StoredInvitation {
    fn from(i: &Invitation) -> Self {
        Self {
            kind: index(&KINDS, &i.kind),
            title: i.title.clone(),
            start: micros(i.start),
            end: micros(i.end),
            all_day: i.all_day,
            location: i.location.clone(),
            organizer: i.organizer.as_ref().map(address),
            attendees: i.attendees.iter().map(address).collect(),
            recurrence: i.recurrence.as_ref().map(|r| StoredRecurrence {
                unit: index(&UNITS, &r.unit),
                every: r.every,
                weekdays: r
                    .weekdays
                    .iter()
                    .map(|d| u8::try_from(d.num_days_from_monday()).unwrap_or_default())
                    .collect(),
                day_of_month: r.day_of_month,
                month: r.month,
                nth: r.nth,
                until: r.until.map(|d| d.num_days_from_ce()),
                count: r.count,
            }),
            reply: index(&REPLIES, &i.reply),
            out_of_date: i.out_of_date,
            response_requested: i.response_requested,
            in_series: i.in_series,
            event: i.event.clone(),
        }
    }
}

impl From<StoredInvitation> for Invitation {
    fn from(s: StoredInvitation) -> Self {
        let time = |micros: Option<i64>| micros.and_then(DateTime::from_timestamp_micros);
        Self {
            kind: KINDS[usize::from(s.kind).min(KINDS.len() - 1)],
            title: s.title,
            start: time(s.start),
            end: time(s.end),
            all_day: s.all_day,
            location: s.location,
            organizer: s.organizer.map(unaddress),
            attendees: s.attendees.into_iter().map(unaddress).collect(),
            recurrence: s.recurrence.map(|r| Recurrence {
                unit: UNITS[usize::from(r.unit).min(UNITS.len() - 1)],
                every: r.every,
                weekdays: r
                    .weekdays
                    .into_iter()
                    .filter_map(|d| Weekday::try_from(d).ok())
                    .collect(),
                day_of_month: r.day_of_month,
                month: r.month,
                nth: r.nth,
                until: r.until.and_then(NaiveDate::from_num_days_from_ce_opt),
                count: r.count,
            }),
            reply: REPLIES[usize::from(s.reply).min(REPLIES.len() - 1)],
            out_of_date: s.out_of_date,
            response_requested: s.response_requested,
            in_series: s.in_series,
            event: s.event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn an_invitation_stored_is_the_one_read() {
        let mut recurrence = Recurrence::new(RecurrenceUnit::Month, 2);
        recurrence.weekdays = vec![Weekday::Mon, Weekday::Sun];
        recurrence.nth = Some(-1);
        recurrence.month = Some(3);
        recurrence.until = NaiveDate::from_ymd_opt(2027, 1, 29);
        recurrence.count = Some(4);
        let invitation = Invitation {
            kind: InvitationKind::Update,
            title: Some("Planning".into()),
            start: Some(Utc.with_ymd_and_hms(2026, 11, 3, 14, 0, 0).unwrap()),
            end: Some(Utc.with_ymd_and_hms(2026, 11, 3, 15, 0, 0).unwrap()),
            all_day: false,
            location: Some("Room 4A".into()),
            organizer: Some(Address::new("alice@x.com").with_name("Alice")),
            attendees: vec![Address::new("bob@x.com")],
            recurrence: Some(recurrence),
            reply: Reply::Tentative,
            out_of_date: true,
            response_requested: false,
            in_series: true,
            event: Some("cal-1".into()),
        };
        let json = serde_json::to_string(&StoredInvitation::from(&invitation)).unwrap();
        let back: Invitation = serde_json::from_str::<StoredInvitation>(&json)
            .unwrap()
            .into();
        assert_eq!(back, invitation);
        assert_eq!(back.start.unwrap().year(), 2026);
    }
}
