//! Sign-in flows against a mock identity platform and Graph.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mett_core::{AccountId, Authenticator, Error};
use mett_msgraph::{
    AuthFlow, AuthSettings, GraphAuthenticator, MemoryStore, SignInPrompt, TokenStore,
};
use serde_json::json;
use url::Url;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN_PATH: &str = "/contoso.com/oauth2/v2.0/token";
const DEVICE_PATH: &str = "/contoso.com/oauth2/v2.0/devicecode";

fn account() -> AccountId {
    AccountId::new("alice@contoso.com")
}

/// Settings that send both identity-platform and Graph requests to `server`.
fn settings(server: &MockServer, flow: AuthFlow) -> AuthSettings {
    let url = Url::parse(&server.uri()).unwrap();
    AuthSettings {
        flow,
        login_url: Some(url.clone()),
        graph_url: Some(url),
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

fn device_code_response(expires_in: u64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "device_code": "dev-code",
        "user_code": "ABCD-EFGH",
        "verification_uri": "https://microsoft.com/devicelogin",
        "expires_in": expires_in,
        "interval": 0,
        "message": "Go to https://microsoft.com/devicelogin and enter ABCD-EFGH",
    }))
}

/// Graph's `GET /me` reports `upn` for requests made with `access`.
async fn me(server: &MockServer, access: &str, upn: &str) {
    Mock::given(method("GET"))
        .and(path("/v1.0/me"))
        .and(header("authorization", format!("Bearer {access}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "userPrincipalName": upn,
        })))
        .mount(server)
        .await;
}

/// Sets up a device code sign-in that issues `access` / `r-new` tokens.
async fn device_sign_in(server: &MockServer, access: &str) {
    Mock::given(method("POST"))
        .and(path(DEVICE_PATH))
        .respond_with(device_code_response(900))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("device_code=dev-code"))
        .respond_with(tokens(access, "r-new"))
        .with_priority(2)
        .mount(server)
        .await;
}

/// A prompter for tests in which no sign-in prompt may appear.
fn no_prompts() -> impl Fn(SignInPrompt) + Send + Sync {
    |prompt| panic!("unexpected sign-in prompt: {prompt:?}")
}

fn authenticator<P: Fn(SignInPrompt) + Send + Sync>(
    settings: AuthSettings,
    store: &Arc<MemoryStore>,
    prompter: P,
) -> GraphAuthenticator<MemoryStore, P> {
    GraphAuthenticator::new(
        settings,
        reqwest::Client::new(),
        Arc::clone(store),
        prompter,
    )
}

fn stored(store: &MemoryStore) -> Option<String> {
    store.load(&account()).unwrap()
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
    me(&server, "a1", "alice@contoso.com").await;

    let store = Arc::new(MemoryStore::default());
    store.save(&account(), "r0").unwrap();
    let auth = authenticator(
        settings(&server, AuthFlow::DeviceCode),
        &store,
        no_prompts(),
    );

    // The second call is served from the cache: the mock expects one request.
    assert_eq!(auth.access_token().await.unwrap().secret(), "a1");
    assert_eq!(auth.access_token().await.unwrap().secret(), "a1");
    assert_eq!(stored(&store).as_deref(), Some("r1"));
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
    // The first poll is still pending; the next one succeeds.
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("device_code=dev-code"))
        .respond_with(oauth_error("authorization_pending"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    device_sign_in(&server, "a2").await;
    me(&server, "a2", "Alice@Contoso.com").await;

    let store = Arc::new(MemoryStore::default());
    store.save(&account(), "revoked").unwrap();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&prompts);
    let auth = authenticator(settings(&server, AuthFlow::DeviceCode), &store, move |p| {
        recorded.lock().unwrap().push(p)
    });

    assert_eq!(auth.access_token().await.unwrap().secret(), "a2");
    assert_eq!(stored(&store).as_deref(), Some("r-new"));
    let prompts = prompts.lock().unwrap();
    assert!(matches!(
        prompts.as_slice(),
        [SignInPrompt::EnterCode { user_code, .. }] if user_code == "ABCD-EFGH"
    ));
}

#[tokio::test]
async fn keeps_the_refresh_token_on_transient_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(oauth_error("temporarily_unavailable"))
        .mount(&server)
        .await;

    let store = Arc::new(MemoryStore::default());
    store.save(&account(), "r0").unwrap();
    let auth = authenticator(
        settings(&server, AuthFlow::DeviceCode),
        &store,
        no_prompts(),
    );

    // Reported as a network problem, so the UI retries instead of asking
    // the user to sign in again.
    assert!(matches!(auth.access_token().await, Err(Error::Network(_))));
    assert_eq!(stored(&store).as_deref(), Some("r0"));
}

#[tokio::test]
async fn rejects_a_sign_in_as_another_user() {
    let server = MockServer::start().await;
    device_sign_in(&server, "a-m").await;
    me(&server, "a-m", "mallory@contoso.com").await;

    let store = Arc::new(MemoryStore::default());
    let auth = authenticator(settings(&server, AuthFlow::DeviceCode), &store, |_| {});

    let err = auth.access_token().await.unwrap_err();
    assert!(
        matches!(&err, Error::Auth(msg) if msg.contains("signed in as mallory@contoso.com")),
        "{err}"
    );
    assert_eq!(stored(&store), None);
}

#[tokio::test]
async fn rejects_a_sign_in_whose_user_cannot_be_confirmed() {
    let server = MockServer::start().await;
    device_sign_in(&server, "a-x").await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let store = Arc::new(MemoryStore::default());
    let auth = authenticator(settings(&server, AuthFlow::DeviceCode), &store, |_| {});

    assert!(matches!(auth.access_token().await, Err(Error::Auth(_))));
    assert_eq!(stored(&store), None);
}

#[tokio::test]
async fn gives_up_when_the_device_code_expires() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(DEVICE_PATH))
        .respond_with(device_code_response(1))
        .mount(&server)
        .await;
    // The poll request stalls for longer than the device code is valid.
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(oauth_error("authorization_pending").set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;

    let store = Arc::new(MemoryStore::default());
    let auth = authenticator(settings(&server, AuthFlow::DeviceCode), &store, |_| {});

    let started = std::time::Instant::now();
    let err = auth.access_token().await.unwrap_err();
    assert!(
        matches!(&err, Error::Auth(msg) if msg.contains("expired")),
        "{err}"
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// Plays the browser: follows the sign-in URL's redirect URI back to mett
/// with `code`. With `stale_first`, it first delivers a late redirect from an
/// earlier sign-in (another `state`), which mett must ignore.
fn browser(code: &'static str, stale_first: bool) -> impl Fn(SignInPrompt) + Send + Sync {
    move |prompt| {
        let SignInPrompt::OpenBrowser { url } = prompt else {
            panic!("expected a browser prompt, got {prompt:?}");
        };
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["login_hint"], "alice@contoso.com");
        let mut redirect = Url::parse(&query["redirect_uri"]).unwrap();
        // Use the IPv4 loopback explicitly; the test host may lack IPv6.
        redirect.set_host(Some("127.0.0.1")).unwrap();
        let with = |code: &str, state: &str| {
            let mut url = redirect.clone();
            url.query_pairs_mut()
                .append_pair("code", code)
                .append_pair("state", state);
            url
        };
        let stale = with("stale-code", "an-earlier-state");
        let current = with(code, &query["state"]);
        tokio::spawn(async move {
            if stale_first {
                let response = reqwest::get(stale).await.unwrap();
                assert_eq!(response.status(), 400);
            }
            reqwest::get(current).await.unwrap();
        });
    }
}

async fn auth_code_sign_in(stale_first: bool) {
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
    me(&server, "a3", "alice@contoso.com").await;

    let store = Arc::new(MemoryStore::default());
    let auth = authenticator(
        settings(&server, AuthFlow::AuthCode { redirect_port: 0 }),
        &store,
        browser("the-code", stale_first),
    );

    assert_eq!(auth.access_token().await.unwrap().secret(), "a3");
    assert_eq!(stored(&store).as_deref(), Some("r3"));
}

#[tokio::test]
async fn signs_in_with_the_authorization_code_flow() {
    auth_code_sign_in(false).await;
}

#[tokio::test]
async fn ignores_a_late_redirect_from_an_earlier_sign_in() {
    auth_code_sign_in(true).await;
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
    let auth = authenticator(settings, &store, no_prompts());

    assert!(matches!(auth.access_token().await, Err(Error::Network(_))));
    // A network failure must not discard the refresh token.
    assert_eq!(stored(&store).as_deref(), Some("r0"));
}

#[tokio::test]
async fn sign_out_forgets_the_refresh_token() {
    let server = MockServer::start().await;
    let store = Arc::new(MemoryStore::default());
    store.save(&account(), "r0").unwrap();
    let auth = authenticator(
        settings(&server, AuthFlow::DeviceCode),
        &store,
        no_prompts(),
    );

    auth.sign_out().await.unwrap();
    assert_eq!(stored(&store), None);
}

#[tokio::test]
async fn uses_a_still_valid_token_when_refresh_fails_transiently() {
    let server = MockServer::start().await;
    // The first refresh issues a token inside the 5-minute refresh margin…
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "a-short",
            "refresh_token": "r1",
            "expires_in": 120,
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    // …and the next refresh fails transiently.
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(oauth_error("temporarily_unavailable"))
        .with_priority(2)
        .mount(&server)
        .await;
    me(&server, "a-short", "alice@contoso.com").await;

    let store = Arc::new(MemoryStore::default());
    store.save(&account(), "r0").unwrap();
    let auth = authenticator(
        settings(&server, AuthFlow::DeviceCode),
        &store,
        no_prompts(),
    );

    assert_eq!(auth.access_token().await.unwrap().secret(), "a-short");
    assert_eq!(auth.access_token().await.unwrap().secret(), "a-short");
}

#[tokio::test]
async fn retries_confirming_the_user_without_signing_in_again() {
    let server = MockServer::start().await;
    device_sign_in(&server, "a4").await;
    // Graph is briefly unavailable, then confirms the user.
    Mock::given(method("GET"))
        .and(path("/v1.0/me"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    me(&server, "a4", "alice@contoso.com").await;

    let store = Arc::new(MemoryStore::default());
    let prompts = Arc::new(Mutex::new(0));
    let counted = Arc::clone(&prompts);
    let auth = authenticator(settings(&server, AuthFlow::DeviceCode), &store, move |_| {
        *counted.lock().unwrap() += 1
    });

    // A Graph outage is a network error: the UI retries.
    assert!(matches!(auth.access_token().await, Err(Error::Network(_))));
    // Unconfirmed tokens are not stored.
    assert_eq!(stored(&store), None);

    assert_eq!(auth.access_token().await.unwrap().secret(), "a4");
    assert_eq!(stored(&store).as_deref(), Some("r-new"));
    assert_eq!(*prompts.lock().unwrap(), 1, "signed in only once");
}

#[tokio::test]
async fn signs_in_again_when_graph_rejects_the_new_token() {
    let server = MockServer::start().await;
    device_sign_in(&server, "a5").await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let store = Arc::new(MemoryStore::default());
    let prompts = Arc::new(Mutex::new(0));
    let counted = Arc::clone(&prompts);
    let auth = authenticator(settings(&server, AuthFlow::DeviceCode), &store, move |_| {
        *counted.lock().unwrap() += 1
    });

    assert!(auth.access_token().await.is_err());
    assert!(auth.access_token().await.is_err());
    // The rejected token was not retried: each call signed in afresh.
    assert_eq!(*prompts.lock().unwrap(), 2);
    assert_eq!(stored(&store), None);
}

/// A token store whose writes fail while `broken` is set.
#[derive(Default)]
struct FlakyStore {
    broken: std::sync::atomic::AtomicBool,
    inner: MemoryStore,
}

impl FlakyStore {
    fn broken() -> Self {
        let store = Self::default();
        store.set_broken(true);
        store
    }

    fn set_broken(&self, broken: bool) {
        self.broken
            .store(broken, std::sync::atomic::Ordering::SeqCst);
    }

    fn check(&self) -> Result<(), mett_msgraph::TokenStoreError> {
        if self.broken.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(mett_msgraph::TokenStoreError::File {
                path: "/read-only/tokens".into(),
                source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            });
        }
        Ok(())
    }
}

impl TokenStore for FlakyStore {
    fn load(&self, account: &AccountId) -> Result<Option<String>, mett_msgraph::TokenStoreError> {
        self.inner.load(account)
    }

    fn save(&self, account: &AccountId, token: &str) -> Result<(), mett_msgraph::TokenStoreError> {
        self.check()?;
        self.inner.save(account, token)
    }

    fn clear(&self, account: &AccountId) -> Result<(), mett_msgraph::TokenStoreError> {
        self.check()?;
        self.inner.clear(account)
    }
}

#[tokio::test]
async fn keeps_working_in_memory_when_tokens_cannot_be_stored() {
    let server = MockServer::start().await;
    device_sign_in(&server, "a6").await;
    me(&server, "a6", "alice@contoso.com").await;

    let prompts = Arc::new(Mutex::new(0));
    let counted = Arc::clone(&prompts);
    let store = Arc::new(FlakyStore::broken());
    let auth = GraphAuthenticator::new(
        settings(&server, AuthFlow::DeviceCode),
        reqwest::Client::new(),
        Arc::clone(&store),
        move |_: SignInPrompt| *counted.lock().unwrap() += 1,
    );

    // The storage failure is reported…
    assert!(
        matches!(auth.access_token().await, Err(Error::Auth(msg)) if msg.contains("stored credentials"))
    );
    // …but the session keeps the tokens and doesn't ask to sign in again.
    assert_eq!(auth.access_token().await.unwrap().secret(), "a6");
    assert_eq!(*prompts.lock().unwrap(), 1);

    // Once the store works again, the refresh token is stored on the next
    // call.
    store.set_broken(false);
    assert_eq!(auth.access_token().await.unwrap().secret(), "a6");
    assert_eq!(store.load(&account()).unwrap().as_deref(), Some("r-new"));
}

#[tokio::test]
async fn sign_out_keeps_state_when_the_stored_token_cannot_be_removed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(tokens("a1", "r0"))
        .mount(&server)
        .await;
    me(&server, "a1", "alice@contoso.com").await;
    let store = Arc::new(FlakyStore::default());
    store.save(&account(), "r0").unwrap();
    let auth = GraphAuthenticator::new(
        settings(&server, AuthFlow::DeviceCode),
        reqwest::Client::new(),
        Arc::clone(&store),
        no_prompts(),
    );
    auth.access_token().await.unwrap();

    store.set_broken(true);
    assert!(auth.sign_out().await.is_err());
    // Still signed in, consistently with the store; sign-out can be retried.
    assert_eq!(auth.access_token().await.unwrap().secret(), "a1");
    store.set_broken(false);
    auth.sign_out().await.unwrap();
    assert_eq!(store.load(&account()).unwrap(), None);
}

#[tokio::test]
async fn throttled_token_requests_are_transient() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": "too_many_requests",
        })))
        .mount(&server)
        .await;

    let store = Arc::new(MemoryStore::default());
    store.save(&account(), "r0").unwrap();
    let auth = authenticator(
        settings(&server, AuthFlow::DeviceCode),
        &store,
        no_prompts(),
    );

    assert!(matches!(auth.access_token().await, Err(Error::Network(_))));
    assert_eq!(stored(&store).as_deref(), Some("r0"));
}

#[tokio::test]
async fn keeps_polling_after_a_transient_device_code_poll_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("device_code=dev-code"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    device_sign_in(&server, "a7").await;
    me(&server, "a7", "alice@contoso.com").await;

    let store = Arc::new(MemoryStore::default());
    let auth = authenticator(settings(&server, AuthFlow::DeviceCode), &store, |_| {});

    assert_eq!(auth.access_token().await.unwrap().secret(), "a7");
}

#[tokio::test]
async fn signs_in_again_when_the_refresh_token_needs_consent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(oauth_error("consent_required"))
        .mount(&server)
        .await;
    device_sign_in(&server, "a8").await;
    me(&server, "a8", "alice@contoso.com").await;

    let store = Arc::new(MemoryStore::default());
    store.save(&account(), "needs-consent").unwrap();
    let auth = authenticator(settings(&server, AuthFlow::DeviceCode), &store, |_| {});

    assert_eq!(auth.access_token().await.unwrap().secret(), "a8");
    assert_eq!(stored(&store).as_deref(), Some("r-new"));
}

#[tokio::test]
async fn confirms_with_a_refreshed_token_after_the_unconfirmed_one_expires() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(DEVICE_PATH))
        .respond_with(device_code_response(900))
        .mount(&server)
        .await;
    // The sign-in issues an access token that is already expired…
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("device_code=dev-code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "a-old",
            "refresh_token": "r-signin",
            "expires_in": 0,
        })))
        .mount(&server)
        .await;
    // …Graph can't confirm the user with it…
    Mock::given(method("GET"))
        .and(path("/v1.0/me"))
        .and(header("authorization", "Bearer a-old"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    // …so the next call refreshes with the sign-in's refresh token and
    // confirms with the new access token.
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("refresh_token=r-signin"))
        .respond_with(tokens("a-fresh", "r-fresh"))
        .mount(&server)
        .await;
    me(&server, "a-fresh", "alice@contoso.com").await;

    let store = Arc::new(MemoryStore::default());
    let prompts = Arc::new(Mutex::new(0));
    let counted = Arc::clone(&prompts);
    let auth = authenticator(settings(&server, AuthFlow::DeviceCode), &store, move |_| {
        *counted.lock().unwrap() += 1
    });

    assert!(auth.access_token().await.is_err());
    assert_eq!(auth.access_token().await.unwrap().secret(), "a-fresh");
    assert_eq!(stored(&store).as_deref(), Some("r-fresh"));
    assert_eq!(*prompts.lock().unwrap(), 1);
}

#[tokio::test]
async fn rejects_a_refreshed_token_for_another_user() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(tokens("a-other", "r-other"))
        .mount(&server)
        .await;
    me(&server, "a-other", "former-alice@contoso.com").await;

    let store = Arc::new(MemoryStore::default());
    store.save(&account(), "r0").unwrap();
    let auth = authenticator(
        settings(&server, AuthFlow::DeviceCode),
        &store,
        no_prompts(),
    );

    let err = auth.access_token().await.unwrap_err();
    assert!(
        matches!(&err, Error::Auth(msg) if msg.contains("former-alice")),
        "{err}"
    );
    // The stored token belongs to someone else, so it is removed.
    assert_eq!(stored(&store), None);
}

/// A store in which another mett instance replaced the token: the first
/// load still sees `r-stale`, later reads see `r-newer`.
#[derive(Default)]
struct ReplacedStore {
    loads: std::sync::atomic::AtomicUsize,
    inner: MemoryStore,
}

impl TokenStore for ReplacedStore {
    fn load(&self, account: &AccountId) -> Result<Option<String>, mett_msgraph::TokenStoreError> {
        if self.loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return Ok(Some("r-stale".into()));
        }
        self.inner.load(account)
    }

    fn save(&self, account: &AccountId, token: &str) -> Result<(), mett_msgraph::TokenStoreError> {
        self.inner.save(account, token)
    }

    fn clear(&self, account: &AccountId) -> Result<(), mett_msgraph::TokenStoreError> {
        self.inner.clear(account)
    }
}

#[tokio::test]
async fn uses_a_newer_stored_token_after_a_stale_one_is_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("refresh_token=r-stale"))
        .respond_with(oauth_error("invalid_grant"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("refresh_token=r-newer"))
        .respond_with(tokens("a-newer", "r-newer"))
        .mount(&server)
        .await;
    me(&server, "a-newer", "alice@contoso.com").await;

    let store = Arc::new(ReplacedStore::default());
    store.inner.save(&account(), "r-newer").unwrap();
    let auth = GraphAuthenticator::new(
        settings(&server, AuthFlow::DeviceCode),
        reqwest::Client::new(),
        Arc::clone(&store),
        no_prompts(),
    );

    // The rejected stale token didn't remove the newer one, which was used.
    assert_eq!(auth.access_token().await.unwrap().secret(), "a-newer");
    assert_eq!(
        store.inner.load(&account()).unwrap().as_deref(),
        Some("r-newer")
    );
}

#[tokio::test]
async fn keeps_using_a_token_without_refresh_token_until_it_expires() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(DEVICE_PATH))
        .respond_with(device_code_response(900))
        .mount(&server)
        .await;
    // No refresh token (no offline_access), and inside the refresh margin.
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "a-only",
            "expires_in": 120,
        })))
        .mount(&server)
        .await;
    me(&server, "a-only", "alice@contoso.com").await;

    let store = Arc::new(MemoryStore::default());
    let prompts = Arc::new(Mutex::new(0));
    let counted = Arc::clone(&prompts);
    let auth = authenticator(settings(&server, AuthFlow::DeviceCode), &store, move |_| {
        *counted.lock().unwrap() += 1
    });

    assert_eq!(auth.access_token().await.unwrap().secret(), "a-only");
    assert_eq!(auth.access_token().await.unwrap().secret(), "a-only");
    assert_eq!(*prompts.lock().unwrap(), 1, "signed in only once");
}

#[tokio::test]
async fn clears_the_stored_token_on_a_late_identity_mismatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(tokens("a-x", "r-x"))
        .mount(&server)
        .await;
    // Graph is down at first; later it reports another user.
    Mock::given(method("GET"))
        .and(path("/v1.0/me"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    me(&server, "a-x", "mallory@contoso.com").await;

    let store = Arc::new(MemoryStore::default());
    store.save(&account(), "r0").unwrap();
    let auth = authenticator(
        settings(&server, AuthFlow::DeviceCode),
        &store,
        no_prompts(),
    );

    assert!(matches!(auth.access_token().await, Err(Error::Network(_))));
    assert_eq!(stored(&store).as_deref(), Some("r0"));
    assert!(matches!(auth.access_token().await, Err(Error::Auth(_))));
    assert_eq!(stored(&store), None);
}

#[tokio::test]
async fn uses_the_cached_token_while_a_refreshed_one_is_unconfirmed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "a-short",
            "refresh_token": "r1",
            "expires_in": 120,
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(tokens("a-next", "r2"))
        .with_priority(2)
        .mount(&server)
        .await;
    me(&server, "a-short", "alice@contoso.com").await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me"))
        .and(header("authorization", "Bearer a-next"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let store = Arc::new(MemoryStore::default());
    store.save(&account(), "r0").unwrap();
    let auth = authenticator(
        settings(&server, AuthFlow::DeviceCode),
        &store,
        no_prompts(),
    );

    assert_eq!(auth.access_token().await.unwrap().secret(), "a-short");
    // a-short is inside the refresh margin; the refreshed a-next can't be
    // confirmed yet, so the still-valid a-short is returned.
    assert_eq!(auth.access_token().await.unwrap().secret(), "a-short");
}

#[tokio::test]
async fn keeps_a_replacement_token_after_an_identity_mismatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("refresh_token=r-stale"))
        .respond_with(tokens("a-other", "r-other"))
        .mount(&server)
        .await;
    me(&server, "a-other", "mallory@contoso.com").await;

    // The first load sees r-stale; meanwhile another instance stored r-newer.
    let store = Arc::new(ReplacedStore::default());
    store.inner.save(&account(), "r-newer").unwrap();
    let auth = GraphAuthenticator::new(
        settings(&server, AuthFlow::DeviceCode),
        reqwest::Client::new(),
        Arc::clone(&store),
        no_prompts(),
    );

    assert!(matches!(auth.access_token().await, Err(Error::Auth(_))));
    assert_eq!(
        store.inner.load(&account()).unwrap().as_deref(),
        Some("r-newer")
    );

    // The next call uses r-newer instead of prompting.
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("refresh_token=r-newer"))
        .respond_with(tokens("a-newer", "r-newer"))
        .mount(&server)
        .await;
    me(&server, "a-newer", "alice@contoso.com").await;
    assert_eq!(auth.access_token().await.unwrap().secret(), "a-newer");
}

#[tokio::test]
async fn tries_a_replacement_token_when_an_unconfirmed_refresh_is_rejected() {
    let server = MockServer::start().await;
    // A refresh from r-stale issues an already-expired token that Graph
    // can't confirm; refreshing it later is rejected.
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("refresh_token=r-stale"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "a-old",
            "refresh_token": "r-rotated",
            "expires_in": 0,
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me"))
        .and(header("authorization", "Bearer a-old"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("refresh_token=r-rotated"))
        .respond_with(oauth_error("invalid_grant"))
        .mount(&server)
        .await;
    // Another instance stored r-newer, which works.
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_string_contains("refresh_token=r-newer"))
        .respond_with(tokens("a-newer", "r-newer"))
        .mount(&server)
        .await;
    me(&server, "a-newer", "alice@contoso.com").await;

    let store = Arc::new(ReplacedStore::default());
    store.inner.save(&account(), "r-newer").unwrap();
    let auth = GraphAuthenticator::new(
        settings(&server, AuthFlow::DeviceCode),
        reqwest::Client::new(),
        Arc::clone(&store),
        no_prompts(),
    );

    assert!(auth.access_token().await.is_err());
    assert_eq!(auth.access_token().await.unwrap().secret(), "a-newer");
}
