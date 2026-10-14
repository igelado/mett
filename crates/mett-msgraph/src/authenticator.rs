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

    /// Caches `tokens` and stores a new refresh token if one was issued.
    async fn keep(&self, state: &mut State, tokens: Tokens) -> Result<AccessToken> {
        if let Some(refresh) = tokens.refresh {
            if state.refresh.as_ref() != Some(&refresh) {
                let (store, account) = (Arc::clone(&self.store), self.settings.account.clone());
                let token = refresh.clone();
                blocking(move || store.save(&account, &token)).await?;
            }
            state.refresh = Some(refresh);
        }
        state.access = Some(tokens.access.clone());
        Ok(tokens.access)
    }
}

impl<S: TokenStore, P: Prompter> Authenticator for GraphAuthenticator<S, P> {
    async fn access_token(&self) -> Result<AccessToken> {
        let mut state = self.state.lock().await;

        if let Some(token) = &state.access
            && !token.expires_within(EXPIRY_MARGIN, Utc::now())
        {
            return Ok(token.clone());
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
                Err(e) => return Err(e.into()),
            }
        }

        let tokens = self.sign_in().await?;
        self.keep(&mut state, tokens).await
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
