//! How interactive sign-in talks to the user.

use url::Url;

/// What the user has to do to finish signing in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInPrompt {
    /// Open `url` in a browser and sign in (authorization code flow). The
    /// browser then redirects back to mett.
    OpenBrowser {
        /// The sign-in page.
        url: Url,
    },
    /// Go to `verification_uri` on any device and enter `user_code` (device
    /// code flow).
    EnterCode {
        /// Where to enter the code.
        verification_uri: String,
        /// The code to enter.
        user_code: String,
        /// Microsoft's ready-made instruction for the user.
        message: String,
    },
}

/// Shows sign-in prompts to the user.
///
/// The TUI implements this by showing the prompt on screen; a closure
/// `Fn(SignInPrompt)` works too.
pub trait Prompter: Send + Sync {
    /// Shows `prompt`. Must not block: sign-in continues in the background.
    fn prompt(&self, prompt: SignInPrompt);
}

impl<F> Prompter for F
where
    F: Fn(SignInPrompt) + Send + Sync,
{
    fn prompt(&self, prompt: SignInPrompt) {
        self(prompt)
    }
}
