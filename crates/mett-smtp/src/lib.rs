//! SMTP adapter for mett.
//!
//! This crate sends mail over SMTP, wrapping the [`lettre`] crate:
//!
//! - [`SmtpSender`] sends a [`mett_core::Draft`]. It connects in
//!   plain text, with STARTTLS or with TLS from the first byte
//!   ([`Security`]), and logs in with a password or, through an
//!   [`mett_core::Authenticator`], with an OAuth2 token (SASL XOAUTH2)
//!   ([`Login`]).
//! - [`compose`] writes a [`mett_core::Draft`] as the MIME message that is
//!   sent, and the copy to keep (which, unlike what goes out, shows the
//!   `Bcc:` recipients). A mailbox that keeps its own sent and draft
//!   messages (IMAP) stores these.
//!
//! Replies are threaded by headers: the caller, which can look the
//! original up, gives its `Message-ID:` and `References:` as
//! [`Threading`].

mod compose;
mod sender;

pub use compose::{Composed, Threading, compose};
pub use mett_core::{Login, NoTokens, Security};
pub use sender::{SmtpSender, SmtpSettings};
