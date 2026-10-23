use std::collections::VecDeque;

use super::*;
use crate::attachment::AttachmentKind;
use crate::ids::AttachmentId;

/// What the fake server does with the next `attach`.
#[derive(Clone, Copy)]
enum Attach {
    /// Attaches it and says its ID.
    Ok,
    /// Attaches it and doesn't say its ID.
    OkWithoutId,
    /// Refuses it.
    Refuse,
    /// Attaches it, but the answer is lost.
    Lost,
}

/// A server whose drafts hold attachments, and that fails on cue.
#[derive(Default)]
struct Fake {
    /// The attachments of each server draft, by ID.
    drafts: Mutex<HashMap<String, Vec<Attachment>>>,
    deleted: Mutex<Vec<String>>,
    script: Mutex<VecDeque<Attach>>,
    created: Mutex<u32>,
    next_attachment: Mutex<u32>,
    /// Deleting a draft fails.
    undeletable: Mutex<bool>,
    /// Updating a draft finds it gone.
    gone: Mutex<bool>,
    /// The answer to listing attachments is lost.
    list_lost: Mutex<bool>,
    /// What `update_draft` was told, in order.
    updates: Mutex<Vec<String>>,
}

impl Fake {
    fn script(&self, outcomes: impl IntoIterator<Item = Attach>) {
        self.script.lock().unwrap().extend(outcomes);
    }

    fn with(&self, id: &str, attachments: &[(&str, &str)]) {
        let attachments = attachments
            .iter()
            .map(|(id, name)| attachment(id, name))
            .collect();
        self.drafts.lock().unwrap().insert(id.into(), attachments);
    }

    fn names(&self, id: &str) -> Vec<String> {
        self.drafts.lock().unwrap()[id]
            .iter()
            .map(|a| a.name.clone())
            .collect()
    }
}

fn attachment(id: &str, name: &str) -> Attachment {
    Attachment {
        id: AttachmentId::new(id),
        name: name.into(),
        content_type: "text/plain".into(),
        size: 1,
        inline: false,
        kind: AttachmentKind::File,
    }
}

fn file(name: &str, data: &str) -> OutgoingAttachment {
    OutgoingAttachment {
        name: name.into(),
        content_type: "text/plain".into(),
        data: data.as_bytes().to_vec(),
    }
}

impl DraftBackend for Fake {
    async fn create_draft(&self, draft: &Draft) -> Result<MessageId> {
        let mut created = self.created.lock().unwrap();
        *created += 1;
        let id = format!("d{created}");
        // A forward carries the original's attachments along.
        let carried = match draft.origin {
            Some(DraftOrigin::Forward(_)) => vec![attachment("orig", "original.txt")],
            _ => Vec::new(),
        };
        self.drafts.lock().unwrap().insert(id.clone(), carried);
        Ok(MessageId::new(id))
    }

    async fn update_draft(&self, id: &MessageId, draft: &Draft) -> Result<()> {
        if *self.gone.lock().unwrap() {
            return Err(Error::NotFound {
                kind: "message",
                id: id.to_string(),
            });
        }
        self.updates
            .lock()
            .unwrap()
            .push(format!("{id}:{}", draft.body));
        Ok(())
    }

    async fn list_attachments(&self, id: &MessageId) -> Result<Vec<Attachment>> {
        if *self.list_lost.lock().unwrap() {
            return Err(Error::Network("lost".into()));
        }
        Ok(self.drafts.lock().unwrap()[id.as_str()].clone())
    }

    async fn attach(&self, id: &MessageId, file: &OutgoingAttachment) -> Result<Option<String>> {
        let outcome = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Attach::Ok);
        if let Attach::Refuse = outcome {
            return Err(Error::Backend("refused".into()));
        }
        let mut next = self.next_attachment.lock().unwrap();
        *next += 1;
        let new = format!("a{next}");
        self.drafts
            .lock()
            .unwrap()
            .get_mut(id.as_str())
            .unwrap()
            .push(attachment(&new, &file.name));
        match outcome {
            Attach::Ok => Ok(Some(new)),
            Attach::OkWithoutId => Ok(None),
            _ => Err(Error::Network("lost".into())),
        }
    }

    async fn remove_attachment(&self, id: &MessageId, attachment: &str) -> Result<()> {
        self.drafts
            .lock()
            .unwrap()
            .get_mut(id.as_str())
            .unwrap()
            .retain(|a| a.id.as_str() != attachment);
        Ok(())
    }

    async fn delete_draft(&self, id: &MessageId) -> Result<()> {
        if *self.undeletable.lock().unwrap() {
            return Err(Error::Backend("denied".into()));
        }
        self.deleted.lock().unwrap().push(id.to_string());
        self.drafts.lock().unwrap().remove(id.as_str());
        Ok(())
    }
}

/// A fake whose updates all fail with a lost answer.
struct LosingUpdates(Fake);

impl DraftBackend for LosingUpdates {
    async fn create_draft(&self, draft: &Draft) -> Result<MessageId> {
        self.0.create_draft(draft).await
    }

    async fn update_draft(&self, _: &MessageId, _: &Draft) -> Result<()> {
        Err(Error::Network("lost".into()))
    }

    async fn list_attachments(&self, id: &MessageId) -> Result<Vec<Attachment>> {
        self.0.list_attachments(id).await
    }

    async fn attach(&self, id: &MessageId, file: &OutgoingAttachment) -> Result<Option<String>> {
        self.0.attach(id, file).await
    }

    async fn remove_attachment(&self, id: &MessageId, attachment: &str) -> Result<()> {
        self.0.remove_attachment(id, attachment).await
    }

    async fn delete_draft(&self, id: &MessageId) -> Result<()> {
        self.0.delete_draft(id).await
    }
}

fn message(files: &[(&str, &str)]) -> Draft {
    Draft {
        body: "hello".into(),
        attachments: files.iter().map(|(n, d)| file(n, d)).collect(),
        ..Draft::default()
    }
}

fn recalled(files: &[(&str, &str)]) -> Draft {
    Draft {
        origin: Some(DraftOrigin::Recalled(MessageId::new("p"))),
        ..message(files)
    }
}

/// The ledger of `draft`: each upload's ID (if known) and file name.
fn uploads_of(session: &DraftSession, draft: &Draft) -> Vec<(Option<String>, String)> {
    session
        .uploads()
        .get(&key(draft))
        .map(|uploads| {
            uploads
                .iter()
                .map(|u| (u.id.clone(), u.name.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn known(id: &str, name: &str) -> (Option<String>, String) {
    (Some(id.into()), name.into())
}

fn lost(name: &str) -> (Option<String>, String) {
    (None, name.into())
}

#[tokio::test]
async fn a_new_message_gets_its_files_and_remembers_no_uploads() {
    let (session, server) = (DraftSession::new(), Fake::default());
    let draft = message(&[("a", "1"), ("b", "2")]);
    let (id, base) = session.server_draft(&server, &draft).await.unwrap();
    assert_eq!(base, Some(Vec::new()));
    assert_eq!(server.names(id.as_str()), ["a", "b"]);
    // Only a recalled draft needs the ledger.
    assert!(uploads_of(&session, &draft).is_empty());
    assert!(!session.has_kept(&draft));
}

#[tokio::test]
async fn a_message_without_files_is_not_listed() {
    let (session, server) = (DraftSession::new(), Fake::default());
    *server.list_lost.lock().unwrap() = true;
    let (_, base) = session.server_draft(&server, &message(&[])).await.unwrap();
    assert_eq!(base, None);
}

#[tokio::test]
async fn a_forward_keeps_what_it_carried_and_adds_its_files() {
    let (session, server) = (DraftSession::new(), Fake::default());
    let draft = Draft {
        origin: Some(DraftOrigin::Forward(MessageId::new("m"))),
        ..message(&[("a", "1")])
    };
    let (id, base) = session.server_draft(&server, &draft).await.unwrap();
    assert_eq!(base, Some(vec!["orig".into()]));
    assert_eq!(server.names(id.as_str()), ["original.txt", "a"]);
    // The server filled in a body: the user's replaced it.
    assert_eq!(*server.updates.lock().unwrap(), ["d1:hello"]);
}

#[tokio::test]
async fn a_refused_file_deletes_the_new_draft() {
    let (session, server) = (DraftSession::new(), Fake::default());
    server.script([Attach::Ok, Attach::Refuse]);
    let draft = message(&[("a", "1"), ("b", "2")]);
    let e = session.server_draft(&server, &draft).await.unwrap_err();
    assert!(matches!(e, Error::Backend(_)));
    assert_eq!(*server.deleted.lock().unwrap(), ["d1"]);
    assert!(!session.has_kept(&draft));
}

#[tokio::test]
async fn a_refused_file_of_a_draft_that_cannot_be_deleted_keeps_it() {
    let (session, server) = (DraftSession::new(), Fake::default());
    *server.undeletable.lock().unwrap() = true;
    server.script([Attach::Refuse]);
    let draft = message(&[("a", "1")]);
    session.server_draft(&server, &draft).await.unwrap_err();
    assert!(session.has_kept(&draft));
}

#[tokio::test]
async fn a_lost_answer_keeps_the_draft_and_a_retry_attaches_each_file_once() {
    let (session, server) = (DraftSession::new(), Fake::default());
    server.script([Attach::Ok, Attach::Lost]);
    let draft = message(&[("a", "1"), ("b", "2")]);
    let e = session.server_draft(&server, &draft).await.unwrap_err();
    assert!(matches!(e, Error::Network(_)));
    assert!(session.has_kept(&draft));
    assert!(server.deleted.lock().unwrap().is_empty());
    // Both files are on it now (the second's answer was lost).
    assert_eq!(server.names("d1"), ["a", "b"]);

    // What the failed attempt attached comes off before the files go on
    // again, and no second draft is made.
    let (id, base) = session.server_draft(&server, &draft).await.unwrap();
    assert_eq!(id.as_str(), "d1");
    assert_eq!(base, Some(Vec::new()));
    assert_eq!(server.names("d1"), ["a", "b"]);
    assert_eq!(*server.created.lock().unwrap(), 1);
    assert!(!session.has_kept(&draft));
}

#[tokio::test]
async fn a_kept_draft_that_is_gone_is_made_again() {
    let (session, server) = (DraftSession::new(), Fake::default());
    let draft = message(&[("a", "1")]);
    session.keep(&draft, MessageId::new("old"), None);
    *server.gone.lock().unwrap() = true;
    let (id, _) = session.server_draft(&server, &draft).await.unwrap();
    assert_eq!(id.as_str(), "d1");
    assert!(!session.has_kept(&draft));
}

#[tokio::test]
async fn a_kept_draft_whose_update_is_lost_stays_kept() {
    let (session, server) = (DraftSession::new(), LosingUpdates(Fake::default()));
    let draft = message(&[("a", "1")]);
    session.keep(&draft, MessageId::new("old"), Some(vec!["x".into()]));
    session.server_draft(&server, &draft).await.unwrap_err();
    assert!(session.has_kept(&draft));
    assert_eq!(*server.0.created.lock().unwrap(), 0);
}

#[tokio::test]
async fn a_lost_update_of_a_new_draft_keeps_it() {
    let (session, server) = (DraftSession::new(), LosingUpdates(Fake::default()));
    let draft = Draft {
        origin: Some(DraftOrigin::Reply(MessageId::new("m"))),
        ..message(&[])
    };
    session.server_draft(&server, &draft).await.unwrap_err();
    assert!(session.has_kept(&draft));
    assert!(server.0.deleted.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_lost_listing_keeps_the_draft() {
    let (session, server) = (DraftSession::new(), Fake::default());
    *server.list_lost.lock().unwrap() = true;
    let draft = Draft {
        origin: Some(DraftOrigin::Forward(MessageId::new("m"))),
        ..message(&[("a", "1")])
    };
    session.server_draft(&server, &draft).await.unwrap_err();
    assert!(session.has_kept(&draft));
    assert!(server.deleted.lock().unwrap().is_empty());
}

#[test]
fn attempts_of_different_messages_are_kept_apart() {
    let session = DraftSession::new();
    let first = message(&[]);
    let second = Draft {
        attempt: 1,
        ..message(&[])
    };
    session.keep(&first, MessageId::new("x"), None);
    assert!(session.has_kept(&first));
    assert!(!session.has_kept(&second));
}

#[tokio::test]
async fn a_recalled_draft_takes_off_what_the_user_dropped_and_keeps_the_rest() {
    let (session, server) = (DraftSession::new(), Fake::default());
    server.with("p", &[("k", "keep.txt"), ("x", "drop.txt")]);
    let draft = Draft {
        drop_attachments: vec![AttachmentId::new("x")],
        ..recalled(&[("new", "1")])
    };
    let (id, base) = session.server_draft(&server, &draft).await.unwrap();
    assert_eq!(id.as_str(), "p");
    assert_eq!(base, Some(vec!["k".into()]));
    assert_eq!(server.names("p"), ["keep.txt", "new"]);
    // The upload is known by its ID.
    assert_eq!(uploads_of(&session, &draft), [known("a1", "new")]);
}

#[tokio::test]
async fn a_recalled_draft_that_is_gone_is_an_error_and_makes_no_other() {
    let (session, server) = (DraftSession::new(), Fake::default());
    *server.gone.lock().unwrap() = true;
    let e = session
        .server_draft(&server, &recalled(&[]))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Backend(_)), "{e}");
    assert_eq!(*server.created.lock().unwrap(), 0);
}

#[tokio::test]
async fn a_refused_upload_leaves_no_trace_but_a_lost_one_does() {
    let draft = recalled(&[("a", "1"), ("b", "2")]);

    let (session, server) = (DraftSession::new(), Fake::default());
    server.with("p", &[("k", "keep.txt")]);
    server.script([Attach::Ok, Attach::Refuse]);
    session.server_draft(&server, &draft).await.unwrap_err();
    assert_eq!(uploads_of(&session, &draft), [known("a1", "a")]);
    // The draft is the user's: never deleted, kept for the next attempt.
    assert!(server.deleted.lock().unwrap().is_empty());
    assert!(session.has_kept(&draft));

    let (session, server) = (DraftSession::new(), Fake::default());
    server.with("p", &[("k", "keep.txt")]);
    server.script([Attach::Ok, Attach::Lost]);
    session.server_draft(&server, &draft).await.unwrap_err();
    assert_eq!(uploads_of(&session, &draft), [known("a1", "a"), lost("b")]);
}

#[tokio::test]
async fn an_upload_whose_id_the_server_did_not_say_is_remembered_by_name() {
    let (session, server) = (DraftSession::new(), Fake::default());
    server.with("p", &[]);
    server.script([Attach::OkWithoutId]);
    let draft = recalled(&[("a", "1")]);
    session.server_draft(&server, &draft).await.unwrap();
    assert_eq!(uploads_of(&session, &draft), [lost("a")]);
}

#[tokio::test]
async fn a_retry_of_a_recalled_draft_takes_off_known_uploads_and_attaches_again() {
    let (session, server) = (DraftSession::new(), Fake::default());
    server.with("p", &[("k", "keep.txt")]);
    server.script([Attach::Ok, Attach::Lost]);
    let draft = recalled(&[("a", "1"), ("b", "2")]);
    session.server_draft(&server, &draft).await.unwrap_err();
    // Meanwhile, someone added a file elsewhere.
    server
        .drafts
        .lock()
        .unwrap()
        .get_mut("p")
        .unwrap()
        .push(attachment("else", "elsewhere.txt"));

    let (_, base) = session.server_draft(&server, &draft).await.unwrap();
    // The known upload (a1) came off and went on again; the one whose
    // answer was lost (a2) can't be told from others of its name and type,
    // so it stays (and the file goes on again: twice, at worst). What was
    // added elsewhere stays.
    assert_eq!(
        server.names("p"),
        ["keep.txt", "b", "elsewhere.txt", "a", "b"]
    );
    assert_eq!(base, Some(vec!["k".into(), "a2".into(), "else".into()]));
    assert_eq!(
        uploads_of(&session, &draft),
        [lost("b"), known("a3", "a"), known("a4", "b")]
    );
}

#[tokio::test]
async fn a_lost_upload_the_user_took_off_stops_the_send() {
    let (session, server) = (DraftSession::new(), Fake::default());
    server.with("p", &[]);
    server.script([Attach::Lost]);
    session
        .server_draft(&server, &recalled(&[("a", "1")]))
        .await
        .unwrap_err();
    // The user takes the file off: whether the lost upload is on the
    // message can't be told.
    let retry = recalled(&[]);
    let e = session.server_draft(&server, &retry).await.unwrap_err();
    assert!(
        matches!(&e, Error::Backend(m) if m.contains("a (taken off)")),
        "{e}"
    );
    assert!(session.has_kept(&retry));
    assert!(server.deleted.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_lost_upload_replaced_by_a_file_of_other_contents_stops_the_send() {
    let (session, server) = (DraftSession::new(), Fake::default());
    server.with("p", &[]);
    server.script([Attach::Lost]);
    session
        .server_draft(&server, &recalled(&[("a", "1")]))
        .await
        .unwrap_err();
    let e = session
        .server_draft(&server, &recalled(&[("a", "other")]))
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Backend(_)), "{e}");
}

#[tokio::test]
async fn more_lost_uploads_than_copies_wanted_stop_the_send() {
    let (session, server) = (DraftSession::new(), Fake::default());
    server.with("p", &[]);
    server.script([Attach::Lost, Attach::Lost]);
    let draft = recalled(&[("a", "1")]);
    // Two attempts, each with its answer lost.
    session.server_draft(&server, &draft).await.unwrap_err();
    session.server_draft(&server, &draft).await.unwrap_err();
    assert_eq!(uploads_of(&session, &draft), [lost("a"), lost("a")]);
    // Whether one or two copies are on the message can't be told, and one
    // is wanted: it may be sent twice or not at all.
    let e = session.server_draft(&server, &draft).await.unwrap_err();
    assert!(matches!(e, Error::Backend(_)), "{e}");
}

#[tokio::test]
async fn an_attachment_the_draft_had_before_is_not_taken_for_a_lost_upload() {
    let (session, server) = (DraftSession::new(), Fake::default());
    // A file of the same name was there before mett's upload.
    server.with("p", &[("old", "a")]);
    server.script([Attach::Lost]);
    session
        .server_draft(&server, &recalled(&[("a", "1")]))
        .await
        .unwrap_err();
    // The upload never reached the server, and the user took the file off.
    server
        .drafts
        .lock()
        .unwrap()
        .get_mut("p")
        .unwrap()
        .retain(|a| a.id.as_str() == "old");
    let (_, base) = session.server_draft(&server, &recalled(&[])).await.unwrap();
    assert_eq!(base, Some(vec!["old".into()]));
}

#[tokio::test]
async fn giving_up_deletes_a_new_draft_and_never_a_recalled_one() {
    let (session, server) = (DraftSession::new(), Fake::default());
    server.with("d", &[]);
    let id = MessageId::new("d");
    assert!(session.discard(&server, &message(&[]), &id).await);
    assert_eq!(*server.deleted.lock().unwrap(), ["d"]);
    let postponed = MessageId::new("p");
    assert!(!session.discard(&server, &recalled(&[]), &postponed).await);
    assert_eq!(*server.deleted.lock().unwrap(), ["d"]);
    assert!(is_recalled(&recalled(&[]), &postponed));
    assert!(!is_recalled(&recalled(&[]), &id));
}

#[tokio::test]
async fn after_a_send_the_draft_is_settled_by_how_it_went() {
    let (session, server) = (DraftSession::new(), Fake::default());
    // Lost: kept, and said.
    let postponed = recalled(&[]);
    let e = session
        .after_send::<_, ()>(
            &server,
            &postponed,
            MessageId::new("p"),
            Some(vec!["x".into()]),
            Err(Error::Network("down".into())),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Network(m) if m.contains("it is in Drafts")),
        "{e}"
    );
    assert!(session.has_kept(&postponed));

    // Refused: deleted (a new message's draft), not kept.
    let plain = message(&[]);
    server.with("d2", &[]);
    session
        .after_send::<_, ()>(
            &server,
            &plain,
            MessageId::new("d2"),
            None,
            Err(Error::Backend("no".into())),
        )
        .await
        .unwrap_err();
    assert!(!session.has_kept(&plain));
    assert_eq!(*server.deleted.lock().unwrap(), ["d2"]);

    // Sent: the uploads are forgotten.
    session.uploads().insert(
        key(&postponed),
        vec![Upload {
            id: None,
            name: "a".into(),
            content_type: "text/plain".into(),
            digest: digest(b"1"),
            prior: Vec::new(),
        }],
    );
    session
        .after_send(&server, &postponed, MessageId::new("p"), None, Ok(()))
        .await
        .unwrap();
    assert!(uploads_of(&session, &postponed).is_empty());
}

#[tokio::test]
async fn discarding_the_kept_draft_deletes_it_unless_it_is_recalled() {
    let (session, server) = (DraftSession::new(), Fake::default());
    let draft = message(&[]);
    server.with("d", &[]);
    session.keep(&draft, MessageId::new("d"), None);
    session.discard_kept(&server, &draft).await.unwrap();
    assert_eq!(*server.deleted.lock().unwrap(), ["d"]);
    assert!(!session.has_kept(&draft));
    // Nothing kept any more.
    session.discard_kept(&server, &draft).await.unwrap();

    // One that can't be deleted stays kept.
    server.with("e", &[]);
    *server.undeletable.lock().unwrap() = true;
    session.keep(&draft, MessageId::new("e"), Some(Vec::new()));
    session.discard_kept(&server, &draft).await.unwrap_err();
    assert!(session.has_kept(&draft));
    *server.undeletable.lock().unwrap() = false;

    // A recalled one stays in Drafts, and its uploads are forgotten.
    let postponed = recalled(&[("a", "1")]);
    server.with("p", &[]);
    server.script([Attach::Lost]);
    session.server_draft(&server, &postponed).await.unwrap_err();
    assert!(!uploads_of(&session, &postponed).is_empty());
    session.discard_kept(&server, &postponed).await.unwrap();
    assert!(uploads_of(&session, &postponed).is_empty());
    assert!(!session.has_kept(&postponed));
    assert_eq!(*server.deleted.lock().unwrap(), ["d"]);
    assert_eq!(server.names("p"), ["a"]);
}

#[test]
fn a_lost_answer_means_different_things_before_and_after_the_hand_over() {
    assert!(matches!(
        unsent(Error::Network("x".into())),
        Error::NotSent(_)
    ));
    assert!(matches!(unsent(Error::Auth("x".into())), Error::Auth(_)));
    assert!(matches!(
        may_be_left(Error::Network("x".into())),
        Error::Network(m) if m.contains("may be left in Drafts")
    ));
    assert!(matches!(
        may_be_left(Error::Auth("x".into())),
        Error::Auth(_)
    ));
}
