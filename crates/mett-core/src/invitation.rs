//! Meeting invitations: what a message that invites to a meeting says.

use std::fmt;

use chrono::{DateTime, NaiveDate, Utc, Weekday};

use crate::message::Address;

/// What a meeting message is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvitationKind {
    /// An invitation to a meeting.
    Request,
    /// A changed meeting, which asks for an answer again.
    Update,
    /// The meeting was cancelled.
    Cancellation,
    /// An attendee accepted the meeting.
    Accepted,
    /// An attendee tentatively accepted the meeting.
    Tentative,
    /// An attendee declined the meeting.
    Declined,
}

impl InvitationKind {
    /// Whether the message asks for an answer, which can be given.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Request | Self::Update)
    }
}

/// How the mailbox's owner answered a meeting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reply {
    /// Not answered yet.
    #[default]
    None,
    /// The owner organizes the meeting.
    Organizer,
    /// The owner accepted.
    Accepted,
    /// The owner tentatively accepted.
    Tentative,
    /// The owner declined.
    Declined,
}

impl fmt::Display for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "not answered",
            Self::Organizer => "organizer",
            Self::Accepted => "accepted",
            Self::Tentative => "tentatively accepted",
            Self::Declined => "declined",
        })
    }
}

/// How the owner answers a meeting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rsvp {
    /// Accept.
    Accept,
    /// Accept tentatively.
    Tentative,
    /// Decline.
    Decline,
}

impl Rsvp {
    /// The [`Reply`] that answering this way leaves.
    pub fn reply(self) -> Reply {
        match self {
            Self::Accept => Reply::Accepted,
            Self::Tentative => Reply::Tentative,
            Self::Decline => Reply::Declined,
        }
    }
}

/// What an answer covers, for a meeting that repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The meeting the message is about: the series, when it is the
    /// series that is invited.
    Series,
    /// Only the occurrence the message is about (the one that starts
    /// at [`Invitation::start`]).
    Occurrence,
}

/// How the owner answers a meeting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// Accept, accept tentatively or decline.
    pub rsvp: Rsvp,
    /// For a meeting that repeats, whether the answer covers the series
    /// or one occurrence.
    pub scope: Scope,
    /// What the owner tells the organizer, if anything.
    pub comment: Option<String>,
    /// Whether the organizer is told: the answer is always recorded in
    /// the owner's calendar.
    pub notify: bool,
}

/// The unit a meeting repeats in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecurrenceUnit {
    /// Every so many days.
    Day,
    /// Every so many weeks.
    Week,
    /// Every so many months.
    Month,
    /// Every so many years.
    Year,
}

/// When a meeting repeats.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recurrence {
    /// The unit it repeats in.
    pub unit: RecurrenceUnit,
    /// Every how many units (1 for every one).
    pub every: u32,
    /// The weekdays it falls on, for weeks (and, with [`Recurrence::nth`],
    /// for months and years).
    pub weekdays: Vec<Weekday>,
    /// The day of the month, for months and years.
    pub day_of_month: Option<u32>,
    /// The month (1 to 12), for years.
    pub month: Option<u32>,
    /// Which weekday of the month (1 to 4, or -1 for the last), for
    /// months and years that fall on a weekday.
    pub nth: Option<i8>,
    /// The last day it repeats on, if it ends that way.
    pub until: Option<NaiveDate>,
    /// How many times it happens, if it ends that way.
    pub count: Option<u32>,
}

impl Recurrence {
    /// A recurrence that never ends, of `unit` every `every`.
    pub fn new(unit: RecurrenceUnit, every: u32) -> Self {
        Self {
            unit,
            every: every.max(1),
            weekdays: Vec::new(),
            day_of_month: None,
            month: None,
            nth: None,
            until: None,
            count: None,
        }
    }
}

fn weekday_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

const WEEK: [Weekday; 7] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
    Weekday::Sat,
    Weekday::Sun,
];
const WEEKDAYS: [Weekday; 5] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
];
const WEEKEND: [Weekday; 2] = [Weekday::Sat, Weekday::Sun];

/// Whether `days` are exactly `set` (in any order).
fn is_set(days: &[Weekday], set: &[Weekday]) -> bool {
    days.len() == set.len() && set.iter().all(|d| days.contains(d))
}

fn ordinal(n: u32) -> String {
    let suffix = match (n % 100, n % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

impl fmt::Display for Recurrence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self
            .weekdays
            .iter()
            .map(|d| weekday_name(*d))
            .collect::<Vec<_>>()
            .join(", ");
        let (singular, plural) = match self.unit {
            RecurrenceUnit::Day => ("Daily", "days"),
            RecurrenceUnit::Week => ("Weekly", "weeks"),
            RecurrenceUnit::Month => ("Monthly", "months"),
            RecurrenceUnit::Year => ("Yearly", "years"),
        };
        if self.every <= 1 {
            f.write_str(singular)?;
        } else {
            write!(f, "Every {} {plural}", self.every)?;
        }
        let nth = |n: i8| match n {
            -1 => "last".to_owned(),
            n => ordinal(n.unsigned_abs().into()),
        };
        let month = self
            .month
            .and_then(|m| NaiveDate::from_ymd_opt(2000, m, 1))
            .map(|d| d.format("%B").to_string());
        let of_month = month.map_or(String::new(), |m| format!(" of {m}"));
        match (self.unit, self.nth, self.day_of_month) {
            (_, Some(n), _) if !days.is_empty() => {
                // The n-th of a set of days is one day of the set: the
                // last weekday, not the last of each.
                let which = match self.weekdays.as_slice() {
                    [_] => days,
                    set if is_set(set, &WEEK) => "day".to_owned(),
                    set if is_set(set, &WEEKDAYS) => "weekday".to_owned(),
                    set if is_set(set, &WEEKEND) => "weekend day".to_owned(),
                    _ => self
                        .weekdays
                        .iter()
                        .map(|d| weekday_name(*d))
                        .collect::<Vec<_>>()
                        .join(" or "),
                };
                write!(f, " on the {} {which}{of_month}", nth(n))?;
            }
            (RecurrenceUnit::Month | RecurrenceUnit::Year, _, Some(day)) => {
                write!(f, " on the {}{of_month}", ordinal(day))?;
            }
            _ if !days.is_empty() => write!(f, " on {days}")?,
            _ => {}
        }
        if let Some(until) = self.until {
            write!(f, ", until {}", until.format("%a %d %b %Y"))?;
        } else if let Some(count) = self.count {
            write!(f, ", {count} times")?;
        }
        Ok(())
    }
}

/// What a message that invites to a meeting (or answers or cancels an
/// invitation) says about the meeting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invitation {
    /// What the message is.
    pub kind: InvitationKind,
    /// The meeting's title, if it differs from the message's subject.
    pub title: Option<String>,
    /// When it starts.
    pub start: Option<DateTime<Utc>>,
    /// When it ends.
    pub end: Option<DateTime<Utc>>,
    /// Whether it lasts whole days: its times are then midnights (UTC) of
    /// the dates it spans, whatever time zone it was made in, and `end` is
    /// the midnight that ends its last day.
    pub all_day: bool,
    /// Where it is.
    pub location: Option<String>,
    /// Who organizes it.
    pub organizer: Option<Address>,
    /// Who is invited, required and optional.
    pub attendees: Vec<Address>,
    /// When it repeats, if it does.
    pub recurrence: Option<Recurrence>,
    /// How the mailbox's owner answered.
    pub reply: Reply,
    /// Whether a newer version of the meeting exists, so answering this
    /// one is out of date.
    pub out_of_date: bool,
    /// Whether the organizer wants an answer.
    pub response_requested: bool,
    /// Whether the meeting is one of a series (an occurrence the message
    /// is about, or the series itself), so an answer can cover the series
    /// or only this one.
    pub in_series: bool,
    /// What the backend needs to find the meeting again to answer it
    /// (its calendar item), if it isn't the message.
    pub event: Option<String>,
}

impl Invitation {
    /// Whether the meeting is one of a series, so an answer can cover
    /// only one occurrence.
    pub fn repeats(&self) -> bool {
        self.recurrence.is_some() || self.in_series
    }

    /// Whether answering with `scope` changes the reply the message shows:
    /// not when the message is about the series and the answer is for one
    /// occurrence of it.
    pub fn answer_changes_reply(&self, scope: Scope) -> bool {
        !(scope == Scope::Occurrence && self.recurrence.is_some())
    }

    /// Why the message can't be answered, if it can't.
    pub fn cannot_answer(&self) -> Option<&'static str> {
        if self.reply == Reply::Organizer {
            Some("you organize this meeting")
        } else if self.out_of_date {
            Some("it is out of date")
        } else if !self.kind.is_open() {
            Some("it doesn't ask for an answer")
        } else {
            None
        }
    }

    /// Whether the message can be answered.
    pub fn can_answer(&self) -> bool {
        self.cannot_answer().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_recurrences() {
        let mut weekly = Recurrence::new(RecurrenceUnit::Week, 1);
        weekly.weekdays = vec![Weekday::Tue, Weekday::Thu];
        assert_eq!(weekly.to_string(), "Weekly on Tue, Thu");

        let mut biweekly = Recurrence::new(RecurrenceUnit::Week, 2);
        biweekly.weekdays = vec![Weekday::Mon];
        biweekly.count = Some(6);
        assert_eq!(biweekly.to_string(), "Every 2 weeks on Mon, 6 times");

        let mut monthly = Recurrence::new(RecurrenceUnit::Month, 1);
        monthly.day_of_month = Some(22);
        assert_eq!(monthly.to_string(), "Monthly on the 22nd");

        let mut nth = Recurrence::new(RecurrenceUnit::Month, 3);
        nth.weekdays = vec![Weekday::Fri];
        nth.nth = Some(-1);
        nth.until = NaiveDate::from_ymd_opt(2027, 1, 29);
        assert_eq!(
            nth.to_string(),
            "Every 3 months on the last Fri, until Fri 29 Jan 2027"
        );

        let mut weekday = Recurrence::new(RecurrenceUnit::Month, 1);
        weekday.weekdays = WEEKDAYS.to_vec();
        weekday.nth = Some(-1);
        assert_eq!(weekday.to_string(), "Monthly on the last weekday");
        weekday.weekdays = vec![Weekday::Sat, Weekday::Sun];
        weekday.nth = Some(1);
        assert_eq!(weekday.to_string(), "Monthly on the 1st weekend day");
        weekday.weekdays = vec![Weekday::Mon, Weekday::Wed];
        assert_eq!(weekday.to_string(), "Monthly on the 1st Mon or Wed");

        let mut yearly = Recurrence::new(RecurrenceUnit::Year, 1);
        yearly.day_of_month = Some(5);
        yearly.month = Some(3);
        assert_eq!(yearly.to_string(), "Yearly on the 5th of March");

        assert_eq!(Recurrence::new(RecurrenceUnit::Day, 1).to_string(), "Daily");
    }

    #[test]
    fn tells_whether_an_answer_changes_the_reply_shown() {
        let mut invitation = Invitation {
            kind: InvitationKind::Request,
            title: None,
            start: None,
            end: None,
            all_day: false,
            location: None,
            organizer: None,
            attendees: Vec::new(),
            recurrence: None,
            reply: Reply::None,
            out_of_date: false,
            response_requested: true,
            in_series: false,
            event: None,
        };
        assert!(!invitation.repeats());
        assert!(invitation.answer_changes_reply(Scope::Series));
        // The message is about an occurrence: what is said of it is its.
        invitation.in_series = true;
        assert!(invitation.repeats());
        assert!(invitation.answer_changes_reply(Scope::Series));
        assert!(invitation.answer_changes_reply(Scope::Occurrence));
        // About the series: an answer for one occurrence leaves it.
        invitation.recurrence = Some(Recurrence::new(RecurrenceUnit::Day, 1));
        assert!(invitation.answer_changes_reply(Scope::Series));
        assert!(!invitation.answer_changes_reply(Scope::Occurrence));
    }

    #[test]
    fn only_open_invitations_can_be_answered() {
        let mut invitation = Invitation {
            kind: InvitationKind::Request,
            title: None,
            start: None,
            end: None,
            all_day: false,
            location: None,
            organizer: None,
            attendees: Vec::new(),
            recurrence: None,
            reply: Reply::None,
            out_of_date: false,
            response_requested: true,
            in_series: false,
            event: None,
        };
        assert!(invitation.can_answer());
        invitation.out_of_date = true;
        assert!(!invitation.can_answer());
        invitation.out_of_date = false;
        invitation.kind = InvitationKind::Cancellation;
        assert!(!invitation.can_answer());
    }
}
