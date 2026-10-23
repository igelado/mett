//! Domain model and ports for mett.
//!
//! This crate is the centre of mett's hexagonal architecture (see
//! `docs/DESIGN.md`). It defines the types the rest of the application works
//! with and the traits (ports) that adapters such as `mett-msgraph`
//! implement. It performs no I/O and depends on no adapter.

mod attachment;
mod draft;
mod draft_session;
mod error;
mod folder;
mod http_policy;
mod ids;
mod invitation;
mod join;
mod mailbox_wire;
mod meeting_time;
mod message;
mod ports;
mod server;

#[cfg(any(test, feature = "mock"))]
pub mod mock;

pub use attachment::{Attachment, AttachmentKind, OutgoingAttachment, content_type_for};
pub use draft::{Draft, DraftOrigin};
pub use draft_session::{DraftBackend, DraftSession, is_recalled, may_be_left, unsent};
pub use error::{Error, Result};
pub use folder::{Folder, UnknownWellKnownFolder, WellKnownFolder};
pub use http_policy::{error_for_status, retry_after, with_fresh_token};
pub use ids::{AccountId, AttachmentId, FolderId, MessageId, PageCursor, SyncState};
pub use invitation::{
    Answer, Invitation, InvitationKind, Recurrence, RecurrenceUnit, Reply, Rsvp, Scope,
};
pub use join::{first_then, join_bounded, join_bounded_until};
pub use mailbox_wire::{MAX_PAGE_SIZE, ROOT, Roles, nameable, parse_time, thread_path};
pub use meeting_time::{nth_of, occurrence_window, whole_day};
pub use message::{
    Address, AddressError, Body, BodyKind, Flags, FlagsUpdate, Message, MessageSummary,
    ThreadPosition,
};
pub use ports::{
    AccessToken, Authenticator, FolderChanges, MailSender, MailStore, MessagePage, Page,
    RECENT_UNREAD,
};
pub use server::{Login, NoTokens, Security, ServerSettings};
