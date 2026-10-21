//! Meeting invitations in Graph messages: what an `eventMessage` says,
//! with the calendar event it is about.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc, Weekday};
use mett_core::{Address, Invitation, InvitationKind, Recurrence, RecurrenceUnit, Reply, Rsvp};
use serde::Deserialize;
use serde_json::json;

use crate::mailbox::EmailAddress;

/// What a message selects to tell about a meeting. The properties of
/// `eventMessage` and `eventMessageRequest` are properties of derived
/// types, so they are selected with the type's name. (Only those the
/// documented types have: Graph refuses a `$select` of any other property,
/// which would fail every message.)
pub(crate) const MEETING_FIELDS: &str = "microsoft.graph.eventMessage/meetingMessageType,\
    microsoft.graph.eventMessageRequest/type,\
    microsoft.graph.eventMessageRequest/isOutOfDate,\
    microsoft.graph.eventMessageRequest/previousStartDateTime,\
    microsoft.graph.eventMessageRequest/previousEndDateTime,\
    microsoft.graph.eventMessageRequest/previousLocation,\
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

/// The Graph action that answers an event as `rsvp`.
pub(crate) fn answer_action(rsvp: Rsvp) -> &'static str {
    match rsvp {
        Rsvp::Accept => "accept",
        Rsvp::Tentative => "tentativelyAccept",
        Rsvp::Decline => "decline",
    }
}

/// The body of an answer: what is told to the organizer, and whether the
/// organizer is told at all.
pub(crate) fn answer_json(comment: Option<&str>, notify: bool) -> serde_json::Value {
    json!({
        "comment": comment.unwrap_or_default(),
        "sendResponse": notify,
    })
}

/// What kind of event a calendar event is, and which series it is of.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EventKind {
    pub(crate) id: String,
    /// `singleInstance`, `occurrence`, `exception` or `seriesMaster`.
    #[serde(rename = "type")]
    kind: String,
    series_master_id: Option<String>,
}

impl EventKind {
    /// The event that answers the whole series the event is of (itself,
    /// if it is a series or doesn't repeat).
    pub(crate) fn series(&self) -> &str {
        match self.kind.as_str() {
            "occurrence" | "exception" => self.series_master_id.as_deref().unwrap_or(&self.id),
            _ => &self.id,
        }
    }

    /// Whether the event is a series, whose occurrences are its
    /// instances.
    pub(crate) fn is_series(&self) -> bool {
        self.kind == "seriesMaster"
    }
}

/// The instances of a series found in a window.
#[derive(Deserialize)]
pub(crate) struct Instances {
    value: Vec<Instance>,
    /// The link to the next page, if there is one.
    #[serde(rename = "@odata.nextLink")]
    next: Option<String>,
}

#[derive(Deserialize)]
struct Instance {
    id: String,
    start: Option<DateTimeZone>,
}

impl Instances {
    /// The link to the next page of instances, if there is one.
    pub(crate) fn next(&self) -> Option<&str> {
        self.next.as_deref()
    }

    /// The instance that starts at `start` (for a meeting of whole days,
    /// on its date), and no other: answering another would tell the
    /// organizer the wrong thing.
    pub(crate) fn at(&self, start: DateTime<Utc>, all_day: bool) -> Option<&str> {
        self.value
            .iter()
            .find(|i| {
                i.start
                    .as_ref()
                    .and_then(|s| s.parse(all_day))
                    .is_some_and(|s| s == start)
            })
            .map(|i| i.id.as_str())
    }
}

/// A meeting message's own properties, which are there only for
/// `eventMessage` and its derived types.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeetingMessage {
    meeting_message_type: Option<String>,
    /// What kind of meeting it is, as the event says.
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    is_out_of_date: bool,
    /// What an update changed, which a new request doesn't have.
    previous_start_date_time: Option<DateTimeZone>,
    previous_end_date_time: Option<DateTimeZone>,
    previous_location: Option<Location>,
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
    /// `singleInstance`, `occurrence`, `exception` or `seriesMaster`.
    #[serde(rename = "type")]
    kind: Option<String>,
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
    /// The zone `date_time` is in: UTC, as asked for with `Prefer`.
    time_zone: Option<String>,
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
            // An update says what it changed: Graph's v1.0 has no property
            // that says it is one, so an update that changes neither the
            // time nor the place (the attendees, say) shows as a request.
            Some("meetingRequest")
                if self.previous_start_date_time.is_some()
                    || self.previous_end_date_time.is_some()
                    || self.previous_location.is_some() =>
            {
                InvitationKind::Update
            }
            Some("meetingRequest") => InvitationKind::Request,
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
        // Without the event (gone from the calendar), the message doesn't
        // say it is of whole days: its times are taken as they are.
        let all_day = event.as_ref().is_some_and(|e| e.is_all_day);
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
        let event_type = event
            .as_ref()
            .and_then(|e| e.kind.as_deref())
            .or(self.kind.as_deref());
        let series_master = recurrence.is_some() || event_type == Some("seriesMaster");
        let in_series = series_master || matches!(event_type, Some("occurrence" | "exception"));
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
            out_of_date: self.is_out_of_date,
            response_requested: self
                .response_requested
                .or(event.as_ref().and_then(|e| e.response_requested))
                .unwrap_or(true),
            in_series,
            series_master,
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
        let utc = self
            .time_zone
            .as_deref()
            .is_none_or(|zone| zone.eq_ignore_ascii_case("UTC"));
        if !utc {
            // Not the zone asked for, and mett has no zone data: the hour
            // would be wrong, so it isn't shown. A day's date is right in
            // whatever zone it is.
            return all_day.then(|| time.date_naive().and_time(chrono::NaiveTime::MIN).and_utc());
        }
        // Graph gives an all-day meeting's times as midnights, not shifted
        // by the zone asked for (as far as is known: this wants checking
        // against a real tenant). If it shifted them, 13 hours later is on
        // the day's date for the zones from UTC-10 to UTC+13, so the date
        // is right in both cases for nearly every zone.
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
    fn tells_updates_by_what_they_changed() {
        assert_eq!(read(request()).unwrap().kind, InvitationKind::Request);
        for (field, value) in [
            (
                "previousStartDateTime",
                json!({"dateTime": "2026-11-03T13:00:00.0000000", "timeZone": "UTC"}),
            ),
            (
                "previousEndDateTime",
                json!({"dateTime": "2026-11-03T14:00:00.0000000", "timeZone": "UTC"}),
            ),
            ("previousLocation", json!({"displayName": "Room 2"})),
        ] {
            let mut message = request();
            message[field] = value;
            assert_eq!(
                read(message).unwrap().kind,
                InvitationKind::Update,
                "{field}"
            );
        }
        let mut message = request();
        message["isOutOfDate"] = json!(true);
        assert!(read(message).unwrap().out_of_date);
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
    }

    #[test]
    fn an_occurrence_is_of_a_series_without_a_recurrence() {
        assert!(!read(request()).unwrap().in_series);
        let mut message = request();
        message["event"]["type"] = json!("occurrence");
        let invitation = read(message).unwrap();
        assert!(invitation.in_series && invitation.recurrence.is_none());
        assert!(!invitation.series_master);
        // A series is still one when its pattern is missing.
        let mut message = request();
        message["event"]["type"] = json!("seriesMaster");
        assert!(read(message).unwrap().series_master);
        let mut message = request();
        message["event"]["type"] = json!("occurrence");
        let invitation = read(message).unwrap();
        assert!(invitation.repeats());
        // The message says it too, when the event is gone.
        let mut message = request();
        message["event"] = json!(null);
        message["type"] = json!("exception");
        assert!(read(message).unwrap().in_series);
        let mut message = request();
        message["type"] = json!("singleInstance");
        assert!(!read(message).unwrap().in_series);
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
    fn times_in_another_zone_are_not_taken_for_utc() {
        let mut message = request();
        message["event"] = json!(null);
        message["startDateTime"]["timeZone"] = json!("Pacific Standard Time");
        message["endDateTime"]["timeZone"] = json!("Pacific Standard Time");
        let invitation = read(message.clone()).unwrap();
        assert_eq!((invitation.start, invitation.end), (None, None));
    }

    #[test]
    fn answers_by_action() {
        assert_eq!(answer_action(Rsvp::Accept), "accept");
        assert_eq!(answer_action(Rsvp::Tentative), "tentativelyAccept");
        assert_eq!(answer_action(Rsvp::Decline), "decline");
        assert_eq!(
            answer_json(Some("Late"), false),
            json!({"comment": "Late", "sendResponse": false})
        );
        assert_eq!(
            answer_json(None, true),
            json!({"comment": "", "sendResponse": true})
        );
    }

    #[test]
    fn tells_the_series_of_an_event() {
        let kind = |value: serde_json::Value| serde_json::from_value::<EventKind>(value).unwrap();
        let single = kind(json!({"id": "e", "type": "singleInstance"}));
        assert_eq!((single.series(), single.is_series()), ("e", false));
        let master = kind(json!({"id": "m", "type": "seriesMaster"}));
        assert_eq!((master.series(), master.is_series()), ("m", true));
        let occurrence = kind(json!({"id": "o", "type": "occurrence", "seriesMasterId": "m"}));
        assert_eq!((occurrence.series(), occurrence.is_series()), ("m", false));
    }

    #[test]
    fn picks_the_instance_that_starts_then() {
        let instances: Instances = serde_json::from_value(json!({"value": [
            {"id": "w2", "start": {"dateTime": "2026-11-10T14:00:00.0000000", "timeZone": "UTC"}},
            {"id": "w1", "start": {"dateTime": "2026-11-03T14:00:00.0000000", "timeZone": "UTC"}},
        ]}))
        .unwrap();
        let start = Utc.with_ymd_and_hms(2026, 11, 3, 14, 0, 0).unwrap();
        assert_eq!(instances.at(start, false), Some("w1"));
        // At another time, no other instance is taken for it.
        let other = Utc.with_ymd_and_hms(2026, 11, 4, 14, 0, 0).unwrap();
        assert_eq!(instances.at(other, false), None);
    }

    #[test]
    fn picks_an_instance_of_whole_days_by_its_date() {
        // Midnights in Los Angeles, shifted to UTC.
        let instances: Instances = serde_json::from_value(json!({"value": [
            {"id": "d2", "start": {"dateTime": "2026-11-02T08:00:00.0000000", "timeZone": "UTC"}},
            {"id": "d3", "start": {"dateTime": "2026-11-03T08:00:00.0000000", "timeZone": "UTC"}},
        ]}))
        .unwrap();
        let start = Utc.with_ymd_and_hms(2026, 11, 3, 0, 0, 0).unwrap();
        assert_eq!(instances.at(start, true), Some("d3"));
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
