//! How interactive sign-in talks to the user.

use std::future::Future;
use std::pin::Pin;

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

/// A future that resolves when the user cancels a sign-in.
pub type Cancelled<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Shows sign-in prompts to the user.
///
/// The TUI implements this with a sign-in window; a closure
/// `Fn(SignInPrompt)` works too, and can't be cancelled.
pub trait Prompter: Send + Sync {
    /// Shows `prompt`. Must not block: sign-in continues in the background.
    fn prompt(&self, prompt: SignInPrompt);

    /// Called when an interactive sign-in ends, with its outcome, so that
    /// the prompt can be taken down. A sign-in can fail before any prompt
    /// was shown, for example if the identity platform rejects the app.
    fn finished(&self, outcome: Result<(), &mett_core::Error>) {
        let _ = outcome;
    }

    /// Resolves when the user cancels the interactive sign-in in progress.
    /// It is created when that sign-in starts, so cancelling an earlier
    /// sign-in must not resolve it. The default never resolves.
    fn cancelled(&self) -> Cancelled<'_> {
        Box::pin(std::future::pending())
    }
}

impl<F> Prompter for F
where
    F: Fn(SignInPrompt) + Send + Sync,
{
    fn prompt(&self, prompt: SignInPrompt) {
        self(prompt)
    }
}
