//! How to reach a mail server and log in, as IMAP and SMTP share it.

use std::fmt;
use std::time::Duration;

use crate::{AccessToken, Authenticator, Error, Result};

/// How the connection to a server is protected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Security {
    /// None: everything, passwords included, goes in plain text.
    None,
    /// The connection starts in plain text and is upgraded with STARTTLS
    /// if the server offers it (mutt's `ssl_starttls` without
    /// `ssl_force_tls`); one that doesn't is used in plain text.
    StartTlsIfOffered,
    /// The connection starts in plain text and is upgraded with STARTTLS
    /// (and fails if the server won't).
    #[default]
    StartTls,
    /// TLS from the first byte (the `imaps` and `smtps` ports).
    Tls,
}

/// Where a server is and how to trust it: what IMAP and SMTP settings share.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerSettings {
    /// The server's name.
    pub host: String,
    /// Its port.
    pub port: u16,
    /// How the connection is protected.
    pub security: Security,
    /// Whether to check that the server's certificate is for the name the
    /// connection is made to (mutt's `ssl_verify_host`). The certificate is
    /// always checked against the authorities; turning this off leaves the
    /// connection open to anyone with a certificate from one of them.
    pub verify_host: bool,
    /// Certificate authorities to trust besides the system's, as PEM.
    pub ca_certificates: Vec<Vec<u8>>,
    /// How long to wait for the server at each step.
    pub timeout: Duration,
}

impl ServerSettings {
    /// Settings for `host` and `port`, with the checks on and a minute to
    /// wait at each step.
    pub fn new(host: impl Into<String>, port: u16, security: Security) -> Self {
        Self {
            host: host.into(),
            port,
            security,
            verify_host: true,
            ca_certificates: Vec::new(),
            timeout: Duration::from_secs(60),
        }
    }
}

/// How to log in.
pub enum Login<A = NoTokens> {
    /// With a password (IMAP `LOGIN`, SASL PLAIN or LOGIN).
    Password {
        /// The user name.
        user: String,
        /// The password.
        password: String,
    },
    /// With an OAuth2 access token (SASL XOAUTH2).
    OAuth {
        /// The user name.
        user: String,
        /// The source of tokens.
        tokens: A,
    },
}

impl<A> Login<A> {
    /// The user name.
    pub fn user(&self) -> &str {
        match self {
            Self::Password { user, .. } | Self::OAuth { user, .. } => user,
        }
    }
}

impl<A> fmt::Debug for Login<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Password { user, .. } => f
                .debug_struct("Password")
                .field("user", user)
                .field("password", &"<redacted>")
                .finish(),
            Self::OAuth { user, .. } => f.debug_struct("OAuth").field("user", user).finish(),
        }
    }
}

/// The tokens of a [`Login`] that uses none.
#[derive(Clone, Copy, Debug)]
pub struct NoTokens;

impl Authenticator for NoTokens {
    async fn access_token(&self) -> Result<AccessToken> {
        Err(Error::Auth("there is no OAuth2 sign-in".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_the_password() {
        let login: Login = Login::Password {
            user: "me".into(),
            password: "hunter2".into(),
        };
        let shown = format!("{login:?}");
        assert!(
            shown.contains("me") && !shown.contains("hunter2"),
            "{shown}"
        );
        assert_eq!(login.user(), "me");
    }

    #[tokio::test]
    async fn no_tokens_has_no_sign_in() {
        assert!(matches!(NoTokens.access_token().await, Err(Error::Auth(_))));
    }
}
