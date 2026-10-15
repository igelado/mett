//! Interactive sign-in inside the TUI.
//!
//! The authenticator runs in the worker; [`TuiPrompter`] forwards its
//! prompts to the main loop as [`SignInEvent`]s, which the app shows in a
//! sign-in window. The window can cancel the sign-in through a
//! [`CancelHandle`].

use std::sync::Arc;

use mett_core::AccountId;
use mett_msgraph::{Cancelled, Prompter, SignInPrompt};
use tokio::sync::{mpsc, watch};

/// What the authenticator reports about an interactive sign-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInEvent {
    /// The user has to act to sign in to `account`.
    Prompt {
        /// The account being signed in to.
        account: AccountId,
        /// What to do.
        prompt: SignInPrompt,
    },
    /// The sign-in to `account` ended.
    Finished {
        /// The account.
        account: AccountId,
        /// Success, or why it failed.
        outcome: Result<(), String>,
    },
}

/// A [`Prompter`] that sends prompts to the main loop.
pub struct TuiPrompter {
    account: AccountId,
    events: mpsc::UnboundedSender<SignInEvent>,
    /// Counts cancellations; a sign-in is cancelled when it changes.
    cancel: Arc<watch::Sender<u64>>,
}

/// Cancels the sign-in in progress.
#[derive(Clone)]
pub struct CancelHandle(Arc<watch::Sender<u64>>);

impl CancelHandle {
    /// Cancels the sign-in in progress, if any. Sign-ins that start later
    /// are not affected.
    pub fn cancel(&self) {
        self.0.send_modify(|count| *count = count.wrapping_add(1));
    }
}

impl TuiPrompter {
    /// A prompter for `account` that sends its events to `events`, and the
    /// handle that cancels its sign-ins.
    pub fn new(
        account: AccountId,
        events: mpsc::UnboundedSender<SignInEvent>,
    ) -> (Self, CancelHandle) {
        let cancel = Arc::new(watch::Sender::new(0));
        let handle = CancelHandle(Arc::clone(&cancel));
        (
            Self {
                account,
                events,
                cancel,
            },
            handle,
        )
    }

    fn send(&self, event: SignInEvent) {
        // The UI may have quit; nothing to do then.
        let _ = self.events.send(event);
    }
}

impl Prompter for TuiPrompter {
    fn prompt(&self, prompt: SignInPrompt) {
        self.send(SignInEvent::Prompt {
            account: self.account.clone(),
            prompt,
        });
    }

    fn finished(&self, outcome: Result<(), &mett_core::Error>) {
        // The window says that signing in failed; don't say it twice.
        let outcome = outcome.map_err(|e| match e {
            mett_core::Error::Auth(message) => message.clone(),
            e => e.to_string(),
        });
        self.send(SignInEvent::Finished {
            account: self.account.clone(),
            outcome,
        });
    }

    fn cancelled(&self) -> Cancelled<'_> {
        // Subscribing marks the current count as seen, so only later
        // cancellations count.
        let mut count = self.cancel.subscribe();
        Box::pin(async move {
            if count.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn cancels_only_sign_ins_in_progress() {
        let (events, mut received) = mpsc::unbounded_channel();
        let (prompter, handle) = TuiPrompter::new(AccountId::new("a@b.c"), events);

        // A cancellation before the sign-in started doesn't affect it.
        handle.cancel();
        let cancelled = prompter.cancelled();
        let early = tokio::time::timeout(Duration::from_millis(50), prompter.cancelled()).await;
        assert!(early.is_err(), "not cancelled yet");

        handle.cancel();
        tokio::time::timeout(Duration::from_secs(5), cancelled)
            .await
            .expect("cancelled");

        prompter.finished(Err(&mett_core::Error::Auth("no".into())));
        assert_eq!(
            received.recv().await,
            Some(SignInEvent::Finished {
                account: AccountId::new("a@b.c"),
                outcome: Err("no".into()),
            })
        );
    }
}
