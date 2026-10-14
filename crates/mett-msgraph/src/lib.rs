//! Microsoft Graph adapter for mett.
//!
//! This crate implements `mett-core`'s ports against Microsoft Graph. It
//! currently provides sign-in: [`GraphAuthenticator`] implements the
//! [`mett_core::Authenticator`] port with the OAuth2 flows described in
//! `docs/CONFIG-OFFICE365.md` §4.

mod authenticator;
mod loopback;
mod oauth;
mod pkce;
mod prompt;
mod settings;
mod token_store;

pub use authenticator::GraphAuthenticator;
pub use prompt::{Prompter, SignInPrompt};
pub use settings::{AuthFlow, AuthSettings, Cloud, DEFAULT_SCOPES, UnknownCloud};
pub use token_store::{FileStore, KeyringStore, MemoryStore, TokenStore, TokenStoreError};
