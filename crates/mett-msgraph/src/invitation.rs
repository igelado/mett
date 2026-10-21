//! Meeting invitations in Graph messages: what an `eventMessage` says,
//! with the calendar event it is about.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc, Weekday};
use mett_core::{Address, Invitation, InvitationKind, Recurrence, RecurrenceUnit, Reply};
use serde::Deserialize;

use crate::mailbox::EmailAddress;

/// What a message selects to tell about a meeting. The properties of
/// `eventMessage` and `eventMessageRequest` are properties of derived
/// types, so they are selected with the type's name.
pub(crate) const MEETING_FIELDS: &str = "microsoft.graph.eventMessage/meetingMessageType,\
    microsoft.graph.eventMessageRequest/meetingRequestType,\
    microsoft.graph.eventMessageRequest/isOutOfDate,\
    microsoft.graph.eventMessageRequest/isAllDay,\
    microsoft.graph.eventMessageRequest/responseRequested,\
    microsoft.graph.eventMessageRequest/startDateTime,\
    microsoft.graph.eventMessageRequest/endDateTime,\
    microsoft.graph.eventMessageRequest/location,\
    microsoft.graph.eventMessageRequest/recurrence";

/// The calendar event a meeting message is about, with what shows of it.
pub(crate) const EVENT_EXPAND: &str = "microsoft.graph.eventMessage/event(\
    $select=id,start,end,isAllDay,location,organizer,attendees,recurrence,\
    responseStatus,responseRequested,type,seriesMasterId)";

/// The time zone Graph is asked to give times in (`Prefer:
/// outlook.timezone`), so they are read as UTC.
pub(crate) const PREFER_UTC: &str = "outlook.timezone=\"UTC\"";

/// A meeting message's own properties, which are there only for
/// `eventMessage` and its derived types.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeetingMessage {
    meeting_message_type: Option<String>,
    /// For a request: whether it is new, an update, or outdated.
    meeting_request_type: Option<String>,
    #[serde(default)]
    is_out_of_date: bool,
    /// Whether the meeting lasts whole days, as the message says (the
    /// event, when it is there, says it too).
    is_all_day: Option<bool>,
    response_requested: Option<bool>,
    start_date_time: Option<DateTimeZone>,
    end_date_time: Option<DateTimeZone>,
    location: Option<Location>,
    recurrence: Option<GraphRecurrence>,
    /// The event the message is about, if it is still in the calendar.
    event: Option<Event>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    id: Option<String>,
    start: Option<DateTimeZone>,
    end: Option<DateTimeZone>,
    #[serde(default)]
    is_all_day: bool,
    location: Option<Location>,
    organizer: Option<Recipient>,
    #[serde(default)]
    attendees: Vec<Recipient>,
    recurrence: Option<GraphRecurrence>,
    response_status: Option<ResponseStatus>,
    response_requested: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DateTimeZone {
    date_time: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Location {
    display_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recipient {
    email_address: EmailAddress,
}

#[derive(Deserialize)]
struct ResponseStatus {
    response: Option<String>,
}

#[derive(Deserialize)]
struct GraphRecurrence {
    pattern: Pattern,
    range: Option<Range>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pattern {
    #[serde(rename = "type")]
    kind: String,
    interval: Option<u32>,
    #[serde(default)]
    days_of_week: Vec<String>,
    day_of_month: Option<u32>,
    month: Option<u32>,
    index: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Range {
    #[serde(rename = "type")]
    kind: String,
    end_date: Option<String>,
    number_of_occurrences: Option<u32>,
}

impl MeetingMessage {
    /// What the message says about a meeting, if it is a meeting message
    /// (`odata_type` is its `@odata.type`).
    pub(crate) fn invitation(self, odata_type: Option<&str>) -> Option<Invitation> {
        if !odata_type?.starts_with("#microsoft.graph.eventMessage") {
            return None;
        }
        let kind = match self.meeting_message_type.as_deref() {
            Some("meetingRequest") => match self.meeting_request_type.as_deref() {
                Some("fullUpdate" | "informationalUpdate" | "silentUpdate") => {
                    InvitationKind::Update
                }
                _ => InvitationKind::Request,
            },
            Some("meetingCancelled") => InvitationKind::Cancellation,
            Some("meetingAccepted") => InvitationKind::Accepted,
            // Graph spells it without the first "t".
            Some("meetingTentativelyAccepted" | "meetingTenativelyAccepted") => {
                InvitationKind::Tentative
            }
            Some("meetingDeclined") => InvitationKind::Declined,
            _ => return None,
        };
        let event = self.event;
        let all_day = event
            .as_ref()
            .map(|e| e.is_all_day)
            .or(self.is_all_day)
            .unwrap_or(false);
        let time = |from_event: Option<&DateTimeZone>, from_message: &Option<DateTimeZone>| {
            from_event
                .or(from_message.as_ref())
                .and_then(|t| t.parse(all_day))
        };
        let location = |l: Option<&Location>| {
            l.and_then(|l| l.display_name.clone())
                .filter(|n| !n.is_empty())
        };
        let recurrence = event
            .as_ref()
            .and_then(|e| e.recurrence.as_ref())
            .or(self.recurrence.as_ref())
            .and_then(GraphRecurrence::recurrence);
        Some(Invitation {
            kind,
            title: None,
            start: time(
                event.as_ref().and_then(|e| e.start.as_ref()),
                &self.start_date_time,
            ),
            end: time(
                event.as_ref().and_then(|e| e.end.as_ref()),
                &self.end_date_time,
            ),
            all_day,
            location: location(event.as_ref().and_then(|e| e.location.as_ref()))
                .or_else(|| location(self.location.as_ref())),
            organizer: event
                .as_ref()
                .and_then(|e| e.organizer.as_ref())
                .map(Recipient::address),
            attendees: event
                .as_ref()
                .map(|e| e.attendees.iter().map(Recipient::address).collect())
                .unwrap_or_default(),
            recurrence,
            reply: match event
                .as_ref()
                .and_then(|e| e.response_status.as_ref())
                .and_then(|s| s.response.as_deref())
            {
                Some("organizer") => Reply::Organizer,
                Some("accepted") => Reply::Accepted,
                Some("tentativelyAccepted") => Reply::Tentative,
                Some("declined") => Reply::Declined,
                _ => Reply::None,
            },
            out_of_date: self.is_out_of_date
                || self.meeting_request_type.as_deref() == Some("outdated"),
            response_requested: self
                .response_requested
                .or(event.as_ref().and_then(|e| e.response_requested))
                .unwrap_or(true),
            event: event.and_then(|e| e.id),
        })
    }
}

impl Recipient {
    fn address(&self) -> Address {
        Address {
            name: self.email_address.name.clone().filter(|n| !n.is_empty()),
            email: self.email_address.address.clone().unwrap_or_default(),
        }
    }
}

impl DateTimeZone {
    /// The time, which Graph gives without a zone (the one asked for
    /// with `Prefer`, UTC). For a meeting of whole days it is the
    /// midnight that starts a day, in the zone the meeting was made in,
    /// which is read as UTC whatever the zone: it is the date that counts.
    fn parse(&self, all_day: bool) -> Option<DateTime<Utc>> {
        let time = NaiveDateTime::parse_from_str(&self.date_time, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
        let time = time.and_utc();
        // An all-day time asked for in UTC may come as the day's midnight
        // in another zone, shifted: 13 hours later is on the day's date for
        // the zones from UTC-10 to UTC+13 (as in `mett-ews`).
        Some(if all_day {
            (time + Duration::hours(13))
                .date_naive()
                .and_time(chrono::NaiveTime::MIN)
                .and_utc()
        } else {
            time
        })
    }
}

fn weekday(name: &str) -> Option<Weekday> {
    name.parse().ok()
}

impl GraphRecurrence {
    fn recurrence(&self) -> Option<Recurrence> {
        let pattern = &self.pattern;
        let every = pattern.interval.unwrap_or(1);
        let unit = match pattern.kind.as_str() {
            "daily" => RecurrenceUnit::Day,
            "weekly" => RecurrenceUnit::Week,
            "absoluteMonthly" | "relativeMonthly" => RecurrenceUnit::Month,
            "absoluteYearly" | "relativeYearly" => RecurrenceUnit::Year,
            _ => return None,
        };
        let mut result = Recurrence::new(unit, every);
        result.weekdays = pattern
            .days_of_week
            .iter()
            .filter_map(|d| weekday(d))
            .collect();
        if pattern.kind.starts_with("absolute") {
            result.day_of_month = pattern.day_of_month;
        }
        if pattern.kind.starts_with("relative") {
            result.nth = match pattern.index.as_deref() {
                Some("first") => Some(1),
                Some("second") => Some(2),
                Some("third") => Some(3),
                Some("fourth") => Some(4),
                Some("last") => Some(-1),
                _ => None,
            };
        }
        if unit == RecurrenceUnit::Year {
            result.month = pattern.month;
        }
        if let Some(range) = &self.range {
            match range.kind.as_str() {
                "endDate" => {
                    result.until = range
                        .end_date
                        .as_deref()
                        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
                }
                "numbered" => result.count = range.number_of_occurrences,
                _ => {}
            }
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn read(value: serde_json::Value) -> Option<Invitation> {
        let odata_type = value["@odata.type"].as_str().map(str::to_owned);
        let meeting: MeetingMessage = serde_json::from_value(value).unwrap();
        meeting.invitation(odata_type.as_deref())
    }

    fn request() -> serde_json::Value {
        json!({
            "@odata.type": "#microsoft.graph.eventMessageRequest",
            "meetingMessageType": "meetingRequest",
            "isOutOfDate": false,
            "responseRequested": true,
            "startDateTime": {"dateTime": "2026-11-03T14:00:00.0000000", "timeZone": "UTC"},
            "endDateTime": {"dateTime": "2026-11-03T15:00:00.0000000", "timeZone": "UTC"},
            "location": {"displayName": "Room 4A"},
            "event": {
                "id": "event-1",
                "start": {"dateTime": "2026-11-03T14:00:00.0000000", "timeZone": "UTC"},
                "end": {"dateTime": "2026-11-03T15:00:00.0000000", "timeZone": "UTC"},
                "isAllDay": false,
                "location": {"displayName": "Room 4A"},
                "organizer": {"emailAddress": {"name": "Alice", "address": "alice@x.com"}},
                "attendees": [
                    {"type": "required", "emailAddress": {"name": "Bob", "address": "bob@x.com"}},
                    {"type": "optional", "emailAddress": {"address": "carol@x.com"}}
                ],
                "responseStatus": {"response": "notResponded"},
                "type": "singleInstance"
            }
        })
    }

    #[test]
    fn reads_a_request() {
        let invitation = read(request()).unwrap();
        assert_eq!(invitation.kind, InvitationKind::Request);
        assert_eq!(
            invitation.start,
            Some(Utc.with_ymd_and_hms(2026, 11, 3, 14, 0, 0).unwrap())
        );
        assert_eq!(
            invitation.end,
            Some(Utc.with_ymd_and_hms(2026, 11, 3, 15, 0, 0).unwrap())
        );
        assert_eq!(invitation.location.as_deref(), Some("Room 4A"));
        assert_eq!(
            invitation.organizer,
            Some(Address::new("alice@x.com").with_name("Alice"))
        );
        assert_eq!(
            invitation.attendees,
            [
                Address::new("bob@x.com").with_name("Bob"),
                Address::new("carol@x.com")
            ]
        );
        assert_eq!(invitation.reply, Reply::None);
        assert_eq!(invitation.event.as_deref(), Some("event-1"));
        assert!(invitation.can_answer());
    }

    #[test]
    fn tells_updates_and_outdated_requests() {
        for (kind, expected, outdated) in [
            ("newMeetingRequest", InvitationKind::Request, false),
            ("fullUpdate", InvitationKind::Update, false),
            ("informationalUpdate", InvitationKind::Update, false),
            ("silentUpdate", InvitationKind::Update, false),
            ("outdated", InvitationKind::Request, true),
        ] {
            let mut message = request();
            message["meetingRequestType"] = json!(kind);
            let invitation = read(message).unwrap();
            assert_eq!(invitation.kind, expected, "{kind}");
            assert_eq!(invitation.out_of_date, outdated, "{kind}");
        }
    }

    #[test]
    fn reads_graph_s_spelling_of_a_tentative_answer() {
        for kind in ["meetingTenativelyAccepted", "meetingTentativelyAccepted"] {
            let invitation = read(json!({
                "@odata.type": "#microsoft.graph.eventMessageResponse",
                "meetingMessageType": kind,
            }))
            .unwrap();
            assert_eq!(invitation.kind, InvitationKind::Tentative, "{kind}");
        }
    }

    #[test]
    fn reads_the_answer_already_given() {
        let mut message = request();
        message["event"]["responseStatus"]["response"] = json!("tentativelyAccepted");
        assert_eq!(read(message).unwrap().reply, Reply::Tentative);
    }

    #[test]
    fn a_meeting_gone_from_the_calendar_still_shows_what_the_message_says() {
        let mut message = request();
        message["event"] = json!(null);
        let invitation = read(message).unwrap();
        assert_eq!(invitation.location.as_deref(), Some("Room 4A"));
        assert_eq!(
            invitation.start,
            Some(Utc.with_ymd_and_hms(2026, 11, 3, 14, 0, 0).unwrap())
        );
        assert!(invitation.organizer.is_none());
        assert!(invitation.event.is_none());

        // A meeting of whole days is still one, without its event.
        let mut message = request();
        message["event"] = json!(null);
        message["isAllDay"] = json!(true);
        message["startDateTime"]["dateTime"] = json!("2026-11-03T00:00:00.0000000");
        message["endDateTime"]["dateTime"] = json!("2026-11-04T00:00:00.0000000");
        let invitation = read(message).unwrap();
        assert!(invitation.all_day);
        assert_eq!(
            invitation.start,
            Some(Utc.with_ymd_and_hms(2026, 11, 3, 0, 0, 0).unwrap())
        );
    }

    #[test]
    fn reads_a_series() {
        let mut message = request();
        message["event"]["recurrence"] = json!({
            "pattern": {
                "type": "weekly", "interval": 1,
                "daysOfWeek": ["tuesday", "thursday"], "firstDayOfWeek": "sunday"
            },
            "range": {"type": "endDate", "startDate": "2026-11-03", "endDate": "2027-01-29"}
        });
        assert_eq!(
            read(message).unwrap().recurrence.unwrap().to_string(),
            "Weekly on Tue, Thu, until Fri 29 Jan 2027"
        );
        let mut message = request();
        message["event"]["recurrence"] = json!({
            "pattern": {
                "type": "relativeYearly", "interval": 1, "month": 3,
                "daysOfWeek": ["monday"], "index": "second"
            },
            "range": {"type": "numbered", "numberOfOccurrences": 4}
        });
        assert_eq!(
            read(message).unwrap().recurrence.unwrap().to_string(),
            "Yearly on the 2nd Mon of March, 4 times"
        );
    }

    #[test]
    fn all_day_meetings_keep_their_dates() {
        let mut message = request();
        message["event"]["isAllDay"] = json!(true);
        message["event"]["start"]["dateTime"] = json!("2026-11-03T00:00:00.0000000");
        message["event"]["end"]["dateTime"] = json!("2026-11-04T00:00:00.0000000");
        let invitation = read(message).unwrap();
        assert!(invitation.all_day);
        assert_eq!(
            invitation.start,
            Some(Utc.with_ymd_and_hms(2026, 11, 3, 0, 0, 0).unwrap())
        );
        assert_eq!(
            invitation.end,
            Some(Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap())
        );
    }

    #[test]
    fn reads_cancellations_and_responses() {
        for (kind, expected) in [
            ("meetingCancelled", InvitationKind::Cancellation),
            ("meetingAccepted", InvitationKind::Accepted),
            ("meetingTentativelyAccepted", InvitationKind::Tentative),
            ("meetingDeclined", InvitationKind::Declined),
        ] {
            let invitation = read(json!({
                "@odata.type": "#microsoft.graph.eventMessage",
                "meetingMessageType": kind,
            }))
            .unwrap();
            assert_eq!(invitation.kind, expected);
        }
    }

    #[test]
    fn plain_messages_have_no_invitation() {
        assert!(read(json!({"subject": "Hi"})).is_none());
        assert!(read(json!({"@odata.type": "#microsoft.graph.message"})).is_none());
    }
}
