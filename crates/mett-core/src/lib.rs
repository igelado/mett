//! Domain model and ports for mett.
//!
//! This crate is the centre of mett's hexagonal architecture (see
//! `docs/DESIGN.md`). It defines the types the rest of the application works
//! with and the traits (ports) that adapters such as `mett-msgraph`
//! implement. It performs no I/O and depends on no adapter.

mod attachment;
mod draft;
mod error;
mod folder;
mod ids;
mod join;
mod message;
mod ports;

#[cfg(any(test, feature = "mock"))]
pub mod mock;

pub use attachment::{Attachment, AttachmentKind, OutgoingAttachment, content_type_for};
pub use draft::{Draft, DraftOrigin};
pub use error::{Error, Result};
pub use folder::{Folder, UnknownWellKnownFolder, WellKnownFolder};
pub use ids::{AccountId, AttachmentId, FolderId, MessageId, PageCursor};
pub use join::{join_bounded, join_bounded_until};
pub use message::{
    Address, AddressError, Body, BodyKind, Flags, FlagsUpdate, Message, MessageSummary,
    ThreadPosition,
};
pub use ports::{
    AccessToken, Authenticator, MailSender, MailStore, MessagePage, Page, RECENT_UNREAD,
};
