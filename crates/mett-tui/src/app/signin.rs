//! The sign-in window, shown over the current screen.

use super::*;

/// The sign-in window, shown over the current screen while the user has
/// to sign in, or after a sign-in failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignIn {
    /// The account being signed in to.
    pub account: AccountId,
    /// What the user has to do, once known.
    pub prompt: Option<SignInPrompt>,
    /// Why the sign-in failed, once it has.
    pub failure: Option<String>,
    /// How far the window's text is scrolled, for terminals too small to
    /// show it all.
    pub scroll: usize,
}

/// What a row of the sign-in window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// Text.
    Text,
    /// The device code, highlighted.
    Code,
    /// Part of the sign-in link.
    Link,
}

impl SignIn {
    /// The window's text wrapped to `width`, with what each row shows. The
    /// keys are on the help line.
    pub fn rows(&self, width: usize) -> Vec<(String, Row)> {
        let mut lines: Vec<(String, Row)> = Vec::new();
        // After a failure, the link or code is spent: don't offer it.
        let prompt = self.prompt.as_ref().filter(|_| self.failure.is_none());
        match prompt {
            Some(SignInPrompt::OpenBrowser { url }) => {
                lines.push((
                    "Sign in in your browser. If it did not open, visit:".into(),
                    Row::Text,
                ));
                lines.push((String::new(), Row::Text));
                lines.push((url.to_string(), Row::Link));
            }
            Some(SignInPrompt::EnterCode {
                verification_uri,
                user_code,
                ..
            }) => {
                lines.push((
                    format!(
                        "To sign in, visit {verification_uri} on any device and enter the code:"
                    ),
                    Row::Text,
                ));
                lines.push((String::new(), Row::Text));
                lines.push((format!("    {user_code}"), Row::Code));
            }
            None => {}
        }
        if !lines.is_empty() {
            lines.push((String::new(), Row::Text));
        }
        lines.push(match &self.failure {
            Some(failure) => (format!("Sign-in failed: {failure}"), Row::Text),
            None => ("Waiting for you to sign in...".into(), Row::Text),
        });
        lines
            .into_iter()
            .flat_map(|(line, kind)| {
                text::wrap(&line, width)
                    .into_iter()
                    .map(move |row| (row, kind))
            })
            .collect()
    }

    /// What `c` copies: the sign-in link, or the device code.
    pub(super) fn copyable(&self) -> Option<String> {
        if self.failure.is_some() {
            return None;
        }
        match self.prompt.as_ref()? {
            SignInPrompt::OpenBrowser { url } => Some(url.to_string()),
            SignInPrompt::EnterCode { user_code, .. } => Some(user_code.clone()),
        }
    }
}
