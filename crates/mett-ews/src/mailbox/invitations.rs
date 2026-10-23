use super::*;

impl<A: Authenticator + 'static> EwsMailStore<A> {
    /// The calendar item a meeting message is about. It can be made again
    /// with another ID when a meeting is answered, so the message says
    /// which it is now.
    async fn associated_item(
        &self,
        id: &MessageId,
        invitation: &Invitation,
        about: About<'_>,
    ) -> Result<String> {
        let xml = self
            .get_items(
                &[id.to_string()],
                &["meeting:AssociatedCalendarItemId"],
                false,
                about,
            )
            .await?;
        let doc = parse(&xml)?;
        let associated = first_item(only_message(&doc, about)?)
            .and_then(|item| child(item, "AssociatedCalendarItemId"))
            .and_then(|item| item.attribute("Id"))
            .map(str::to_owned);
        associated
            .or_else(|| invitation.event.clone())
            .ok_or_else(|| Error::Backend("the meeting isn't in your calendar yet".into()))
    }

    /// The calendar item of the series that the occurrence a message is
    /// about belongs to.
    pub(super) async fn series_item(
        &self,
        id: &MessageId,
        invitation: &Invitation,
        about: About<'_>,
    ) -> Result<String> {
        let occurrence = self.associated_item(id, invitation, about).await?;
        let body = format!(
            "<m:GetItem>{}<m:ItemIds><t:RecurringMasterItemId OccurrenceId=\"{}\"/>\
             </m:ItemIds></m:GetItem>",
            item_shape(&[], None),
            escape(&occurrence)
        );
        let xml = self.call(&body, about).await?;
        let doc = parse(&xml)?;
        first_item(only_message(&doc, about)?)
            .and_then(item_id)
            .ok_or_else(|| Error::Backend("the meeting's series isn't in your calendar".into()))
    }

    /// The calendar item of the occurrence an invitation to a series is
    /// about: the one of the series that starts when the invitation says.
    pub(super) async fn occurrence_item(
        &self,
        id: &MessageId,
        invitation: &Invitation,
        about: About<'_>,
    ) -> Result<String> {
        let Some(start) = invitation.start else {
            return Err(Error::Backend("the meeting has no start".into()));
        };
        let event = self.associated_item(id, invitation, about).await?;
        // The series' occurrences share its UID, and are in the calendar it
        // is in (which may not be the default one).
        let fields = ["calendar:UID", "item:ParentFolderId"];
        let xml = self.get_items(&[event], &fields, false, about).await?;
        let doc = parse(&xml)?;
        let item = first_item(only_message(&doc, about)?)
            .ok_or_else(|| unexpected("GetItem", "no item"))?;
        let uid = child_text(item, "UID").ok_or_else(|| unexpected("GetItem", "no UID"))?;
        let calendar = child(item, "ParentFolderId")
            .and_then(|f| f.attribute("Id"))
            .map_or_else(
                || "<t:DistinguishedFolderId Id=\"calendar\"/>".to_owned(),
                |id| folder_id(&FolderId::new(id)),
            );
        let body = format!(
            "<m:FindItem Traversal=\"Shallow\">{}<m:CalendarView {}/>\
             <m:ParentFolderIds>{calendar}</m:ParentFolderIds></m:FindItem>",
            item_shape(
                &["calendar:UID", "calendar:Start", "calendar:StartWallClock"],
                None
            ),
            occurrence_view(start, invitation.end),
        );
        let xml = self.call(&body, about).await?;
        let doc = parse(&xml)?;
        let message = only_message(&doc, about)?;
        let items = descendant(message, "RootFolder")
            .and_then(|root| child(root, "Items"))
            .ok_or_else(|| unexpected("FindItem", "no Items"))?;
        find_occurrence(items, &uid, start, invitation.all_day)
            .ok_or_else(|| Error::Backend("that occurrence isn't in your calendar".into()))
    }
}
