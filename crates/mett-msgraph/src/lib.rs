//! Microsoft Graph adapter for mett.
//!
//! This crate implements `mett-core`'s ports against Microsoft Graph:
//!
//! - [`GraphAuthenticator`] implements [`mett_core::Authenticator`] with the
//!   OAuth2 flows described in `docs/CONFIG-OFFICE365.md` §4.
//! - [`GraphMailStore`] implements [`mett_core::MailStore`] on the signed-in
//!   user's mailbox.

mod authenticator;
mod invitation;
mod loopback;
mod mailbox;
mod oauth;
mod pkce;
mod prompt;
mod settings;
mod token_store;

pub use authenticator::GraphAuthenticator;
pub use mailbox::{GraphMailStore, needs_server_draft};
pub use prompt::{Cancelled, Prompter, SignInPrompt};
pub use settings::{AuthFlow, AuthSettings, Cloud, DEFAULT_SCOPES, IdentityCheck, UnknownCloud};
pub use token_store::{FileStore, KeyringStore, MemoryStore, TokenStore, TokenStoreError};
