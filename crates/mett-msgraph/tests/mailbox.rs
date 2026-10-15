//! GraphMailStore against a mock Microsoft Graph.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use mett_core::{
    AccessToken, Address, Authenticator, BodyKind, Draft, DraftOrigin, Error, FlagsUpdate,
    FolderId, MailSender, MailStore, MessageId, Page, PageCursor, WellKnownFolder,
};
use mett_msgraph::{AuthSettings, GraphMailStore};
use serde_json::{Value, json};
use url::Url;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Hands out a fixed token.
struct StaticAuth;

impl Authenticator for StaticAuth {
    async fn access_token(&self) -> mett_core::Result<AccessToken> {
        Ok(AccessToken::new("tok", Utc::now() + TimeDelta::hours(1)))
    }
}

fn store(server: &MockServer) -> GraphMailStore<StaticAuth> {
    let settings = AuthSettings {
        graph_url: Some(Url::parse(&server.uri()).unwrap()),
        ..AuthSettings::new("alice@contoso.com".into(), "client")
    };
    GraphMailStore::new(reqwest::Client::new(), &settings, Arc::new(StaticAuth))
}

fn folder(id: &str, name: &str) -> Value {
    child(id, name, "root-id", 0)
}

fn child(id: &str, name: &str, parent: &str, children: u32) -> Value {
    json!({
        "id": id,
        "displayName": name,
        "parentFolderId": parent,
        "childFolderCount": children,
        "unreadItemCount": 2,
        "totalItemCount": 5,
    })
}

/// Answers the `$batch` role lookup: the root is `root-id` and only the
/// Inbox (`inbox-id`) exists among the well-known folders.
async fn mount_roles(server: &MockServer) {
    mount_roles_with(server, 404).await;
}

/// Like [`mount_roles`], with `status` for the well-known folders other
/// than the Inbox.
async fn mount_roles_with(server: &MockServer, status: u16) {
    let names = [
        "msgfolderroot",
        "inbox",
        "drafts",
        "sentitems",
        "deleteditems",
        "junkemail",
        "archive",
        "outbox",
    ];
    let responses: Vec<Value> = names
        .iter()
        .enumerate()
        .map(|(i, name)| match *name {
            "msgfolderroot" => json!({ "id": i.to_string(), "status": 200, "body": { "id": "root-id" } }),
            "inbox" => json!({ "id": i.to_string(), "status": 200, "body": { "id": "inbox-id" } }),
            _ => json!({ "id": i.to_string(), "status": status, "body": { "error": { "code": "Error", "message": "" } } }),
        })
        .collect();
    Mock::given(method("POST"))
        .and(path("/v1.0/$batch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "responses": responses })))
        .mount(server)
        .await;
}

/// A message sent at `sent`.
fn message(id: &str, sent: &str) -> Value {
    json!({
        "id": id,
        "parentFolderId": "inbox-id",
        "subject": format!("Subject {id}"),
        "from": { "emailAddress": { "name": "Bob", "address": "bob@fabrikam.com" } },
        "toRecipients": [{ "emailAddress": { "name": "", "address": "alice@contoso.com" } }],
        "sentDateTime": sent,
        "isRead": false,
        "flag": { "flagStatus": "flagged" },
        "hasAttachments": true,
    })
}

#[tokio::test]
async fn finds_well_known_folders_and_labels_them() {
    let server = MockServer::start().await;
    mount_roles(&server).await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/inbox"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(folder("inbox-id", "Inbox")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [folder("inbox-id", "Inbox"), folder("other-id", "Projects")],
        })))
        .mount(&server)
        .await;

    let store = store(&server);
    // Listing first still labels the Inbox.
    let folders = store.list_folders().await.unwrap();
    let inbox = store
        .well_known_folder(WellKnownFolder::Inbox)
        .await
        .unwrap();
    assert_eq!(inbox.id, FolderId::new("inbox-id"));
    assert_eq!(inbox.well_known, Some(WellKnownFolder::Inbox));
    assert_eq!(inbox.parent, None, "top-level folders have no parent");
    assert_eq!((inbox.unread_count, inbox.total_count), (2, 5));

    let labels: Vec<_> = folders
        .iter()
        .map(|f| (f.name.as_str(), f.well_known))
        .collect();
    assert_eq!(
        labels,
        [("Inbox", Some(WellKnownFolder::Inbox)), ("Projects", None)]
    );
}

#[tokio::test]
async fn reads_a_folder_s_counts_by_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/msgfolderroot"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "root-id" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/other-id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(folder("other-id", "Projects")))
        .mount(&server)
        .await;
    let folder = store(&server)
        .folder(&FolderId::new("other-id"))
        .await
        .unwrap();
    assert_eq!(folder.name, "Projects");
    assert_eq!(folder.parent, None, "top-level folders have no parent");
    assert_eq!((folder.unread_count, folder.total_count), (2, 5));
}

#[tokio::test]
async fn lists_messages_changed_since_a_time() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/inbox-id/messages"))
        .and(query_param(
            "$filter",
            "lastModifiedDateTime ge 2024-05-01T10:00:00Z",
        ))
        .and(query_param("$orderby", "lastModifiedDateTime desc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [message("late", "2024-04-01T08:00:00Z")],
        })))
        .mount(&server)
        .await;
    let since = DateTime::parse_from_rfc3339("2024-05-01T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let page = store(&server)
        .changed_messages(&FolderId::new("inbox-id"), since, Page::first(10))
        .await
        .unwrap();
    assert_eq!(page.messages[0].id, MessageId::new("late"));
    assert_eq!(page.next, None);
}

#[tokio::test]
async fn finds_when_unread_mail_last_changed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/inbox-id/messages"))
        .and(query_param(
            "$filter",
            "lastModifiedDateTime ge 1900-01-01T00:00:00Z and isRead eq false",
        ))
        .and(query_param("$orderby", "lastModifiedDateTime desc"))
        .and(query_param("$top", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [{ "lastModifiedDateTime": "2024-05-01T10:00:00Z" }],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/empty-id/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
        .mount(&server)
        .await;
    let store = store(&server);
    let newest = store
        .newest_unread(&FolderId::new("inbox-id"))
        .await
        .unwrap();
    assert_eq!(
        newest.map(|t| t.to_rfc3339()),
        Some("2024-05-01T10:00:00+00:00".to_owned())
    );
    assert_eq!(
        store
            .newest_unread(&FolderId::new("empty-id"))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn does_not_cache_roles_after_a_failed_lookup() {
    let server = MockServer::start().await;
    mount_roles_with(&server, 503).await;
    let store = store(&server);
    assert!(matches!(store.list_folders().await, Err(Error::Network(_))));
    // Nothing was cached: the next call looks the roles up again.
    server.reset().await;
    mount_roles(&server).await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
        .mount(&server)
        .await;
    assert!(store.list_folders().await.is_ok());
}

#[tokio::test]
async fn lists_nested_folders() {
    let server = MockServer::start().await;
    mount_roles(&server).await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders"))
        .and(query_param("includeHiddenFolders", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [child("inbox-id", "Inbox", "root-id", 1)],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/inbox-id/childFolders"))
        .and(query_param("includeHiddenFolders", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [child("projects-id", "Projects", "inbox-id", 1)],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/projects-id/childFolders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [child("mett-id", "mett", "projects-id", 0)],
        })))
        .mount(&server)
        .await;

    let folders = store(&server).list_folders().await.unwrap();
    let tree: Vec<_> = folders
        .iter()
        .map(|f| (f.name.as_str(), f.parent.as_ref().map(FolderId::as_str)))
        .collect();
    assert_eq!(
        tree,
        [
            ("Inbox", None),
            ("Projects", Some("inbox-id")),
            ("mett", Some("projects-id")),
        ]
    );
}

#[tokio::test]
async fn later_pages_replay_graphs_link_unchanged() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/inbox-id/messages"))
        .and(query_param("$skip", "10"))
        .and(query_param("$top", "10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
        .expect(1)
        .mount(&server)
        .await;
    let cursor = PageCursor::new(format!(
        "inbox-id {}/v1.0/me/mailFolders/inbox-id/messages?$top=10&$skip=10",
        server.uri()
    ));
    // A different size doesn't alter Graph's continuation link.
    store(&server)
        .list_messages(&FolderId::new("inbox-id"), Page::after(cursor, 3))
        .await
        .unwrap();
}

#[tokio::test]
async fn lists_messages_newest_first_with_paging() {
    let server = MockServer::start().await;
    let next = format!(
        "{}/v1.0/me/mailFolders/inbox-id/messages?$skiptoken=abc",
        server.uri()
    );
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/inbox-id/messages"))
        .and(query_param("$top", "2"))
        .and(query_param(
            "$orderby",
            "sentDateTime desc,createdDateTime desc",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [message("m2", "2026-10-14T10:00:00Z"), message("m1", "2026-10-13T10:00:00Z")],
            "@odata.nextLink": next,
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/inbox-id/messages"))
        .and(query_param("$skiptoken", "abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [message("m0", "2026-10-12T10:00:00Z")],
        })))
        .mount(&server)
        .await;

    let store = store(&server);
    let inbox = FolderId::new("inbox-id");
    let first = store.list_messages(&inbox, Page::first(2)).await.unwrap();
    assert_eq!(first.messages.len(), 2);
    let m2 = &first.messages[0];
    assert_eq!(m2.id, MessageId::new("m2"));
    assert_eq!(m2.subject, "Subject m2");
    assert_eq!(
        m2.from.as_ref().unwrap().to_string(),
        "Bob <bob@fabrikam.com>"
    );
    assert_eq!(m2.to[0].name, None, "empty names are dropped");
    assert!(!m2.flags.read && m2.flags.flagged && m2.has_attachments);
    assert_eq!(
        m2.received,
        DateTime::parse_from_rfc3339("2026-10-14T10:00:00Z").unwrap()
    );

    let cursor = first.next.expect("a next page");
    let second = store
        .list_messages(&inbox, Page::after(cursor, 2))
        .await
        .unwrap();
    assert_eq!(second.messages[0].id, MessageId::new("m0"));
    assert_eq!(second.next, None);
}

#[tokio::test]
async fn refuses_page_links_to_other_hosts() {
    let server = MockServer::start().await;
    let store = store(&server);
    let cursor = PageCursor::new("inbox-id https://attacker.example/v1.0/me/messages");
    let result = store
        .list_messages(&FolderId::new("inbox-id"), Page::after(cursor, 10))
        .await;
    assert!(matches!(result, Err(Error::Backend(msg)) if msg.contains("another host")));

    // A scheme-relative remainder can't escape to another host either.
    let sneaky = "inbox-id https://graph.microsoft.com/v1.0///attacker.example/x";
    let result = store
        .list_messages(
            &FolderId::new("inbox-id"),
            Page::after(PageCursor::new(sneaky), 10),
        )
        .await;
    assert!(matches!(result, Err(Error::Backend(_))), "{result:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn fetches_a_message_as_text() {
    let server = MockServer::start().await;
    let mut body = message("m1", "2026-10-13T10:00:00Z");
    body["ccRecipients"] =
        json!([{ "emailAddress": { "name": "Carol", "address": "carol@contoso.com" } }]);
    body["replyTo"] = json!([{ "emailAddress": { "address": "list@contoso.com" } }]);
    body["internetMessageId"] = json!("<abc@contoso.com>");
    body["body"] = json!({ "contentType": "text", "content": "Hello" });
    Mock::given(method("GET"))
        .and(path("/v1.0/me/messages/m1"))
        .and(header("prefer", "outlook.body-content-type=\"text\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let message = store(&server)
        .fetch_message(&MessageId::new("m1"))
        .await
        .unwrap();
    assert_eq!(message.body.kind, BodyKind::Text);
    assert_eq!(message.body.content, "Hello");
    assert_eq!(message.cc[0].to_string(), "Carol <carol@contoso.com>");
    assert_eq!(message.reply_to, [Address::new("list@contoso.com")]);
    assert_eq!(
        message.internet_message_id.as_deref(),
        Some("<abc@contoso.com>")
    );
}

#[tokio::test]
async fn escapes_ids_in_paths() {
    let server = MockServer::start().await;
    // Graph IDs are base64 and may contain '/' and '+'.
    Mock::given(method("GET"))
        .and(path("/v1.0/me/messages/AA%2Fbb+cc="))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(message("AA/bb+cc=", "2026-10-13T10:00:00Z")),
        )
        .expect(1)
        .mount(&server)
        .await;
    store(&server)
        .fetch_message(&MessageId::new("AA/bb+cc="))
        .await
        .unwrap();
}

#[tokio::test]
async fn updates_flags_with_a_patch() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/m1"))
        .and(body_json(
            json!({ "isRead": true, "flag": { "flagStatus": "notFlagged" } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let store = store(&server);
    store
        .update_flags(
            &MessageId::new("m1"),
            FlagsUpdate {
                read: Some(true),
                flagged: Some(false),
            },
        )
        .await
        .unwrap();
    // An empty update sends nothing (the mock expects exactly one call).
    store
        .update_flags(&MessageId::new("m1"), FlagsUpdate::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn moves_copies_and_permanently_deletes_messages() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/m1/move"))
        .and(body_json(json!({ "destinationId": "archive-id" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "m1-moved" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/m1-moved/permanentDelete"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/m1-moved/copy"))
        .and(body_json(json!({ "destinationId": "inbox-id" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "m1-copy" })))
        .expect(1)
        .mount(&server)
        .await;

    let store = store(&server);
    let moved = store
        .move_message(&MessageId::new("m1"), &FolderId::new("archive-id"))
        .await
        .unwrap();
    assert_eq!(moved, MessageId::new("m1-moved"));
    let copy = store
        .copy_message(&moved, &FolderId::new("inbox-id"))
        .await
        .unwrap();
    assert_eq!(copy, MessageId::new("m1-copy"));
    store.delete_message(&moved).await.unwrap();
}

#[tokio::test]
async fn maps_graph_errors_to_port_errors() {
    let server = MockServer::start().await;
    let error = |code: &str| json!({ "error": { "code": code, "message": "details" } });
    Mock::given(path("/v1.0/me/messages/missing"))
        .respond_with(ResponseTemplate::new(404).set_body_json(error("ErrorItemNotFound")))
        .mount(&server)
        .await;
    Mock::given(path("/v1.0/me/messages/busy"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "7")
                .set_body_json(error("TooManyRequests")),
        )
        .mount(&server)
        .await;
    Mock::given(path("/v1.0/me/messages/denied"))
        .respond_with(ResponseTemplate::new(403).set_body_json(error("ErrorAccessDenied")))
        .mount(&server)
        .await;
    Mock::given(path("/v1.0/me/messages/down"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let store = store(&server);
    let fetch = |id: &'static str| {
        let store = &store;
        async move { store.fetch_message(&MessageId::new(id)).await.unwrap_err() }
    };
    assert!(matches!(
        fetch("missing").await,
        Error::NotFound { kind: "message", id } if id == "missing"
    ));
    assert!(matches!(
        fetch("busy").await,
        Error::Throttled { retry_after: Some(d) } if d == Duration::from_secs(7)
    ));
    assert!(matches!(fetch("denied").await, Error::Auth(msg) if msg.contains("ErrorAccessDenied")));
    assert!(matches!(fetch("down").await, Error::Network(_)));
}

#[tokio::test]
async fn unsent_drafts_use_their_creation_time() {
    let server = MockServer::start().await;
    let mut draft = message("d1", "2026-10-14T10:00:00Z");
    // Graph may fill in a placeholder sent date for a draft.
    draft["sentDateTime"] = json!("0001-01-01T00:00:00Z");
    draft["isDraft"] = json!(true);
    draft["createdDateTime"] = json!("2026-10-10T08:00:00Z");
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/drafts-id/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [draft] })))
        .mount(&server)
        .await;

    let page = store(&server)
        .list_messages(&FolderId::new("drafts-id"), Page::first(10))
        .await
        .unwrap();
    assert_eq!(
        page.messages[0].received,
        DateTime::parse_from_rfc3339("2026-10-10T08:00:00Z").unwrap()
    );
}

#[tokio::test]
async fn batch_item_failures_keep_their_meaning() {
    let server = MockServer::start().await;
    let item = |status: u16, headers: Value| json!({ "responses": [{ "id": "0", "status": status, "headers": headers, "body": {} }] });
    Mock::given(method("POST"))
        .and(path("/v1.0/$batch"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(item(429, json!({ "Retry-After": "9" }))),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/$batch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(item(403, json!({}))))
        .with_priority(2)
        .mount(&server)
        .await;

    let store = store(&server);
    assert!(matches!(
        store.list_folders().await,
        Err(Error::Throttled { retry_after: Some(d) }) if d == Duration::from_secs(9)
    ));
    assert!(matches!(store.list_folders().await, Err(Error::Auth(_))));
}

#[tokio::test]
async fn caps_page_sizes_at_graphs_limit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/inbox-id/messages"))
        .and(query_param("$top", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
        .expect(1)
        .mount(&server)
        .await;
    store(&server)
        .list_messages(&FolderId::new("inbox-id"), Page::first(5000))
        .await
        .unwrap();
}

#[tokio::test]
async fn cursors_belong_to_their_folder() {
    let server = MockServer::start().await;
    let link = format!(
        "{}/v1.0/me/mailFolders/inbox-id/messages?$skip=10",
        server.uri()
    );
    let result = store(&server)
        .list_messages(
            &FolderId::new("archive-id"),
            Page::after(PageCursor::new(format!("inbox-id {link}")), 10),
        )
        .await;
    assert!(matches!(result, Err(Error::Backend(msg)) if msg.contains("not for folder")));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn follows_canonical_graph_links_through_a_proxy() {
    let server = MockServer::start().await;
    // Behind a proxy, Graph still writes its own host into page links.
    let link = "https://graph.microsoft.com/v1.0/me/mailFolders/inbox-id/messages?$skip=10";
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/inbox-id/messages"))
        .and(query_param("$skip", "10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
        .expect(1)
        .mount(&server)
        .await;
    store(&server)
        .list_messages(
            &FolderId::new("inbox-id"),
            Page::after(PageCursor::new(format!("inbox-id {link}")), 10),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn messages_are_dated_and_sorted_by_when_they_were_sent() {
    let server = MockServer::start().await;
    let mut message = message("s1", "2026-10-13T09:00:00Z");
    // Received a day later; the index still shows the sent date, as mutt
    // does, so the date matches the sort order.
    message["receivedDateTime"] = json!("2026-10-14T09:00:00Z");
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/sent-id/messages"))
        .and(query_param(
            "$orderby",
            "sentDateTime desc,createdDateTime desc",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [message] })))
        .mount(&server)
        .await;
    let page = store(&server)
        .list_messages(&FolderId::new("sent-id"), Page::first(10))
        .await
        .unwrap();
    assert_eq!(
        page.messages[0].received,
        DateTime::parse_from_rfc3339("2026-10-13T09:00:00Z").unwrap()
    );
}

#[tokio::test]
async fn opening_the_inbox_needs_only_the_root() {
    let server = MockServer::start().await;
    // Any $batch would fail; opening the Inbox must not need it.
    Mock::given(method("POST"))
        .and(path("/v1.0/$batch"))
        .respond_with(ResponseTemplate::new(503))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/msgfolderroot"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "root-id" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/inbox"))
        .respond_with(ResponseTemplate::new(200).set_body_json(folder("inbox-id", "Inbox")))
        .mount(&server)
        .await;

    let store = store(&server);
    for _ in 0..2 {
        let inbox = store
            .well_known_folder(WellKnownFolder::Inbox)
            .await
            .unwrap();
        assert_eq!(inbox.parent, None);
    }
}

/// Hands out `old` until told it was rejected, then `new`.
struct RotatingAuth {
    rejected: std::sync::atomic::AtomicBool,
}

impl Authenticator for RotatingAuth {
    async fn access_token(&self) -> mett_core::Result<AccessToken> {
        let secret = if self.rejected.load(std::sync::atomic::Ordering::SeqCst) {
            "new"
        } else {
            "old"
        };
        Ok(AccessToken::new(secret, DateTime::<Utc>::MAX_UTC))
    }

    async fn token_rejected(&self, token: &AccessToken) {
        assert_eq!(token.secret(), "old");
        self.rejected
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test]
async fn retries_once_with_a_new_token_after_a_401() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/m1"))
        .and(header("authorization", "Bearer old"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/m1"))
        .and(header("authorization", "Bearer new"))
        .and(body_json(json!({ "isRead": true })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let settings = AuthSettings {
        graph_url: Some(Url::parse(&server.uri()).unwrap()),
        ..AuthSettings::new("alice@contoso.com".into(), "client")
    };
    let auth = Arc::new(RotatingAuth {
        rejected: Default::default(),
    });
    let store = GraphMailStore::new(reqwest::Client::new(), &settings, auth);
    store
        .update_flags(
            &MessageId::new("m1"),
            FlagsUpdate {
                read: Some(true),
                flagged: None,
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn permanent_delete_is_reported_unavailable_in_national_clouds() {
    let server = MockServer::start().await;
    let settings = AuthSettings {
        cloud: mett_msgraph::Cloud::UsGov,
        graph_url: Some(Url::parse(&server.uri()).unwrap()),
        ..AuthSettings::new("alice@contoso.com".into(), "client")
    };
    let store = GraphMailStore::new(reqwest::Client::new(), &settings, Arc::new(StaticAuth));
    let result = store.delete_message(&MessageId::new("m1")).await;
    assert!(matches!(result, Err(Error::Backend(msg)) if msg.contains("usgov")));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn recognises_well_known_folders_created_later() {
    let server = MockServer::start().await;
    // At first the mailbox has no Archive folder…
    mount_roles(&server).await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [folder("archive-id", "Archive")],
        })))
        .mount(&server)
        .await;
    let store = store(&server);
    let first = store.list_folders().await.unwrap();
    assert_eq!(first[0].well_known, None);

    // …then it is created, and the next listing recognises it.
    let with_archive: Vec<Value> = [
        json!({ "id": "0", "status": 200, "body": { "id": "root-id" } }),
        json!({ "id": "6", "status": 200, "body": { "id": "archive-id" } }),
    ]
    .into();
    Mock::given(method("POST"))
        .and(path("/v1.0/$batch"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "responses": with_archive })),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    let second = store.list_folders().await.unwrap();
    assert_eq!(second[0].well_known, Some(WellKnownFolder::Archive));
}

/// Counts rejected tokens.
#[derive(Default)]
struct CountingAuth {
    rejected: std::sync::atomic::AtomicUsize,
}

impl Authenticator for CountingAuth {
    async fn access_token(&self) -> mett_core::Result<AccessToken> {
        Ok(AccessToken::new("tok", DateTime::<Utc>::MAX_UTC))
    }

    async fn token_rejected(&self, _: &AccessToken) {
        self.rejected
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_401_inside_a_batch_is_retried_once_with_a_new_token() {
    let server = MockServer::start().await;
    let rejected = json!({ "responses": [{ "id": "0", "status": 401, "body": {} }] });
    Mock::given(method("POST"))
        .and(path("/v1.0/$batch"))
        .and(header("authorization", "Bearer old"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rejected))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/$batch"))
        .and(header("authorization", "Bearer new"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "responses": [{ "id": "0", "status": 200, "body": { "id": "root-id" } }],
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
        .mount(&server)
        .await;

    let settings = AuthSettings {
        graph_url: Some(Url::parse(&server.uri()).unwrap()),
        ..AuthSettings::new("alice@contoso.com".into(), "client")
    };
    let auth = Arc::new(RotatingAuth {
        rejected: Default::default(),
    });
    let store = GraphMailStore::new(reqwest::Client::new(), &settings, auth);
    store.list_folders().await.unwrap();
}

#[tokio::test]
async fn a_repeated_401_inside_a_batch_is_an_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/$batch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "responses": [{ "id": "0", "status": 401, "body": {} }],
        })))
        .expect(2)
        .mount(&server)
        .await;
    let settings = AuthSettings {
        graph_url: Some(Url::parse(&server.uri()).unwrap()),
        ..AuthSettings::new("alice@contoso.com".into(), "client")
    };
    let auth = Arc::new(CountingAuth::default());
    let store = GraphMailStore::new(reqwest::Client::new(), &settings, Arc::clone(&auth));
    assert!(matches!(store.list_folders().await, Err(Error::Auth(_))));
    assert_eq!(auth.rejected.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_token_rejected_on_retry_is_reported_too() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/messages/m1"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let settings = AuthSettings {
        graph_url: Some(Url::parse(&server.uri()).unwrap()),
        ..AuthSettings::new("alice@contoso.com".into(), "client")
    };
    let auth = Arc::new(RotatingAuth {
        rejected: Default::default(),
    });
    let rejections = Arc::new(std::sync::Mutex::new(Vec::new()));
    let store = GraphMailStore::new(
        reqwest::Client::new(),
        &settings,
        Arc::new(Recording {
            inner: auth,
            rejections: Arc::clone(&rejections),
        }),
    );
    assert!(matches!(
        store.fetch_message(&MessageId::new("m1")).await,
        Err(Error::Auth(_))
    ));
    assert_eq!(*rejections.lock().unwrap(), ["old", "new"]);
}

/// Wraps an authenticator, recording which tokens were rejected.
struct Recording {
    inner: Arc<RotatingAuth>,
    rejections: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Authenticator for Recording {
    async fn access_token(&self) -> mett_core::Result<AccessToken> {
        self.inner.access_token().await
    }

    async fn token_rejected(&self, token: &AccessToken) {
        self.rejections
            .lock()
            .unwrap()
            .push(token.secret().to_owned());
        if token.secret() == "old" {
            self.inner.token_rejected(token).await;
        }
    }
}

fn draft(origin: Option<DraftOrigin>) -> Draft {
    Draft {
        to: vec![Address::new("bob@fabrikam.com").with_name("Bob")],
        cc: vec![Address::new("carol@fabrikam.com")],
        subject: "Hello".into(),
        body: "Hi Bob\n".into(),
        origin,
        save_copy: true,
        ..Draft::default()
    }
}

/// The Graph `message` that [`draft`] becomes.
fn draft_json() -> Value {
    json!({
        "subject": "Hello",
        "body": { "contentType": "Text", "content": "Hi Bob\n" },
        "toRecipients": [{ "emailAddress": { "address": "bob@fabrikam.com", "name": "Bob" } }],
        "ccRecipients": [{ "emailAddress": { "address": "carol@fabrikam.com" } }],
        "bccRecipients": [],
    })
}

#[tokio::test]
async fn sends_new_mail_with_send_mail() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/sendMail"))
        .and(body_json(
            json!({ "message": draft_json(), "saveToSentItems": true }),
        ))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    store(&server).send(&draft(None)).await.unwrap();
}

#[tokio::test]
async fn sends_replies_through_a_server_draft() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createReply"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "d1" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/d1"))
        .and(body_json(draft_json()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "d1" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/d1/send"))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    let reply = draft(Some(DraftOrigin::Reply(MessageId::new("orig"))));
    store(&server).send(&reply).await.unwrap();
}

#[tokio::test]
async fn discards_the_server_draft_when_sending_fails() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createForward"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "d1" })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/d1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "d1" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/d1/send"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": { "code": "ErrorInvalidRecipients", "message": "bad recipient" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/d1/permanentDelete"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let forward = draft(Some(DraftOrigin::Forward(MessageId::new("orig"))));
    let err = store(&server).send(&forward).await.unwrap_err();
    assert!(matches!(err, Error::Backend(m) if m.contains("bad recipient")));
}

#[tokio::test]
async fn saves_drafts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages"))
        .and(body_json(draft_json()))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "new" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createReply"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "reply" })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/reply"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "reply" })))
        .mount(&server)
        .await;
    let store = store(&server);
    assert_eq!(
        store.save_draft(&draft(None)).await.unwrap(),
        MessageId::new("new")
    );
    let reply = draft(Some(DraftOrigin::Reply(MessageId::new("orig"))));
    assert_eq!(
        store.save_draft(&reply).await.unwrap(),
        MessageId::new("reply")
    );
}

#[tokio::test]
async fn keeps_the_server_draft_when_the_send_outcome_is_unknown() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createReply"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "d1" })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/d1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "d1" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/d1/send"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/d1/permanentDelete"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&server)
        .await;
    let reply = draft(Some(DraftOrigin::Reply(MessageId::new("orig"))));
    let err = store(&server).send(&reply).await.unwrap_err();
    assert!(matches!(err, Error::Network(m) if m.contains("in Drafts")));
}

#[tokio::test]
async fn postponing_after_an_unknown_send_outcome_reuses_the_draft() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createReply"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "d1" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/d1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "d1" })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/d1/send"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let store = store(&server);
    let reply = draft(Some(DraftOrigin::Reply(MessageId::new("orig"))));
    store.send(&reply).await.unwrap_err();
    assert_eq!(
        store.save_draft(&reply).await.unwrap(),
        MessageId::new("d1")
    );
}

#[tokio::test]
async fn keeps_the_server_draft_when_its_update_outcome_is_unknown() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createForward"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "d1" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/d1"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/d1/permanentDelete"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&server)
        .await;
    let store = store(&server);
    let forward = draft(Some(DraftOrigin::Forward(MessageId::new("orig"))));
    let err = store.save_draft(&forward).await.unwrap_err();
    assert!(matches!(err, Error::Network(_)), "{err:?}");
    // Trying again updates the same draft.
    store.save_draft(&forward).await.unwrap_err();
}

#[tokio::test]
async fn a_kept_draft_that_is_gone_is_made_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createReply"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "d1" })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createReply"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "d2" })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/d1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "d1" })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/d1"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": { "code": "ErrorItemNotFound", "message": "gone" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/d2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "d2" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/d1/send"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let store = store(&server);
    let reply = draft(Some(DraftOrigin::Reply(MessageId::new("orig"))));
    store.send(&reply).await.unwrap_err();
    assert_eq!(
        store.save_draft(&reply).await.unwrap(),
        MessageId::new("d2")
    );
}

#[tokio::test]
async fn a_failure_before_sending_is_not_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createReply"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "d1" })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/d1"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/d1/send"))
        .respond_with(ResponseTemplate::new(202))
        .expect(0)
        .mount(&server)
        .await;
    let reply = draft(Some(DraftOrigin::Reply(MessageId::new("orig"))));
    let err = store(&server).send(&reply).await.unwrap_err();
    assert!(matches!(err, Error::NotSent(_)), "{err:?}");
}

#[tokio::test]
async fn another_reply_to_the_same_message_gets_its_own_draft() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createReply"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "d1" })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createReply"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "d2" })))
        .mount(&server)
        .await;
    for id in ["d1", "d2"] {
        Mock::given(method("PATCH"))
            .and(path(format!("/v1.0/me/messages/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": id })))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/d1/send"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let store = store(&server);
    let first = Draft {
        attempt: 1,
        ..draft(Some(DraftOrigin::Reply(MessageId::new("orig"))))
    };
    store.send(&first).await.unwrap_err();
    let second = Draft {
        attempt: 2,
        ..first.clone()
    };
    assert_eq!(
        store.save_draft(&second).await.unwrap(),
        MessageId::new("d2")
    );
    // The first is still kept for its own message.
    assert_eq!(
        store.save_draft(&first).await.unwrap(),
        MessageId::new("d1")
    );
}

#[tokio::test]
async fn a_draft_that_cannot_be_deleted_is_kept_for_the_next_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createReply"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "d1" })))
        .expect(1)
        .mount(&server)
        .await;
    let denied = || {
        ResponseTemplate::new(403).set_body_json(json!({
            "error": { "code": "ErrorAccessDenied", "message": "denied" }
        }))
    };
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/d1"))
        .respond_with(denied())
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/d1/permanentDelete"))
        .respond_with(denied())
        .mount(&server)
        .await;
    let store = store(&server);
    let reply = draft(Some(DraftOrigin::Reply(MessageId::new("orig"))));
    store.save_draft(&reply).await.unwrap_err();
    // Trying again updates that draft rather than making another.
    store.save_draft(&reply).await.unwrap_err();
}

/// An authenticator that can't reach the identity platform.
struct OfflineAuth;

impl Authenticator for OfflineAuth {
    async fn access_token(&self) -> mett_core::Result<AccessToken> {
        Err(Error::Network("identity platform unreachable".into()))
    }
}

#[tokio::test]
async fn failing_to_get_a_token_leaves_mail_unsent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(202))
        .expect(0)
        .mount(&server)
        .await;
    let settings = AuthSettings {
        graph_url: Some(Url::parse(&server.uri()).unwrap()),
        ..AuthSettings::new("alice@contoso.com".into(), "client")
    };
    let store = GraphMailStore::new(reqwest::Client::new(), &settings, Arc::new(OfflineAuth));
    let err = store.send(&draft(None)).await.unwrap_err();
    assert!(matches!(err, Error::NotSent(_)), "{err:?}");
    let reply = draft(Some(DraftOrigin::Reply(MessageId::new("orig"))));
    let err = store.send(&reply).await.unwrap_err();
    assert!(
        matches!(&err, Error::NotSent(m) if !m.contains("Drafts")),
        "nothing reached Graph: {err:?}"
    );
}

#[tokio::test]
async fn a_lost_create_reply_response_may_leave_a_draft() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createReply"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let reply = draft(Some(DraftOrigin::Reply(MessageId::new("orig"))));
    let err = store(&server).send(&reply).await.unwrap_err();
    assert!(
        matches!(&err, Error::NotSent(m) if m.contains("may be left in Drafts")),
        "{err:?}"
    );
}

#[tokio::test]
async fn giving_up_deletes_the_kept_draft() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createReply"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "d1" })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/d1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "d1" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/d1/send"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/d1/permanentDelete"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let store = store(&server);
    let reply = draft(Some(DraftOrigin::Reply(MessageId::new("orig"))));
    store.send(&reply).await.unwrap_err();
    store.discard_kept(&reply).await.unwrap();
    // Nothing kept any more.
    store.discard_kept(&reply).await.unwrap();
}

#[tokio::test]
async fn national_clouds_delete_drafts_into_deleted_items() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/messages/orig/createReply"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "d1" })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/d1"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": { "code": "ErrorInvalidRecipients", "message": "bad recipient" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v1.0/me/messages/d1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let settings = AuthSettings {
        cloud: mett_msgraph::Cloud::UsGov,
        graph_url: Some(Url::parse(&server.uri()).unwrap()),
        ..AuthSettings::new("alice@contoso.com".into(), "client")
    };
    let store = GraphMailStore::new(reqwest::Client::new(), &settings, Arc::new(StaticAuth));
    let reply = draft(Some(DraftOrigin::Reply(MessageId::new("orig"))));
    store.send(&reply).await.unwrap_err();
}

#[tokio::test]
async fn finds_the_users_own_addresses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me"))
        .and(query_param(
            "$select",
            "mail,userPrincipalName,proxyAddresses",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "mail": "Alice.Smith@contoso.com",
            "userPrincipalName": "asmith@contoso.onmicrosoft.com",
            "proxyAddresses": [
                "SMTP:Alice.Smith@contoso.com",
                "smtp:alice@contoso.com",
                "x500:/o=ExchangeLabs/cn=Recipients/cn=alice",
            ],
        })))
        .mount(&server)
        .await;
    let addresses = store(&server).own_addresses().await.unwrap();
    assert_eq!(
        addresses,
        [
            "Alice.Smith@contoso.com",
            "asmith@contoso.onmicrosoft.com",
            "Alice.Smith@contoso.com",
            "alice@contoso.com",
        ]
    );
}
