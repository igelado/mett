//! Meeting invitations in EWS items: what `MeetingRequest`,
//! `MeetingCancellation` and `MeetingResponse` items say.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Utc, Weekday};
use mett_core::{Invitation, InvitationKind, Recurrence, RecurrenceUnit, Reply, Rsvp};
use roxmltree::Node;

use crate::soap::{child, child_text, children, escape};

/// The fields that tell what a meeting message says. Items that aren't
/// about meetings have none of them, and EWS leaves them out.
pub(crate) const INVITATION_FIELDS: &[&str] = &[
    "item:ItemClass",
    "meeting:AssociatedCalendarItemId",
    "meeting:IsOutOfDate",
    "meeting:ResponseType",
    "meetingRequest:MeetingRequestType",
    "calendar:Start",
    "calendar:End",
    "calendar:StartWallClock",
    "calendar:EndWallClock",
    "calendar:IsAllDayEvent",
    "calendar:Location",
    "calendar:Organizer",
    "calendar:RequiredAttendees",
    "calendar:OptionalAttendees",
    "calendar:Recurrence",
    "calendar:IsResponseRequested",
];

/// What `item` says about a meeting, if it is a meeting message.
pub(crate) fn invitation(item: Node<'_, '_>) -> Option<Invitation> {
    let name = item.tag_name().name();
    let class = child_text(item, "ItemClass").unwrap_or_default();
    let kind = match name {
        "MeetingRequest" => {
            // An update is a request that comes again for a meeting
            // already known.
            match child_text(item, "MeetingRequestType").as_deref() {
                Some("FullUpdate" | "InformationalUpdate" | "SilentUpdate") => {
                    InvitationKind::Update
                }
                _ => InvitationKind::Request,
            }
        }
        "MeetingCancellation" => InvitationKind::Cancellation,
        "MeetingResponse" => match class.rsplit('.').next() {
            Some("Pos") => InvitationKind::Accepted,
            Some("Tent") => InvitationKind::Tentative,
            Some("Neg") => InvitationKind::Declined,
            _ => return None,
        },
        _ => return None,
    };
    let all_day = child_text(item, "IsAllDayEvent").as_deref() == Some("true");
    let time = |name: &str| {
        child_text(item, name)
            .and_then(|t| DateTime::parse_from_rfc3339(t.trim()).ok())
            .map(|t| t.with_timezone(&Utc))
    };
    // A meeting of whole days is by its dates, which the wall clock times
    // (in the zone the meeting was made in) give exactly.
    let wall = |name: &str| {
        let text = child_text(item, name)?;
        NaiveDate::parse_from_str(text.trim().get(..10)?, "%Y-%m-%d")
            .ok()
            .map(|d| d.and_time(NaiveTime::MIN).and_utc())
    };
    let day = |wall_clock: &str, utc: &str| wall(wall_clock).or_else(|| time(utc).map(whole_day));
    let (start, end) = if all_day {
        (day("StartWallClock", "Start"), day("EndWallClock", "End"))
    } else {
        (time("Start"), time("End"))
    };
    let attendees = ["RequiredAttendees", "OptionalAttendees"]
        .into_iter()
        .filter_map(|list| child(item, list))
        .flat_map(|list| children(list, "Attendee"))
        .filter_map(|a| child(a, "Mailbox"))
        .map(crate::mailbox::address)
        .collect();
    Some(Invitation {
        kind,
        title: None,
        start,
        end,
        all_day,
        location: child_text(item, "Location").filter(|l| !l.is_empty()),
        organizer: child(item, "Organizer")
            .and_then(|o| child(o, "Mailbox"))
            .map(crate::mailbox::address),
        attendees,
        recurrence: child(item, "Recurrence").and_then(recurrence),
        reply: match child_text(item, "ResponseType").as_deref() {
            Some("Organizer") => Reply::Organizer,
            Some("Accept") => Reply::Accepted,
            Some("Tentative") => Reply::Tentative,
            Some("Decline") => Reply::Declined,
            _ => Reply::None,
        },
        out_of_date: child_text(item, "IsOutOfDate").as_deref() == Some("true")
            || child_text(item, "MeetingRequestType").as_deref() == Some("Outdated"),
        response_requested: child_text(item, "IsResponseRequested").as_deref() != Some("false"),
        event: child(item, "AssociatedCalendarItemId")
            .and_then(|id| id.attribute("Id"))
            .map(str::to_owned),
    })
}

/// The response object that answers the item `reference` as `rsvp`, with
/// what the owner tells the organizer.
pub(crate) fn answer_xml(rsvp: Rsvp, reference: &str, comment: Option<&str>) -> String {
    let element = match rsvp {
        Rsvp::Accept => "AcceptItem",
        Rsvp::Tentative => "TentativelyAcceptItem",
        Rsvp::Decline => "DeclineItem",
    };
    let body = comment
        .filter(|c| !c.is_empty())
        .map(|c| format!("<t:Body BodyType=\"Text\">{}</t:Body>", escape(c)))
        .unwrap_or_default();
    format!(
        "<t:{element}>{body}<t:ReferenceItemId Id=\"{}\"/></t:{element}>",
        escape(reference)
    )
}

/// The time window of a calendar view of the occurrence that starts at
/// `start` and ends at `end`, as `StartDate` and `EndDate` attributes.
pub(crate) fn occurrence_view(start: DateTime<Utc>, end: Option<DateTime<Utc>>) -> String {
    let format = |t: DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    // The view lists what is there in its window: a window of an instant
    // can have nothing at its end.
    let end = end
        .filter(|e| *e > start)
        .unwrap_or(start + Duration::seconds(1));
    format!(
        "StartDate=\"{}\" EndDate=\"{}\"",
        format(start),
        format(end)
    )
}

/// The ID of the occurrence among the items a calendar view found that
/// belongs to the series with `uid` and starts at `start` (or, if none
/// starts then, the first that belongs to it).
pub(crate) fn find_occurrence(
    items: Node<'_, '_>,
    uid: &str,
    start: Option<DateTime<Utc>>,
) -> Option<String> {
    let ours: Vec<_> = items
        .children()
        .filter(Node::is_element)
        .filter(|item| child_text(item.to_owned(), "UID").as_deref() == Some(uid))
        .collect();
    let at = |item: &Node<'_, '_>| {
        child_text(*item, "Start")
            .and_then(|t| DateTime::parse_from_rfc3339(t.trim()).ok())
            .map(|t| t.with_timezone(&Utc))
    };
    let chosen = ours
        .iter()
        .find(|item| start.is_some() && at(item) == start)
        .or(ours.first())?;
    child(*chosen, "ItemId")
        .and_then(|id| id.attribute("Id"))
        .map(str::to_owned)
}

/// The midnight (UTC) of the day an all-day meeting's time falls in, for
/// a server that gave no wall clock time. EWS gives it as its midnight in
/// the time zone it was made in, shifted to UTC. Adding 13 hours gives the
/// day's date for the zones from UTC-10 (Hawaii) to UTC+13 (New Zealand
/// in summer): a few zones beyond, such as UTC+14, are off by a day.
fn whole_day(time: DateTime<Utc>) -> DateTime<Utc> {
    (time + Duration::hours(13))
        .date_naive()
        .and_time(chrono::NaiveTime::MIN)
        .and_utc()
}

/// The weekdays of a `DaysOfWeek` value, such as `Tuesday Thursday` (or
/// `Weekday`, `WeekendDay` and `Day`).
fn weekdays(value: &str) -> Vec<Weekday> {
    let mut days = Vec::new();
    for word in value.split_whitespace() {
        let more: &[Weekday] = match word {
            "Monday" => &[Weekday::Mon],
            "Tuesday" => &[Weekday::Tue],
            "Wednesday" => &[Weekday::Wed],
            "Thursday" => &[Weekday::Thu],
            "Friday" => &[Weekday::Fri],
            "Saturday" => &[Weekday::Sat],
            "Sunday" => &[Weekday::Sun],
            "Weekday" => &[
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
            ],
            "WeekendDay" => &[Weekday::Sat, Weekday::Sun],
            "Day" => &[
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
                Weekday::Sat,
                Weekday::Sun,
            ],
            _ => &[],
        };
        days.extend_from_slice(more);
    }
    days
}

/// A recurrence element's number, such as its `Interval`.
fn number(node: Node<'_, '_>, name: &str) -> Option<u32> {
    child_text(node, name)?.trim().parse().ok()
}

/// A `Month` value, such as `March`, as its number.
fn month(value: &str) -> Option<u32> {
    let date = NaiveDate::parse_from_str(&format!("1 {} 2000", value.trim()), "%d %B %Y").ok()?;
    Some(date.month())
}

/// A `DayOfWeekIndex` value as the n-th (-1 for the last).
fn nth(value: &str) -> Option<i8> {
    Some(match value.trim() {
        "First" => 1,
        "Second" => 2,
        "Third" => 3,
        "Fourth" => 4,
        "Last" => -1,
        _ => return None,
    })
}

/// A `Recurrence` element: a pattern, then how it ends.
fn recurrence(node: Node<'_, '_>) -> Option<Recurrence> {
    let pattern = node.children().find(|n| {
        n.is_element()
            && matches!(
                n.tag_name().name(),
                "DailyRecurrence"
                    | "WeeklyRecurrence"
                    | "AbsoluteMonthlyRecurrence"
                    | "RelativeMonthlyRecurrence"
                    | "AbsoluteYearlyRecurrence"
                    | "RelativeYearlyRecurrence"
            )
    })?;
    let every = number(pattern, "Interval").unwrap_or(1);
    let days = child_text(pattern, "DaysOfWeek").map(|d| weekdays(&d));
    let mut result = match pattern.tag_name().name() {
        "DailyRecurrence" => Recurrence::new(RecurrenceUnit::Day, every),
        "WeeklyRecurrence" => {
            let mut r = Recurrence::new(RecurrenceUnit::Week, every);
            r.weekdays = days.unwrap_or_default();
            r
        }
        "AbsoluteMonthlyRecurrence" => {
            let mut r = Recurrence::new(RecurrenceUnit::Month, every);
            r.day_of_month = number(pattern, "DayOfMonth");
            r
        }
        "RelativeMonthlyRecurrence" => {
            let mut r = Recurrence::new(RecurrenceUnit::Month, every);
            r.weekdays = days.unwrap_or_default();
            r.nth = child_text(pattern, "DayOfWeekIndex").and_then(|n| nth(&n));
            r
        }
        "AbsoluteYearlyRecurrence" => {
            let mut r = Recurrence::new(RecurrenceUnit::Year, 1);
            r.day_of_month = number(pattern, "DayOfMonth");
            r.month = child_text(pattern, "Month").and_then(|m| month(&m));
            r
        }
        _ => {
            let mut r = Recurrence::new(RecurrenceUnit::Year, 1);
            r.weekdays = days.unwrap_or_default();
            r.nth = child_text(pattern, "DayOfWeekIndex").and_then(|n| nth(&n));
            r.month = child_text(pattern, "Month").and_then(|m| month(&m));
            r
        }
    };
    if let Some(end) = child(node, "EndDateRecurrence") {
        // A date may carry a time zone offset (`2027-01-29Z`).
        result.until = child_text(end, "EndDate")
            .and_then(|d| NaiveDate::parse_from_str(d.get(..10)?, "%Y-%m-%d").ok());
    } else if let Some(numbered) = child(node, "NumberedRecurrence") {
        result.count = number(numbered, "NumberOfOccurrences");
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use mett_core::Address;

    /// The invitation in an item given as XML.
    fn read(xml: &str) -> Option<Invitation> {
        let xml = format!(
            "<Items xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\">{xml}</Items>"
        );
        let doc = roxmltree::Document::parse(&xml).unwrap();
        invitation(doc.root_element().first_element_child().unwrap())
    }

    const REQUEST: &str = r#"<t:MeetingRequest>
        <t:ItemClass>IPM.Schedule.Meeting.Request</t:ItemClass>
        <t:AssociatedCalendarItemId Id="cal-1" ChangeKey="k"/>
        <t:IsOutOfDate>false</t:IsOutOfDate>
        <t:ResponseType>Unknown</t:ResponseType>
        <t:MeetingRequestType>NewMeetingRequest</t:MeetingRequestType>
        <t:Start>2026-11-03T14:00:00Z</t:Start>
        <t:End>2026-11-03T15:00:00Z</t:End>
        <t:IsAllDayEvent>false</t:IsAllDayEvent>
        <t:Location>Room 4A</t:Location>
        <t:Organizer><t:Mailbox><t:Name>Alice</t:Name><t:EmailAddress>alice@x.com</t:EmailAddress></t:Mailbox></t:Organizer>
        <t:RequiredAttendees><t:Attendee><t:Mailbox><t:Name>Bob</t:Name><t:EmailAddress>bob@x.com</t:EmailAddress></t:Mailbox></t:Attendee></t:RequiredAttendees>
        <t:OptionalAttendees><t:Attendee><t:Mailbox><t:EmailAddress>carol@x.com</t:EmailAddress></t:Mailbox></t:Attendee></t:OptionalAttendees>
        <t:IsResponseRequested>true</t:IsResponseRequested>
        RECURRENCE
    </t:MeetingRequest>"#;

    #[test]
    fn reads_a_request() {
        let invitation = read(&REQUEST.replace("RECURRENCE", "")).unwrap();
        assert_eq!(invitation.kind, InvitationKind::Request);
        assert_eq!(
            invitation.start,
            Some(Utc.with_ymd_and_hms(2026, 11, 3, 14, 0, 0).unwrap())
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
        assert_eq!(invitation.event.as_deref(), Some("cal-1"));
        assert_eq!(invitation.reply, Reply::None);
        assert!(invitation.response_requested);
        assert!(invitation.recurrence.is_none());
        assert!(invitation.can_answer());
    }

    #[test]
    fn reads_updates_and_outdated_requests() {
        let update = REQUEST
            .replace("RECURRENCE", "")
            .replace("NewMeetingRequest", "FullUpdate");
        assert_eq!(read(&update).unwrap().kind, InvitationKind::Update);
        let outdated = REQUEST
            .replace("RECURRENCE", "")
            .replace("NewMeetingRequest", "Outdated");
        assert!(read(&outdated).unwrap().out_of_date);
    }

    #[test]
    fn reads_a_weekly_series() {
        let series = r#"<t:Recurrence>
            <t:WeeklyRecurrence><t:Interval>2</t:Interval><t:DaysOfWeek>Tuesday Thursday</t:DaysOfWeek></t:WeeklyRecurrence>
            <t:EndDateRecurrence><t:StartDate>2026-11-03Z</t:StartDate><t:EndDate>2027-01-29Z</t:EndDate></t:EndDateRecurrence>
        </t:Recurrence>"#;
        let invitation = read(&REQUEST.replace("RECURRENCE", series)).unwrap();
        assert_eq!(
            invitation.recurrence.unwrap().to_string(),
            "Every 2 weeks on Tue, Thu, until Fri 29 Jan 2027"
        );
    }

    #[test]
    fn every_day_is_all_the_weekdays() {
        assert_eq!(weekdays("Day").len(), 7);
        assert_eq!(weekdays("Weekday WeekendDay").len(), 7);
    }

    #[test]
    fn reads_monthly_and_yearly_series() {
        let series = r#"<t:Recurrence>
            <t:RelativeMonthlyRecurrence><t:Interval>1</t:Interval><t:DaysOfWeek>Friday</t:DaysOfWeek><t:DayOfWeekIndex>Last</t:DayOfWeekIndex></t:RelativeMonthlyRecurrence>
            <t:NumberedRecurrence><t:StartDate>2026-11-03Z</t:StartDate><t:NumberOfOccurrences>6</t:NumberOfOccurrences></t:NumberedRecurrence>
        </t:Recurrence>"#;
        let invitation = read(&REQUEST.replace("RECURRENCE", series)).unwrap();
        assert_eq!(
            invitation.recurrence.unwrap().to_string(),
            "Monthly on the last Fri, 6 times"
        );
        let yearly = r#"<t:Recurrence>
            <t:AbsoluteYearlyRecurrence><t:DayOfMonth>5</t:DayOfMonth><t:Month>March</t:Month></t:AbsoluteYearlyRecurrence>
            <t:NoEndRecurrence><t:StartDate>2026-03-05Z</t:StartDate></t:NoEndRecurrence>
        </t:Recurrence>"#;
        let invitation = read(&REQUEST.replace("RECURRENCE", yearly)).unwrap();
        assert_eq!(
            invitation.recurrence.unwrap().to_string(),
            "Yearly on the 5th of March"
        );
    }

    #[test]
    fn all_day_meetings_keep_their_dates_in_any_time_zone() {
        for (start, end) in [
            // Midnights in Los Angeles, Berlin and Auckland.
            ("2026-11-03T08:00:00Z", "2026-11-04T08:00:00Z"),
            ("2026-11-02T23:00:00Z", "2026-11-03T23:00:00Z"),
            ("2026-11-02T11:00:00Z", "2026-11-03T11:00:00Z"),
        ] {
            let item = REQUEST
                .replace("RECURRENCE", "")
                .replace("2026-11-03T14:00:00Z", start)
                .replace("2026-11-03T15:00:00Z", end)
                .replace("<t:IsAllDayEvent>false", "<t:IsAllDayEvent>true");
            let invitation = read(&item).unwrap();
            assert_eq!(
                invitation.start,
                Some(Utc.with_ymd_and_hms(2026, 11, 3, 0, 0, 0).unwrap()),
                "{start}"
            );
            assert_eq!(
                invitation.end,
                Some(Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap()),
                "{start}"
            );
        }
    }

    #[test]
    fn all_day_meetings_take_their_dates_from_the_wall_clock() {
        // Midnights in Kiribati (UTC+14) and American Samoa (UTC-11),
        // which no fixed shift of the UTC times gets right for both.
        for (start, end) in [
            ("2026-11-02T10:00:00Z", "2026-11-03T10:00:00Z"),
            ("2026-11-03T11:00:00Z", "2026-11-04T11:00:00Z"),
        ] {
            let item = REQUEST
                .replace("RECURRENCE", "")
                .replace("2026-11-03T14:00:00Z", start)
                .replace("2026-11-03T15:00:00Z", end)
                .replace(
                    "<t:IsAllDayEvent>false</t:IsAllDayEvent>",
                    "<t:IsAllDayEvent>true</t:IsAllDayEvent>\
                     <t:StartWallClock>2026-11-03T00:00:00</t:StartWallClock>\
                     <t:EndWallClock>2026-11-04T00:00:00</t:EndWallClock>",
                );
            let invitation = read(&item).unwrap();
            assert_eq!(
                invitation.start,
                Some(Utc.with_ymd_and_hms(2026, 11, 3, 0, 0, 0).unwrap()),
                "{start}"
            );
            assert_eq!(
                invitation.end,
                Some(Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap()),
                "{start}"
            );
        }
    }

    #[test]
    fn reads_cancellations_and_responses() {
        let cancelled = read(
            "<t:MeetingCancellation><t:ItemClass>IPM.Schedule.Meeting.Canceled</t:ItemClass>\
             <t:Start>2026-11-03T14:00:00Z</t:Start></t:MeetingCancellation>",
        )
        .unwrap();
        assert_eq!(cancelled.kind, InvitationKind::Cancellation);
        assert!(!cancelled.can_answer());
        for (class, kind) in [
            ("Pos", InvitationKind::Accepted),
            ("Tent", InvitationKind::Tentative),
            ("Neg", InvitationKind::Declined),
        ] {
            let response = read(&format!(
                "<t:MeetingResponse><t:ItemClass>IPM.Schedule.Meeting.Resp.{class}</t:ItemClass>\
                 </t:MeetingResponse>"
            ))
            .unwrap();
            assert_eq!(response.kind, kind);
        }
    }

    #[test]
    fn answers_with_the_comment_in_the_response_object() {
        assert_eq!(
            answer_xml(Rsvp::Tentative, "AAA=", Some("Maybe <late>")),
            "<t:TentativelyAcceptItem><t:Body BodyType=\"Text\">Maybe &lt;late&gt;</t:Body>\
             <t:ReferenceItemId Id=\"AAA=\"/></t:TentativelyAcceptItem>"
        );
        assert_eq!(
            answer_xml(Rsvp::Decline, "AAA=", None),
            "<t:DeclineItem><t:ReferenceItemId Id=\"AAA=\"/></t:DeclineItem>"
        );
        assert!(answer_xml(Rsvp::Accept, "A", Some("")).starts_with("<t:AcceptItem><t:Reference"));
    }

    #[test]
    fn finds_the_occurrence_of_a_series() {
        let xml = "<Items xmlns:t=\"urn:t\">\
            <t:CalendarItem><t:ItemId Id=\"other\"/><t:UID>x</t:UID><t:Start>2026-11-03T14:00:00Z</t:Start></t:CalendarItem>\
            <t:CalendarItem><t:ItemId Id=\"week2\"/><t:UID>series</t:UID><t:Start>2026-11-10T14:00:00Z</t:Start></t:CalendarItem>\
            <t:CalendarItem><t:ItemId Id=\"week1\"/><t:UID>series</t:UID><t:Start>2026-11-03T14:00:00Z</t:Start></t:CalendarItem>\
            </Items>";
        let doc = roxmltree::Document::parse(xml).unwrap();
        let start = DateTime::parse_from_rfc3339("2026-11-03T14:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let items = doc.root_element();
        assert_eq!(
            find_occurrence(items, "series", Some(start)).as_deref(),
            Some("week1")
        );
        // Not at that time, the first of the series is the nearest there is.
        assert_eq!(
            find_occurrence(items, "series", None).as_deref(),
            Some("week2")
        );
        assert_eq!(find_occurrence(items, "nothing", Some(start)), None);
    }

    #[test]
    fn views_the_window_of_an_occurrence() {
        let start = DateTime::parse_from_rfc3339("2026-11-03T14:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            occurrence_view(start, Some(start + Duration::hours(1))),
            "StartDate=\"2026-11-03T14:00:00Z\" EndDate=\"2026-11-03T15:00:00Z\""
        );
        assert_eq!(
            occurrence_view(start, None),
            "StartDate=\"2026-11-03T14:00:00Z\" EndDate=\"2026-11-03T14:00:01Z\""
        );
    }

    #[test]
    fn plain_messages_have_no_invitation() {
        assert!(read("<t:Message><t:Subject>Hi</t:Subject></t:Message>").is_none());
    }
}
