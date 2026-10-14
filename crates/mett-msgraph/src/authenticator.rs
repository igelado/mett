//! [`GraphAuthenticator`]: the [`Authenticator`] port for Microsoft Graph.

use std::sync::Arc;

use chrono::{TimeDelta, Utc};
use mett_core::{AccessToken, Authenticator, Result};
use tokio::sync::Mutex;

use crate::oauth::{self, FlowError, Tokens};
use crate::prompt::Prompter;
use crate::settings::{AuthFlow, AuthSettings};
use crate::token_store::TokenStore;

/// Access tokens this close to expiry are refreshed before use.
const EXPIRY_MARGIN: TimeDelta = TimeDelta::minutes(5);

/// Provides access tokens for one account.
///
/// It returns the cached access token while it is valid, otherwise refreshes
/// it with the stored refresh token, and signs in interactively (through the
/// [`Prompter`]) only when there is no usable refresh token. Concurrent
/// callers share a single refresh or sign-in.
pub struct GraphAuthenticator<S, P> {
    settings: AuthSettings,
    http: reqwest::Client,
    store: Arc<S>,
    prompter: P,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    access: Option<AccessToken>,
    refresh: Option<String>,
    loaded: bool,
    /// Tokens from an interactive sign-in whose user could not be confirmed
    /// yet (for example, Graph was briefly unavailable). They are neither
    /// used nor stored until the check succeeds, but keeping them avoids
    /// asking the user to sign in again.
    unconfirmed: Option<Tokens>,
}

/// The outcome of checking which user an interactive sign-in was for.
enum Identity {
    Confirmed,
    /// Definitely another user: discard the tokens.
    Mismatch(mett_core::Error),
    /// Could not tell yet: keep the tokens and retry later.
    Unknown(mett_core::Error),
}

impl<S: TokenStore, P: Prompter> GraphAuthenticator<S, P> {
    /// Creates an authenticator. Nothing happens until the first
    /// [`Authenticator::access_token`] call.
    pub fn new(settings: AuthSettings, http: reqwest::Client, store: Arc<S>, prompter: P) -> Self {
        Self {
            settings,
            http,
            store,
            prompter,
            state: Mutex::new(State::default()),
        }
    }

    /// The settings this authenticator signs in with.
    pub fn settings(&self) -> &AuthSettings {
        &self.settings
    }

    /// Forgets the cached tokens and removes the stored refresh token, so the
    /// next call signs in interactively.
    pub async fn sign_out(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        *state = State {
            loaded: true,
            ..State::default()
        };
        let (store, account) = (Arc::clone(&self.store), self.settings.account.clone());
        blocking(move || store.clear(&account)).await
    }

    async fn sign_in(&self) -> Result<Tokens, FlowError> {
        match self.settings.flow {
            AuthFlow::AuthCode { redirect_port } => {
                oauth::auth_code(&self.http, &self.settings, redirect_port, &self.prompter).await
            }
            AuthFlow::DeviceCode => {
                oauth::device_code(&self.http, &self.settings, &self.prompter).await
            }
        }
    }

    /// Checks that an interactive sign-in was for the configured account.
    ///
    /// `login_hint` is only a hint, and the device code flow has none, so the
    /// user can sign in as someone else. Accepting those tokens would show
    /// another mailbox under this account's name. Refreshed tokens don't need
    /// the check: they belong to the same user as the refresh token, which
    /// was checked when it was issued.
    async fn check_identity(&self, tokens: &Tokens) -> Identity {
        let expected = self.settings.account.as_str();
        match oauth::user_principal_name(&self.http, &self.settings, &tokens.access).await {
            Ok(user) if user.eq_ignore_ascii_case(expected) => Identity::Confirmed,
            Ok(user) => Identity::Mismatch(mett_core::Error::Auth(format!(
                "signed in as {user}, but the account is {expected}"
            ))),
            // Graph rejected the token: retrying with it can't help.
            Err(e @ FlowError::OAuth { .. }) => Identity::Mismatch(mett_core::Error::Auth(
                format!("could not confirm that the sign-in was for {expected}: {e}"),
            )),
            Err(e) => Identity::Unknown(mett_core::Error::Auth(format!(
                "could not confirm that the sign-in was for {expected}: {e}"
            ))),
        }
    }

    /// Accepts `tokens` from an interactive sign-in once their user is
    /// confirmed, or keeps them as unconfirmed if that is not possible yet.
    async fn confirm(&self, state: &mut State, tokens: Tokens) -> Result<AccessToken> {
        match self.check_identity(&tokens).await {
            Identity::Confirmed => self.keep(state, tokens).await,
            Identity::Mismatch(e) => Err(e),
            Identity::Unknown(e) => {
                state.unconfirmed = Some(tokens);
                Err(e)
            }
        }
    }

    /// Caches `tokens` and stores a new refresh token if one was issued.
    ///
    /// The tokens are kept in memory first, so that if storing the refresh
    /// token fails, this session keeps working without another sign-in; the
    /// error is still reported.
    async fn keep(&self, state: &mut State, tokens: Tokens) -> Result<AccessToken> {
        // From now on the in-memory tokens are authoritative; don't let a
        // later load from the store replace them.
        state.loaded = true;
        state.access = Some(tokens.access.clone());
        if let Some(refresh) = tokens.refresh
            && state.refresh.as_ref() != Some(&refresh)
        {
            state.refresh = Some(refresh.clone());
            let (store, account) = (Arc::clone(&self.store), self.settings.account.clone());
            blocking(move || store.save(&account, &refresh)).await?;
        }
        Ok(tokens.access)
    }
}

impl<S: TokenStore, P: Prompter> Authenticator for GraphAuthenticator<S, P> {
    async fn access_token(&self) -> Result<AccessToken> {
        let mut state = self.state.lock().await;

        let now = Utc::now();
        if let Some(token) = &state.access
            && !token.expires_within(EXPIRY_MARGIN, now)
        {
            return Ok(token.clone());
        }

        // Retry confirming a previous sign-in before asking for a new one.
        if let Some(tokens) = state.unconfirmed.take()
            && !tokens.access.expires_within(TimeDelta::zero(), now)
        {
            let token = self.confirm(&mut state, tokens).await?;
            // The check may have taken a while; if the token expired in the
            // meantime, continue with the confirmed refresh token below.
            if !token.expires_within(EXPIRY_MARGIN, Utc::now()) {
                return Ok(token);
            }
        }

        if !state.loaded {
            let (store, account) = (Arc::clone(&self.store), self.settings.account.clone());
            state.refresh = blocking(move || store.load(&account)).await?;
            state.loaded = true;
        }

        if let Some(refresh) = state.refresh.clone() {
            match oauth::refresh(&self.http, &self.settings, &refresh).await {
                Ok(tokens) => return self.keep(&mut state, tokens).await,
                // The refresh token was revoked, expired or needs new
                // consent: fall back to an interactive sign-in.
                Err(e) if e.needs_sign_in() => {
                    state.refresh = None;
                    let (store, account) = (Arc::clone(&self.store), self.settings.account.clone());
                    blocking(move || store.clear(&account)).await?;
                }
                // A transient failure: keep the refresh token, and use the
                // cached access token while it is still valid.
                Err(e) => {
                    return match &state.access {
                        // Checked against the time after the refresh attempt,
                        // which may have taken a while.
                        Some(token) if !token.expires_within(TimeDelta::zero(), Utc::now()) => {
                            Ok(token.clone())
                        }
                        _ => Err(e.into()),
                    };
                }
            }
        }

        let tokens = self.sign_in().await?;
        self.confirm(&mut state, tokens).await
    }
}

/// Runs a blocking token-store call off the async runtime.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, crate::TokenStoreError> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| mett_core::Error::Auth(format!("credential store task failed: {e}")))?
        .map_err(Into::into)
}
