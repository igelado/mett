//! Messages being written: what mett sends or saves as a draft.

use crate::ids::MessageId;
use crate::message::Address;

/// What a draft answers or passes on, if anything. Backends use it to
/// thread a reply under the original, and to keep a forwarded message's
/// attachments.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DraftOrigin {
    /// A reply to this message.
    Reply(MessageId),
    /// A forward of this message.
    Forward(MessageId),
}

/// A message to send, or to save as a draft.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    /// The sender, if it should differ from the signed-in user.
    pub from: Option<Address>,
    /// The `To:` recipients.
    pub to: Vec<Address>,
    /// The `Cc:` recipients.
    pub cc: Vec<Address>,
    /// The `Bcc:` recipients.
    pub bcc: Vec<Address>,
    /// The subject line.
    pub subject: String,
    /// The body, as plain text.
    pub body: String,
    /// The message this one replies to or forwards.
    pub origin: Option<DraftOrigin>,
    /// Whether to keep a copy in the sent-items folder (mutt's `copy`).
    pub save_copy: bool,
    /// Tells this message apart from others answering the same message:
    /// the same for every attempt to send or postpone one composed
    /// message, different for each message composed. A backend can then
    /// reuse what an earlier attempt left behind.
    pub attempt: u64,
}

impl Draft {
    /// Whether the draft has at least one recipient.
    pub fn has_recipients(&self) -> bool {
        !(self.to.is_empty() && self.cc.is_empty() && self.bcc.is_empty())
    }
}
