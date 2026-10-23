use mett_core::{AccessToken, Address};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::drafts::UPLOAD_CHUNK;
use super::*;

#[test]
fn large_uploads_are_given_time_to_finish() {
    assert_eq!(upload_timeout(0), REQUEST_TIMEOUT);
    assert_eq!(
        upload_timeout(10 * BYTES_PER_SECOND),
        REQUEST_TIMEOUT + Duration::from_secs(10)
    );
    // A chunk of a large attachment, at the slowest speed allowed for.
    assert!(upload_timeout(UPLOAD_CHUNK) > Duration::from_secs(UPLOAD_CHUNK as u64 / 65536));
    assert!(DOWNLOAD_TIMEOUT > REQUEST_TIMEOUT);
}

/// Drafts of every shape the size of a request depends on.
fn drafts() -> Vec<Draft> {
    let file = |name: &str, size: usize| OutgoingAttachment {
        name: name.into(),
        content_type: "application/octet-stream".into(),
        data: (0..size).map(|i| (i * 7) as u8).collect(),
    };
    let plain = Draft {
        to: vec![Address::new("bob@fabrikam.com").with_name("Bob \"B\" Smith")],
        subject: "Héllo\t\u{1}".into(),
        body: "Line \\ one\nlíne \"two\" \u{7f}\u{2028}".into(),
        save_copy: true,
        ..Draft::default()
    };
    let mut drafts = vec![plain.clone()];
    for sizes in [
        vec![0],
        vec![1],
        vec![2],
        vec![3],
        vec![4, 5, 6],
        vec![1000, 99_999],
    ] {
        let mut draft = plain.clone();
        draft.attachments = sizes
            .iter()
            .enumerate()
            .map(|(i, size)| file(&format!("f\"{i}\\é.bin"), *size))
            .collect();
        draft.from = (sizes.len() > 1).then(|| Address::new("me@contoso.com"));
        draft.cc = vec![Address::new("c@z.org"); sizes.len()];
        draft.save_copy = sizes.len() % 2 == 0;
        drafts.push(draft);
    }
    drafts
}

#[test]
fn measures_the_send_mail_request_without_encoding_it() {
    for draft in drafts() {
        let sent = serde_json::to_vec(&send_mail_json(&draft)).unwrap();
        assert_eq!(send_mail_size(&draft).unwrap(), sent.len(), "{draft:?}");
    }
}

/// Hands out a fixed token.
struct StaticAuth;

impl Authenticator for StaticAuth {
    async fn access_token(&self) -> Result<AccessToken> {
        Ok(AccessToken::new(
            "tok",
            Utc::now() + chrono::TimeDelta::hours(1),
        ))
    }
}

#[tokio::test]
async fn sends_the_request_measured() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/me/sendMail"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    let settings = AuthSettings {
        graph_url: Some(Url::parse(&server.uri()).unwrap()),
        ..AuthSettings::new("alice@contoso.com".into(), "client")
    };
    let store = GraphMailStore::new(reqwest::Client::new(), &settings, Arc::new(StaticAuth));
    for draft in drafts() {
        assert!(!needs_server_draft(&draft));
        MailSender::send(&store, &draft).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let sent = &requests.last().unwrap().body;
        assert_eq!(*sent, serde_json::to_vec(&send_mail_json(&draft)).unwrap());
        assert_eq!(sent.len(), send_mail_size(&draft).unwrap());
    }
}
