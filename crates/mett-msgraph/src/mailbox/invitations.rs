use mett_core::occurrence_window;

use super::*;

impl<A: Authenticator> GraphMailStore<A> {
    /// Answers the calendar event an invitation is about: the series, one
    /// occurrence of it, or the meeting that doesn't repeat.
    pub(super) async fn answer_event(
        &self,
        id: &MessageId,
        invitation: &Invitation,
        answer: &Answer,
    ) -> Result<()> {
        let about = About::message(id.as_str());
        let Some(event) = invitation.event.as_deref() else {
            // Declining takes a meeting out of the calendar.
            return Err(Error::Backend(
                "the meeting isn't in your calendar (declined ones leave it)".into(),
            ));
        };
        let target = if invitation.repeats() {
            let mut url = self.url(&["me", "events", event]);
            url.query_pairs_mut()
                .append_pair("$select", "id,type,seriesMasterId");
            let kind: EventKind = self.get(url, about).await?;
            match answer.scope {
                Scope::Series => kind.series().to_owned(),
                Scope::Occurrence if kind.is_series() => {
                    self.instance_at(&kind.id, invitation, about).await?
                }
                Scope::Occurrence => kind.id,
            }
        } else {
            event.to_owned()
        };
        let url = self.url(&["me", "events", &target, answer_action(answer.rsvp)]);
        let body = answer_json(answer.comment.as_deref(), answer.notify);
        self.send(self.request(Method::POST, url).json(&body), about)
            .await
            .map(drop)
    }

    /// The instance of the series `master` that starts when `invitation`
    /// says.
    async fn instance_at(
        &self,
        master: &str,
        invitation: &Invitation,
        about: About<'_>,
    ) -> Result<String> {
        let Some(start) = invitation.start else {
            return Err(Error::Backend("the meeting has no start".into()));
        };
        let (start, end) = occurrence_window(start, invitation.end);
        let mut url = self.url(&["me", "events", master, "instances"]);
        url.query_pairs_mut()
            .append_pair(
                "startDateTime",
                &start.to_rfc3339_opts(SecondsFormat::Secs, true),
            )
            .append_pair(
                "endDateTime",
                &end.to_rfc3339_opts(SecondsFormat::Secs, true),
            )
            .append_pair("$select", "id,start");
        // Graph pages the instances (a daily series has many that overlap
        // a window), so the pages are followed until it is found.
        let mut pages = 0;
        loop {
            let request = self
                .request(Method::GET, url.clone())
                .header("Prefer", PREFER_UTC);
            let instances: Instances = self.json(request, about).await?;
            if let Some(found) = instances.at(start, invitation.all_day) {
                return Ok(found.to_owned());
            }
            pages += 1;
            match instances.next() {
                Some(link) if pages < MAX_INSTANCE_PAGES => url = self.next_page_url(link)?,
                _ => {
                    return Err(Error::Backend(
                        "that occurrence isn't in your calendar".into(),
                    ));
                }
            }
        }
    }
}

/// The most pages of a series' instances looked through for an occurrence.
const MAX_INSTANCE_PAGES: usize = 20;
