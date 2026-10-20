//! EwsMailStore against a mock Exchange Web Services endpoint.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use mett_core::{
    AccessToken, Address, AttachmentKind, Authenticator, BodyKind, Draft, DraftOrigin, Error,
    FlagsUpdate, FolderId, MailSender, MailStore, MessageId, OutgoingAttachment, Page, SyncState,
    WellKnownFolder,
};
use mett_ews::EwsMailStore;
use url::Url;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockBuilder, MockServer, Request, ResponseTemplate};

/// Hands out a fixed token.
struct StaticAuth;

impl Authenticator for StaticAuth {
    async fn access_token(&self) -> mett_core::Result<AccessToken> {
        Ok(AccessToken::new("tok", Utc::now() + TimeDelta::hours(1)))
    }
}

const ENDPOINT: &str = "/EWS/Exchange.asmx";

fn store(server: &MockServer) -> EwsMailStore<StaticAuth> {
    store_with(server, Arc::new(StaticAuth))
}

fn store_with<A: Authenticator + 'static>(server: &MockServer, auth: Arc<A>) -> EwsMailStore<A> {
    let url = Url::parse(&format!("{}{ENDPOINT}", server.uri())).unwrap();
    EwsMailStore::new(
        reqwest::Client::new(),
        url,
        "alice@contoso.com".into(),
        auth,
    )
}

/// Calls of the EWS operation whose request contains `text`.
fn ews(text: &str) -> MockBuilder {
    Mock::given(method("POST"))
        .and(path(ENDPOINT))
        .and(body_string_contains(text))
}

/// A successful response holding `body` (the operation's response
/// element).
fn soap(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
             <s:Body xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\" \
             xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\">{body}\
             </s:Body></s:Envelope>"
        ),
        "text/xml; charset=utf-8",
    )
}

/// A response of `operation` with `messages` (response message bodies).
fn responses(operation: &str, messages: &[String]) -> ResponseTemplate {
    let messages: String = messages
        .iter()
        .map(|m| {
            format!(
                "<m:{operation}ResponseMessage ResponseClass=\"Success\">\
                 <m:ResponseCode>NoError</m:ResponseCode>{m}</m:{operation}ResponseMessage>"
            )
        })
        .collect();
    soap(&format!(
        "<m:{operation}Response><m:ResponseMessages>{messages}</m:ResponseMessages>\
         </m:{operation}Response>"
    ))
}

/// A response of `operation` with one successful message holding `body`.
fn success(operation: &str, body: &str) -> ResponseTemplate {
    responses(operation, &[body.to_owned()])
}

/// A response of `operation` that failed with `code`.
fn failure(operation: &str, code: &str) -> ResponseTemplate {
    soap(&format!(
        "<m:{operation}Response><m:ResponseMessages>\
         <m:{operation}ResponseMessage ResponseClass=\"Error\">\
         <m:MessageText>It failed.</m:MessageText><m:ResponseCode>{code}</m:ResponseCode>\
         </m:{operation}ResponseMessage></m:ResponseMessages></m:{operation}Response>"
    ))
}

/// The folder roles: the root and the Inbox; the other well-known folders
/// are missing.
fn roles() -> ResponseTemplate {
    let found =
        |id: &str| format!("<m:Folders><t:Folder><t:FolderId Id=\"{id}\"/></t:Folder></m:Folders>");
    let missing = "<m:GetFolderResponseMessage ResponseClass=\"Error\">\
                   <m:ResponseCode>ErrorFolderNotFound</m:ResponseCode>\
                   </m:GetFolderResponseMessage>";
    soap(&format!(
        "<m:GetFolderResponse><m:ResponseMessages>\
         <m:GetFolderResponseMessage ResponseClass=\"Success\"><m:ResponseCode>NoError</m:ResponseCode>{}\
         </m:GetFolderResponseMessage>\
         <m:GetFolderResponseMessage ResponseClass=\"Success\"><m:ResponseCode>NoError</m:ResponseCode>{}\
         </m:GetFolderResponseMessage>{}\
         </m:ResponseMessages></m:GetFolderResponse>",
        found("root"),
        found("inbox-id"),
        missing.repeat(5),
    ))
}

/// A folder element.
fn folder(element: &str, id: &str, parent: &str, name: &str, class: &str) -> String {
    format!(
        "<t:{element}><t:FolderId Id=\"{id}\"/><t:ParentFolderId Id=\"{parent}\"/>\
         <t:FolderClass>{class}</t:FolderClass><t:DisplayName>{name}</t:DisplayName>\
         <t:TotalCount>10</t:TotalCount><t:ChildFolderCount>0</t:ChildFolderCount>\
         <t:UnreadCount>2</t:UnreadCount></t:{element}>"
    )
}

/// A message element with summary fields.
fn message(id: &str, subject: &str, sent: &str) -> String {
    format!(
        "<t:Message><t:ItemId Id=\"{id}\" ChangeKey=\"ck\"/><t:ParentFolderId Id=\"inbox-id\"/>\
         <t:Subject>{subject}</t:Subject><t:DateTimeReceived>{sent}</t:DateTimeReceived>\
         <t:HasAttachments>false</t:HasAttachments><t:DateTimeSent>{sent}</t:DateTimeSent>\
         <t:DateTimeCreated>{sent}</t:DateTimeCreated><t:IsDraft>false</t:IsDraft>\
         <t:LastModifiedTime>{sent}</t:LastModifiedTime>\
         <t:ConversationId Id=\"conv\"/>\
         <t:ToRecipients><t:Mailbox><t:Name>Alice</t:Name><t:EmailAddress>alice@contoso.com</t:EmailAddress>\
         <t:RoutingType>SMTP</t:RoutingType></t:Mailbox></t:ToRecipients>\
         <t:IsRead>false</t:IsRead>\
         <t:From><t:Mailbox><t:Name>Bob</t:Name><t:EmailAddress>bob@contoso.com</t:EmailAddress>\
         </t:Mailbox></t:From><t:Flag><t:FlagStatus>Flagged</t:FlagStatus></t:Flag>\
         <t:ConversationIndex>AQHZAAAAAAAAAAAAAAAAAAAAAAAAAA==</t:ConversationIndex>\
         </t:Message>"
    )
}

/// A `FindItem` page with the items `ids`.
fn found_items(ids: &[&str], last: bool, next: u32) -> ResponseTemplate {
    let items: String = ids
        .iter()
        .map(|id| format!("<t:Message><t:ItemId Id=\"{id}\"/></t:Message>"))
        .collect();
    success(
        "FindItem",
        &format!(
            "<m:RootFolder IndexedPagingOffset=\"{next}\" TotalItemsInView=\"99\" \
             IncludesLastItemInRange=\"{last}\"><t:Items>{items}</t:Items></m:RootFolder>"
        ),
    )
}

/// The draft "draft", with the change key "ck2", as sending it looks it up.
async fn change_key(server: &MockServer) {
    for (id, key) in [("draft", "ck2"), ("orig", "ck-orig")] {
        ews("<m:GetItem>")
            .and(body_string_contains(format!("Id=\"{id}\"")))
            .respond_with(success(
                "GetItem",
                &format!(
                    "<m:Items><t:Message><t:ItemId Id=\"{id}\" ChangeKey=\"{key}\"/></t:Message>\
                     </m:Items>"
                ),
            ))
            .mount(server)
            .await;
    }
}

/// The bodies of the requests `server` received.
async fn requests(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r: &Request| String::from_utf8_lossy(&r.body).into_owned())
        .collect()
}

#[tokio::test]
async fn sends_soap_with_the_token_and_the_mailbox_to_route_by() {
    let server = MockServer::start().await;
    ews("<m:GetFolder>")
        .and(header("authorization", "Bearer tok"))
        .and(header("x-anchormailbox", "alice@contoso.com"))
        .and(header("content-type", "text/xml; charset=utf-8"))
        .respond_with(roles())
        .mount(&server)
        .await;
    ews("<m:FindFolder")
        .respond_with(success(
            "FindFolder",
            "<m:RootFolder IncludesLastItemInRange=\"true\"><t:Folders/></m:RootFolder>",
        ))
        .mount(&server)
        .await;
    assert_eq!(store(&server).list_folders().await.unwrap(), []);
}

#[tokio::test]
async fn lists_the_mail_folders_as_a_tree() {
    let server = MockServer::start().await;
    ews("<m:GetFolder>")
        .respond_with(roles())
        .mount(&server)
        .await;
    let folders = [
        folder("Folder", "inbox-id", "root", "Inbox", "IPF.Note"),
        folder(
            "CalendarFolder",
            "cal",
            "root",
            "Calendar",
            "IPF.Appointment",
        ),
        folder("Folder", "proj", "inbox-id", "Projects", "IPF.Note"),
        folder("Folder", "arch", "root", "Archive", "IPF.Note"),
        folder("SearchFolder", "s", "root", "Search", "IPF.Note"),
    ]
    .concat();
    ews("Traversal=\"Deep\"")
        .respond_with(success(
            "FindFolder",
            &format!(
                "<m:RootFolder IndexedPagingOffset=\"5\" IncludesLastItemInRange=\"true\">\
                 <t:Folders>{folders}</t:Folders></m:RootFolder>"
            ),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let listed = store(&server).list_folders().await.unwrap();
    let names: Vec<&str> = listed.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["Archive", "Inbox", "Projects"]);
    let inbox = &listed[1];
    assert_eq!(inbox.well_known, Some(WellKnownFolder::Inbox));
    assert_eq!(inbox.parent, None, "top-level folders have no parent");
    assert_eq!((inbox.unread_count, inbox.total_count), (2, 10));
    assert_eq!(listed[2].parent, Some(FolderId::new("inbox-id")));
}

#[tokio::test]
async fn folder_listings_follow_every_page() {
    let server = MockServer::start().await;
    ews("<m:GetFolder>")
        .respond_with(roles())
        .mount(&server)
        .await;
    ews("Offset=\"0\"")
        .respond_with(success(
            "FindFolder",
            &format!(
                "<m:RootFolder IndexedPagingOffset=\"1\" IncludesLastItemInRange=\"false\">\
                 <t:Folders>{}</t:Folders></m:RootFolder>",
                folder("Folder", "a", "root", "A", "IPF.Note")
            ),
        ))
        .mount(&server)
        .await;
    ews("Offset=\"1\"")
        .respond_with(success(
            "FindFolder",
            &format!(
                "<m:RootFolder IndexedPagingOffset=\"2\" IncludesLastItemInRange=\"true\">\
                 <t:Folders>{}</t:Folders></m:RootFolder>",
                folder("Folder", "b", "root", "B", "IPF.Note")
            ),
        ))
        .mount(&server)
        .await;
    let listed = store(&server).child_folders(None).await.unwrap();
    assert_eq!(listed.len(), 2);
    let sent = requests(&server).await;
    assert!(sent.iter().any(|r| r.contains("Traversal=\"Shallow\"")));
}

#[tokio::test]
async fn the_archive_folder_has_no_distinguished_name() {
    let server = MockServer::start().await;
    let e = store(&server)
        .well_known_folder(WellKnownFolder::Archive)
        .await
        .unwrap_err();
    assert!(matches!(e, Error::NotFound { .. }), "{e:?}");
}

#[tokio::test]
async fn lists_messages_newest_first_a_page_at_a_time() {
    let server = MockServer::start().await;
    ews("<m:FindItem")
        .and(body_string_contains("Offset=\"0\""))
        .respond_with(found_items(&["m1", "gone", "m2"], false, 3))
        .mount(&server)
        .await;
    ews("<m:FindItem")
        .and(body_string_contains("Offset=\"3\""))
        .respond_with(found_items(&["m3"], true, 4))
        .mount(&server)
        .await;
    let gone = "<m:GetItemResponseMessage ResponseClass=\"Error\">\
                <m:ResponseCode>ErrorItemNotFound</m:ResponseCode></m:GetItemResponseMessage>";
    ews("<m:GetItem>")
        .and(body_string_contains("Id=\"m1\""))
        .respond_with(soap(&format!(
            "<m:GetItemResponse><m:ResponseMessages>\
             <m:GetItemResponseMessage ResponseClass=\"Success\"><m:ResponseCode>NoError</m:ResponseCode>\
             <m:Items>{}</m:Items></m:GetItemResponseMessage>{gone}\
             <m:GetItemResponseMessage ResponseClass=\"Success\"><m:ResponseCode>NoError</m:ResponseCode>\
             <m:Items>{}</m:Items></m:GetItemResponseMessage>\
             </m:ResponseMessages></m:GetItemResponse>",
            message("m1", "First", "2026-10-19T10:00:00Z"),
            message("m2", "Second &amp; more", "2026-10-18T10:00:00Z"),
        )))
        .mount(&server)
        .await;
    ews("<m:GetItem>")
        .and(body_string_contains("Id=\"m3\""))
        .respond_with(success(
            "GetItem",
            &format!(
                "<m:Items>{}</m:Items>",
                message("m3", "Third", "2026-10-17T10:00:00Z")
            ),
        ))
        .mount(&server)
        .await;

    let store = store(&server);
    let folder = FolderId::new("inbox-id");
    let first = store.list_messages(&folder, Page::first(3)).await.unwrap();
    assert_eq!(
        first.messages.len(),
        2,
        "the one gone meanwhile is left out"
    );
    let m1 = &first.messages[0];
    assert_eq!(m1.subject, "First");
    assert_eq!(
        m1.from,
        Some(Address::new("bob@contoso.com").with_name("Bob"))
    );
    assert_eq!(
        m1.to,
        [Address::new("alice@contoso.com").with_name("Alice")]
    );
    assert!(!m1.flags.read);
    assert!(m1.flags.flagged);
    assert!(!m1.draft);
    assert_eq!(m1.received.to_rfc3339(), "2026-10-19T10:00:00+00:00");
    let thread = m1.thread.as_ref().unwrap();
    assert_eq!(thread.conversation, "conv");
    assert_eq!(thread.path.len(), 1);
    assert_eq!(first.messages[1].subject, "Second & more");

    let next = first.next.expect("more to come");
    let second = store
        .list_messages(&folder, Page::after(next.clone(), 3))
        .await
        .unwrap();
    assert_eq!(second.messages[0].subject, "Third");
    assert_eq!(second.next, None);
    // A cursor is only good for its own listing.
    assert!(
        store
            .changed_messages(&folder, Utc::now(), Page::after(next, 3))
            .await
            .is_err()
    );

    let sent = requests(&server).await;
    let find = sent.iter().find(|r| r.contains("<m:FindItem")).unwrap();
    assert!(
        find.contains("item:DateTimeSent"),
        "sorted by the sent date: {find}"
    );
}

#[tokio::test]
async fn recent_unread_mail_is_found_by_when_it_changed() {
    let server = MockServer::start().await;
    ews("<m:FindItem")
        .and(body_string_contains("message:IsRead"))
        .and(body_string_contains("2026-10-19T08:00:00Z"))
        .respond_with(success(
            "FindItem",
            "<m:RootFolder IncludesLastItemInRange=\"true\"><t:Items>\
             <t:Message><t:ItemId Id=\"m1\"/><t:LastModifiedTime>2026-10-19T09:00:00Z</t:LastModifiedTime></t:Message>\
             </t:Items></m:RootFolder>",
        ))
        .mount(&server)
        .await;
    let since = DateTime::parse_from_rfc3339("2026-10-19T08:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let recent = store(&server)
        .recent_unread(&FolderId::new("inbox-id"), Some(since))
        .await
        .unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].0, MessageId::new("m1"));
    assert_eq!(recent[0].1.to_rfc3339(), "2026-10-19T09:00:00+00:00");
}

#[tokio::test]
async fn fetches_messages_with_text_bodies() {
    let server = MockServer::start().await;
    let full = message("m1", "Hi", "2026-10-19T10:00:00Z").replace(
        "</t:Message>",
        "<t:Body BodyType=\"Text\">Hello &lt;there&gt;</t:Body>\
         <t:CcRecipients><t:Mailbox><t:EmailAddress>carol@contoso.com</t:EmailAddress></t:Mailbox></t:CcRecipients>\
         <t:InternetMessageId>&lt;id@contoso.com&gt;</t:InternetMessageId></t:Message>",
    );
    ews("<t:BodyType>Text</t:BodyType>")
        .respond_with(success("GetItem", &format!("<m:Items>{full}</m:Items>")))
        .mount(&server)
        .await;
    let message = store(&server)
        .fetch_message(&MessageId::new("m1"))
        .await
        .unwrap();
    assert_eq!(message.body.kind, BodyKind::Text);
    assert_eq!(message.body.content, "Hello <there>");
    assert_eq!(message.cc, [Address::new("carol@contoso.com")]);
    assert_eq!(
        message.internet_message_id.as_deref(),
        Some("<id@contoso.com>")
    );
}

#[tokio::test]
async fn missing_messages_are_not_found() {
    let server = MockServer::start().await;
    ews("<m:GetItem>")
        .respond_with(failure("GetItem", "ErrorItemNotFound"))
        .mount(&server)
        .await;
    let e = store(&server)
        .fetch_message(&MessageId::new("m1"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            e,
            Error::NotFound {
                kind: "message",
                ..
            }
        ),
        "{e:?}"
    );
}

#[tokio::test]
async fn busy_servers_throttle() {
    let server = MockServer::start().await;
    let fault = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                 <s:Fault><faultcode>a:ErrorServerBusy</faultcode><faultstring>Busy</faultstring>\
                 <detail><e:ResponseCode xmlns:e=\"errors\">ErrorServerBusy</e:ResponseCode>\
                 <e:MessageXml xmlns:e=\"errors\"><t:Value xmlns:t=\"types\" \
                 Name=\"BackOffMilliseconds\">3000</t:Value></e:MessageXml></detail>\
                 </s:Fault></s:Body></s:Envelope>";
    ews("<m:GetItem>")
        .respond_with(ResponseTemplate::new(500).set_body_raw(fault, "text/xml"))
        .mount(&server)
        .await;
    let e = store(&server)
        .message_folder(&MessageId::new("m1"))
        .await
        .unwrap_err();
    assert!(
        matches!(e, Error::Throttled { retry_after: Some(d) } if d == Duration::from_secs(3)),
        "{e:?}"
    );
}

#[tokio::test]
async fn a_send_refused_with_a_fault_was_not_sent() {
    let server = MockServer::start().await;
    let fault = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                 <s:Fault><faultcode>a:ErrorInvalidRecipients</faultcode>\
                 <faultstring>No recipients.</faultstring><detail>\
                 <e:ResponseCode xmlns:e=\"errors\">ErrorInvalidRecipients</e:ResponseCode>\
                 </detail></s:Fault></s:Body></s:Envelope>";
    ews("<m:CreateItem")
        .respond_with(ResponseTemplate::new(500).set_body_raw(fault, "text/xml"))
        .mount(&server)
        .await;
    let e = store(&server).send(&Draft::default()).await.unwrap_err();
    assert!(
        matches!(e, Error::Backend(ref m) if m.contains("ErrorInvalidRecipients")),
        "{e:?}"
    );
}

#[tokio::test]
async fn updates_read_and_flagged_state() {
    let server = MockServer::start().await;
    ews("<m:UpdateItem")
        .and(body_string_contains("<t:IsRead>true</t:IsRead>"))
        .and(body_string_contains(
            "<t:FlagStatus>NotFlagged</t:FlagStatus>",
        ))
        .respond_with(success("UpdateItem", "<m:Items/>"))
        .expect(1)
        .mount(&server)
        .await;
    store(&server)
        .update_flags(
            &MessageId::new("m1"),
            FlagsUpdate {
                read: Some(true),
                flagged: Some(false),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn moves_return_the_new_id() {
    let server = MockServer::start().await;
    ews("<m:MoveItem")
        .and(body_string_contains("<t:FolderId Id=\"trash\"/>"))
        .respond_with(success(
            "MoveItem",
            "<m:Items><t:Message><t:ItemId Id=\"moved\"/></t:Message></m:Items>",
        ))
        .mount(&server)
        .await;
    let id = store(&server)
        .move_message(&MessageId::new("m1"), &FolderId::new("trash"))
        .await
        .unwrap();
    assert_eq!(id, MessageId::new("moved"));
}

#[tokio::test]
async fn lists_and_fetches_attachments() {
    let server = MockServer::start().await;
    ews("item:Attachments")
        .respond_with(success(
            "GetItem",
            "<m:Items><t:Message><t:ItemId Id=\"m1\"/><t:Attachments>\
             <t:FileAttachment><t:AttachmentId Id=\"a1\"/><t:Name>notes.txt</t:Name>\
             <t:ContentType>text/plain</t:ContentType><t:Size>5</t:Size><t:IsInline>false</t:IsInline>\
             </t:FileAttachment>\
             <t:ItemAttachment><t:AttachmentId Id=\"a2\"/><t:Name>Fwd</t:Name><t:Size>900</t:Size>\
             </t:ItemAttachment></t:Attachments></t:Message></m:Items>",
        ))
        .mount(&server)
        .await;
    ews("<m:GetAttachment>")
        .and(body_string_contains("Id=\"a1\""))
        .respond_with(success(
            "GetAttachment",
            "<m:Attachments><t:FileAttachment><t:AttachmentId Id=\"a1\" RootItemId=\"m1\"/>\
             <t:Name>notes.txt</t:Name><t:Content>aGVsbG8=</t:Content></t:FileAttachment>\
             </m:Attachments>",
        ))
        .mount(&server)
        .await;
    let store = store(&server);
    let message = MessageId::new("m1");
    let listed = store.list_attachments(&message).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].name, "notes.txt");
    assert_eq!(listed[0].size, 5);
    assert_eq!(listed[1].kind, AttachmentKind::Item);
    assert_eq!(listed[1].content_type, "message/rfc822");
    let content = store
        .fetch_attachment(&message, &listed[0].id)
        .await
        .unwrap();
    assert_eq!(content, b"hello");
    // Another message's attachment isn't this one's.
    let e = store
        .fetch_attachment(&MessageId::new("m2"), &listed[0].id)
        .await
        .unwrap_err();
    assert!(matches!(e, Error::NotFound { .. }), "{e:?}");
}

#[tokio::test]
async fn new_mail_with_attachments_is_saved_before_it_is_sent() {
    let server = MockServer::start().await;
    change_key(&server).await;
    ews("MessageDisposition=\"SaveOnly\"")
        .and(body_string_contains(
            "<t:DistinguishedFolderId Id=\"drafts\"/>",
        ))
        .respond_with(success(
            "CreateItem",
            "<m:Items><t:Message><t:ItemId Id=\"draft\"/></t:Message></m:Items>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    ews("<m:CreateAttachment><m:ParentItemId Id=\"draft\"/>")
        .and(body_string_contains("<t:Content>aGk=</t:Content>"))
        .respond_with(success(
            "CreateAttachment",
            "<m:Attachments><t:FileAttachment><t:AttachmentId Id=\"a1\"/></t:FileAttachment>\
             </m:Attachments>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    ews("<m:SendItem SaveItemToFolder=\"true\">")
        .and(body_string_contains(
            "<t:DistinguishedFolderId Id=\"sentitems\"/>",
        ))
        .respond_with(success("SendItem", ""))
        .expect(1)
        .mount(&server)
        .await;
    let draft = Draft {
        to: vec![Address::new("bob@contoso.com")],
        subject: "Hi".into(),
        body: "Hello".into(),
        attachments: vec![OutgoingAttachment {
            name: "hi.txt".into(),
            content_type: "text/plain".into(),
            data: b"hi".to_vec(),
        }],
        save_copy: true,
        ..Draft::default()
    };
    store(&server).send(&draft).await.unwrap();
    let created = &requests(&server).await[0];
    assert!(!created.contains("<t:Attachments>"), "{created}");
}

#[tokio::test]
async fn new_mail_with_a_copy_is_sent_in_one_request() {
    let server = MockServer::start().await;
    ews("MessageDisposition=\"SendAndSaveCopy\"")
        .and(body_string_contains(
            "<t:DistinguishedFolderId Id=\"sentitems\"/>",
        ))
        .respond_with(success("CreateItem", "<m:Items/>"))
        .expect(1)
        .mount(&server)
        .await;
    let draft = Draft {
        to: vec![Address::new("bob@contoso.com")],
        save_copy: true,
        ..Draft::default()
    };
    store(&server).send(&draft).await.unwrap();
}

#[tokio::test]
async fn new_mail_without_a_copy_is_only_sent() {
    let server = MockServer::start().await;
    ews("MessageDisposition=\"SendOnly\"")
        .respond_with(success("CreateItem", "<m:Items/>"))
        .expect(1)
        .mount(&server)
        .await;
    let draft = Draft {
        to: vec![Address::new("bob@contoso.com")],
        ..Draft::default()
    };
    store(&server).send(&draft).await.unwrap();
    assert!(!requests(&server).await[0].contains("SavedItemFolderId"));
}

fn reply() -> Draft {
    Draft {
        to: vec![Address::new("bob@contoso.com")],
        subject: "Re: Hi".into(),
        body: "Thanks".into(),
        origin: Some(DraftOrigin::Reply(MessageId::new("orig"))),
        save_copy: false,
        attempt: 7,
        ..Draft::default()
    }
}

#[tokio::test]
async fn replies_go_through_a_server_draft() {
    let server = MockServer::start().await;
    change_key(&server).await;
    ews("<t:ReplyToItem><t:ReferenceItemId Id=\"orig\" ChangeKey=\"ck-orig\"/>")
        .and(body_string_contains("MessageDisposition=\"SaveOnly\""))
        .respond_with(success(
            "CreateItem",
            "<m:Items><t:Message><t:ItemId Id=\"draft\"/></t:Message></m:Items>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    ews("<m:UpdateItem")
        .and(body_string_contains(
            "<t:Body BodyType=\"Text\">Thanks</t:Body>",
        ))
        .and(body_string_contains(
            "<t:DeleteItemField><t:FieldURI FieldURI=\"message:CcRecipients\"/>",
        ))
        .respond_with(success("UpdateItem", "<m:Items/>"))
        .expect(1)
        .mount(&server)
        .await;
    ews("<m:SendItem SaveItemToFolder=\"false\">")
        .and(body_string_contains(
            "<t:ItemId Id=\"draft\" ChangeKey=\"ck2\"/>",
        ))
        .respond_with(success("SendItem", ""))
        .expect(1)
        .mount(&server)
        .await;
    store(&server).send(&reply()).await.unwrap();
}

#[tokio::test]
async fn mail_from_another_address_without_a_copy_is_saved_first() {
    let server = MockServer::start().await;
    change_key(&server).await;
    ews("MessageDisposition=\"SaveOnly\"")
        .and(body_string_contains("<t:From>"))
        .respond_with(success(
            "CreateItem",
            "<m:Items><t:Message><t:ItemId Id=\"draft\"/></t:Message></m:Items>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    ews("<m:SendItem SaveItemToFolder=\"false\">")
        .respond_with(success("SendItem", ""))
        .expect(1)
        .mount(&server)
        .await;
    let draft = Draft {
        from: Some(Address::new("team@contoso.com")),
        to: vec![Address::new("bob@contoso.com")],
        ..Draft::default()
    };
    store(&server).send(&draft).await.unwrap();
}

#[tokio::test]
async fn a_reply_to_a_message_gone_makes_no_draft() {
    let server = MockServer::start().await;
    ews("<m:GetItem>")
        .and(body_string_contains("Id=\"orig\""))
        .respond_with(failure("GetItem", "ErrorItemNotFound"))
        .mount(&server)
        .await;
    ews("<m:CreateItem")
        .respond_with(success("CreateItem", ""))
        .expect(0)
        .mount(&server)
        .await;
    let store = store(&server);
    let e = store.send(&reply()).await.unwrap_err();
    assert!(matches!(e, Error::NotFound { .. }), "{e:?}");
    assert!(!store.keeps_draft(&reply()));
}

#[tokio::test]
async fn a_draft_gone_before_it_is_sent_was_not_sent() {
    let server = MockServer::start().await;
    ews("<m:GetItem>")
        .and(body_string_contains("Id=\"orig\""))
        .respond_with(success(
            "GetItem",
            "<m:Items><t:Message><t:ItemId Id=\"orig\" ChangeKey=\"ck-orig\"/></t:Message></m:Items>",
        ))
        .mount(&server)
        .await;
    ews("<t:ReplyToItem>")
        .respond_with(success(
            "CreateItem",
            "<m:Items><t:Message><t:ItemId Id=\"draft\"/></t:Message></m:Items>",
        ))
        .mount(&server)
        .await;
    ews("<m:UpdateItem")
        .respond_with(success("UpdateItem", "<m:Items/>"))
        .mount(&server)
        .await;
    ews("<m:GetItem>")
        .and(body_string_contains("Id=\"draft\""))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    ews("<m:SendItem")
        .respond_with(success("SendItem", ""))
        .expect(0)
        .mount(&server)
        .await;
    let store = store(&server);
    let e = store.send(&reply()).await.unwrap_err();
    assert!(matches!(e, Error::NotSent(_)), "{e:?}");
    // Kept, for the next attempt to send.
    assert!(store.keeps_draft(&reply()));
}

#[tokio::test]
async fn a_lost_send_keeps_the_draft_for_the_next_attempt() {
    let server = MockServer::start().await;
    change_key(&server).await;
    ews("<t:ReplyToItem>")
        .respond_with(success(
            "CreateItem",
            "<m:Items><t:Message><t:ItemId Id=\"draft\"/></t:Message></m:Items>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    ews("<m:UpdateItem")
        .respond_with(success("UpdateItem", "<m:Items/>"))
        .mount(&server)
        .await;
    ews("<m:SendItem")
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    ews("<m:DeleteItem")
        .and(body_string_contains("Id=\"draft\""))
        .respond_with(success("DeleteItem", ""))
        .expect(1)
        .mount(&server)
        .await;
    let store = store(&server);
    let draft = reply();
    let e = store.send(&draft).await.unwrap_err();
    assert!(
        matches!(e, Error::Network(ref m) if m.contains("Drafts")),
        "{e:?}"
    );
    assert!(store.keeps_draft(&draft));
    // Trying again updates the same draft rather than making another.
    assert!(store.send(&draft).await.is_err());
    store.discard_kept(&draft).await.unwrap();
    assert!(!store.keeps_draft(&draft));
}

#[tokio::test]
async fn a_refused_send_deletes_its_draft() {
    let server = MockServer::start().await;
    change_key(&server).await;
    ews("<t:ReplyToItem>")
        .respond_with(success(
            "CreateItem",
            "<m:Items><t:Message><t:ItemId Id=\"draft\"/></t:Message></m:Items>",
        ))
        .mount(&server)
        .await;
    ews("<m:UpdateItem")
        .respond_with(success("UpdateItem", "<m:Items/>"))
        .mount(&server)
        .await;
    ews("<m:SendItem")
        .respond_with(failure("SendItem", "ErrorQuotaExceeded"))
        .mount(&server)
        .await;
    ews("<m:DeleteItem")
        .respond_with(success("DeleteItem", ""))
        .expect(1)
        .mount(&server)
        .await;
    let store = store(&server);
    let e = store.send(&reply()).await.unwrap_err();
    assert!(
        matches!(e, Error::Backend(ref m) if m.contains("ErrorQuotaExceeded")),
        "{e:?}"
    );
    assert!(!store.keeps_draft(&reply()));
}

#[tokio::test]
async fn postponing_saves_a_draft() {
    let server = MockServer::start().await;
    ews("MessageDisposition=\"SaveOnly\"")
        .and(body_string_contains(
            "<t:DistinguishedFolderId Id=\"drafts\"/>",
        ))
        .respond_with(success(
            "CreateItem",
            "<m:Items><t:Message><t:ItemId Id=\"saved\"/></t:Message></m:Items>",
        ))
        .mount(&server)
        .await;
    let id = store(&server).save_draft(&Draft::default()).await.unwrap();
    assert_eq!(id, MessageId::new("saved"));
}

#[tokio::test]
async fn searches_the_directory() {
    let server = MockServer::start().await;
    ews("<m:ResolveNames")
        .and(body_string_contains("SearchScope=\"ActiveDirectoryContacts\""))
        .respond_with(soap(
            "<m:ResolveNamesResponse><m:ResponseMessages>\
             <m:ResolveNamesResponseMessage ResponseClass=\"Warning\">\
             <m:ResponseCode>ErrorNameResolutionMultipleResults</m:ResponseCode>\
             <m:ResolutionSet TotalItemsInView=\"2\">\
             <t:Resolution><t:Mailbox><t:Name>Bob One</t:Name><t:EmailAddress>bob1@contoso.com</t:EmailAddress>\
             <t:RoutingType>SMTP</t:RoutingType></t:Mailbox></t:Resolution>\
             <t:Resolution><t:Mailbox><t:Name>Bob List</t:Name><t:EmailAddress>/o=Org/cn=bobs</t:EmailAddress>\
             <t:RoutingType>EX</t:RoutingType></t:Mailbox></t:Resolution>\
             </m:ResolutionSet></m:ResolveNamesResponseMessage>\
             </m:ResponseMessages></m:ResolveNamesResponse>",
        ))
        .mount(&server)
        .await;
    let found = store(&server).search_people("bob").await.unwrap();
    assert_eq!(
        found,
        [Address::new("bob1@contoso.com").with_name("Bob One")]
    );
}

#[tokio::test]
async fn nobody_found_is_no_error() {
    let server = MockServer::start().await;
    ews("<m:ResolveNames")
        .respond_with(failure("ResolveNames", "ErrorNameResolutionNoResults"))
        .mount(&server)
        .await;
    assert_eq!(store(&server).search_people("zzz").await.unwrap(), []);
}

#[tokio::test]
async fn own_addresses_need_not_include_the_account_name() {
    let server = MockServer::start().await;
    ews("<m:ResolveNames ReturnFullContactData=\"true\"")
        .respond_with(success(
            "ResolveNames",
            "<m:ResolutionSet><t:Resolution><t:Mailbox><t:Name>Alice</t:Name>\
             <t:EmailAddress>alice.example@contoso.com</t:EmailAddress><t:RoutingType>SMTP</t:RoutingType>\
             </t:Mailbox></t:Resolution></m:ResolutionSet>",
        ))
        .mount(&server)
        .await;
    assert_eq!(
        store(&server).own_addresses().await.unwrap(),
        ["alice@contoso.com", "alice.example@contoso.com"]
    );
}

#[tokio::test]
async fn own_addresses_include_the_aliases() {
    let server = MockServer::start().await;
    ews("<m:ResolveNames ReturnFullContactData=\"true\"")
        .respond_with(success(
            "ResolveNames",
            "<m:ResolutionSet><t:Resolution><t:Mailbox><t:Name>Alice</t:Name>\
             <t:EmailAddress>alice@contoso.com</t:EmailAddress><t:RoutingType>SMTP</t:RoutingType>\
             </t:Mailbox><t:Contact><t:EmailAddresses>\
             <t:Entry Key=\"EmailAddress1\">SMTP:alice@contoso.com</t:Entry>\
             <t:Entry Key=\"EmailAddress2\">smtp:a.example@contoso.com</t:Entry>\
             <t:Entry Key=\"EmailAddress3\">X500:/o=Org</t:Entry>\
             </t:EmailAddresses></t:Contact></t:Resolution></m:ResolutionSet>",
        ))
        .mount(&server)
        .await;
    assert_eq!(
        store(&server).own_addresses().await.unwrap(),
        ["alice@contoso.com", "a.example@contoso.com"]
    );
}

/// Hands out "old" until it is rejected, then "new".
#[derive(Default)]
struct RotatingAuth {
    rejected: AtomicBool,
}

impl Authenticator for RotatingAuth {
    async fn access_token(&self) -> mett_core::Result<AccessToken> {
        let secret = if self.rejected.load(Ordering::SeqCst) {
            "new"
        } else {
            "old"
        };
        Ok(AccessToken::new(secret, DateTime::<Utc>::MAX_UTC))
    }

    async fn token_rejected(&self, token: &AccessToken) {
        assert_eq!(token.secret(), "old");
        self.rejected.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_rejected_token_is_replaced_once() {
    let server = MockServer::start().await;
    ews("<m:GetItem>")
        .and(header("authorization", "Bearer old"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    ews("<m:GetItem>")
        .and(header("authorization", "Bearer new"))
        .respond_with(success(
            "GetItem",
            "<m:Items><t:Message><t:ItemId Id=\"m1\"/><t:ParentFolderId Id=\"f\"/></t:Message></m:Items>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let store = store_with(&server, Arc::new(RotatingAuth::default()));
    let folder = store.message_folder(&MessageId::new("m1")).await.unwrap();
    assert_eq!(folder, FolderId::new("f"));
}

#[tokio::test]
async fn failing_before_handing_over_leaves_mail_unsent() {
    struct Offline;
    impl Authenticator for Offline {
        async fn access_token(&self) -> mett_core::Result<AccessToken> {
            Err(Error::Network("offline".into()))
        }
    }
    let server = MockServer::start().await;
    let store = store_with(&server, Arc::new(Offline));
    let draft = Draft {
        to: vec![Address::new("bob@contoso.com")],
        ..Draft::default()
    };
    let e = store.send(&draft).await.unwrap_err();
    assert!(matches!(e, Error::NotSent(_)), "{e:?}");
}

/// A `SyncFolderItems` answer with `changes`, the new state `state`, and
/// whether it is the last batch.
fn sync_answer(changes: &str, state: &str, last: bool) -> ResponseTemplate {
    success(
        "SyncFolderItems",
        &format!(
            "<m:SyncState>{state}</m:SyncState>\
             <m:IncludesLastItemInRange>{last}</m:IncludesLastItemInRange>\
             <m:Changes>{changes}</m:Changes>"
        ),
    )
}

#[tokio::test]
async fn a_first_sync_tells_of_every_message() {
    let server = MockServer::start().await;
    ews("<m:SyncFolderItems>")
        .and(body_string_contains(
            "<m:MaxChangesReturned>512</m:MaxChangesReturned>",
        ))
        .respond_with(sync_answer(
            "<t:Create><t:Message><t:ItemId Id=\"m1\"/></t:Message></t:Create>\
             <t:Create><t:Message><t:ItemId Id=\"gone\"/></t:Message></t:Create>",
            "s1",
            false,
        ))
        .mount(&server)
        .await;
    let gone = "<m:GetItemResponseMessage ResponseClass=\"Error\">\
                <m:ResponseCode>ErrorItemNotFound</m:ResponseCode></m:GetItemResponseMessage>";
    ews("<m:GetItem>")
        .respond_with(soap(&format!(
            "<m:GetItemResponse><m:ResponseMessages>\
             <m:GetItemResponseMessage ResponseClass=\"Success\"><m:ResponseCode>NoError</m:ResponseCode>\
             <m:Items>{}</m:Items></m:GetItemResponseMessage>{gone}\
             </m:ResponseMessages></m:GetItemResponse>",
            message("m1", "First", "2026-10-19T10:00:00Z"),
        )))
        .mount(&server)
        .await;
    let store = store(&server);
    assert!(store.can_sync());
    let changes = store
        .sync_folder(&FolderId::new("inbox-id"), None)
        .await
        .unwrap();
    assert!(!requests(&server).await[0].contains("<m:SyncState>"));
    assert_eq!(changes.changed.len(), 1);
    assert_eq!(changes.changed[0].subject, "First");
    assert_eq!(changes.removed, [MessageId::new("gone")], "gone meanwhile");
    assert_eq!(changes.state.as_str(), "s1");
    assert!(changes.more);
}

#[tokio::test]
async fn a_sync_goes_on_from_its_state() {
    let server = MockServer::start().await;
    ews("<m:SyncState>s1</m:SyncState>")
        .respond_with(sync_answer(
            "<t:ReadFlagChange><t:ItemId Id=\"m1\"/><t:IsRead>true</t:IsRead></t:ReadFlagChange>\
             <t:Delete><t:ItemId Id=\"m3\"/></t:Delete>",
            "s2",
            true,
        ))
        .mount(&server)
        .await;
    ews("<m:GetItem>")
        .respond_with(success(
            "GetItem",
            &format!(
                "<m:Items>{}</m:Items>",
                message("m1", "First", "2026-10-19T10:00:00Z")
            ),
        ))
        .mount(&server)
        .await;
    let changes = store(&server)
        .sync_folder(&FolderId::new("inbox-id"), Some(&SyncState::new("s1")))
        .await
        .unwrap();
    assert_eq!(changes.changed[0].id, MessageId::new("m1"));
    assert_eq!(changes.removed, [MessageId::new("m3")]);
    assert_eq!(changes.state.as_str(), "s2");
    assert!(!changes.more);
}

#[tokio::test]
async fn a_stale_sync_state_starts_over() {
    for code in ["ErrorInvalidSyncStateData", "ErrorSyncStateNotFound"] {
        let server = MockServer::start().await;
        ews("<m:SyncFolderItems>")
            .respond_with(failure("SyncFolderItems", code))
            .mount(&server)
            .await;
        let e = store(&server)
            .sync_folder(&FolderId::new("inbox-id"), Some(&SyncState::new("old")))
            .await
            .unwrap_err();
        assert!(matches!(e, Error::StaleSync), "{code}: {e:?}");
    }
}

#[tokio::test]
async fn items_are_looked_up_a_hundred_at_a_time() {
    let server = MockServer::start().await;
    let ids: Vec<String> = (0..150).map(|i| format!("m{i}")).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    ews("<m:FindItem")
        .respond_with(found_items(&refs, true, 150))
        .mount(&server)
        .await;
    ews("<m:GetItem>")
        .respond_with(success("GetItem", "<m:Items/>"))
        .expect(2)
        .mount(&server)
        .await;
    store(&server)
        .list_messages(&FolderId::new("inbox-id"), Page::first(150))
        .await
        .unwrap();
    let sent = requests(&server).await;
    let looked_up: Vec<usize> = sent
        .iter()
        .filter(|r| r.contains("<m:GetItem>"))
        .map(|r| r.matches("<t:ItemId ").count())
        .collect();
    assert_eq!(looked_up, [100, 50]);
}

/// One envelope of a `GetStreamingEvents` stream, with `rest` in its
/// response message, of `code`.
fn streamed(code: &str, rest: &str) -> String {
    let class = if code == "NoError" {
        "Success"
    } else {
        "Error"
    };
    format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
         <s:Body xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\" \
         xmlns:t=\"http://schemas.microsoft.com/exchange/services/2006/types\">\
         <m:GetStreamingEventsResponse><m:ResponseMessages>\
         <m:GetStreamingEventsResponseMessage ResponseClass=\"{class}\">\
         <m:ResponseCode>{code}</m:ResponseCode>{rest}</m:GetStreamingEventsResponseMessage>\
         </m:ResponseMessages></m:GetStreamingEventsResponse></s:Body></s:Envelope>"
    )
}

/// A stream telling of new mail in `folder`.
fn new_mail(folder: &str) -> ResponseTemplate {
    let mail = streamed(
        "NoError",
        &format!(
            "<m:Notifications><m:Notification>\
             <t:NewMailEvent><t:ItemId Id=\"m9\"/><t:ParentFolderId Id=\"{folder}\"/>\
             </t:NewMailEvent></m:Notification></m:Notifications>\
             <m:ConnectionStatus>Closed</m:ConnectionStatus>"
        ),
    );
    ResponseTemplate::new(200).set_body_raw(mail, "text/xml")
}

/// A subscription answered with ID `id`.
fn subscribed(id: &str) -> ResponseTemplate {
    success(
        "Subscribe",
        &format!("<m:SubscriptionId>{id}</m:SubscriptionId>"),
    )
}

#[tokio::test]
async fn watches_folders_by_streaming_notifications() {
    let server = MockServer::start().await;
    ews("<m:Subscribe>")
        .and(body_string_contains("<m:StreamingSubscriptionRequest>"))
        .and(body_string_contains("<t:FolderId Id=\"inbox-id\"/>"))
        .and(body_string_contains(
            "<t:EventType>NewMailEvent</t:EventType>",
        ))
        .respond_with(subscribed("sub1"))
        .expect(1)
        .mount(&server)
        .await;
    // The server says it is alive, then mail arrives.
    let alive = streamed(
        "NoError",
        "<m:Notifications><m:Notification><t:StatusEvent/></m:Notification></m:Notifications>",
    );
    let mail = streamed(
        "NoError",
        "<m:Notifications><m:Notification><t:SubscriptionId>sub1</t:SubscriptionId>\
         <t:NewMailEvent><t:ItemId Id=\"m9\"/><t:ParentFolderId Id=\"inbox-id\"/></t:NewMailEvent>\
         </m:Notification></m:Notifications>",
    );
    ews("<m:GetStreamingEvents>")
        .and(body_string_contains(
            "<t:SubscriptionId>sub1</t:SubscriptionId>",
        ))
        .and(body_string_contains(
            "<m:ConnectionTimeout>5</m:ConnectionTimeout>",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(format!("{alive}{mail}"), "text/xml"))
        .mount(&server)
        .await;
    let store = store(&server);
    assert!(store.can_watch());
    let inbox = [FolderId::new("inbox-id")];
    assert_eq!(store.watch(&inbox).await.unwrap(), inbox);
    // Watched again, the subscription goes on.
    assert_eq!(store.watch(&inbox).await.unwrap(), inbox);
}

#[tokio::test]
async fn an_expired_subscription_is_made_again() {
    let server = MockServer::start().await;
    ews("<m:Subscribe>")
        .respond_with(subscribed("sub1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    ews("<m:Subscribe>")
        .respond_with(subscribed("sub2"))
        .mount(&server)
        .await;
    ews("<t:SubscriptionId>sub1</t:SubscriptionId>")
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(streamed("ErrorSubscriptionNotFound", ""), "text/xml"),
        )
        .mount(&server)
        .await;
    ews("<t:SubscriptionId>sub2</t:SubscriptionId>")
        .respond_with(new_mail("inbox-id"))
        .mount(&server)
        .await;
    let inbox = [FolderId::new("inbox-id")];
    assert_eq!(store(&server).watch(&inbox).await.unwrap(), inbox);
}

#[tokio::test]
async fn a_subscription_dropped_again_at_once_is_an_error() {
    let server = MockServer::start().await;
    ews("<m:Subscribe>")
        .respond_with(subscribed("sub"))
        .mount(&server)
        .await;
    ews("<m:GetStreamingEvents>")
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(streamed("ErrorInvalidSubscription", ""), "text/xml"),
        )
        .mount(&server)
        .await;
    assert!(
        store(&server)
            .watch(&[FolderId::new("inbox-id")])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn watching_other_folders_ends_the_old_subscription() {
    let server = MockServer::start().await;
    ews("<m:Subscribe>")
        .and(body_string_contains("<t:FolderId Id=\"inbox-id\"/>"))
        .respond_with(subscribed("sub1"))
        .mount(&server)
        .await;
    ews("<m:Subscribe>")
        .and(body_string_contains("<t:FolderId Id=\"work-id\"/>"))
        .respond_with(subscribed("sub2"))
        .mount(&server)
        .await;
    ews("<m:Unsubscribe><m:SubscriptionId>sub1</m:SubscriptionId>")
        .respond_with(success("Unsubscribe", ""))
        .expect(1)
        .mount(&server)
        .await;
    ews("<m:GetStreamingEvents>")
        .respond_with(new_mail("inbox-id"))
        .mount(&server)
        .await;
    let store = store(&server);
    store.watch(&[FolderId::new("inbox-id")]).await.unwrap();
    store.watch(&[FolderId::new("work-id")]).await.unwrap();
}

#[tokio::test]
async fn a_stream_ended_early_is_an_error() {
    let server = MockServer::start().await;
    ews("<m:Subscribe>")
        .respond_with(subscribed("sub"))
        .mount(&server)
        .await;
    ews("<m:GetStreamingEvents>")
        .respond_with(ResponseTemplate::new(200).set_body_raw("<s:Envel", "text/xml"))
        .mount(&server)
        .await;
    let store = store(&server);
    let inbox = [FolderId::new("inbox-id")];
    let e = store.watch(&inbox).await.unwrap_err();
    assert!(matches!(e, mett_core::Error::Network(_)), "{e:?}");
    // Part of an envelope came: the next watch tells the folder may have
    // changed, at once.
    assert_eq!(store.watch(&inbox).await.unwrap(), inbox);
}

#[tokio::test]
async fn a_canceled_watch_keeps_the_subscription_it_made() {
    let server = MockServer::start().await;
    ews("<m:Subscribe>")
        .respond_with(subscribed("sub1").set_delay(Duration::from_millis(300)))
        .expect(1)
        .mount(&server)
        .await;
    ews("<m:Unsubscribe>")
        .respond_with(success("Unsubscribe", ""))
        .expect(0)
        .mount(&server)
        .await;
    ews("<t:SubscriptionId>sub1</t:SubscriptionId>")
        .respond_with(new_mail("inbox-id"))
        .mount(&server)
        .await;
    let store = store(&server);
    let inbox = [FolderId::new("inbox-id")];
    // Canceled while subscribing: the subscription is made all the same,
    // and kept for the next watch, which makes no other.
    let canceled = tokio::time::timeout(Duration::from_millis(50), store.watch(&inbox)).await;
    assert!(canceled.is_err());
    assert_eq!(store.watch(&inbox).await.unwrap(), inbox);
}

/// A stream's envelope telling of new mail in the Inbox.
fn inbox_mail() -> String {
    streamed(
        "NoError",
        "<m:Notifications><m:Notification>\
         <t:NewMailEvent><t:ItemId Id=\"m9\"/><t:ParentFolderId Id=\"inbox-id\"/>\
         </t:NewMailEvent></m:Notification></m:Notifications>",
    )
}

/// A store on a server whose stream of events sends `events`, then stays
/// open for `hold`, and breaks: wiremock can't break one.
async fn store_breaking_streams(events: String, hold: Duration) -> EwsMailStore<StaticAuth> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mail = Arc::new(events);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mail = mail.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                // The whole request: its headers, then as long a body as
                // they say.
                let body = loop {
                    let n = socket.read(&mut buffer).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..n]);
                    let text = String::from_utf8_lossy(&request).into_owned();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else {
                        continue;
                    };
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|n| n.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break body.to_owned();
                    }
                };
                let envelope = if body.contains("<m:Subscribe>") {
                    "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
                     <s:Body xmlns:m=\"http://schemas.microsoft.com/exchange/services/2006/messages\">\
                     <m:SubscribeResponse><m:ResponseMessages>\
                     <m:SubscribeResponseMessage ResponseClass=\"Success\">\
                     <m:ResponseCode>NoError</m:ResponseCode><m:SubscriptionId>sub</m:SubscriptionId>\
                     </m:SubscribeResponseMessage></m:ResponseMessages></m:SubscribeResponse>\
                     </s:Body></s:Envelope>"
                        .to_owned()
                } else {
                    mail.to_string()
                };
                let chunked = !body.contains("<m:Subscribe>");
                let response = if chunked {
                    // One chunk, and no end.
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\n\
                         Transfer-Encoding: chunked\r\n\r\n{:x}\r\n{envelope}\r\n",
                        envelope.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\n\
                         Connection: close\r\nContent-Length: {}\r\n\r\n{envelope}",
                        envelope.len()
                    )
                };
                socket.write_all(response.as_bytes()).await.unwrap();
                if chunked {
                    socket.flush().await.unwrap();
                    tokio::time::sleep(hold).await;
                    drop(socket);
                }
            });
        }
    });
    let url = Url::parse(&format!("http://{address}{ENDPOINT}")).unwrap();
    EwsMailStore::new(
        reqwest::Client::new(),
        url,
        "alice@contoso.com".into(),
        Arc::new(StaticAuth),
    )
}

#[tokio::test]
async fn changes_read_before_a_broken_stream_are_kept() {
    let store = store_breaking_streams(inbox_mail(), Duration::from_millis(50)).await;
    // What came after the mail is lost: any folder watched may have
    // changed.
    let folders = [FolderId::new("inbox-id"), FolderId::new("work-id")];
    assert_eq!(store.watch(&folders).await.unwrap(), folders);
}

#[tokio::test]
async fn changes_read_by_a_canceled_watch_are_told_by_the_next() {
    // Canceled while reading the rest of the burst.
    let store = store_breaking_streams(inbox_mail(), Duration::from_secs(5)).await;
    let inbox = [FolderId::new("inbox-id")];
    let canceled = tokio::time::timeout(Duration::from_millis(500), store.watch(&inbox)).await;
    assert!(canceled.is_err());
    // EWS won't tell again.
    let told = tokio::time::timeout(Duration::from_millis(100), store.watch(&inbox)).await;
    assert_eq!(told.expect("told at once").unwrap(), inbox);
}

#[tokio::test]
async fn subscriptions_that_could_not_be_ended_are_ended_later() {
    let server = MockServer::start().await;
    ews("<m:Subscribe>")
        .and(body_string_contains("<t:FolderId Id=\"inbox-id\"/>"))
        .respond_with(subscribed("sub1"))
        .mount(&server)
        .await;
    ews("<m:Subscribe>")
        .and(body_string_contains("<t:FolderId Id=\"work-id\"/>"))
        .respond_with(subscribed("sub2"))
        .mount(&server)
        .await;
    ews("<m:Unsubscribe><m:SubscriptionId>sub1</m:SubscriptionId>")
        .respond_with(failure("Unsubscribe", "ErrorInternalServerError"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    ews("<m:Unsubscribe><m:SubscriptionId>sub1</m:SubscriptionId>")
        .respond_with(success("Unsubscribe", ""))
        .expect(1)
        .mount(&server)
        .await;
    // Gone already: as good as ended.
    ews("<m:Unsubscribe><m:SubscriptionId>sub2</m:SubscriptionId>")
        .respond_with(failure("Unsubscribe", "ErrorSubscriptionNotFound"))
        .expect(1)
        .mount(&server)
        .await;
    ews("<m:GetStreamingEvents>")
        .respond_with(new_mail("inbox-id"))
        .mount(&server)
        .await;
    let store = store(&server);
    store.watch(&[FolderId::new("inbox-id")]).await.unwrap();
    // sub1 can't be ended now: it is, the next time.
    store.watch(&[FolderId::new("work-id")]).await.unwrap();
    assert!(store.watch(&[]).await.is_ok());
    assert!(store.watch(&[]).await.is_ok());
}

#[tokio::test]
async fn a_stream_closed_at_once_is_an_error() {
    let server = MockServer::start().await;
    ews("<m:Subscribe>")
        .respond_with(subscribed("sub"))
        .mount(&server)
        .await;
    ews("<m:GetStreamingEvents>")
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            streamed("NoError", "<m:ConnectionStatus>Closed</m:ConnectionStatus>"),
            "text/xml",
        ))
        .expect(1)
        .mount(&server)
        .await;
    // Watched again at once, it could be cut short again and again: the
    // caller waits a while.
    let e = store(&server)
        .watch(&[FolderId::new("inbox-id")])
        .await
        .unwrap_err();
    assert!(matches!(e, mett_core::Error::Network(_)), "{e:?}");
}

#[tokio::test]
async fn changes_told_before_the_subscription_ends_are_kept() {
    let server = MockServer::start().await;
    ews("<m:Subscribe>")
        .respond_with(subscribed("sub"))
        .mount(&server)
        .await;
    let mail = streamed(
        "NoError",
        "<m:Notifications><m:Notification>\
         <t:NewMailEvent><t:ItemId Id=\"m9\"/><t:ParentFolderId Id=\"inbox-id\"/>\
         </t:NewMailEvent></m:Notification></m:Notifications>",
    );
    let gone = streamed("ErrorSubscriptionNotFound", "");
    let busy = streamed("ErrorServerBusy", "");
    ews("<m:GetStreamingEvents>")
        .respond_with(ResponseTemplate::new(200).set_body_raw(format!("{mail}{gone}"), "text/xml"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    ews("<m:GetStreamingEvents>")
        .respond_with(ResponseTemplate::new(200).set_body_raw(format!("{mail}{busy}"), "text/xml"))
        .mount(&server)
        .await;
    let store = store(&server);
    let inbox = FolderId::new("inbox-id");
    let folders = [inbox.clone(), FolderId::new("work-id")];
    // The subscription ended after the mail came: the mail is told.
    assert_eq!(store.watch(&folders).await.unwrap(), [inbox]);
    // An error after it: what else changed is unknown.
    assert_eq!(store.watch(&folders).await.unwrap(), folders);
}

#[tokio::test]
async fn watching_no_folders_ends_the_subscription() {
    let server = MockServer::start().await;
    ews("<m:Subscribe>")
        .respond_with(subscribed("sub1"))
        .mount(&server)
        .await;
    ews("<m:GetStreamingEvents>")
        .respond_with(new_mail("inbox-id"))
        .mount(&server)
        .await;
    ews("<m:Unsubscribe><m:SubscriptionId>sub1</m:SubscriptionId>")
        .respond_with(success("Unsubscribe", ""))
        .expect(1)
        .mount(&server)
        .await;
    let store = store(&server);
    // Nothing to end yet.
    assert!(store.watch(&[]).await.unwrap().is_empty());
    store.watch(&[FolderId::new("inbox-id")]).await.unwrap();
    assert!(store.watch(&[]).await.unwrap().is_empty());
    // Ended once.
    assert!(store.watch(&[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn watching_stops_after_a_canceled_subscription_is_made() {
    let server = MockServer::start().await;
    ews("<m:Subscribe>")
        .respond_with(subscribed("sub1").set_delay(Duration::from_millis(300)))
        .mount(&server)
        .await;
    ews("<m:Unsubscribe><m:SubscriptionId>sub1</m:SubscriptionId>")
        .respond_with(success("Unsubscribe", ""))
        .expect(1)
        .mount(&server)
        .await;
    let store = store(&server);
    let canceled = tokio::time::timeout(
        Duration::from_millis(50),
        store.watch(&[FolderId::new("inbox-id")]),
    )
    .await;
    assert!(canceled.is_err());
    // Stopped after: the subscription made meanwhile is ended.
    assert!(store.watch(&[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_envelope_a_canceled_watch_read_part_of_is_told_by_the_next() {
    let mail = inbox_mail();
    let store =
        store_breaking_streams(mail[..mail.len() / 2].to_owned(), Duration::from_secs(5)).await;
    let folders = [FolderId::new("inbox-id"), FolderId::new("work-id")];
    let canceled = tokio::time::timeout(Duration::from_millis(500), store.watch(&folders)).await;
    assert!(canceled.is_err());
    // Any folder watched may have changed: EWS won't tell again.
    let told = tokio::time::timeout(Duration::from_millis(100), store.watch(&folders)).await;
    assert_eq!(told.expect("told at once").unwrap(), folders);
}

#[tokio::test]
async fn an_envelope_cut_short_by_a_broken_stream_is_told() {
    let mail = inbox_mail();
    let store =
        store_breaking_streams(mail[..mail.len() / 2].to_owned(), Duration::from_millis(50)).await;
    // Any folder watched may have changed.
    let folders = [FolderId::new("inbox-id"), FolderId::new("work-id")];
    assert_eq!(store.watch(&folders).await.unwrap(), folders);
}
