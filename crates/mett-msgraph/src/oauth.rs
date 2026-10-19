//! OAuth2 against the Microsoft identity platform (v2.0 endpoints).
//!
//! Implements the three requests mett needs: refreshing a token, the device
//! code flow and the authorization code flow with PKCE. They are plain form
//! POSTs, so no OAuth2 library is needed.

use std::time::Duration;

use chrono::{TimeDelta, Utc};
use mett_core::AccessToken;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::time::Instant;
use url::Url;

use crate::loopback::Listener;
use crate::pkce::{Pkce, random_token};
use crate::prompt::{Prompter, SignInPrompt};
use crate::settings::AuthSettings;

/// How long to wait for the user to finish signing in in the browser.
const BROWSER_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Upper bound on each request to the identity platform, whatever the HTTP
/// client's own settings, so a stalled response can't block sign-in forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Whether an HTTP status means "try again later": throttling, a timeout
/// or a server error.
fn is_transient_status(status: reqwest::StatusCode) -> bool {
    use reqwest::StatusCode;
    status.is_server_error()
        || matches!(
            status,
            StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT
        )
}

/// The tokens returned by a successful request.
#[derive(Debug)]
pub(crate) struct Tokens {
    pub access: AccessToken,
    pub refresh: Option<String>,
}

/// Returns the user principal name of the user `token` was issued to, as
/// Microsoft Graph reports it (`GET /me`). Unlike the ID token's username
/// claims, this is the directory's authoritative identifier.
pub(crate) async fn user_principal_name(
    http: &reqwest::Client,
    settings: &AuthSettings,
    token: &AccessToken,
) -> Result<String, FlowError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Me {
        user_principal_name: String,
    }
    let mut url = settings.graph_api("me");
    url.query_pairs_mut()
        .append_pair("$select", "userPrincipalName");
    let response = http
        .get(url)
        .bearer_auth(token.secret())
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        // A definitive answer: Graph does not accept this token at all.
        return Err(FlowError::OAuth {
            code: "invalid_token".into(),
            description: "Graph rejected the access token".into(),
        });
    }
    if is_transient_status(status) {
        return Err(FlowError::Unavailable(format!(
            "could not look up the signed-in user: Graph returned HTTP {status}"
        )));
    }
    if !status.is_success() {
        return Err(FlowError::Other(format!(
            "could not look up the signed-in user: Graph returned HTTP {status}"
        )));
    }
    let body = response.bytes().await?;
    let me: Me = serde_json::from_slice(&body)
        .map_err(|e| FlowError::Other(format!("invalid response from Graph /me: {e}")))?;
    Ok(me.user_principal_name)
}

/// Returns the user principal name in `token`'s `upn` claim. The token is
/// not verified: it came straight from the identity platform, over TLS.
pub(crate) fn token_user_principal_name(token: &AccessToken) -> Result<String, FlowError> {
    use base64::Engine;

    #[derive(Deserialize)]
    struct Claims {
        upn: Option<String>,
    }
    let bad = |why: &str| FlowError::Other(format!("could not read the access token: {why}"));
    let payload = token
        .secret()
        .split('.')
        .nth(1)
        .ok_or_else(|| bad("it is not a JWT"))?;
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| bad("its claims are not base64"))?;
    let claims: Claims =
        serde_json::from_slice(&json).map_err(|_| bad("its claims are not JSON"))?;
    claims.upn.ok_or_else(|| bad("it has no upn claim"))
}

/// Why a sign-in request failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FlowError {
    /// The identity platform rejected the request, for example with
    /// `invalid_grant`.
    #[error("{code}: {description}")]
    OAuth { code: String, description: String },
    /// The identity platform could not be reached.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// The identity platform failed temporarily, for example with HTTP 503.
    #[error("{0}")]
    Unavailable(String),
    /// Anything else, such as a timeout or a malformed response.
    #[error("{0}")]
    Other(String),
}

impl FlowError {
    /// Whether the error shows that a refresh token can't be used any more,
    /// so the user has to sign in again. Other errors, such as
    /// `temporarily_unavailable`, leave the refresh token valid.
    pub(crate) fn needs_sign_in(&self) -> bool {
        matches!(self, Self::OAuth { code, .. }
            if matches!(code.as_str(),
                "invalid_grant" | "interaction_required" | "consent_required" | "login_required"))
    }

    /// Whether retrying the same request later may succeed: the identity
    /// platform was unreachable or temporarily failing.
    pub(crate) fn is_transient(&self) -> bool {
        match self {
            Self::Http(_) | Self::Unavailable(_) => true,
            Self::OAuth { code, .. } => code == "temporarily_unavailable" || code == "server_error",
            Self::Other(_) => false,
        }
    }
}

impl From<FlowError> for mett_core::Error {
    fn from(e: FlowError) -> Self {
        // Transient failures are network errors, not authentication errors:
        // the UI should retry, not ask the user to sign in again.
        if e.is_transient() {
            Self::Network(e.to_string())
        } else {
            Self::Auth(e.to_string())
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
    message: String,
}

fn default_interval() -> u64 {
    5
}

async fn post_form<T: DeserializeOwned>(
    http: &reqwest::Client,
    url: Url,
    form: &[(&str, &str)],
) -> Result<T, FlowError> {
    let response = http
        .post(url)
        .form(form)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await?;
    let status = response.status();
    let body = response.bytes().await?;
    if status.is_success() {
        return serde_json::from_slice(&body).map_err(|e| {
            FlowError::Other(format!("invalid response from identity platform: {e}"))
        });
    }
    let error = serde_json::from_slice::<ErrorResponse>(&body).ok();
    // Throttling and server errors are temporary whatever the body says.
    if is_transient_status(status) {
        let code = error.map(|e| format!(" ({})", e.error)).unwrap_or_default();
        return Err(FlowError::Unavailable(format!(
            "identity platform returned HTTP {status}{code}"
        )));
    }
    match error {
        Some(e) => Err(FlowError::OAuth {
            code: e.error,
            description: e.error_description.unwrap_or_default(),
        }),
        None => Err(FlowError::Other(format!(
            "identity platform returned HTTP {status}"
        ))),
    }
}

async fn request_token(
    http: &reqwest::Client,
    settings: &AuthSettings,
    form: &[(&str, &str)],
) -> Result<Tokens, FlowError> {
    let issued = Utc::now();
    let r: TokenResponse = post_form(http, settings.endpoint("token"), form).await?;
    Ok(Tokens {
        access: AccessToken::new(r.access_token, issued + TimeDelta::seconds(r.expires_in)),
        refresh: r.refresh_token,
    })
}

/// Exchanges a refresh token for new tokens.
pub(crate) async fn refresh(
    http: &reqwest::Client,
    settings: &AuthSettings,
    refresh_token: &str,
) -> Result<Tokens, FlowError> {
    let scope = settings.scope_param();
    request_token(
        http,
        settings,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &settings.client_id),
            ("refresh_token", refresh_token),
            ("scope", &scope),
        ],
    )
    .await
}

/// Signs in with the device code flow.
pub(crate) async fn device_code(
    http: &reqwest::Client,
    settings: &AuthSettings,
    prompter: &dyn Prompter,
) -> Result<Tokens, FlowError> {
    let scope = settings.scope_param();
    let code: DeviceCodeResponse = post_form(
        http,
        settings.endpoint("devicecode"),
        &[("client_id", &settings.client_id), ("scope", &scope)],
    )
    .await?;
    prompter.prompt(SignInPrompt::EnterCode {
        verification_uri: code.verification_uri,
        user_code: code.user_code,
        message: code.message,
    });

    // The deadline bounds both the waits and the poll requests themselves,
    // so a stalled request can't outlive the device code.
    let deadline = Instant::now() + Duration::from_secs(code.expires_in);
    let expired = || FlowError::Other("the device code expired".into());
    let mut interval = Duration::from_secs(code.interval);
    loop {
        tokio::time::sleep_until(deadline.min(Instant::now() + interval)).await;
        if Instant::now() >= deadline {
            return Err(expired());
        }
        let form = [
            ("grant_type", DEVICE_CODE_GRANT),
            ("client_id", settings.client_id.as_str()),
            ("device_code", code.device_code.as_str()),
        ];
        let poll = request_token(http, settings, &form);
        let result = tokio::time::timeout_at(deadline, poll)
            .await
            .map_err(|_| expired())?;
        match result {
            Err(FlowError::OAuth { code, .. }) if code == "authorization_pending" => {}
            Err(FlowError::OAuth { code, .. }) if code == "slow_down" => {
                interval += Duration::from_secs(5);
            }
            // The user may already have entered the code: keep polling
            // until it expires rather than making them start over.
            Err(e) if e.is_transient() => {}
            other => return other,
        }
    }
}

/// Signs in with the authorization code flow, using PKCE and a loopback
/// redirect on `port`.
pub(crate) async fn auth_code(
    http: &reqwest::Client,
    settings: &AuthSettings,
    port: u16,
    prompter: &dyn Prompter,
) -> Result<Tokens, FlowError> {
    let random_error = |e: getrandom::Error| FlowError::Other(format!("no randomness: {e}"));
    let pkce = Pkce::generate().map_err(random_error)?;
    let state = random_token().map_err(random_error)?;
    let listener = Listener::bind(port)
        .await
        .map_err(|e| FlowError::Other(format!("cannot listen on localhost:{port}: {e}")))?;
    let redirect_uri = listener.redirect_uri();
    let scope = settings.scope_param();

    let mut url = settings.endpoint("authorize");
    url.query_pairs_mut()
        .append_pair("client_id", &settings.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_mode", "query")
        .append_pair("scope", &scope)
        .append_pair("state", &state)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("login_hint", settings.account.as_str());
    prompter.prompt(SignInPrompt::OpenBrowser { url });

    let params = tokio::time::timeout(BROWSER_TIMEOUT, listener.wait_for_redirect(&state))
        .await
        .map_err(|_| FlowError::Other("timed out waiting for the browser sign-in".into()))?
        .map_err(|e| FlowError::Other(format!("loopback listener failed: {e}")))?;

    if params.get("state") != Some(&state) {
        return Err(FlowError::Other(
            "the sign-in response did not match the request (state mismatch)".into(),
        ));
    }
    if let Some(code) = params.get("error") {
        return Err(FlowError::OAuth {
            code: code.clone(),
            description: params.get("error_description").cloned().unwrap_or_default(),
        });
    }
    let code = params
        .get("code")
        .ok_or_else(|| FlowError::Other("the sign-in response had no code".into()))?;

    request_token(
        http,
        settings,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", &settings.client_id),
            ("code", code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &pkce.verifier),
            ("scope", &scope),
        ],
    )
    .await
}
