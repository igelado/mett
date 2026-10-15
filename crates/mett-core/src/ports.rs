//! Ports: the traits that adapters implement.
//!
//! The methods return `impl Future + Send` rather than using `async fn`
//! directly. This lets implementations write plain `async fn`s, while
//! callers can still move the futures onto a multi-threaded runtime.

use std::fmt;
use std::future::Future;

use chrono::{DateTime, TimeDelta, Utc};

use crate::attachment::Attachment;
use crate::draft::Draft;
use crate::error::Result;
use crate::folder::{Folder, WellKnownFolder};
use crate::ids::{AttachmentId, FolderId, MessageId, PageCursor};
use crate::message::{Address, FlagsUpdate, Message, MessageSummary};

/// How many unread messages [`MailStore::recent_unread`] returns at most.
pub const RECENT_UNREAD: usize = 250;

/// Which page of a listing to fetch.
///
/// A page always holds at least one item, so that following
/// [`MessagePage::next`] is guaranteed to make progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    size: u32,
    cursor: Option<PageCursor>,
}

impl Page {
    /// The first page, with up to `size` items (at least one).
    pub fn first(size: u32) -> Self {
        Self {
            size: size.max(1),
            cursor: None,
        }
    }

    /// The page that starts at `cursor`, with up to `size` items (at least
    /// one). A listing keeps the size it started with: backends that page
    /// with opaque continuation links (such as Microsoft Graph) may ignore
    /// a different `size` here.
    pub fn after(cursor: PageCursor, size: u32) -> Self {
        Self {
            size: size.max(1),
            cursor: Some(cursor),
        }
    }

    /// Maximum number of items to return. Never zero.
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Where to continue from, or `None` for the first page.
    pub fn cursor(&self) -> Option<&PageCursor> {
        self.cursor.as_ref()
    }
}

/// One page of message summaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessagePage {
    /// The messages, newest first.
    pub messages: Vec<MessageSummary>,
    /// The cursor for the next page, or `None` if this is the last one.
    pub next: Option<PageCursor>,
}

/// Access to a mailbox.
pub trait MailStore: Send + Sync {
    /// The mailbox owner's own addresses, as far as the backend knows
    /// them (say, aliases): mail from them is the user's. Backends that
    /// can't tell return none.
    fn own_addresses(&self) -> impl Future<Output = Result<Vec<String>>> + Send {
        std::future::ready(Ok(Vec::new()))
    }

    /// People whose name or address matches `text`: those the user works
    /// with, and the organisation's directory, as far as the backend can
    /// tell. Backends without either find no one.
    fn search_people(&self, text: &str) -> impl Future<Output = Result<Vec<Address>>> + Send {
        let _ = text;
        std::future::ready(Ok(Vec::new()))
    }

    /// Lists all folders in the mailbox.
    fn list_folders(&self) -> impl Future<Output = Result<Vec<Folder>>> + Send;

    /// Returns a folder by its identifier, with fresh counts: polling for
    /// new mail reads these.
    fn folder(&self, id: &FolderId) -> impl Future<Output = Result<Folder>> + Send;

    /// The unread messages of a folder that arrived or changed at or after
    /// `since` (up to [`RECENT_UNREAD`]; without `since`, the last ones),
    /// most recently first, with when: unread mail that arrives (or is
    /// moved in, or marked unread) after the user last saw the folder is
    /// new.
    fn recent_unread(
        &self,
        folder: &FolderId,
        since: Option<DateTime<Utc>>,
    ) -> impl Future<Output = Result<Vec<(MessageId, DateTime<Utc>)>>> + Send;

    /// Returns the folder with a well-known role, such as the Inbox.
    fn well_known_folder(
        &self,
        which: WellKnownFolder,
    ) -> impl Future<Output = Result<Folder>> + Send;

    /// Lists the messages in a folder, newest first.
    fn list_messages(
        &self,
        folder: &FolderId,
        page: Page,
    ) -> impl Future<Output = Result<MessagePage>> + Send;

    /// Lists the messages in a folder that changed or arrived at or after
    /// `since` (by the backend's clock), most recently changed first. New
    /// mail can arrive with an old date (delivered late, or moved in from
    /// another folder), so checking for it takes this as well as the
    /// newest messages.
    fn changed_messages(
        &self,
        folder: &FolderId,
        since: DateTime<Utc>,
        page: Page,
    ) -> impl Future<Output = Result<MessagePage>> + Send;

    /// Fetches a full message, including its body.
    fn fetch_message(&self, id: &MessageId) -> impl Future<Output = Result<Message>> + Send;

    /// Lists a message's attachments.
    fn list_attachments(
        &self,
        message: &MessageId,
    ) -> impl Future<Output = Result<Vec<Attachment>>> + Send;

    /// Fetches an attachment's content: a file's bytes, or an attached
    /// message as MIME.
    fn fetch_attachment(
        &self,
        message: &MessageId,
        attachment: &AttachmentId,
    ) -> impl Future<Output = Result<Vec<u8>>> + Send;

    /// Changes a message's flags.
    fn update_flags(
        &self,
        id: &MessageId,
        update: FlagsUpdate,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Moves a message to another folder and returns its new identifier.
    /// Backends such as Microsoft Graph assign a new ID on every move.
    fn move_message(
        &self,
        id: &MessageId,
        to: &FolderId,
    ) -> impl Future<Output = Result<MessageId>> + Send;

    /// Copies a message to another folder and returns the copy's
    /// identifier.
    fn copy_message(
        &self,
        id: &MessageId,
        to: &FolderId,
    ) -> impl Future<Output = Result<MessageId>> + Send;

    /// Deletes a message permanently. To move it to the trash instead, use
    /// [`MailStore::move_message`].
    fn delete_message(&self, id: &MessageId) -> impl Future<Output = Result<()>> + Send;

    /// Saves `draft` in the drafts folder (mutt's `postponed`) and returns
    /// the saved message's identifier.
    fn save_draft(&self, draft: &Draft) -> impl Future<Output = Result<MessageId>> + Send;
}

/// Sends mail.
pub trait MailSender: Send + Sync {
    /// Sends `draft`, keeping a copy in the sent-items folder if
    /// [`Draft::save_copy`] asks for one. (A backend may keep one
    /// regardless for some messages: Office365 always does for replies and
    /// forwards.)
    fn send(&self, draft: &Draft) -> impl Future<Output = Result<()>> + Send;

    /// Deletes what earlier attempts to send or save `draft` (the same
    /// [`Draft::attempt`]) left behind with the backend, now that the user
    /// has given up on it. Backends that leave nothing behind do nothing.
    fn discard_kept(&self, draft: &Draft) -> impl Future<Output = Result<()>> + Send {
        let _ = draft;
        std::future::ready(Ok(()))
    }

    /// Whether an attempt to send or save `draft` left something with the
    /// backend that the next attempt goes on from (with Office365, a
    /// server draft, which is sent with a copy in Sent Items).
    fn keeps_draft(&self, draft: &Draft) -> bool {
        let _ = draft;
        false
    }
}

/// An OAuth2 access token.
///
/// Its [`fmt::Debug`] output hides the secret, so tokens can't leak into
/// logs by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken {
    secret: String,
    expires_at: DateTime<Utc>,
}

impl AccessToken {
    /// Creates a token that expires at `expires_at`.
    pub fn new(secret: impl Into<String>, expires_at: DateTime<Utc>) -> Self {
        Self {
            secret: secret.into(),
            expires_at,
        }
    }

    /// The token itself, for use in an `Authorization: Bearer` header.
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// When the token expires.
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    /// Whether the token expires within `margin` of `now`.
    pub fn expires_within(&self, margin: TimeDelta, now: DateTime<Utc>) -> bool {
        self.expires_at - margin <= now
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessToken")
            .field("secret", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// A source of access tokens for one account.
pub trait Authenticator: Send + Sync {
    /// Returns a valid access token, refreshing or signing in again if
    /// needed.
    fn access_token(&self) -> impl Future<Output = Result<AccessToken>> + Send;

    /// Tells the authenticator that a service rejected `token` (for
    /// example, HTTP 401) before it expired, so the next
    /// [`Authenticator::access_token`] gets a new one instead of the same.
    fn token_rejected(&self, token: &AccessToken) -> impl Future<Output = ()> + Send {
        let _ = token;
        async {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_never_empty() {
        assert_eq!(Page::first(0).size(), 1);
        assert_eq!(Page::after(PageCursor::new("c"), 0).size(), 1);
        assert_eq!(Page::first(50).size(), 50);
    }

    #[test]
    fn access_token_debug_hides_secret() {
        let token = AccessToken::new("s3cret", DateTime::UNIX_EPOCH);
        let debug = format!("{token:?}");
        assert!(!debug.contains("s3cret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn access_token_expiry_margin() {
        let now = DateTime::UNIX_EPOCH + TimeDelta::hours(1);
        let token = AccessToken::new("t", now + TimeDelta::minutes(4));
        assert!(token.expires_within(TimeDelta::minutes(5), now));
        assert!(!token.expires_within(TimeDelta::minutes(3), now));
    }
}
