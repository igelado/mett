//! Exchange Web Services adapter for mett.
//!
//! This crate implements `mett-core`'s ports against Exchange Web Services
//! (EWS), the SOAP API of Exchange Online and Exchange Server:
//!
//! - [`EwsMailStore`] implements [`mett_core::MailStore`] and
//!   [`mett_core::MailSender`] on the signed-in user's mailbox.
//!
//! It takes any [`mett_core::Authenticator`] for its OAuth2 tokens: with
//! Exchange Online, `mett-msgraph`'s, asking for the `EWS.AccessAsUser.All`
//! scope (see `docs/CONFIG-OFFICE365.md`).

mod invitation;
mod mailbox;
mod soap;
mod stream;

pub use mailbox::{EwsMailStore, needs_server_draft};
