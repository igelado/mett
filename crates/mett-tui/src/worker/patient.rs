//! Trying throttled calls again after the wait the server asks for.

use std::time::Duration;

use chrono::{DateTime, Utc};
use mett_core::{
    Answer, Attachment, AttachmentId, Draft, Error, FlagsUpdate, Folder, FolderId, Invitation,
    MailSender, MailStore, Message, MessageId, MessagePage, Page, Result, WellKnownFolder,
};

/// How many times a call the server throttles is tried again before its
/// failure is the answer.
pub const THROTTLED_RETRIES: u32 = 5;

/// The longest wait for a throttled call, whatever the server asks for.
const LONGEST_WAIT: Duration = Duration::from_secs(5 * 60);

/// How long to wait before trying a throttled call again for the
/// `attempt`th time when the server doesn't say: twice as long each time,
/// from two seconds.
fn back_off(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt.clamp(1, 8))
}

/// A store whose calls the server throttles are tried again after the wait
/// it asks for (up to [`THROTTLED_RETRIES`] times), telling `notify` how
/// long each wait is and which attempt it precedes. Throttled calls were
/// not carried out, so trying them again is safe, sending mail included.
/// Each call is tried again on its own, so a request of many calls carries
/// on where it was throttled.
pub(super) struct Patient<'a, S> {
    pub(super) store: &'a S,
    pub(super) notify: &'a (dyn Fn(Duration, u32) + Sync),
}

impl<S> Patient<'_, S> {
    /// `call`'s outcome, trying it again while the server throttles it.
    async fn patiently<T, F: Future<Output = Result<T>>>(&self, call: impl Fn() -> F) -> Result<T> {
        let mut attempt = 0;
        loop {
            match call().await {
                Err(Error::Throttled { retry_after }) if attempt < THROTTLED_RETRIES => {
                    attempt += 1;
                    let wait = retry_after
                        .unwrap_or_else(|| back_off(attempt))
                        .min(LONGEST_WAIT);
                    (self.notify)(wait, attempt);
                    tokio::time::sleep(wait).await;
                }
                outcome => return outcome,
            }
        }
    }
}

impl<S: MailStore> MailStore for Patient<'_, S> {
    async fn own_addresses(&self) -> Result<Vec<String>> {
        self.patiently(|| self.store.own_addresses()).await
    }

    async fn search_people(&self, text: &str) -> Result<Vec<mett_core::Address>> {
        self.patiently(|| self.store.search_people(text)).await
    }

    async fn list_folders(&self) -> Result<Vec<Folder>> {
        self.patiently(|| self.store.list_folders()).await
    }

    async fn child_folders(&self, parent: Option<&FolderId>) -> Result<Vec<Folder>> {
        self.patiently(|| self.store.child_folders(parent)).await
    }

    async fn folder(&self, id: &FolderId) -> Result<Folder> {
        self.patiently(|| self.store.folder(id)).await
    }

    async fn recent_unread(
        &self,
        folder: &FolderId,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<(MessageId, DateTime<Utc>)>> {
        self.patiently(|| self.store.recent_unread(folder, since))
            .await
    }

    async fn well_known_folder(&self, which: WellKnownFolder) -> Result<Folder> {
        self.patiently(|| self.store.well_known_folder(which)).await
    }

    async fn list_messages(&self, folder: &FolderId, page: Page) -> Result<MessagePage> {
        self.patiently(|| self.store.list_messages(folder, page.clone()))
            .await
    }

    async fn changed_messages(
        &self,
        folder: &FolderId,
        since: DateTime<Utc>,
        page: Page,
    ) -> Result<MessagePage> {
        self.patiently(|| self.store.changed_messages(folder, since, page.clone()))
            .await
    }

    async fn fetch_message(&self, id: &MessageId) -> Result<Message> {
        self.patiently(|| self.store.fetch_message(id)).await
    }

    async fn message_folder(&self, id: &MessageId) -> Result<FolderId> {
        self.patiently(|| self.store.message_folder(id)).await
    }

    async fn list_attachments(&self, message: &MessageId) -> Result<Vec<Attachment>> {
        self.patiently(|| self.store.list_attachments(message))
            .await
    }

    async fn fetch_attachment(
        &self,
        message: &MessageId,
        attachment: &AttachmentId,
    ) -> Result<Vec<u8>> {
        self.patiently(|| self.store.fetch_attachment(message, attachment))
            .await
    }

    async fn update_flags(&self, id: &MessageId, update: FlagsUpdate) -> Result<()> {
        self.patiently(|| self.store.update_flags(id, update)).await
    }

    async fn move_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        self.patiently(|| self.store.move_message(id, to)).await
    }

    async fn copy_message(&self, id: &MessageId, to: &FolderId) -> Result<MessageId> {
        self.patiently(|| self.store.copy_message(id, to)).await
    }

    async fn delete_message(&self, id: &MessageId) -> Result<()> {
        self.patiently(|| self.store.delete_message(id)).await
    }

    async fn answer_invitation(
        &self,
        id: &MessageId,
        invitation: &Invitation,
        answer: &Answer,
    ) -> Result<()> {
        self.patiently(|| self.store.answer_invitation(id, invitation, answer))
            .await
    }

    async fn save_draft(&self, draft: &Draft) -> Result<MessageId> {
        self.patiently(|| self.store.save_draft(draft)).await
    }

    fn folder_names_ignore_case(&self) -> bool {
        self.store.folder_names_ignore_case()
    }

    fn can_watch(&self) -> bool {
        self.store.can_watch()
    }

    async fn watch(&self, folders: &[FolderId]) -> Result<Vec<FolderId>> {
        self.patiently(|| self.store.watch(folders)).await
    }
}

impl<S: MailSender> MailSender for Patient<'_, S> {
    async fn send(&self, draft: &Draft) -> Result<()> {
        self.patiently(|| self.store.send(draft)).await
    }

    async fn discard_kept(&self, draft: &Draft) -> Result<()> {
        self.patiently(|| self.store.discard_kept(draft)).await
    }

    fn keeps_draft(&self, draft: &Draft) -> bool {
        self.store.keeps_draft(draft)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Calls `patiently` on a call that the server throttles `times` times
    /// (asking for `retry_after`) before it succeeds, and returns its
    /// outcome, the waits announced, and how long it all took.
    async fn throttled(
        times: u32,
        retry_after: Option<Duration>,
    ) -> (Result<u32>, Vec<(Duration, u32)>, Duration) {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let waits = std::sync::Mutex::new(Vec::new());
        let notify = |wait, attempt| waits.lock().unwrap().push((wait, attempt));
        let patient = Patient {
            store: &(),
            notify: &notify,
        };
        let start = tokio::time::Instant::now();
        let outcome = patient
            .patiently(|| async {
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call < times {
                    Err(Error::Throttled { retry_after })
                } else {
                    Ok(call)
                }
            })
            .await;
        let waits = waits.into_inner().unwrap();
        (outcome, waits, start.elapsed())
    }

    #[tokio::test(start_paused = true)]
    async fn throttled_calls_are_tried_again_after_the_wait_asked_for() {
        let three = Duration::from_secs(3);
        let (outcome, waits, took) = throttled(2, Some(three)).await;
        assert_eq!(outcome.unwrap(), 2, "the third call");
        assert_eq!(waits, [(three, 1), (three, 2)]);
        assert!(took >= 2 * three, "{took:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_wait_asked_for_each_is_twice_the_last() {
        let (outcome, waits, _) = throttled(u32::MAX, None).await;
        assert!(matches!(outcome, Err(Error::Throttled { .. })), "gives up");
        let seconds: Vec<u64> = waits.iter().map(|(wait, _)| wait.as_secs()).collect();
        assert_eq!(seconds, [2, 4, 8, 16, 32]);
        assert_eq!(waits.len(), THROTTLED_RETRIES as usize);
    }

    #[tokio::test(start_paused = true)]
    async fn long_waits_are_cut_short() {
        let (outcome, waits, _) = throttled(1, Some(Duration::from_secs(3600))).await;
        assert!(outcome.is_ok());
        assert_eq!(waits, [(LONGEST_WAIT, 1)]);
    }
}
