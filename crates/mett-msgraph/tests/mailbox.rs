//! GraphMailStore against a mock Microsoft Graph.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mett_core::mock::{CountingAuth, RotatingAuth, StaticAuth};
use mett_core::{
    AccessToken, Address, AttachmentKind, Authenticator, BodyKind, Draft, DraftOrigin, Error,
    FlagsUpdate, FolderId, MailSender, MailStore, MessageId, OutgoingAttachment, Page, PageCursor,
    SyncState, WellKnownFolder,
};
use mett_msgraph::{AuthSettings, Cloud, GraphMailStore};
use serde_json::{Value, json};
use url::Url;
use wiremock::matchers::{body_json, header, header_exists, method, path, query_param};
use wiremock::{Mock, MockBuilder, MockServer, ResponseTemplate};

fn store(server: &MockServer) -> GraphMailStore<StaticAuth> {
    store_with(server, Cloud::default(), Arc::new(StaticAuth))
}

/// A store in `cloud`, against `server`, with `auth` for tokens.
fn store_with<A: Authenticator>(
    server: &MockServer,
    cloud: Cloud,
    auth: Arc<A>,
) -> GraphMailStore<A> {
    let settings = AuthSettings {
        cloud,
        graph_url: Some(Url::parse(&server.uri()).unwrap()),
        ..AuthSettings::new("alice@contoso.com".into(), "client")
    };
    GraphMailStore::new(reqwest::Client::new(), &settings, auth)
}

/// Mocks for `verb` requests to `at`, below the Graph root, to add
/// matchers to.
fn route(verb: &str, at: &str) -> MockBuilder {
    Mock::given(method(verb)).and(path(format!("/v1.0{at}")))
}

/// Shorthands for mounting a mock.
trait On {
    /// Answers `verb` requests to `at` (see [`route`]) with `response`.
    async fn on(&self, verb: &str, at: &str, response: ResponseTemplate);
    /// Like [`On::on`], only the first `n` times.
    async fn on_n(&self, verb: &str, at: &str, n: u64, response: ResponseTemplate);
}

impl On for MockServer {
    async fn on(&self, verb: &str, at: &str, response: ResponseTemplate) {
        route(verb, at).respond_with(response).mount(self).await;
    }

    async fn on_n(&self, verb: &str, at: &str, n: u64, response: ResponseTemplate) {
        route(verb, at)
            .respond_with(response)
            .up_to_n_times(n)
            .mount(self)
            .await;
    }
}

/// Answers a reply to `orig` with the draft `draft`, which takes updates.
async fn reply_draft(server: &MockServer, draft: &str) {
    server
        .on("POST", "/me/messages/orig/createReply", id(201, draft))
        .await;
    server
        .on("PATCH", &format!("/me/messages/{draft}"), id(200, draft))
        .await;
}

/// Mocks for `PUT` requests to the upload session, to add matchers to.
fn session() -> MockBuilder {
    Mock::given(method("PUT")).and(path("/upload/session"))
}

/// A `status` response with Graph's error `code` and `message`.
fn graph_error(status: u16, code: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .set_body_json(json!({ "error": { "code": code, "message": message } }))
}

/// A `status` response with the ID of what was made or changed.
fn id(status: u16, id: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({ "id": id }))
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
    server
        .on(
            "POST",
            "/$batch",
            ResponseTemplate::new(200).set_body_json(json!({ "responses": responses })),
        )
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
    route("GET", "/me/mailFolders/inbox")
        .and(header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(folder("inbox-id", "Inbox")))
        .mount(&server)
        .await;
    route("GET", "/me/mailFolders")
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
    server
        .on("GET", "/me/mailFolders/msgfolderroot", id(200, "root-id"))
        .await;
    server
        .on(
            "GET",
            "/me/mailFolders/other-id",
            ResponseTemplate::new(200).set_body_json(folder("other-id", "Projects")),
        )
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
    route("GET", "/me/mailFolders/inbox-id/messages")
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
    route("GET", "/me/mailFolders/inbox-id/messages")
        .and(query_param(
            "$filter",
            "lastModifiedDateTime ge 1900-01-01T00:00:00Z and isRead eq false",
        ))
        .and(query_param("$orderby", "lastModifiedDateTime desc"))
        .and(query_param("$top", "50"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [{ "id": "m9", "lastModifiedDateTime": "2024-05-01T10:00:00Z" }],
        })))
        .mount(&server)
        .await;
    server
        .on(
            "GET",
            "/me/mailFolders/empty-id/messages",
            ResponseTemplate::new(200).set_body_json(json!({ "value": [] })),
        )
        .await;
    let store = store(&server);
    let recent = store
        .recent_unread(&FolderId::new("inbox-id"), None)
        .await
        .unwrap();
    assert_eq!(recent[0].0, MessageId::new("m9"));
    assert_eq!(recent[0].1.to_rfc3339(), "2024-05-01T10:00:00+00:00");
    assert!(
        store
            .recent_unread(&FolderId::new("empty-id"), None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn unread_mail_changed_since_a_time_comes_on_one_page() {
    let server = MockServer::start().await;
    let page = |from: usize, to: usize, next: Option<String>| {
        let value: Vec<Value> = (from..to)
            .map(|i| json!({ "id": format!("m{i}"), "lastModifiedDateTime": "2024-05-01T10:00:00Z" }))
            .collect();
        let mut body = json!({ "value": value });
        if let Some(next) = next {
            body["@odata.nextLink"] = json!(next);
        }
        ResponseTemplate::new(200).set_body_json(body)
    };
    route("GET", "/me/mailFolders/inbox-id/messages")
        .and(query_param("$skip", "250"))
        .respond_with(page(250, 300, None))
        .with_priority(1)
        .expect(0)
        .mount(&server)
        .await;
    route("GET", "/me/mailFolders/inbox-id/messages")
        .and(query_param(
            "$filter",
            "lastModifiedDateTime ge 2024-05-01T00:00:00Z and isRead eq false",
        ))
        .and(query_param("$top", "250"))
        .respond_with(page(
            0,
            250,
            Some(format!(
                "{}/v1.0/me/mailFolders/inbox-id/messages?$skip=250",
                server.uri()
            )),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let since = DateTime::parse_from_rfc3339("2024-05-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let recent = store(&server)
        .recent_unread(&FolderId::new("inbox-id"), Some(since))
        .await
        .unwrap();
    // The most there are, newest first.
    let ids: Vec<_> = recent.iter().map(|(id, _)| id.to_string()).collect();
    let expected: Vec<_> = (0..250).map(|i| format!("m{i}")).collect();
    assert_eq!(ids, expected);
}

#[tokio::test]
async fn reads_where_messages_are_in_their_conversations() {
    use base64::Engine;

    let server = MockServer::start().await;
    let header = [7u8; 22];
    let reply: Vec<u8> = header.iter().copied().chain([1, 2, 3, 4, 5]).collect();
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut first = message("first", "2024-05-01T10:00:00Z");
    first["conversationId"] = json!("conv");
    first["conversationIndex"] = json!(encode(&header));
    let mut second = message("second", "2024-05-01T11:00:00Z");
    second["conversationId"] = json!("conv");
    second["conversationIndex"] = json!(encode(&reply));
    let mut odd = message("odd", "2024-05-01T12:00:00Z");
    odd["conversationId"] = json!("other");
    odd["conversationIndex"] = json!(encode(&[1, 2, 3]));
    route("GET", "/me/mailFolders/inbox-id/messages")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [second, first, odd, message("lone", "2024-05-01T09:00:00Z")],
        })))
        .mount(&server)
        .await;
    let page = store(&server)
        .list_messages(&FolderId::new("inbox-id"), Page::first(10))
        .await
        .unwrap();
    let threads: Vec<_> = page
        .messages
        .iter()
        .map(|m| {
            m.thread
                .as_ref()
                .map(|t| (t.conversation.as_str(), t.path.len()))
        })
        .collect();
    assert_eq!(
        threads,
        [
            Some(("conv", 2)),
            Some(("conv", 1)),
            Some(("other", 0)),
            None
        ]
    );
    let second = page.messages[0].thread.as_ref().unwrap();
    assert_eq!(second.path[1], [1, 2, 3, 4, 5]);
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
    server
        .on(
            "GET",
            "/me/mailFolders",
            ResponseTemplate::new(200).set_body_json(json!({ "value": [] })),
        )
        .await;
    assert!(store.list_folders().await.is_ok());
}

#[tokio::test]
async fn lists_nested_folders() {
    let server = MockServer::start().await;
    mount_roles(&server).await;
    route("GET", "/me/mailFolders")
        .and(query_param("includeHiddenFolders", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [child("inbox-id", "Inbox", "root-id", 1)],
        })))
        .mount(&server)
        .await;
    route("GET", "/me/mailFolders/inbox-id/childFolders")
        .and(query_param("includeHiddenFolders", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [child("projects-id", "Projects", "inbox-id", 1)],
        })))
        .mount(&server)
        .await;
    route("GET", "/me/mailFolders/projects-id/childFolders")
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

/// Answers the listings of a tree, depth first:
///
/// ```text
/// A ─ A1 ─ A1x
///   ─ A2
/// B
/// C ─ C1 ─ C1x
/// ```
///
/// with A's folders on two pages.
async fn mount_tree(server: &MockServer) {
    let listing =
        |folders: Vec<Value>| ResponseTemplate::new(200).set_body_json(json!({ "value": folders }));
    server
        .on(
            "GET",
            "/me/mailFolders",
            listing(vec![
                child("a", "A", "root-id", 2),
                child("b", "B", "root-id", 0),
                child("c", "C", "root-id", 1),
            ]),
        )
        .await;
    route("GET", "/me/mailFolders/a/childFolders")
        .and(query_param("$skip", "1"))
        .respond_with(listing(vec![child("a2", "A2", "a", 0)]))
        .with_priority(1)
        .mount(server)
        .await;
    server
        .on(
            "GET",
            "/me/mailFolders/a/childFolders",
            ResponseTemplate::new(200).set_body_json(json!({
                "value": [child("a1", "A1", "a", 1)],
                "@odata.nextLink": format!(
                    "{}/v1.0/me/mailFolders/a/childFolders?$skip=1",
                    server.uri()
                ),
            })),
        )
        .await;
    for (parent, id, name) in [
        ("a1", "a1x", "A1x"),
        ("c", "c1", "C1"),
        ("c1", "c1x", "C1x"),
    ] {
        let children = u32::from(id == "c1");
        server
            .on(
                "GET",
                &format!("/me/mailFolders/{parent}/childFolders"),
                listing(vec![child(id, name, parent, children)]),
            )
            .await;
    }
}

#[tokio::test]
async fn lists_the_tree_depth_first_as_one_listing_at_a_time_would() {
    let server = MockServer::start().await;
    mount_roles(&server).await;
    mount_tree(&server).await;
    let names: Vec<_> = store(&server)
        .list_folders()
        .await
        .unwrap()
        .into_iter()
        .map(|f| f.name)
        .collect();
    // The top level, then always below the last folder found with
    // folders below it.
    assert_eq!(names, ["A", "B", "C", "C1", "C1x", "A1", "A2", "A1x"]);
}

#[tokio::test]
async fn a_failed_folder_listing_is_the_one_a_walk_would_stop_at() {
    let server = MockServer::start().await;
    mount_roles(&server).await;
    // A is listed before C1, but a walk lists C1 (below the last) first.
    route("GET", "/me/mailFolders/a/childFolders")
        .respond_with(ResponseTemplate::new(403))
        .with_priority(1)
        .mount(&server)
        .await;
    route("GET", "/me/mailFolders/c1/childFolders")
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .mount(&server)
        .await;
    mount_tree(&server).await;
    let listed = store(&server).list_folders().await;
    assert!(matches!(listed, Err(Error::Network(_))), "{listed:?}");

    // The roles are looked up meanwhile; failing, they are the answer.
    server.reset().await;
    mount_roles_with(&server, 403).await;
    server
        .on("GET", "/me/mailFolders", ResponseTemplate::new(503))
        .await;
    let listed = store(&server).list_folders().await;
    assert!(matches!(listed, Err(Error::Auth(_))), "{listed:?}");
}

/// Longer than any test should take: a request answered this late is
/// never waited for.
const STALLED: Duration = Duration::from_secs(5);

#[tokio::test]
async fn a_decisive_folder_listing_error_is_not_held_up_by_others() {
    let server = MockServer::start().await;
    mount_roles(&server).await;
    server
        .on(
            "GET",
            "/me/mailFolders",
            ResponseTemplate::new(200).set_body_json(json!({
                "value": [child("a", "A", "root-id", 1), child("b", "B", "root-id", 1)],
            })),
        )
        .await;
    // A walk lists B (the last) first, and stops at its failure.
    server
        .on(
            "GET",
            "/me/mailFolders/a/childFolders",
            ResponseTemplate::new(200)
                .set_body_json(json!({ "value": [] }))
                .set_delay(STALLED),
        )
        .await;
    server
        .on(
            "GET",
            "/me/mailFolders/b/childFolders",
            ResponseTemplate::new(503),
        )
        .await;
    let started = std::time::Instant::now();
    let listed = store(&server).list_folders().await;
    assert!(matches!(listed, Err(Error::Network(_))), "{listed:?}");
    assert!(started.elapsed() < STALLED / 2, "{:?}", started.elapsed());

    // Nor are the roles' by the listing.
    server.reset().await;
    mount_roles_with(&server, 403).await;
    route("GET", "/me/mailFolders")
        .respond_with(ResponseTemplate::new(503).set_delay(STALLED))
        .mount(&server)
        .await;
    route("GET", "/me/mailFolders/a/childFolders")
        .respond_with(ResponseTemplate::new(503).set_delay(STALLED))
        .mount(&server)
        .await;
    let started = std::time::Instant::now();
    let store = store(&server);
    let listed = store.list_folders().await;
    assert!(matches!(listed, Err(Error::Auth(_))), "{listed:?}");
    let below = store.child_folders(Some(&FolderId::new("a"))).await;
    assert!(matches!(below, Err(Error::Auth(_))), "{below:?}");
    assert!(started.elapsed() < STALLED / 2, "{:?}", started.elapsed());
}

#[tokio::test]
async fn lists_a_few_folders_at_once() {
    let server = MockServer::start().await;
    mount_roles(&server).await;
    let top: Vec<Value> = (0..8)
        .map(|i| child(&format!("f{i}"), &format!("F{i}"), "root-id", 1))
        .collect();
    server
        .on(
            "GET",
            "/me/mailFolders",
            ResponseTemplate::new(200).set_body_json(json!({ "value": top })),
        )
        .await;
    let delay = Duration::from_millis(300);
    for i in 0..8 {
        server
            .on(
                "GET",
                &format!("/me/mailFolders/f{i}/childFolders"),
                ResponseTemplate::new(200)
                    .set_body_json(
                        json!({ "value": [child(&format!("s{i}"), "S", &format!("f{i}"), 0)] }),
                    )
                    .set_delay(delay),
            )
            .await;
    }
    let started = std::time::Instant::now();
    let folders = store(&server).list_folders().await.unwrap();
    // One after the other, the listings would take 8 times the delay.
    assert!(started.elapsed() < delay * 6, "{:?}", started.elapsed());
    assert_eq!(folders.len(), 16);
    let below: Vec<_> = folders[8..].iter().map(|f| f.id.as_str()).collect();
    assert_eq!(below, ["s7", "s6", "s5", "s4", "s3", "s2", "s1", "s0"]);
}

#[tokio::test]
async fn lists_the_folders_below_one_as_the_whole_tree_does() {
    let server = MockServer::start().await;
    mount_roles(&server).await;
    mount_tree(&server).await;
    let store = store(&server);
    let all = store.list_folders().await.unwrap();
    for parent in [None, Some("a"), Some("c1"), Some("a1")] {
        let parent = parent.map(FolderId::new);
        server.reset().await;
        mount_roles(&server).await;
        mount_tree(&server).await;
        let below = store.child_folders(parent.as_ref()).await.unwrap();
        let expected: Vec<_> = all.iter().filter(|f| f.parent == parent).cloned().collect();
        assert_eq!(below, expected, "below {parent:?}");
        // One listing (with its pages); the roles are those of the listing
        // of every folder, not looked up again.
        let requests = server.received_requests().await.unwrap();
        let listings: Vec<_> = requests
            .iter()
            .filter(|r| r.method.as_str() == "GET")
            .map(|r| r.url.path().to_owned())
            .collect();
        assert!(requests.iter().all(|r| r.method.as_str() != "POST"));
        let at = match &parent {
            None => "/v1.0/me/mailFolders".to_owned(),
            Some(id) => format!("/v1.0/me/mailFolders/{id}/childFolders"),
        };
        assert!(listings.iter().all(|l| *l == at), "{listings:?}");
        let pages = if parent == Some(FolderId::new("a")) {
            2
        } else {
            1
        };
        assert_eq!(listings.len(), pages);
        // The first page as a listing of the whole tree asks for it.
        let first = requests
            .iter()
            .find(|r| r.method.as_str() == "GET")
            .unwrap();
        let query: Vec<_> = first.url.query_pairs().collect();
        assert_eq!(
            query,
            [
                ("$top".into(), "100".into()),
                ("includeHiddenFolders".into(), "true".into())
            ]
        );
    }
    // The roles label well-known folders, and tell the top level.
    let top = store.child_folders(None).await.unwrap();
    assert!(top.iter().all(|f| f.parent.is_none()));

    // With no roles known (none looked up yet), failing, they are the
    // answer.
    let store = self::store(&server);
    server.reset().await;
    mount_roles_with(&server, 503).await;
    mount_tree(&server).await;
    assert!(matches!(
        store.child_folders(None).await,
        Err(Error::Network(_))
    ));
    server.reset().await;
    mount_roles(&server).await;
    assert!(matches!(
        store.child_folders(Some(&FolderId::new("nope"))).await,
        Err(Error::NotFound { kind: "folder", id }) if id == "nope"
    ));
    // The listing of every folder looks the roles up afresh.
    server.reset().await;
    mount_roles(&server).await;
    mount_tree(&server).await;
    store.list_folders().await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method.as_str() == "POST")
            .count(),
        1
    );
}

#[tokio::test]
async fn finds_which_folder_a_message_is_in_without_its_body() {
    let server = MockServer::start().await;
    route("GET", "/me/messages/m1")
        .and(query_param("$select", "id,parentFolderId"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "id": "m1", "parentFolderId": "inbox-id" })),
        )
        .mount(&server)
        .await;
    let store = store(&server);
    assert_eq!(
        store.message_folder(&MessageId::new("m1")).await.unwrap(),
        FolderId::new("inbox-id")
    );
    assert!(matches!(
        store.message_folder(&MessageId::new("m2")).await,
        Err(Error::NotFound { kind: "message", id }) if id == "m2"
    ));
}

#[tokio::test]
async fn later_pages_replay_graphs_link_unchanged() {
    let server = MockServer::start().await;
    route("GET", "/me/mailFolders/inbox-id/messages")
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
    route("GET", "/me/mailFolders/inbox-id/messages")
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
    route("GET", "/me/mailFolders/inbox-id/messages")
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
    route("GET", "/me/messages/m1")
        .and(wiremock::matchers::header_regex(
            "prefer",
            r#"^outlook\.body-content-type="text", outlook\.timezone="UTC"$"#,
        ))
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
async fn fetches_a_meeting_request_with_its_event() {
    let server = MockServer::start().await;
    let mut body = message("m1", "2026-10-13T10:00:00Z");
    body["@odata.type"] = json!("#microsoft.graph.eventMessageRequest");
    body["meetingMessageType"] = json!("meetingRequest");
    body["body"] = json!({ "contentType": "text", "content": "Join us" });
    body["event"] = json!({
        "id": "event-1",
        "start": { "dateTime": "2026-11-03T14:00:00.0000000", "timeZone": "UTC" },
        "end": { "dateTime": "2026-11-03T15:00:00.0000000", "timeZone": "UTC" },
        "location": { "displayName": "Room 4A" },
        "responseStatus": { "response": "notResponded" },
    });
    route("GET", "/me/messages/m1")
        .and(wiremock::matchers::query_param(
            "$expand",
            "microsoft.graph.eventMessage/event(\
             $select=id,start,end,isAllDay,location,organizer,attendees,recurrence,\
             responseStatus,responseRequested,type,seriesMasterId)",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let message = store(&server)
        .fetch_message(&MessageId::new("m1"))
        .await
        .unwrap();
    let invitation = message.invitation.expect("an invitation");
    assert_eq!(invitation.kind, mett_core::InvitationKind::Request);
    assert_eq!(invitation.location.as_deref(), Some("Room 4A"));
    assert_eq!(invitation.event.as_deref(), Some("event-1"));
    assert!(invitation.can_answer());
}

#[tokio::test]
async fn escapes_ids_in_paths() {
    let server = MockServer::start().await;
    // Graph IDs are base64 and may contain '/' and '+'.
    route("GET", "/me/messages/AA%2Fbb+cc=")
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
    route("PATCH", "/me/messages/m1")
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
    route("POST", "/me/messages/m1/move")
        .and(body_json(json!({ "destinationId": "archive-id" })))
        .respond_with(id(201, "m1-moved"))
        .mount(&server)
        .await;
    route("POST", "/me/messages/m1-moved/permanentDelete")
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    route("POST", "/me/messages/m1-moved/copy")
        .and(body_json(json!({ "destinationId": "inbox-id" })))
        .respond_with(id(201, "m1-copy"))
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
    Mock::given(path("/v1.0/me/messages/missing"))
        .respond_with(graph_error(404, "ErrorItemNotFound", "details"))
        .mount(&server)
        .await;
    Mock::given(path("/v1.0/me/messages/busy"))
        .respond_with(
            graph_error(429, "TooManyRequests", "details").insert_header("Retry-After", "7"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/v1.0/me/messages/denied"))
        .respond_with(graph_error(403, "ErrorAccessDenied", "details"))
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
    server
        .on(
            "GET",
            "/me/mailFolders/drafts-id/messages",
            ResponseTemplate::new(200).set_body_json(json!({ "value": [draft] })),
        )
        .await;

    let page = store(&server)
        .list_messages(&FolderId::new("drafts-id"), Page::first(10))
        .await
        .unwrap();
    assert_eq!(
        page.messages[0].received,
        DateTime::parse_from_rfc3339("2026-10-10T08:00:00Z").unwrap()
    );
    assert!(page.messages[0].draft);
}

#[tokio::test]
async fn mail_without_a_sent_date_is_listed_with_the_drafts() {
    let server = MockServer::start().await;
    let mut odd = message("m1", "2026-10-14T10:00:00Z");
    odd["sentDateTime"] = json!(null);
    odd["createdDateTime"] = json!("2026-10-10T08:00:00Z");
    route("GET", "/me/mailFolders/in/messages")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [odd, message("m2", "2026-10-14T09:00:00Z")],
        })))
        .mount(&server)
        .await;
    let page = store(&server)
        .list_messages(&FolderId::new("in"), Page::first(10))
        .await
        .unwrap();
    let drafts: Vec<bool> = page.messages.iter().map(|m| m.draft).collect();
    assert_eq!(drafts, [true, false]);
}

#[tokio::test]
async fn batch_item_failures_keep_their_meaning() {
    let server = MockServer::start().await;
    let item = |status: u16, headers: Value| json!({ "responses": [{ "id": "0", "status": status, "headers": headers, "body": {} }] });
    route("POST", "/$batch")
        .respond_with(
            ResponseTemplate::new(200).set_body_json(item(429, json!({ "Retry-After": "9" }))),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    route("POST", "/$batch")
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
    route("GET", "/me/mailFolders/inbox-id/messages")
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
    route("GET", "/me/mailFolders/inbox-id/messages")
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
    route("GET", "/me/mailFolders/sent-id/messages")
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
    route("POST", "/$batch")
        .respond_with(ResponseTemplate::new(503))
        .expect(0)
        .mount(&server)
        .await;
    route("GET", "/me/mailFolders/msgfolderroot")
        .respond_with(id(200, "root-id"))
        .expect(1)
        .mount(&server)
        .await;
    server
        .on(
            "GET",
            "/me/mailFolders/inbox",
            ResponseTemplate::new(200).set_body_json(folder("inbox-id", "Inbox")),
        )
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

#[tokio::test]
async fn retries_once_with_a_new_token_after_a_401() {
    let server = MockServer::start().await;
    route("PATCH", "/me/messages/m1")
        .and(header("authorization", "Bearer old"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    route("PATCH", "/me/messages/m1")
        .and(header("authorization", "Bearer new"))
        .and(body_json(json!({ "isRead": true })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let auth = Arc::new(RotatingAuth {
        rejected: Default::default(),
    });
    let store = store_with(&server, Cloud::default(), auth);
    store
        .update_flags(&MessageId::new("m1"), FlagsUpdate::read(true))
        .await
        .unwrap();
}

#[tokio::test]
async fn permanent_delete_is_reported_unavailable_in_national_clouds() {
    let server = MockServer::start().await;
    let store = store_with(&server, Cloud::UsGov, Arc::new(StaticAuth));
    let result = store.delete_message(&MessageId::new("m1")).await;
    assert!(matches!(result, Err(Error::Backend(msg)) if msg.contains("usgov")));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn recognises_well_known_folders_created_later() {
    let server = MockServer::start().await;
    // At first the mailbox has no Archive folder…
    mount_roles(&server).await;
    route("GET", "/me/mailFolders")
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
    route("POST", "/$batch")
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "responses": with_archive })),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    let second = store.list_folders().await.unwrap();
    assert_eq!(second[0].well_known, Some(WellKnownFolder::Archive));
}

#[tokio::test]
async fn a_401_inside_a_batch_is_retried_once_with_a_new_token() {
    let server = MockServer::start().await;
    let rejected = json!({ "responses": [{ "id": "0", "status": 401, "body": {} }] });
    route("POST", "/$batch")
        .and(header("authorization", "Bearer old"))
        .respond_with(ResponseTemplate::new(200).set_body_json(rejected))
        .expect(1)
        .mount(&server)
        .await;
    route("POST", "/$batch")
        .and(header("authorization", "Bearer new"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "responses": [{ "id": "0", "status": 200, "body": { "id": "root-id" } }],
        })))
        .expect(1)
        .mount(&server)
        .await;
    server
        .on(
            "GET",
            "/me/mailFolders",
            ResponseTemplate::new(200).set_body_json(json!({ "value": [] })),
        )
        .await;

    let auth = Arc::new(RotatingAuth {
        rejected: Default::default(),
    });
    let store = store_with(&server, Cloud::default(), auth);
    store.list_folders().await.unwrap();
}

#[tokio::test]
async fn a_repeated_401_inside_a_batch_is_an_auth_error() {
    let server = MockServer::start().await;
    route("POST", "/$batch")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "responses": [{ "id": "0", "status": 401, "body": {} }],
        })))
        .expect(2)
        .mount(&server)
        .await;
    let auth = Arc::new(CountingAuth::default());
    let store = store_with(&server, Cloud::default(), Arc::clone(&auth));
    assert!(matches!(store.list_folders().await, Err(Error::Auth(_))));
    assert_eq!(auth.rejected.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_token_rejected_on_retry_is_reported_too() {
    let server = MockServer::start().await;
    server
        .on("GET", "/me/messages/m1", ResponseTemplate::new(401))
        .await;
    let auth = Arc::new(RotatingAuth {
        rejected: Default::default(),
    });
    let rejections = Arc::new(std::sync::Mutex::new(Vec::new()));
    let store = store_with(
        &server,
        Cloud::default(),
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
    route("POST", "/me/sendMail")
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
    route("POST", "/me/messages/orig/createReply")
        .respond_with(id(201, "d1"))
        .expect(1)
        .mount(&server)
        .await;
    route("PATCH", "/me/messages/d1")
        .and(body_json(draft_json()))
        .respond_with(id(200, "d1"))
        .expect(1)
        .mount(&server)
        .await;
    route("POST", "/me/messages/d1/send")
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
    server
        .on("POST", "/me/messages/orig/createForward", id(201, "d1"))
        .await;
    server.on("PATCH", "/me/messages/d1", id(200, "d1")).await;
    route("POST", "/me/messages/d1/send")
        .respond_with(graph_error(400, "ErrorInvalidRecipients", "bad recipient"))
        .mount(&server)
        .await;
    route("POST", "/me/messages/d1/permanentDelete")
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
    route("POST", "/me/messages")
        .and(body_json(draft_json()))
        .respond_with(id(201, "new"))
        .expect(1)
        .mount(&server)
        .await;
    reply_draft(&server, "reply").await;
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
    reply_draft(&server, "d1").await;
    server
        .on("POST", "/me/messages/d1/send", ResponseTemplate::new(503))
        .await;
    route("POST", "/me/messages/d1/permanentDelete")
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
    route("POST", "/me/messages/orig/createReply")
        .respond_with(id(201, "d1"))
        .expect(1)
        .mount(&server)
        .await;
    route("PATCH", "/me/messages/d1")
        .respond_with(id(200, "d1"))
        .expect(2)
        .mount(&server)
        .await;
    server
        .on("POST", "/me/messages/d1/send", ResponseTemplate::new(503))
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
    route("POST", "/me/messages/orig/createForward")
        .respond_with(id(201, "d1"))
        .expect(1)
        .mount(&server)
        .await;
    server
        .on("PATCH", "/me/messages/d1", ResponseTemplate::new(503))
        .await;
    route("POST", "/me/messages/d1/permanentDelete")
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
    server
        .on_n("POST", "/me/messages/orig/createReply", 1, id(201, "d1"))
        .await;
    server
        .on("POST", "/me/messages/orig/createReply", id(201, "d2"))
        .await;
    server
        .on_n("PATCH", "/me/messages/d1", 1, id(200, "d1"))
        .await;
    route("PATCH", "/me/messages/d1")
        .respond_with(graph_error(404, "ErrorItemNotFound", "gone"))
        .mount(&server)
        .await;
    server.on("PATCH", "/me/messages/d2", id(200, "d2")).await;
    server
        .on("POST", "/me/messages/d1/send", ResponseTemplate::new(503))
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
    server
        .on("POST", "/me/messages/orig/createReply", id(201, "d1"))
        .await;
    server
        .on("PATCH", "/me/messages/d1", ResponseTemplate::new(503))
        .await;
    route("POST", "/me/messages/d1/send")
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
    server
        .on_n("POST", "/me/messages/orig/createReply", 1, id(201, "d1"))
        .await;
    server
        .on("POST", "/me/messages/orig/createReply", id(201, "d2"))
        .await;
    for draft in ["d1", "d2"] {
        server
            .on("PATCH", &format!("/me/messages/{draft}"), id(200, draft))
            .await;
    }
    server
        .on("POST", "/me/messages/d1/send", ResponseTemplate::new(503))
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
    route("POST", "/me/messages/orig/createReply")
        .respond_with(id(201, "d1"))
        .expect(1)
        .mount(&server)
        .await;
    let denied = || graph_error(403, "ErrorAccessDenied", "denied");
    route("PATCH", "/me/messages/d1")
        .respond_with(denied())
        .expect(2)
        .mount(&server)
        .await;
    server
        .on("POST", "/me/messages/d1/permanentDelete", denied())
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
    let store = store_with(&server, Cloud::default(), Arc::new(OfflineAuth));
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
    server
        .on(
            "POST",
            "/me/messages/orig/createReply",
            ResponseTemplate::new(503),
        )
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
    reply_draft(&server, "d1").await;
    server
        .on("POST", "/me/messages/d1/send", ResponseTemplate::new(503))
        .await;
    route("POST", "/me/messages/d1/permanentDelete")
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
    server
        .on("POST", "/me/messages/orig/createReply", id(201, "d1"))
        .await;
    route("PATCH", "/me/messages/d1")
        .respond_with(graph_error(400, "ErrorInvalidRecipients", "bad recipient"))
        .mount(&server)
        .await;
    route("DELETE", "/me/messages/d1")
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let store = store_with(&server, Cloud::UsGov, Arc::new(StaticAuth));
    let reply = draft(Some(DraftOrigin::Reply(MessageId::new("orig"))));
    store.send(&reply).await.unwrap_err();
}

#[tokio::test]
async fn finds_the_users_own_addresses() {
    let server = MockServer::start().await;
    route("GET", "/me")
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

#[tokio::test]
async fn lists_and_fetches_attachments() {
    let server = MockServer::start().await;
    route("GET", "/me/messages/m1/attachments")
        .and(query_param(
            "$expand",
            "microsoft.graph.itemAttachment/item($select=id)",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                {
                    "@odata.type": "#microsoft.graph.fileAttachment",
                    "id": "a1", "name": "notes.txt", "contentType": "text/plain",
                    "size": 5, "isInline": false,
                },
                {
                    "@odata.type": "#microsoft.graph.itemAttachment",
                    "id": "a2", "name": "Fwd", "size": 900,
                },
                {
                    "@odata.type": "#microsoft.graph.referenceAttachment",
                    "id": "a3", "name": "link", "contentType": null, "size": 0,
                },
                {
                    "@odata.type": "#microsoft.graph.itemAttachment",
                    "id": "a4", "name": "Lunch", "size": 300,
                    "item": { "@odata.type": "#microsoft.graph.event", "id": "e1" },
                },
            ],
        })))
        .mount(&server)
        .await;
    server
        .on(
            "GET",
            "/me/messages/m1/attachments/a1/$value",
            ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()),
        )
        .await;
    let store = store(&server);
    let list = store.list_attachments(&MessageId::new("m1")).await.unwrap();
    let kinds: Vec<_> = list
        .iter()
        .map(|a| (a.name.as_str(), a.content_type.as_str(), a.kind))
        .collect();
    assert_eq!(
        kinds,
        [
            ("notes.txt", "text/plain", AttachmentKind::File),
            ("Fwd", "message/rfc822", AttachmentKind::Item),
            ("link", "application/octet-stream", AttachmentKind::Link),
            ("Lunch", "text/calendar", AttachmentKind::Item),
        ]
    );
    let data = store
        .fetch_attachment(&MessageId::new("m1"), &list[0].id)
        .await
        .unwrap();
    assert_eq!(data, b"hello");
}

fn attached(name: &str, size: usize) -> OutgoingAttachment {
    OutgoingAttachment {
        name: name.into(),
        content_type: "application/octet-stream".into(),
        data: vec![7; size],
    }
}

#[tokio::test]
async fn sends_small_attachments_with_the_message() {
    let server = MockServer::start().await;
    let mut message = draft_json();
    message["attachments"] = json!([{
        "@odata.type": "#microsoft.graph.fileAttachment",
        "name": "a.bin",
        "contentType": "application/octet-stream",
        "contentBytes": "BwcH",
    }]);
    route("POST", "/me/sendMail")
        .and(body_json(
            json!({ "message": message, "saveToSentItems": true }),
        ))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    let mut draft = draft(None);
    draft.attachments.push(attached("a.bin", 3));
    store(&server).send(&draft).await.unwrap();
}

#[tokio::test]
async fn uploads_large_attachments_without_the_token() {
    let server = MockServer::start().await;
    route("POST", "/me/messages")
        .respond_with(id(201, "d1"))
        .expect(1)
        .mount(&server)
        .await;
    route("POST", "/me/messages/d1/attachments/createUploadSession")
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "uploadUrl": format!("{}/upload/session", server.uri()),
        })))
        .expect(1)
        .mount(&server)
        .await;
    // The upload URL carries its own authorisation.
    session()
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(418))
        .with_priority(1)
        .mount(&server)
        .await;
    session()
        .respond_with(ResponseTemplate::new(200))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    session()
        .and(header("content-range", "bytes 3276800-3999999/4000000"))
        .and(header("content-type", "application/octet-stream"))
        .respond_with(ResponseTemplate::new(201).insert_header(
            "Location",
            "https://x/api/v2.0/Users('u')/Messages('d1')/Attachments('att')",
        ))
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;
    route("POST", "/me/messages/d1/send")
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    let mut draft = draft(None);
    draft.attachments.push(attached("big.bin", 4_000_000));
    store(&server).send(&draft).await.unwrap();
}

#[tokio::test]
async fn follows_every_page_of_attachments() {
    let server = MockServer::start().await;
    let next = format!(
        "{}/v1.0/me/messages/m1/attachments?$skiptoken=2",
        server.uri()
    );
    route("GET", "/me/messages/m1/attachments")
        .and(query_param("$skiptoken", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [{ "id": "a2", "name": "two", "size": 1 }],
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    route("GET", "/me/messages/m1/attachments")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [{ "id": "a1", "name": "one", "size": 1 }],
            "@odata.nextLink": next,
        })))
        .mount(&server)
        .await;
    let names: Vec<String> = store(&server)
        .list_attachments(&MessageId::new("m1"))
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.name)
        .collect();
    assert_eq!(names, ["one", "two"]);
}

#[tokio::test]
async fn a_retry_takes_off_what_an_earlier_attempt_attached() {
    let server = MockServer::start().await;
    route("POST", "/me/messages/orig/createForward")
        .respond_with(id(201, "d1"))
        .expect(1)
        .mount(&server)
        .await;
    server.on("PATCH", "/me/messages/d1", id(200, "d1")).await;
    // The forward carries the original's attachment along.
    route("GET", "/me/messages/d1/attachments")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [{ "id": "carried", "name": "original.pdf", "size": 1 }],
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // The first upload's answer is lost: it was attached as "lost".
    server
        .on_n(
            "POST",
            "/me/messages/d1/attachments",
            1,
            ResponseTemplate::new(503),
        )
        .await;
    let mut forward = draft(Some(DraftOrigin::Forward(MessageId::new("orig"))));
    forward.attachments.push(attached("a.bin", 3));
    let store = store(&server);
    assert!(matches!(store.send(&forward).await, Err(Error::NotSent(_))));

    // The retry finds it there, takes it off, and attaches the file again,
    // leaving the forwarded one.
    route("GET", "/me/messages/d1/attachments")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                { "id": "carried", "name": "original.pdf", "size": 1 },
                { "id": "lost", "name": "a.bin", "size": 3 },
            ],
        })))
        .mount(&server)
        .await;
    route("DELETE", "/me/messages/d1/attachments/lost")
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    route("POST", "/me/messages/d1/attachments")
        .respond_with(id(201, "again"))
        .expect(1)
        .mount(&server)
        .await;
    route("POST", "/me/messages/d1/send")
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    store.send(&forward).await.unwrap();
}

#[tokio::test]
async fn a_retry_without_files_takes_off_those_attached_before() {
    let server = MockServer::start().await;
    route("POST", "/me/messages/orig/createReply")
        .respond_with(id(201, "d1"))
        .expect(1)
        .mount(&server)
        .await;
    server.on("PATCH", "/me/messages/d1", id(200, "d1")).await;
    server
        .on_n(
            "GET",
            "/me/messages/d1/attachments",
            1,
            ResponseTemplate::new(200).set_body_json(json!({ "value": [] })),
        )
        .await;
    route("POST", "/me/messages/d1/attachments")
        .respond_with(id(201, "a1"))
        .expect(1)
        .mount(&server)
        .await;
    // The send's answer is lost.
    server
        .on_n(
            "POST",
            "/me/messages/d1/send",
            1,
            ResponseTemplate::new(503),
        )
        .await;
    let mut reply = draft(Some(DraftOrigin::Reply(MessageId::new("orig"))));
    reply.attachments.push(attached("a.bin", 3));
    let store = store(&server);
    assert!(store.send(&reply).await.is_err());

    // The file is taken off the message: the retry takes it off the draft.
    reply.attachments.clear();
    route("GET", "/me/messages/d1/attachments")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [{ "id": "a1", "name": "a.bin", "size": 3 }],
        })))
        .mount(&server)
        .await;
    route("DELETE", "/me/messages/d1/attachments/a1")
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    route("POST", "/me/messages/d1/send")
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    store.send(&reply).await.unwrap();
}

#[tokio::test]
async fn a_kept_draft_is_sent_once_its_large_files_are_taken_off() {
    let server = MockServer::start().await;
    route("POST", "/me/messages")
        .respond_with(id(201, "d1"))
        .expect(1)
        .mount(&server)
        .await;
    // The upload's outcome is unknown: the draft is kept.
    server
        .on(
            "POST",
            "/me/messages/d1/attachments/createUploadSession",
            ResponseTemplate::new(503),
        )
        .await;
    let mut message = draft(None);
    message.attachments.push(attached("big.bin", 4_000_000));
    let store = store(&server);
    assert!(store.send(&message).await.is_err());

    // Small enough to go inline now, but the kept draft is what is sent.
    message.attachments.clear();
    route("PATCH", "/me/messages/d1")
        .respond_with(id(200, "d1"))
        .expect(1)
        .mount(&server)
        .await;
    server
        .on(
            "GET",
            "/me/messages/d1/attachments",
            ResponseTemplate::new(200).set_body_json(json!({ "value": [] })),
        )
        .await;
    route("POST", "/me/messages/d1/send")
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    route("POST", "/me/sendMail")
        .respond_with(ResponseTemplate::new(202))
        .expect(0)
        .mount(&server)
        .await;
    store.send(&message).await.unwrap();
}

#[test]
fn escaped_text_counts_towards_the_request_size() {
    let mut message = draft(None);
    message.attachments.push(attached("a.bin", 1_000_000));
    message.body = "\\".repeat(1_500_000);
    assert!(mett_msgraph::needs_server_draft(&message));
}

#[test]
fn a_long_body_counts_towards_the_request_size() {
    let mut message = draft(None);
    message.attachments.push(attached("a.bin", 2_400_000));
    assert!(!mett_msgraph::needs_server_draft(&message));
    message.body = "x".repeat(1_000_000);
    assert!(mett_msgraph::needs_server_draft(&message));
}

#[tokio::test]
async fn searches_people_and_the_directory() {
    let server = MockServer::start().await;
    route("GET", "/me/people")
        .and(query_param("$search", "\"car\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [{
                "displayName": "Carol Jones",
                "scoredEmailAddresses": [{ "address": "carol@z.org" }],
            }],
        })))
        .mount(&server)
        .await;
    route("GET", "/users")
        .and(header("consistencylevel", "eventual"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                { "displayName": "Carol Jones", "mail": "carol@z.org" },
                { "displayName": "Carlos", "mail": "carlos@z.org" },
                { "displayName": "No Mail", "mail": null },
            ],
        })))
        .mount(&server)
        .await;
    let found: Vec<String> = store(&server)
        .search_people("car")
        .await
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        found,
        ["Carol Jones <carol@z.org>", "Carlos <carlos@z.org>"]
    );

    // Without the directory, the people the user works with.
    server.reset().await;
    server
        .on(
            "GET",
            "/me/people",
            ResponseTemplate::new(200).set_body_json(json!({ "value": [] })),
        )
        .await;
    server.on("GET", "/users", ResponseTemplate::new(403)).await;
    assert!(
        store(&server)
            .search_people("car")
            .await
            .unwrap()
            .is_empty()
    );

    // Without the people the user works with, the directory.
    server.reset().await;
    server
        .on("GET", "/me/people", ResponseTemplate::new(403))
        .await;
    route("GET", "/users")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [{ "displayName": "Carlos", "mail": "carlos@z.org" }],
        })))
        .mount(&server)
        .await;
    let found: Vec<String> = store(&server)
        .search_people("car")
        .await
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(found, ["Carlos <carlos@z.org>"]);

    // A broken request is said, not taken for a refusal.
    route("GET", "/me/people")
        .respond_with(ResponseTemplate::new(400))
        .with_priority(1)
        .mount(&server)
        .await;
    assert!(store(&server).search_people("car").await.is_err());
    server.reset().await;
    server
        .on("GET", "/me/people", ResponseTemplate::new(403))
        .await;
    // Neither: why.
    route("GET", "/users")
        .respond_with(ResponseTemplate::new(403))
        .with_priority(1)
        .mount(&server)
        .await;
    assert!(store(&server).search_people("car").await.is_err());
}

#[tokio::test]
async fn searches_people_and_the_directory_at_once() {
    let server = MockServer::start().await;
    let delay = Duration::from_millis(400);
    route("GET", "/me/people")
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "value": [{
                        "displayName": "Carol",
                        "scoredEmailAddresses": [{ "address": "carol@z.org" }],
                    }],
                }))
                .set_delay(delay),
        )
        .mount(&server)
        .await;
    route("GET", "/users")
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "value": [
                        { "displayName": "Carlos", "mail": "carlos@z.org" },
                        { "displayName": "Carol J", "mail": "CAROL@z.org" },
                    ],
                }))
                .set_delay(delay),
        )
        .mount(&server)
        .await;
    let started = std::time::Instant::now();
    let found: Vec<String> = store(&server)
        .search_people("car")
        .await
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(started.elapsed() < delay * 2, "{:?}", started.elapsed());
    // The people the user works with first, as before.
    assert_eq!(found, ["Carol <carol@z.org>", "Carlos <carlos@z.org>"]);

    // Failing, the people the user works with are said first.
    for (people, users, network) in [(503, 400, true), (403, 503, true), (400, 503, false)] {
        server.reset().await;
        server
            .on("GET", "/me/people", ResponseTemplate::new(people))
            .await;
        server
            .on("GET", "/users", ResponseTemplate::new(users))
            .await;
        let found = store(&server).search_people("car").await;
        match found {
            Err(Error::Network(_)) => assert!(network, "{people} {users}"),
            Err(Error::Backend(_)) => assert!(!network, "{people} {users}"),
            other => panic!("{people} {users}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_failed_people_search_is_not_held_up_by_the_directory() {
    let server = MockServer::start().await;
    server
        .on("GET", "/me/people", ResponseTemplate::new(400))
        .await;
    server
        .on(
            "GET",
            "/users",
            ResponseTemplate::new(200)
                .set_body_json(json!({ "value": [] }))
                .set_delay(STALLED),
        )
        .await;
    let started = std::time::Instant::now();
    let found = store(&server).search_people("car").await;
    assert!(matches!(found, Err(Error::Backend(_))), "{found:?}");
    assert!(started.elapsed() < STALLED / 2, "{:?}", started.elapsed());
}

/// A recalled postponed message, `d1`.
fn recalled() -> Draft {
    draft(Some(DraftOrigin::Recalled(MessageId::new("d1"))))
}

/// Mocks that fail the test if a draft is made or deleted.
async fn no_new_or_deleted_drafts(server: &MockServer) {
    for (verb, at) in [
        ("POST", "/me/messages"),
        ("POST", "/me/messages/d1/permanentDelete"),
        ("DELETE", "/me/messages/d1"),
    ] {
        route(verb, at)
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(server)
            .await;
    }
}

#[tokio::test]
async fn sends_a_recalled_draft_as_itself() {
    let server = MockServer::start().await;
    no_new_or_deleted_drafts(&server).await;
    route("PATCH", "/me/messages/d1")
        .and(body_json(draft_json()))
        .respond_with(id(200, "d1"))
        .expect(1)
        .mount(&server)
        .await;
    route("POST", "/me/messages/d1/send")
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    store(&server).send(&recalled()).await.unwrap();
}

#[tokio::test]
async fn postpones_a_recalled_draft_in_place() {
    let server = MockServer::start().await;
    no_new_or_deleted_drafts(&server).await;
    route("PATCH", "/me/messages/d1")
        .respond_with(id(200, "d1"))
        .expect(1)
        .mount(&server)
        .await;
    let id = store(&server).save_draft(&recalled()).await.unwrap();
    assert_eq!(id, MessageId::new("d1"));
}

#[tokio::test]
async fn a_recalled_draft_is_never_deleted() {
    let server = MockServer::start().await;
    no_new_or_deleted_drafts(&server).await;
    route("PATCH", "/me/messages/d1")
        .respond_with(id(200, "d1"))
        .expect(2)
        .mount(&server)
        .await;
    // Graph refuses to send it: it stays, for the next attempt.
    route("POST", "/me/messages/d1/send")
        .respond_with(graph_error(400, "ErrorInvalidRecipients", "bad recipient"))
        .expect(2)
        .mount(&server)
        .await;
    let store = store(&server);
    let message = recalled();
    assert!(store.send(&message).await.is_err());
    assert!(store.send(&message).await.is_err());
    // Nor when the user gives up on it.
    store.discard_kept(&message).await.unwrap();
}

#[tokio::test]
async fn a_recalled_draft_gone_from_drafts_is_not_sent_anew() {
    let server = MockServer::start().await;
    no_new_or_deleted_drafts(&server).await;
    route("PATCH", "/me/messages/d1")
        .respond_with(graph_error(404, "ErrorItemNotFound", "not found"))
        .mount(&server)
        .await;
    let e = store(&server).send(&recalled()).await.unwrap_err();
    assert!(e.to_string().contains("no longer in Drafts"), "{e}");
}

#[tokio::test]
async fn takes_attachments_off_a_recalled_draft() {
    let server = MockServer::start().await;
    no_new_or_deleted_drafts(&server).await;
    server.on("PATCH", "/me/messages/d1", id(200, "d1")).await;
    route("GET", "/me/messages/d1/attachments")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                { "id": "a1", "name": "a.pdf", "size": 3 },
                { "id": "a2", "name": "b.pdf", "size": 3 },
                // Added in Outlook since it was recalled.
                { "id": "a3", "name": "c.pdf", "size": 3 },
            ],
        })))
        // Listed once: what Outlook adds between two listings could be
        // taken for mett's.
        .expect(1)
        .mount(&server)
        .await;
    route("DELETE", "/me/messages/d1/attachments/a1")
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    for id in ["a2", "a3"] {
        route("DELETE", &format!("/me/messages/d1/attachments/{id}"))
            .respond_with(ResponseTemplate::new(204))
            .expect(0)
            .mount(&server)
            .await;
    }
    let mut message = recalled();
    message.drop_attachments = vec![mett_core::AttachmentId::new("a1")];
    store(&server).save_draft(&message).await.unwrap();
}

#[tokio::test]
async fn every_attachment_can_come_off_a_recalled_draft_even_on_retry() {
    let server = MockServer::start().await;
    no_new_or_deleted_drafts(&server).await;
    server.on("PATCH", "/me/messages/d1", id(200, "d1")).await;
    route("GET", "/me/messages/d1/attachments")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                { "id": "a1", "name": "a.pdf", "size": 3 },
                { "id": "a2", "name": "b.pdf", "size": 3 },
            ],
        })))
        .mount(&server)
        .await;
    for id in ["a1", "a2"] {
        server
            .on(
                "DELETE",
                &format!("/me/messages/d1/attachments/{id}"),
                ResponseTemplate::new(204),
            )
            .await;
    }
    // The first attempt keeps a2, and its send's answer is lost.
    server
        .on_n(
            "POST",
            "/me/messages/d1/send",
            1,
            ResponseTemplate::new(503),
        )
        .await;
    let store = store(&server);
    let mut message = recalled();
    message.drop_attachments = vec![mett_core::AttachmentId::new("a1")];
    assert!(store.send(&message).await.is_err());
    // Then a2 comes off too.
    server
        .on("POST", "/me/messages/d1/send", ResponseTemplate::new(202))
        .await;
    message
        .drop_attachments
        .push(mett_core::AttachmentId::new("a2"));
    store.send(&message).await.unwrap();
    let deleted = deleted(&server).await;
    assert!(
        deleted.contains(&"/v1.0/me/messages/d1/attachments/a2".to_owned()),
        "{deleted:?}"
    );
}

/// d1, a recalled draft with a1, to which mett attaches new.bin: the
/// upload's answer is `upload` (Graph's status and body), and the send's
/// answer is lost. Returns the store, the message, and the server.
async fn recalled_upload_lost_send(
    upload: ResponseTemplate,
) -> (MockServer, GraphMailStore<StaticAuth>, Draft) {
    let server = MockServer::start().await;
    no_new_or_deleted_drafts(&server).await;
    server.on("PATCH", "/me/messages/d1", id(200, "d1")).await;
    route("GET", "/me/messages/d1/attachments")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [{ "id": "a1", "name": "a.pdf", "size": 3 }],
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    server
        .on_n("POST", "/me/messages/d1/attachments", 1, upload)
        .await;
    server
        .on_n(
            "POST",
            "/me/messages/d1/send",
            1,
            ResponseTemplate::new(503),
        )
        .await;
    let store = store(&server);
    let mut message = recalled();
    message.attachments.push(attached("new.bin", 3));
    assert!(store.send(&message).await.is_err());
    (server, store, message)
}

/// What d1 has on a retry: a1, mett's upload n1, and o1, a file of the
/// same name and type added in Outlook meanwhile.
async fn listed_on_retry(server: &MockServer) {
    route("GET", "/me/messages/d1/attachments")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                { "id": "a1", "name": "a.pdf", "size": 3 },
                { "id": "n1", "name": "new.bin", "contentType": "application/octet-stream", "size": 3 },
                { "id": "o1", "name": "new.bin", "contentType": "application/octet-stream", "size": 3 },
            ],
        })))
        .mount(server)
        .await;
}

/// What d1 has after an upload's answer was lost: a1, and n1, which may
/// be the upload.
async fn listed_a1_n1(server: &MockServer) {
    route("GET", "/me/messages/d1/attachments")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                { "id": "a1", "name": "a.pdf", "size": 3 },
                { "id": "n1", "name": "new.bin", "contentType": "application/octet-stream", "size": 3 },
            ],
        })))
        .mount(server)
        .await;
}

/// The paths of the DELETE requests `server` received.
async fn deleted(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "DELETE")
        .map(|r| r.url.path().to_owned())
        .collect()
}

#[tokio::test]
async fn a_retry_takes_off_only_mett_s_upload_of_a_recalled_draft() {
    let upload = id(201, "n1");
    let (server, store, message) = recalled_upload_lost_send(upload).await;
    listed_on_retry(&server).await;
    server
        .on(
            "DELETE",
            "/me/messages/d1/attachments/n1",
            ResponseTemplate::new(204),
        )
        .await;
    server
        .on("POST", "/me/messages/d1/attachments", id(201, "n2"))
        .await;
    server
        .on("POST", "/me/messages/d1/send", ResponseTemplate::new(202))
        .await;
    store.send(&message).await.unwrap();
    // By its ID: o1, with the same name, stays.
    assert_eq!(
        deleted(&server).await,
        ["/v1.0/me/messages/d1/attachments/n1"]
    );
}

#[tokio::test]
async fn a_file_taken_off_stays_known_until_it_is_gone() {
    let upload = id(201, "n1");
    let (server, store, mut message) = recalled_upload_lost_send(upload).await;
    listed_on_retry(&server).await;
    // The user takes the file off; taking it off the draft fails once.
    message.attachments.clear();
    server
        .on_n(
            "DELETE",
            "/me/messages/d1/attachments/n1",
            1,
            ResponseTemplate::new(500),
        )
        .await;
    assert!(store.send(&message).await.is_err());
    // The next attempt takes it off still.
    server
        .on(
            "DELETE",
            "/me/messages/d1/attachments/n1",
            ResponseTemplate::new(204),
        )
        .await;
    server
        .on("POST", "/me/messages/d1/send", ResponseTemplate::new(202))
        .await;
    store.send(&message).await.unwrap();
    assert_eq!(
        deleted(&server).await,
        ["/v1.0/me/messages/d1/attachments/n1"; 2]
    );
}

#[tokio::test]
async fn an_upload_whose_answer_was_lost_is_left_alone() {
    // The upload's answer is lost: the attempt fails there.
    let (server, store, message) = recalled_upload_lost_send(ResponseTemplate::new(503)).await;
    listed_a1_n1(&server).await;
    route("POST", "/me/messages/d1/attachments")
        .respond_with(id(201, "n2"))
        .expect(1)
        .mount(&server)
        .await;
    route("POST", "/me/messages/d1/send")
        .respond_with(ResponseTemplate::new(202))
        .with_priority(1)
        .mount(&server)
        .await;
    // n1 may be mett's, or another's: it stays, and the file goes on again.
    store.send(&message).await.unwrap();
    assert!(deleted(&server).await.is_empty());
}

#[tokio::test]
async fn a_file_taken_off_after_a_lost_upload_is_not_sent_unsure() {
    let (server, store, mut message) = recalled_upload_lost_send(ResponseTemplate::new(503)).await;
    listed_a1_n1(&server).await;
    route("POST", "/me/messages/d1/send")
        .respond_with(ResponseTemplate::new(202))
        .with_priority(1)
        .expect(0)
        .mount(&server)
        .await;
    message.attachments.clear();
    let e = store.send(&message).await.unwrap_err();
    assert!(e.to_string().contains("check it in Drafts"), "{e}");
    assert!(deleted(&server).await.is_empty());
}

#[tokio::test]
async fn a_file_replaced_after_a_lost_upload_is_not_sent_unsure() {
    let (server, store, mut message) = recalled_upload_lost_send(ResponseTemplate::new(503)).await;
    listed_a1_n1(&server).await;
    route("POST", "/me/messages/d1/send")
        .respond_with(ResponseTemplate::new(202))
        .with_priority(1)
        .expect(0)
        .mount(&server)
        .await;
    // Another file of the same name and type: the one uploaded may be
    // there, and isn't to go.
    message.attachments[0].data = vec![9; 3];
    let e = store.send(&message).await.unwrap_err();
    assert!(e.to_string().contains("check it in Drafts"), "{e}");
}

#[tokio::test]
async fn more_lost_uploads_than_copies_are_not_sent_unsure() {
    // Two attempts lose their upload's answer.
    let (server, store, message) = recalled_upload_lost_send(ResponseTemplate::new(503)).await;
    route("GET", "/me/messages/d1/attachments")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [{ "id": "a1", "name": "a.pdf", "size": 3 }],
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    server
        .on_n(
            "POST",
            "/me/messages/d1/attachments",
            1,
            ResponseTemplate::new(503),
        )
        .await;
    assert!(store.send(&message).await.is_err());
    listed_on_retry(&server).await;
    route("POST", "/me/messages/d1/send")
        .respond_with(ResponseTemplate::new(202))
        .expect(0)
        .mount(&server)
        .await;
    // Either may be there, and one copy is wanted: which can't be told.
    let e = store.send(&message).await.unwrap_err();
    assert!(e.to_string().contains("check it in Drafts"), "{e}");
}

#[tokio::test]
async fn a_lost_upload_stays_unsure_over_later_attempts() {
    // The upload's answer is lost.
    let (server, store, mut message) = recalled_upload_lost_send(ResponseTemplate::new(503)).await;
    // The next attempt lists it (maybe), uploads the file again (n2), and
    // loses the send's answer.
    route("GET", "/me/messages/d1/attachments")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                { "id": "a1", "name": "a.pdf", "size": 3 },
                { "id": "n1", "name": "new.bin", "contentType": "application/octet-stream", "size": 3 },
            ],
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    server
        .on_n("POST", "/me/messages/d1/attachments", 1, id(201, "n2"))
        .await;
    server
        .on_n(
            "POST",
            "/me/messages/d1/send",
            1,
            ResponseTemplate::new(503),
        )
        .await;
    assert!(store.send(&message).await.is_err());
    // The user takes the file off: n1 may still be the lost upload.
    route("GET", "/me/messages/d1/attachments")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                { "id": "a1", "name": "a.pdf", "size": 3 },
                { "id": "n1", "name": "new.bin", "contentType": "application/octet-stream", "size": 3 },
                { "id": "n2", "name": "new.bin", "contentType": "application/octet-stream", "size": 3 },
            ],
        })))
        .mount(&server)
        .await;
    route("POST", "/me/messages/d1/send")
        .respond_with(ResponseTemplate::new(202))
        .expect(0)
        .mount(&server)
        .await;
    message.attachments.clear();
    let e = store.send(&message).await.unwrap_err();
    assert!(e.to_string().contains("check it in Drafts"), "{e}");
}

#[tokio::test]
async fn an_upload_whose_answer_was_lost_is_not_guessed_among_look_alikes() {
    let (server, store, message) = recalled_upload_lost_send(ResponseTemplate::new(503)).await;
    // n1 and o1 both look like it: neither is taken off.
    listed_on_retry(&server).await;
    server
        .on("POST", "/me/messages/d1/attachments", id(201, "n2"))
        .await;
    route("POST", "/me/messages/d1/send")
        .respond_with(ResponseTemplate::new(202))
        .with_priority(1)
        .mount(&server)
        .await;
    store.send(&message).await.unwrap();
    assert!(deleted(&server).await.is_empty());
}

#[tokio::test]
async fn a_refused_upload_leaves_nothing_to_take_off() {
    let refused = graph_error(400, "ErrorInvalidRequest", "bad file");
    let (server, store, message) = recalled_upload_lost_send(refused).await;
    // A file of that name and type added in Outlook since stays.
    route("GET", "/me/messages/d1/attachments")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                { "id": "a1", "name": "a.pdf", "size": 3 },
                { "id": "o1", "name": "new.bin", "contentType": "application/octet-stream", "size": 3 },
            ],
        })))
        .mount(&server)
        .await;
    server
        .on("POST", "/me/messages/d1/attachments", id(201, "n2"))
        .await;
    route("POST", "/me/messages/d1/send")
        .respond_with(ResponseTemplate::new(202))
        .with_priority(1)
        .mount(&server)
        .await;
    store.send(&message).await.unwrap();
    assert!(deleted(&server).await.is_empty());
}

#[tokio::test]
async fn a_large_upload_is_known_by_its_decoded_id() {
    let server = MockServer::start().await;
    no_new_or_deleted_drafts(&server).await;
    server.on("PATCH", "/me/messages/d1", id(200, "d1")).await;
    server
        .on_n(
            "GET",
            "/me/messages/d1/attachments",
            1,
            ResponseTemplate::new(200).set_body_json(json!({ "value": [] })),
        )
        .await;
    route("POST", "/me/messages/d1/attachments/createUploadSession")
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "uploadUrl": format!("{}/upload/session", server.uri()),
        })))
        .mount(&server)
        .await;
    session()
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    session()
        .and(header("content-range", "bytes 3276800-3999999/4000000"))
        .respond_with(ResponseTemplate::new(201).insert_header(
            "Location",
            "https://x/api/v2.0/Users('u')/Messages('d1')/Attachments('big%3D1')",
        ))
        .with_priority(1)
        .mount(&server)
        .await;
    // The send's answer is lost.
    server
        .on_n(
            "POST",
            "/me/messages/d1/send",
            1,
            ResponseTemplate::new(503),
        )
        .await;
    let store = store(&server);
    let mut message = recalled();
    message.attachments.push(attached("big.bin", 4_000_000));
    assert!(store.send(&message).await.is_err());
    // The user takes it off: it comes off, by its ID as listed.
    message.attachments.clear();
    route("GET", "/me/messages/d1/attachments")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [{ "id": "big=1", "name": "big.bin", "size": 4_000_000 }],
        })))
        .mount(&server)
        .await;
    route("DELETE", "/me/messages/d1/attachments/big=1")
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    server
        .on("POST", "/me/messages/d1/send", ResponseTemplate::new(202))
        .await;
    store.send(&message).await.unwrap();
}

#[tokio::test]
async fn syncs_a_folder_with_delta_links() {
    let server = MockServer::start().await;
    let base = server.uri();
    route("GET", "/me/mailFolders/inbox-id/messages/delta")
        .and(header("prefer", "odata.maxpagesize=500"))
        .and(wiremock::matchers::query_param_is_missing("$skiptoken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [message("m1", "2026-10-14T10:00:00Z")],
            "@odata.nextLink": format!("{base}/v1.0/me/mailFolders/inbox-id/messages/delta?$skiptoken=p2"),
        })))
        .mount(&server)
        .await;
    route("GET", "/me/mailFolders/inbox-id/messages/delta")
        .and(query_param("$skiptoken", "p2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                message("m2", "2026-10-14T11:00:00Z"),
                { "id": "m0", "@removed": { "reason": "deleted" } },
                // Gone, then back: it is here.
                { "id": "m4", "@removed": { "reason": "deleted" } },
                message("m4", "2026-10-14T13:00:00Z"),
                // An entry that leaves out its folder is in this one.
                {
                    "id": "m3",
                    "subject": "Partial",
                    "sentDateTime": "2026-10-14T12:00:00Z",
                },
                // Removed twice: where it was first.
                { "id": "m5", "@removed": { "reason": "deleted" } },
                { "id": "m6", "@removed": { "reason": "deleted" } },
                { "id": "m5", "@removed": { "reason": "deleted" } },
                // Changed again: where it last was.
                message("m2", "2026-10-14T11:30:00Z"),
            ],
            "@odata.deltaLink": format!("{base}/v1.0/me/mailFolders/inbox-id/messages/delta?$deltatoken=d1"),
        })))
        .mount(&server)
        .await;
    let store = store(&server);
    assert!(store.can_sync());
    let inbox = FolderId::new("inbox-id");
    let first = store.sync_folder(&inbox, None).await.unwrap();
    assert_eq!(first.changed[0].id, MessageId::new("m1"));
    assert!(first.more);
    let second = store.sync_folder(&inbox, Some(&first.state)).await.unwrap();
    let changed: Vec<&str> = second.changed.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(changed, ["m4", "m3", "m2"]);
    assert_eq!(second.changed[1].folder, inbox, "m3 left out its folder");
    let removed: Vec<&str> = second.removed.iter().map(|m| m.as_str()).collect();
    assert_eq!(removed, ["m0", "m5", "m6"]);
    assert!(!second.more);
    assert!(second.state.as_str().ends_with("$deltatoken=d1"));
    // A link of another endpoint is never followed.
    let elsewhere = SyncState::new(
        "inbox-id https://elsewhere.example/v1.0/me/mailFolders/inbox-id/messages/delta?$deltatoken=x",
    );
    assert!(matches!(
        store.sync_folder(&inbox, Some(&elsewhere)).await,
        Err(Error::StaleSync)
    ));
    // A state is only good for its own folder.
    let other = store
        .sync_folder(&FolderId::new("other"), Some(&second.state))
        .await;
    assert!(matches!(other, Err(Error::StaleSync)));
}

#[tokio::test]
async fn an_expired_delta_link_starts_over() {
    let server = MockServer::start().await;
    route("GET", "/me/mailFolders/inbox-id/messages/delta")
        .respond_with(graph_error(410, "SyncStateNotFound", "expired"))
        .mount(&server)
        .await;
    let state = SyncState::new(format!(
        "inbox-id {}/v1.0/me/mailFolders/inbox-id/messages/delta?$deltatoken=old",
        server.uri()
    ));
    let e = store(&server)
        .sync_folder(&FolderId::new("inbox-id"), Some(&state))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::StaleSync), "{e:?}");
}

/// A request for a meeting that repeats weekly (`repeats`) or not, whose
/// event is `event`.
fn meeting(repeats: bool, event: &str) -> mett_core::Invitation {
    use chrono::TimeZone;
    mett_core::Invitation {
        kind: mett_core::InvitationKind::Request,
        title: None,
        start: Some(Utc.with_ymd_and_hms(2026, 11, 3, 14, 0, 0).unwrap()),
        end: Some(Utc.with_ymd_and_hms(2026, 11, 3, 15, 0, 0).unwrap()),
        all_day: false,
        location: None,
        organizer: None,
        attendees: Vec::new(),
        recurrence: repeats.then(|| mett_core::Recurrence::new(mett_core::RecurrenceUnit::Week, 1)),
        reply: mett_core::Reply::None,
        out_of_date: false,
        response_requested: true,
        in_series: false,
        series_master: false,
        event: Some(event.into()),
    }
}

fn answer(rsvp: mett_core::Rsvp, scope: mett_core::Scope, notify: bool) -> mett_core::Answer {
    mett_core::Answer {
        rsvp,
        scope,
        comment: Some("See you".into()),
        notify,
    }
}

#[tokio::test]
async fn answers_a_meeting_that_does_not_repeat() {
    let server = MockServer::start().await;
    route("POST", "/me/events/ev1/tentativelyAccept")
        .and(body_json(
            json!({"comment": "See you", "sendResponse": true}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    store(&server)
        .answer_invitation(
            &MessageId::new("m1"),
            &meeting(false, "ev1"),
            &answer(mett_core::Rsvp::Tentative, mett_core::Scope::Series, true),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn answers_a_series_through_its_master() {
    let server = MockServer::start().await;
    // The invitation is about one occurrence, whose series is answered.
    route("GET", "/me/events/occ1")
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"id": "occ1", "type": "occurrence", "seriesMasterId": "master"}),
            ),
        )
        .mount(&server)
        .await;
    route("POST", "/me/events/master/decline")
        .and(body_json(
            json!({"comment": "See you", "sendResponse": false}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    store(&server)
        .answer_invitation(
            &MessageId::new("m1"),
            &meeting(true, "occ1"),
            &answer(mett_core::Rsvp::Decline, mett_core::Scope::Series, false),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn answers_one_occurrence_of_a_series() {
    let server = MockServer::start().await;
    route("GET", "/me/events/master")
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "master", "type": "seriesMaster"})),
        )
        .mount(&server)
        .await;
    route("GET", "/me/events/master/instances")
        .and(query_param("startDateTime", "2026-11-03T14:00:00Z"))
        .and(query_param("endDateTime", "2026-11-03T15:00:00Z"))
        .and(header("prefer", "outlook.timezone=\"UTC\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [
            {"id": "week1", "start": {"dateTime": "2026-11-03T14:00:00.0000000", "timeZone": "UTC"}}
        ]})))
        .mount(&server)
        .await;
    route("POST", "/me/events/week1/accept")
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    store(&server)
        .answer_invitation(
            &MessageId::new("m1"),
            &meeting(true, "master"),
            &answer(mett_core::Rsvp::Accept, mett_core::Scope::Occurrence, true),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn looks_for_the_occurrence_through_the_pages_of_instances() {
    let server = MockServer::start().await;
    route("GET", "/me/events/master")
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "master", "type": "seriesMaster"})),
        )
        .mount(&server)
        .await;
    let next = format!("{}/v1.0/me/events/master/instances?page=2", server.uri());
    route("GET", "/me/events/master/instances")
        .and(query_param("startDateTime", "2026-11-03T14:00:00Z"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                {"id": "earlier", "start": {"dateTime": "2026-11-02T14:00:00.0000000", "timeZone": "UTC"}}
            ],
            "@odata.nextLink": next,
        })))
        .mount(&server)
        .await;
    route("GET", "/me/events/master/instances")
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [
            {"id": "week1", "start": {"dateTime": "2026-11-03T14:00:00.0000000", "timeZone": "UTC"}}
        ]})))
        .mount(&server)
        .await;
    route("POST", "/me/events/week1/accept")
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    store(&server)
        .answer_invitation(
            &MessageId::new("m1"),
            &meeting(true, "master"),
            &answer(mett_core::Rsvp::Accept, mett_core::Scope::Occurrence, true),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn an_occurrence_that_is_not_there_is_not_answered() {
    let server = MockServer::start().await;
    route("GET", "/me/events/master")
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": "master", "type": "seriesMaster"})),
        )
        .mount(&server)
        .await;
    route("GET", "/me/events/master/instances")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [
            {"id": "other", "start": {"dateTime": "2026-11-10T14:00:00.0000000", "timeZone": "UTC"}}
        ]})))
        .mount(&server)
        .await;
    let e = store(&server)
        .answer_invitation(
            &MessageId::new("m1"),
            &meeting(true, "master"),
            &answer(mett_core::Rsvp::Accept, mett_core::Scope::Occurrence, true),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(e, Error::Backend(ref m) if m.contains("isn't in your calendar")),
        "{e:?}"
    );
}

#[tokio::test]
async fn a_missing_calendar_permission_says_so() {
    let server = MockServer::start().await;
    route("POST", "/me/events/ev1/accept")
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": {"code": "ErrorAccessDenied", "message": "Access is denied."}
        })))
        .mount(&server)
        .await;
    let e = store(&server)
        .answer_invitation(
            &MessageId::new("m1"),
            &meeting(false, "ev1"),
            &answer(mett_core::Rsvp::Accept, mett_core::Scope::Series, true),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(e, Error::Backend(ref m) if m.contains("Calendars.ReadWrite")),
        "{e:?}"
    );
}

#[tokio::test]
async fn an_invitation_without_its_event_cannot_be_answered() {
    let server = MockServer::start().await;
    let mut invitation = meeting(false, "ev1");
    invitation.event = None;
    let e = store(&server)
        .answer_invitation(
            &MessageId::new("m1"),
            &invitation,
            &answer(mett_core::Rsvp::Accept, mett_core::Scope::Series, true),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(e, Error::Backend(ref m) if m.contains("calendar")),
        "{e:?}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}
