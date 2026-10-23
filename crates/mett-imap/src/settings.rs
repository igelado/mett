//! How to reach an IMAP server and log in.

use std::fmt;
use std::time::Duration;

pub use mett_smtp::{NoTokens, Security};

/// How to reach an IMAP server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImapSettings {
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

impl ImapSettings {
    /// Settings for `host`, with the port that goes with `security`
    /// (143, or 993 for TLS from the first byte) and the checks on.
    pub fn new(host: impl Into<String>, security: Security) -> Self {
        Self {
            host: host.into(),
            port: match security {
                Security::Tls => 993,
                _ => 143,
            },
            security,
            verify_host: true,
            ca_certificates: Vec::new(),
            timeout: Duration::from_secs(60),
        }
    }
}

/// How to log in.
pub enum Login<A = NoTokens> {
    /// With a password (`LOGIN`).
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
