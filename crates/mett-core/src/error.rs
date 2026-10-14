//! Errors returned through the ports.

use std::time::Duration;

/// Result type used throughout `mett-core` and its ports.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors a port can report.
///
/// Adapters map their own failures onto these variants so that the UI can
/// react to them (for example, re-authenticate on [`Error::Auth`]) without
/// knowing which backend produced them.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The requested object does not exist.
    #[error("{kind} not found: {id}")]
    NotFound {
        /// What was looked up, for example `"message"`.
        kind: &'static str,
        /// The identifier that was not found.
        id: String,
    },

    /// Authentication failed or is required again.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// The backend asked the client to slow down.
    #[error("throttled by the server")]
    Throttled {
        /// How long the server asked the client to wait, if it said.
        retry_after: Option<Duration>,
    },

    /// The backend could not be reached.
    #[error("network error: {0}")]
    Network(String),

    /// The backend returned an error not covered by another variant.
    #[error("backend error: {0}")]
    Backend(String),
}

impl Error {
    /// Shorthand for [`Error::NotFound`].
    pub fn not_found(kind: &'static str, id: impl ToString) -> Self {
        Self::NotFound {
            kind,
            id: id.to_string(),
        }
    }
}
