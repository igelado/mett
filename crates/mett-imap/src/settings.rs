//! How to reach an IMAP server.

use mett_core::{Security, ServerSettings};

/// How to reach an IMAP server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImapSettings {
    /// The server, and how to trust it.
    pub server: ServerSettings,
}

impl ImapSettings {
    /// Settings for `host`, with the port that goes with `security`
    /// (143, or 993 for TLS from the first byte) and the checks on.
    pub fn new(host: impl Into<String>, security: Security) -> Self {
        let port = match security {
            Security::Tls => 993,
            _ => 143,
        };
        Self {
            server: ServerSettings::new(host, port, security),
        }
    }
}
