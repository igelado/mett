//! Sign-in flows against a mock identity platform.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mett_core::{AccountId, Authenticator, Error};
use mett_msgraph::{
    AuthFlow, AuthSettings, GraphAuthenticator, MemoryStore, SignInPrompt, TokenStore,
};
use serde_json::json;
use url::Url;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN_PATH: &str = "/contoso.com/oauth2/v2.0/token";
const DEVICE_PATH: &str = "/contoso.com/oauth2/v2.0/devicecode";

fn account() -> AccountId {
    AccountId::new("alice@contoso.com")
}

fn settings(server: &MockServer, flow: AuthFlow) -> AuthSettings {
    AuthSettings {
        flow,
        login_url: Some(Url::parse(&server.uri()).unwrap()),
        ..AuthSettings::new(account(), "client-id")
    }
}

fn tokens(access: &str, refresh: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "token_type": "Bearer",
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": 3600,
    }))
}

fn oauth_error(code: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({
        "error": code,
        "error_description": format!("{code} for test"),
    }))
}

/// A prompter that records prompts and must not be asked for anything.
fn no_prompts() -> impl Fn(SignInPrompt) + Send + Sync {
    |prompt| panic!("unexpected sign-in prompt: {prompt:?}")
}

#[tokio::test]
async fn uses_and_rotates_the_stored_refresh_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=r0"))
        .and(body_string_contains("client_id=client-id"))
        .respond_with(tokens("a1", "r1"))
        .expect(1)
        .mount(&server)
        .await;

    let store = Arc::new(MemoryStore::default());
    store.save(&account(), "r0").unwrap();
    let auth = GraphAuthenticator::new(
        settings(&server, AuthFlow::DeviceCode),
        reqwest::Client::new(),
        Arc::clone(&store),
        no_prompts(),
    );

    // The second call is served from the cache: the mock expects one request.
    assert_eq!(auth.access_token().await.unwrap().secret(), "a1");
    assert_eq!(auth.access_token().await.unwrap().secret(), "a1");
    assert_eq!(store.load(&account()).unwrap().as_deref(), Some("r1"));
}

#[tokio::test]
async fn falls_back_to_device_code_when_the_refresh_token_is_revoked() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(oauth_error("invalid_grant"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(DEVICE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dev-code",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://microsoft.com/devicelogin",
            "expires_in": 900,
            "interval": 0,
            "message": "Go to https://microsoft.com/devicelogin and enter ABCD-EFGH",
        })))
        .mount(&server)
        .await;
    // The first poll is still pending, the second succeeds.
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("device_code=dev-code"))
        .respond_with(oauth_error("authorization_pending"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("device_code=dev-code"))
        .respond_with(tokens("a2", "r2"))
        .with_priority(2)
        .mount(&server)
        .await;

    let store = Arc::new(MemoryStore::default());
    store.save(&account(), "revoked").unwrap();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&prompts);
    let auth = GraphAuthenticator::new(
        settings(&server, AuthFlow::DeviceCode),
        reqwest::Client::new(),
        Arc::clone(&store),
        move |p| recorded.lock().unwrap().push(p),
    );

    assert_eq!(auth.access_token().await.unwrap().secret(), "a2");
    assert_eq!(store.load(&account()).unwrap().as_deref(), Some("r2"));
    let prompts = prompts.lock().unwrap();
    assert!(matches!(
        prompts.as_slice(),
        [SignInPrompt::EnterCode { user_code, .. }] if user_code == "ABCD-EFGH"
    ));
}

/// Plays the browser: follows the sign-in URL's redirect URI back to mett,
/// passing `code` and either the request's `state` or `state_override`.
fn browser(
    code: &'static str,
    state_override: Option<&'static str>,
) -> impl Fn(SignInPrompt) + Send + Sync {
    move |prompt| {
        let SignInPrompt::OpenBrowser { url } = prompt else {
            panic!("expected a browser prompt, got {prompt:?}");
        };
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["login_hint"], "alice@contoso.com");
        let state = state_override.map_or_else(|| query["state"].clone(), str::to_owned);
        let mut redirect = Url::parse(&query["redirect_uri"]).unwrap();
        // Use the IPv4 loopback explicitly; the test host may lack IPv6.
        redirect.set_host(Some("127.0.0.1")).unwrap();
        redirect
            .query_pairs_mut()
            .append_pair("code", code)
            .append_pair("state", &state);
        tokio::spawn(async move {
            reqwest::get(redirect).await.unwrap();
        });
    }
}

#[tokio::test]
async fn signs_in_with_the_authorization_code_flow() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=the-code"))
        .and(body_string_contains("code_verifier="))
        .and(body_string_contains(
            "redirect_uri=http%3A%2F%2Flocalhost%3A",
        ))
        .respond_with(tokens("a3", "r3"))
        .expect(1)
        .mount(&server)
        .await;

    let store = Arc::new(MemoryStore::default());
    let auth = GraphAuthenticator::new(
        settings(&server, AuthFlow::AuthCode { redirect_port: 0 }),
        reqwest::Client::new(),
        Arc::clone(&store),
        browser("the-code", None),
    );

    assert_eq!(auth.access_token().await.unwrap().secret(), "a3");
    assert_eq!(store.load(&account()).unwrap().as_deref(), Some("r3"));
}

#[tokio::test]
async fn rejects_a_redirect_with_the_wrong_state() {
    let server = MockServer::start().await;
    let auth = GraphAuthenticator::new(
        settings(&server, AuthFlow::AuthCode { redirect_port: 0 }),
        reqwest::Client::new(),
        Arc::new(MemoryStore::default()),
        browser("the-code", Some("forged")),
    );

    let err = auth.access_token().await.unwrap_err();
    assert!(
        matches!(&err, Error::Auth(msg) if msg.contains("state mismatch")),
        "{err}"
    );
}

#[tokio::test]
async fn reports_an_unreachable_identity_platform_as_a_network_error() {
    // A port that nothing listens on. (A dropped MockServer goes back to
    // wiremock's pool and keeps answering.)
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let settings = AuthSettings {
        login_url: Some(Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap()),
        ..AuthSettings::new(account(), "client-id")
    };

    let store = Arc::new(MemoryStore::default());
    store.save(&account(), "r0").unwrap();
    let auth = GraphAuthenticator::new(
        settings,
        reqwest::Client::new(),
        Arc::clone(&store),
        no_prompts(),
    );

    assert!(matches!(auth.access_token().await, Err(Error::Network(_))));
    // A network failure must not discard the refresh token.
    assert_eq!(store.load(&account()).unwrap().as_deref(), Some("r0"));
}

#[tokio::test]
async fn sign_out_forgets_the_refresh_token() {
    let server = MockServer::start().await;
    let store = Arc::new(MemoryStore::default());
    store.save(&account(), "r0").unwrap();
    let auth = GraphAuthenticator::new(
        settings(&server, AuthFlow::DeviceCode),
        reqwest::Client::new(),
        Arc::clone(&store),
        no_prompts(),
    );

    auth.sign_out().await.unwrap();
    assert_eq!(store.load(&account()).unwrap(), None);
}
