//! GraphMailStore against a mock Microsoft Graph.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use mett_core::{
    AccessToken, Authenticator, BodyKind, Error, FlagsUpdate, FolderId, MailStore, MessageId, Page,
    PageCursor, WellKnownFolder,
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
async fn moves_and_permanently_deletes_messages() {
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

    let store = store(&server);
    let moved = store
        .move_message(&MessageId::new("m1"), &FolderId::new("archive-id"))
        .await
        .unwrap();
    assert_eq!(moved, MessageId::new("m1-moved"));
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
