//! The message line at the bottom of the screen: its text, and what kind of
//! message that is, so that the code never has to recognize a message by
//! its words.

use std::fmt;
use std::ops::Deref;

/// What a message on the message line is about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatusKind {
    /// Anything else: the result of a command, a notice.
    #[default]
    Info,
    /// A failure, shown in the `error` color.
    Error,
    /// A folder (or more of one) is being loaded.
    Loading,
    /// A search is running.
    Searching,
    /// The user cancelled a sign-in.
    Cancelled,
    /// A search found nothing.
    NoMatches,
    /// An attachment is being fetched.
    Fetching,
    /// Files are being attached to a message.
    Attaching,
    /// A postponed message is being looked for or fetched.
    Recalling,
}

/// The message line: a text and the [`StatusKind`] of message it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    text: String,
    kind: StatusKind,
}

impl Status {
    /// A line that says `text`, a plain message.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Info,
        }
    }

    /// Says `text`, a message of `kind`.
    pub fn say(&mut self, text: impl Into<String>, kind: StatusKind) {
        self.text = text.into();
        self.kind = kind;
    }

    /// Says `text`, a plain message.
    pub fn info(&mut self, text: impl Into<String>) {
        self.say(text, StatusKind::Info);
    }

    /// The text on the line.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the line shows an error.
    pub fn is_error(&self) -> bool {
        self.is(StatusKind::Error)
    }

    /// Whether the line shows a message of `kind`.
    pub fn is(&self, kind: StatusKind) -> bool {
        !self.text.is_empty() && self.kind == kind
    }

    /// Empties the line.
    pub fn clear(&mut self) {
        self.text.clear();
        self.kind = StatusKind::Info;
    }

    /// Empties the line if it shows a message of `kind`, which no longer
    /// holds.
    pub fn clear_if(&mut self, kind: StatusKind) {
        if self.is(kind) {
            self.clear();
        }
    }
}

impl Deref for Status {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl PartialEq<str> for Status {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

impl PartialEq<&str> for Status {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kind_is_cleared_only_when_it_is_the_lines() {
        let mut status = Status::default();
        status.say("Searching...", StatusKind::Searching);
        status.clear_if(StatusKind::Loading);
        assert_eq!(status, "Searching...");
        status.clear_if(StatusKind::Searching);
        assert!(status.is_empty());
        status.say("Failed", StatusKind::Error);
        assert!(status.is_error());
        status.info("Failed");
        assert!(!status.is_error(), "the words don't make an error");
        status.clear();
        assert!(!status.is(StatusKind::Info), "an empty line says nothing");
    }
}
