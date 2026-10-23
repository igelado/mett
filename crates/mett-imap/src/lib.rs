//! IMAP adapter for mett.
//!
//! This crate implements `mett-core`'s ports against an IMAP server,
//! wrapping the [`async_imap`] crate (TLS by rustls), and sends mail
//! through [`mett_smtp`]:
//!
//! - [`ImapMailStore`] implements [`mett_core::MailStore`] and, given an
//!   SMTP server ([`ImapMailStore::with_smtp`]), [`mett_core::MailSender`].
//!   It connects in plain text, with STARTTLS or with TLS from the first
//!   byte ([`Security`]), and logs in with a password or an OAuth2 token
//!   (SASL XOAUTH2) ([`Login`]).
//!
//! A folder is called by its name on the server. A message is called by its
//! folder, the folder's `UIDVALIDITY` and its `UID`, so a message that
//! moves has a new identifier, as with Microsoft Graph.
//!
//! IMAP can't tell what changed in a folder as Exchange's APIs can, so
//! this adapter doesn't sync (mett doesn't cache it) or watch folders:
//! mett asks again.

mod conn;
mod ids;
mod parse;
mod settings;
mod store;
mod tls;

pub use settings::{ImapSettings, Login, NoTokens, Security};
pub use store::{ImapMailStore, account};
