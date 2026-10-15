//! [`GraphAuthenticator`]: the [`Authenticator`] port for Microsoft Graph.

use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
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
    /// `refresh` could not be stored yet; storing it is retried on each call.
    unsaved: bool,
    /// Tokens whose user could not be confirmed yet (for example, Graph was
    /// briefly unavailable). They are neither used nor stored until the
    /// check succeeds, but keeping them avoids asking the user to sign in
    /// again.
    unconfirmed: Option<Unconfirmed>,
}

struct Unconfirmed {
    tokens: Tokens,
    /// The stored refresh token they were obtained with, if any, to remove
    /// if they turn out to belong to another user.
    stored: Option<String>,
}

impl State {
    /// Whether the cached access token can be returned at `now`. Without a
    /// refresh token, refreshing early isn't possible, so the token is used
    /// until it actually expires.
    fn usable(&self, token: &AccessToken, now: chrono::DateTime<Utc>) -> bool {
        let margin = if self.refresh.is_some() {
            EXPIRY_MARGIN
        } else {
            TimeDelta::zero()
        };
        !token.expires_within(margin, now)
    }

    /// The cached access token, if it has not actually expired yet.
    fn unexpired_access(&self) -> Option<AccessToken> {
        self.access
            .clone()
            .filter(|t| !t.expires_within(TimeDelta::zero(), Utc::now()))
    }
}

/// The outcome of checking which user an interactive sign-in was for.
enum Identity {
    Confirmed,
    /// Definitely another user: discard the tokens.
    Mismatch(mett_core::Error),
    /// Could not tell yet: keep the tokens and retry later.
    Unknown(mett_core::Error),
    /// Graph rejected the access token itself (HTTP 401), for example
    /// because it expired before the check. That says nothing about the
    /// user, so retry with a fresh access token from the refresh token.
    Rejected(mett_core::Error),
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
    ///
    /// The stored token is removed first: if that fails, the cached tokens
    /// are kept too, so that the state stays consistent and sign-out can be
    /// retried.
    pub async fn sign_out(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        let (store, account) = (Arc::clone(&self.store), self.settings.account.clone());
        blocking(move || store.clear(&account)).await?;
        *state = State {
            loaded: true,
            ..State::default()
        };
        Ok(())
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

    /// Checks that tokens were issued to the configured account.
    ///
    /// `login_hint` is only a hint, and the device code flow has none, so the
    /// user can sign in as someone else. Refreshed tokens are checked too: if
    /// the user's UPN changed and the old one now belongs to someone else, a
    /// stored refresh token would otherwise show the original user's mailbox
    /// under this account's name.
    async fn check_identity(&self, tokens: &Tokens) -> Identity {
        let expected = self.settings.account.as_str();
        match oauth::user_principal_name(&self.http, &self.settings, &tokens.access).await {
            Ok(user) if user.eq_ignore_ascii_case(expected) => Identity::Confirmed,
            Ok(user) => Identity::Mismatch(mett_core::Error::Auth(format!(
                "signed in as {user}, but the account is {expected}"
            ))),
            Err(e @ FlowError::OAuth { .. }) => Identity::Rejected(mett_core::Error::Auth(
                format!("could not confirm that the sign-in was for {expected}: {e}"),
            )),
            Err(e) => {
                let message = format!("could not confirm that the sign-in was for {expected}: {e}");
                // Keep the transient/permanent distinction, so that a Graph
                // outage is retried rather than treated as an auth failure.
                Identity::Unknown(if e.is_transient() {
                    mett_core::Error::Network(message)
                } else {
                    mett_core::Error::Auth(message)
                })
            }
        }
    }

    /// Accepts `tokens` once their user is confirmed, or keeps them as
    /// unconfirmed if that is not possible yet.
    ///
    /// `stored` is the stored refresh token the tokens were obtained with, if
    /// any; it is removed if it turns out to belong to another user.
    async fn confirm(
        &self,
        state: &mut State,
        tokens: Tokens,
        stored: Option<&str>,
    ) -> Result<AccessToken> {
        match self.check_identity(&tokens).await {
            Identity::Confirmed => {
                let token = self.keep(state, tokens).await?;
                // The check may have outlasted a short-lived token. The
                // refresh token is kept, so the next call refreshes it.
                if token.expires_within(TimeDelta::zero(), Utc::now()) {
                    return Err(mett_core::Error::Network(
                        "the access token expired while confirming the user; retry".into(),
                    ));
                }
                Ok(token)
            }
            Identity::Mismatch(e) => {
                if let Some(stored) = stored {
                    state.access = None;
                    // Another mett instance may have stored a newer token in
                    // the meantime: keep that one for the next attempt.
                    state.refresh = self.clear_if_stored(stored).await?;
                }
                Err(e)
            }
            Identity::Unknown(e) => {
                state.unconfirmed = Some(Unconfirmed {
                    tokens,
                    stored: stored.map(str::to_owned),
                });
                Err(e)
            }
            // Retrying with the rejected access token can't help. Keep the
            // refresh token, with the access token marked as expired, so
            // the next call gets a new access token and checks again.
            // Without a refresh token, the next call signs in afresh.
            Identity::Rejected(e) => {
                if tokens.refresh.is_some() {
                    let expired = AccessToken::new(tokens.access.secret(), DateTime::UNIX_EPOCH);
                    state.unconfirmed = Some(Unconfirmed {
                        tokens: Tokens {
                            access: expired,
                            ..tokens
                        },
                        stored: stored.map(str::to_owned),
                    });
                }
                Err(e)
            }
        }
    }

    /// Signs in interactively, unless the user cancels, and tells the
    /// prompter how it ended.
    ///
    /// Cancelling also interrupts confirming the user and storing the
    /// tokens, so the sign-in can't go on to succeed after the user gave
    /// up on it.
    async fn sign_in_and_confirm(&self, state: &mut State) -> Result<AccessToken> {
        let signed_in = tokio::select! {
            result = async {
                let tokens = self.sign_in().await?;
                self.confirm(state, tokens, None).await
            } => Some(result),
            () = self.prompter.cancelled() => None,
        };
        let result = signed_in.unwrap_or_else(|| {
            // Interactive sign-in only starts without a usable token, so
            // there is nothing to keep but what the interrupted sign-in
            // may have left half done. Loading from the store again picks
            // up a refresh token it had already stored.
            *state = State::default();
            Err(mett_core::Error::Auth("sign-in cancelled".into()))
        });
        self.prompter.finished(result.as_ref().map(|_| ()));
        result
    }

    /// Removes `token` from the store if it is still the stored one, and
    /// returns the stored token otherwise.
    async fn clear_if_stored(&self, token: &str) -> Result<Option<String>> {
        let (store, account) = (Arc::clone(&self.store), self.settings.account.clone());
        let token = token.to_owned();
        blocking(move || store.clear_if(&account, &token)).await
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
            state.refresh = Some(refresh);
            state.unsaved = true;
        }
        self.save_refresh_token(state).await?;
        Ok(tokens.access)
    }

    /// Stores the in-memory refresh token if it hasn't been stored yet.
    async fn save_refresh_token(&self, state: &mut State) -> Result<()> {
        let Some(refresh) = state.refresh.clone().filter(|_| state.unsaved) else {
            return Ok(());
        };
        let (store, account) = (Arc::clone(&self.store), self.settings.account.clone());
        blocking(move || store.save(&account, &refresh)).await?;
        state.unsaved = false;
        Ok(())
    }
}

impl<S: TokenStore, P: Prompter> Authenticator for GraphAuthenticator<S, P> {
    /// Forgets the cached access token if it is `token`, so the next call
    /// refreshes it.
    async fn token_rejected(&self, token: &AccessToken) {
        let mut state = self.state.lock().await;
        if state.access.as_ref() == Some(token) {
            state.access = None;
        }
    }

    async fn access_token(&self) -> Result<AccessToken> {
        let mut state = self.state.lock().await;

        let now = Utc::now();
        if let Some(token) = state.access.clone()
            && state.usable(&token, now)
        {
            // A previous attempt to store the refresh token failed: retry,
            // but don't fail a caller who has a valid token. The failure
            // was reported when it first happened.
            if state.unsaved {
                let _ = self.save_refresh_token(&mut state).await;
            }
            return Ok(token);
        }

        // Retry confirming a previous sign-in before asking for a new one.
        // Breaking out of this block continues with the normal refresh path.
        'unconfirmed: {
            let Some(Unconfirmed { mut tokens, stored }) = state.unconfirmed.take() else {
                break 'unconfirmed;
            };
            if tokens.access.expires_within(TimeDelta::zero(), now) {
                // The access token expired before the user could be
                // confirmed: get a fresh one with the sign-in's refresh token
                // and confirm with that.
                let Some(refresh) = tokens.refresh.clone() else {
                    break 'unconfirmed;
                };
                match oauth::refresh(&self.http, &self.settings, &refresh).await {
                    Ok(fresh) => {
                        tokens = Tokens {
                            refresh: fresh.refresh.or(Some(refresh)),
                            ..fresh
                        };
                    }
                    // Rejected: remove it if it is the stored token, and let
                    // the normal path try any replacement another mett
                    // instance has stored before prompting.
                    Err(e) if e.needs_sign_in() => {
                        state.refresh = self.clear_if_stored(&refresh).await?;
                        state.loaded = true;
                        break 'unconfirmed;
                    }
                    Err(e) => {
                        state.unconfirmed = Some(Unconfirmed { tokens, stored });
                        return Err(e.into());
                    }
                }
            }
            let token = match self.confirm(&mut state, tokens, stored.as_deref()).await {
                Ok(token) => token,
                Err(e) => return state.unexpired_access().ok_or(e),
            };
            // The check may have taken a while; if the token is no longer
            // usable, continue with the confirmed refresh token below.
            if state.usable(&token, Utc::now()) {
                return Ok(token);
            }
        }

        if !state.loaded {
            let (store, account) = (Arc::clone(&self.store), self.settings.account.clone());
            state.refresh = blocking(move || store.load(&account)).await?;
            state.loaded = true;
        }

        // Two attempts: if the refresh token is rejected but another mett
        // instance has stored a newer one in the meantime, try that once.
        let mut attempts = 0;
        while let Some(refresh) = state.refresh.clone() {
            attempts += 1;
            match oauth::refresh(&self.http, &self.settings, &refresh).await {
                Ok(tokens) => {
                    let tokens = Tokens {
                        refresh: tokens.refresh.or_else(|| Some(refresh.clone())),
                        ..tokens
                    };
                    // If the user can't be confirmed right now, keep using
                    // the cached token while it is valid, as for a failed
                    // refresh.
                    return match self.confirm(&mut state, tokens, Some(&refresh)).await {
                        Ok(token) => Ok(token),
                        Err(e) => state.unexpired_access().ok_or(e),
                    };
                }
                // The refresh token was revoked, expired or needs new
                // consent. Remove it, unless it has been replaced, and fall
                // back to an interactive sign-in.
                Err(e) if e.needs_sign_in() => {
                    let newer = self.clear_if_stored(&refresh).await?;
                    state.refresh = newer.filter(|_| attempts < 2);
                }
                // A transient failure: keep the refresh token, and use the
                // cached access token while it is still valid.
                // (Checked against the time after the refresh attempt, which
                // may have taken a while.)
                Err(e) => return state.unexpired_access().ok_or_else(|| e.into()),
            }
        }

        self.sign_in_and_confirm(&mut state).await
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
