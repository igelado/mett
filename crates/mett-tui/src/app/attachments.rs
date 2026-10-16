//! Attachments, as in mutt: the attachment menu (`v`), viewing and saving
//! a message's attachments, and attaching files to a message being written
//! (`a` and `D` in the compose menu).

use std::path::PathBuf;

use mett_core::{Attachment, AttachmentKind, MessageId, OutgoingAttachment};

use super::composing::{Input, Question};
use super::{App, Effect, Screen, effect_id};
use crate::action::Action;
use crate::config::expand_home;
use crate::text;
use crate::worker::{Request, RequestId};

/// The largest text attachment shown in mett; larger ones are opened
/// like other files.
const MAX_TEXT: usize = 4 * 1024 * 1024;

/// The attachment menu: a message's attachments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachMenu {
    /// The message.
    pub message: MessageId,
    /// Its attachments, once listed.
    pub entries: Vec<Attachment>,
    /// The selected entry.
    pub selected: usize,
    /// The request listing them, while it is pending.
    pub(super) listing: Option<RequestId>,
    /// Listing them failed for want of a sign-in: a retry lists them.
    pub(super) unlisted: bool,
}

impl AttachMenu {
    /// Whether the attachments are still being listed.
    pub fn listing(&self) -> bool {
        self.listing.is_some()
    }
}

/// Text shown on its own screen, such as a text attachment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextView {
    /// What it is, for the status bar.
    pub title: String,
    /// The lines, not wrapped.
    pub lines: Vec<String>,
    /// The first row shown, after wrapping.
    pub scroll: usize,
    /// The lines wrapped to `rows_width`, kept so that a long file isn't
    /// wrapped again at every key.
    pub rows: Vec<String>,
    /// The width `rows` were wrapped to.
    pub rows_width: Option<usize>,
}

impl TextView {
    /// A view of `lines`, wrapped to `width`.
    pub fn new(title: String, lines: Vec<String>, width: usize) -> TextView {
        let mut view = TextView {
            title,
            lines,
            scroll: 0,
            rows: Vec::new(),
            rows_width: None,
        };
        view.layout(width);
        view
    }

    /// Wraps the lines again if the width changed.
    pub(super) fn layout(&mut self, width: usize) {
        let width = width.max(1);
        if self.rows_width != Some(width) {
            self.rows = text::wrap_all(&self.lines, width);
            self.rows_width = Some(width);
        }
    }
}

/// What to do with an attachment once fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Use {
    /// Show it.
    View,
    /// Save it to this file.
    Save(PathBuf),
}

/// An attachment being fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Fetching {
    pub(super) request: RequestId,
    pub(super) message: MessageId,
    pub(super) attachment: Attachment,
    pub(super) to: Use,
}

impl App {
    /// Opens the attachment menu for the message acted on.
    pub(super) fn view_attachments(&mut self) -> Vec<Effect> {
        let message = match self.screens.last() {
            Some(Screen::Pager(pager)) => Some(pager.id.clone()),
            _ => self.messages.get(self.selected).map(|m| m.id.clone()),
        };
        let Some(message) = message else {
            self.fail("No messages.");
            return Vec::new();
        };
        let request = self.request(Request::ListAttachments(message.clone()));
        self.screens.push(Screen::Attachments(AttachMenu {
            message,
            entries: Vec::new(),
            selected: 0,
            listing: Some(effect_id(&request)),
            unlisted: false,
        }));
        vec![request]
    }

    /// Retries what failed for want of a sign-in: listing attachments,
    /// and fetching the one asked for (if its menu is still there).
    pub(super) fn retry_attachments(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        for position in 0..self.screens.len() {
            let message = match &self.screens[position] {
                Screen::Attachments(menu) if menu.unlisted => menu.message.clone(),
                _ => continue,
            };
            let request = self.request(Request::ListAttachments(message));
            if let Screen::Attachments(menu) = &mut self.screens[position] {
                menu.unlisted = false;
                menu.listing = Some(effect_id(&request));
            }
            effects.push(request);
        }
        // While its menu is there (under the help, say).
        if let Some((message, attachment, to)) = self.unfetched_attachment.take()
            && self
                .screens
                .iter()
                .any(|s| matches!(s, Screen::Attachments(m) if m.message == message))
        {
            effects.extend(self.fetch_attachment(message, attachment, to));
        }
        effects
    }

    /// Fills the attachment menu once its attachments are listed.
    pub(super) fn on_attachments_listed(
        &mut self,
        id: RequestId,
        result: mett_core::Result<Vec<Attachment>>,
    ) -> Vec<Effect> {
        // The menu may be under another screen (help) by now.
        let found = self
            .screens
            .iter_mut()
            .enumerate()
            .rev()
            .find_map(|(i, s)| match s {
                Screen::Attachments(menu) if menu.listing == Some(id) => Some((i, menu)),
                _ => None,
            });
        let Some((position, menu)) = found else {
            return Vec::new();
        };
        menu.listing = None;
        match result {
            Ok(entries) if entries.is_empty() => {
                self.screens.remove(position);
                self.fail("This message has no attachments.");
            }
            Ok(entries) => menu.entries = entries,
            // Kept for a retry once the user signs in.
            Err(e @ mett_core::Error::Auth(_)) => {
                menu.unlisted = true;
                self.report(e);
            }
            Err(e) => {
                self.screens.remove(position);
                self.report(e);
            }
        }
        Vec::new()
    }

    /// The request listing the attachment menu's attachments, while it
    /// is pending.
    pub(super) fn attachments_request(&self) -> Option<RequestId> {
        self.screens.iter().find_map(|s| match s {
            Screen::Attachments(menu) => menu.listing,
            _ => None,
        })
    }

    /// The selected attachment of the attachment menu.
    fn selected_attachment(&self) -> Option<(MessageId, Attachment)> {
        match self.screens.last() {
            Some(Screen::Attachments(menu)) => menu
                .entries
                .get(menu.selected)
                .map(|a| (menu.message.clone(), a.clone())),
            _ => None,
        }
    }

    pub(super) fn on_attach_action(&mut self, action: Action) -> Vec<Effect> {
        let page = self.body_height.max(1);
        let Some(Screen::Attachments(menu)) = self.screens.last_mut() else {
            return Vec::new();
        };
        if super::navigate(&mut menu.selected, action, menu.entries.len(), page) {
            return Vec::new();
        }
        match action {
            Action::Exit | Action::Quit => {
                self.screens.pop();
                self.fetching = None;
            }
            Action::ViewAttach | Action::DisplayMessage => {
                let Some((message, attachment)) = self.selected_attachment() else {
                    return Vec::new();
                };
                return self.fetch_attachment(message, attachment, Use::View);
            }
            Action::SaveEntry => {
                let Some((_, attachment)) = self.selected_attachment() else {
                    return Vec::new();
                };
                if attachment.kind == AttachmentKind::Link {
                    self.fail("It is a link to a file kept elsewhere; open it in Outlook.");
                    return Vec::new();
                }
                self.ask(
                    Question::SaveAttachment,
                    "Save to file: ",
                    &file_name(&attachment),
                );
            }
            _ => {}
        }
        Vec::new()
    }

    /// Fetches an attachment, to view or save.
    fn fetch_attachment(
        &mut self,
        message: MessageId,
        attachment: Attachment,
        to: Use,
    ) -> Vec<Effect> {
        if attachment.kind == AttachmentKind::Link {
            self.fail("It is a link to a file kept elsewhere; open it in Outlook.");
            return Vec::new();
        }
        // One at a time: another would take its place.
        if self.fetching.is_some() {
            self.fail("Please wait: an attachment is still being fetched.");
            return Vec::new();
        }
        let request = self.request(Request::FetchAttachment(
            message.clone(),
            attachment.id.clone(),
        ));
        self.fetching = Some(Fetching {
            request: effect_id(&request),
            message,
            attachment,
            to,
        });
        self.status = "Fetching attachment...".into();
        vec![request]
    }

    /// Saves the selected attachment to `path` (typed at the question).
    pub(super) fn answer_save_attachment(&mut self, path: String) -> Vec<Effect> {
        let path = path.trim();
        if path.is_empty() {
            return Vec::new();
        }
        let Some((message, attachment)) = self.selected_attachment() else {
            return Vec::new();
        };
        let path = PathBuf::from(expand_home(path, self.home.as_deref()));
        self.fetch_attachment(message, attachment, Use::Save(path))
    }

    /// Takes in a fetched attachment: shows it, or saves it.
    pub(super) fn on_attachment_fetched(
        &mut self,
        id: RequestId,
        result: mett_core::Result<Vec<u8>>,
    ) -> Vec<Effect> {
        let Some(Fetching {
            message,
            attachment,
            to,
            ..
        }) = self.fetching.take_if(|f| f.request == id)
        else {
            return Vec::new();
        };
        let data = match result {
            Ok(data) => data,
            // Fetched again once the user signs in.
            Err(e @ mett_core::Error::Auth(_)) => {
                self.unfetched_attachment = Some((message, attachment, to));
                self.report(e);
                return Vec::new();
            }
            Err(e) => {
                self.report(e);
                return Vec::new();
            }
        };
        self.clear_status_if(&["Fetching attachment..."]);
        match to {
            Use::Save(path) => {
                let op = self.wait_for_file(path.clone());
                vec![Effect::SaveFile {
                    op,
                    path,
                    data,
                    overwrite: false,
                }]
            }
            Use::View => match shown_as_text(&attachment, &data) {
                Some(lines) => {
                    self.screens.push(Screen::Text(TextView::new(
                        format!("{} [{}]", attachment.name, attachment.content_type),
                        lines,
                        self.width,
                    )));
                    Vec::new()
                }
                // Anything else opens in the program the system has for
                // it.
                None => vec![Effect::OpenFile {
                    name: file_name(&attachment),
                    data,
                }],
            },
        }
    }

    /// Takes in how saving a file went; one that exists may be
    /// overwritten.
    pub(super) fn on_file_saved(&mut self, op: FileOp, outcome: FileOutcome) -> Vec<Effect> {
        // One the user gave up waiting for is done with.
        if !self.file_waited(op) {
            return Vec::new();
        }
        match outcome {
            FileOutcome::Done(path) => {
                self.status = format!("Saved to {}.", path.display());
            }
            FileOutcome::Exists(path, data) => {
                self.unsaved = Some((path.clone(), data));
                self.input = Some(Input::YesNo {
                    prompt: format!("{} exists, overwrite?", path.display()),
                    default: false,
                    question: Question::Overwrite,
                });
            }
            FileOutcome::Failed(_, e) => self.fail(e),
        }
        Vec::new()
    }

    /// Overwrites the file that existed, if the user says so.
    pub(super) fn answer_overwrite(&mut self, overwrite: bool) -> Vec<Effect> {
        let Some((path, data)) = self.unsaved.take() else {
            return Vec::new();
        };
        if !overwrite {
            self.status = "Not saved.".into();
            return Vec::new();
        }
        let op = self.wait_for_file(path.clone());
        vec![Effect::SaveFile {
            op,
            path,
            data,
            overwrite: true,
        }]
    }

    pub(super) fn on_text_action(&mut self, action: Action) -> Vec<Effect> {
        let page = self.body_height.max(1);
        let width = self.width;
        let height = self.body_height;
        let Some(Screen::Text(view)) = self.screens.last_mut() else {
            return Vec::new();
        };
        view.layout(width);
        let last = view.rows.len().saturating_sub(height);
        match action {
            Action::Exit | Action::Quit => {
                self.screens.pop();
            }
            // There is no other message to go to: the pager's `j` and `k`
            // scroll, and its first and last entry go to the top and end.
            Action::NextLine | Action::NextEntry => view.scroll = (view.scroll + 1).min(last),
            Action::PreviousLine | Action::PreviousEntry => {
                view.scroll = view.scroll.saturating_sub(1);
            }
            Action::FirstEntry => view.scroll = 0,
            Action::LastEntry => view.scroll = last,
            Action::NextPage => view.scroll = (view.scroll + page).min(last),
            Action::PreviousPage => view.scroll = view.scroll.saturating_sub(page),
            // The pager's keys for it do something here too: say so.
            Action::Search | Action::SearchNext => {
                self.fail("Searching an attachment isn't supported yet; save it to search it.");
            }
            _ => {}
        }
        Vec::new()
    }

    /// `a` in the compose menu: asks for a file to attach.
    pub(super) fn ask_attach_file(&mut self) {
        self.ask(Question::AttachFile, "Attach file: ", "");
    }

    /// Reads the file to attach.
    pub(super) fn answer_attach_file(&mut self, path: String) -> Vec<Effect> {
        let path = path.trim();
        if path.is_empty() {
            return Vec::new();
        }
        let path = PathBuf::from(expand_home(path, self.home.as_deref()));
        self.status = "Attaching...".into();
        let op = self.wait_for_file(path.clone());
        vec![Effect::ReadFile(op, path)]
    }

    /// Keys wait for the file at `path` to be read or saved, as the
    /// returned operation.
    fn wait_for_file(&mut self, path: PathBuf) -> FileOp {
        self.last_file_op += 1;
        let op = FileOp(self.last_file_op);
        self.file_wait = Some((op, path));
        op
    }

    /// Whether `op` is what keys wait for: then they don't any more.
    fn file_waited(&mut self, op: FileOp) -> bool {
        let waited = self
            .file_wait
            .as_ref()
            .is_some_and(|(waiting, _)| *waiting == op);
        if waited {
            self.file_wait = None;
        }
        waited
    }

    /// Attaches a file read for the compose menu.
    pub(super) fn on_file_read(
        &mut self,
        op: FileOp,
        path: PathBuf,
        result: Result<Vec<u8>, String>,
    ) -> Vec<Effect> {
        // One the user gave up waiting for is done with.
        if !self.file_waited(op) {
            return Vec::new();
        }
        self.clear_status_if(&["Attaching..."]);
        let data = match result {
            Ok(data) => data,
            Err(e) => {
                self.fail(format!("{}: {e}", path.display()));
                return Vec::new();
            }
        };
        let Some(Screen::Compose(compose)) = self
            .screens
            .iter_mut()
            .rev()
            .find(|s| matches!(s, Screen::Compose(_)))
        else {
            return Vec::new();
        };
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        compose.draft.attachments.push(OutgoingAttachment {
            content_type: mett_core::content_type_for(&name).to_owned(),
            name,
            data,
        });
        // The new attachment is selected, as in mutt.
        compose.selected = compose.parts() - 1;
        Vec::new()
    }

    /// `D` in the compose menu: takes the selected file off the message.
    pub(super) fn detach_file(&mut self) {
        let Some(Screen::Compose(compose)) = self.screens.last_mut() else {
            return;
        };
        let first_file = compose.parts() - compose.draft.attachments.len();
        let first_kept = first_file - compose.kept.len();
        if compose.selected < first_kept {
            self.fail("Only attached files can be detached.");
            return;
        }
        if compose.selected < first_file {
            // A recalled message's: it comes off the draft when it goes.
            let dropped = compose.kept.remove(compose.selected - first_kept);
            compose.draft.drop_attachments.push(dropped.id);
        } else {
            compose
                .draft
                .attachments
                .remove(compose.selected - first_file);
        }
        compose.selected = compose.selected.min(compose.parts() - 1);
    }
}

/// Which reading or saving of a file a result is for: the user may have
/// given up on one, and asked for the same file again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileOp(pub u64);

/// How saving a file went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileOutcome {
    /// Saved there.
    Done(PathBuf),
    /// The file exists; here is what was to go in it.
    Exists(PathBuf, Vec<u8>),
    /// Saving there failed, and why.
    Failed(PathBuf, String),
}

/// `data` as text, in the charset `content_type` declares; without one,
/// UTF-8, or Windows-1252 if it isn't. `None` for a charset mett doesn't
/// know (the file is opened with another program instead).
fn decode(content_type: &str, data: &[u8]) -> Option<String> {
    let charset = content_type.split(';').skip(1).find_map(|param| {
        let (name, value) = param.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"'))
    });
    let encoding = match charset {
        Some(label) => encoding_rs::Encoding::for_label(label.as_bytes())?,
        None if std::str::from_utf8(data).is_ok() => encoding_rs::UTF_8,
        None => encoding_rs::WINDOWS_1252,
    };
    Some(encoding.decode(data).0.into_owned())
}

/// The attachment as lines of text, if it is text (and not too large).
fn shown_as_text(attachment: &Attachment, data: &[u8]) -> Option<Vec<String>> {
    let content_type = attachment.content_type.to_ascii_lowercase();
    let textual =
        content_type.starts_with("text/") || media_type(&content_type) == "message/rfc822";
    if !textual || data.len() > MAX_TEXT {
        return None;
    }
    let content = decode(&content_type, data)?;
    let kind = if content_type.starts_with("text/html") {
        mett_core::BodyKind::Html
    } else {
        mett_core::BodyKind::Text
    };
    Some(text::body_lines(&mett_core::Body { kind, content }))
}

/// A content type without its parameters (`text/calendar; charset=...`),
/// in lower case.
fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// A safe file name for an attachment: its name without directories, or
/// a name for one without.
fn file_name(attachment: &Attachment) -> String {
    let name = attachment
        .name
        // Nor a drive (`C:report.pdf`), on Windows.
        .rsplit(['/', '\\', ':'])
        .next()
        .unwrap_or_default()
        .trim();
    // An attached item is named by its kind of content.
    let extension = match media_type(&attachment.content_type).as_str() {
        "message/rfc822" => Some("eml"),
        "text/calendar" => Some("ics"),
        "text/vcard" => Some("vcf"),
        _ => None,
    }
    .filter(|_| attachment.kind == AttachmentKind::Item);
    match (name, extension) {
        ("" | "." | "..", Some(extension)) => format!("item.{extension}"),
        ("" | "." | "..", None) => "attachment".into(),
        (name, Some(extension)) if !name.to_lowercase().ends_with(&format!(".{extension}")) => {
            format!("{name}.{extension}")
        }
        (name, _) => name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use mett_core::{AttachmentId, mock};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::app::testing::*;
    use crate::app::{Msg, Settings};
    use crate::worker::Event;

    #[test]
    fn text_is_decoded_in_its_charset() {
        assert_eq!(
            decode("text/plain; charset=windows-1252", b"caf\xe9").as_deref(),
            Some("caf\u{e9}")
        );
        assert_eq!(
            decode("text/plain; charset=\"UTF-8\"", "café".as_bytes()).as_deref(),
            Some("café")
        );
        assert_eq!(
            decode("text/plain", b"caf\xe9").as_deref(),
            Some("caf\u{e9}")
        );
        assert_eq!(decode("text/plain; charset=x-unknown", b"x"), None);
    }

    fn app() -> App {
        let settings = Settings {
            home: Some("/home/alice".into()),
            ..Settings::default()
        };
        let mut message = mock::message("m0", "in", "Files", DateTime::UNIX_EPOCH).summary;
        message.has_attachments = true;
        opened(settings, 24, vec![message], None).0
    }

    fn attachment(id: &str, name: &str, content_type: &str, kind: AttachmentKind) -> Attachment {
        Attachment {
            id: AttachmentId::new(id),
            name: name.into(),
            content_type: content_type.into(),
            size: 5,
            inline: false,
            kind,
        }
    }

    fn notes() -> Attachment {
        attachment("a0", "notes.txt", "text/plain", AttachmentKind::File)
    }

    /// The attachment menu of m0, with a text file, a PDF and a link.
    fn menu() -> App {
        let mut app = app();
        let (id, request) = sent(&act(&mut app, Action::ViewAttachments));
        assert_eq!(request, Request::ListAttachments(MessageId::new("m0")));
        respond(
            &mut app,
            id,
            Event::AttachmentsListed(Ok(vec![
                notes(),
                attachment("a1", "report.pdf", "application/pdf", AttachmentKind::File),
                attachment(
                    "a2",
                    "shared",
                    "application/octet-stream",
                    AttachmentKind::Link,
                ),
            ])),
        );
        app
    }

    #[test]
    fn attachments_are_listed_and_fetched_again_after_a_sign_in() {
        let mut app = app();
        let (id, _) = sent(&act(&mut app, Action::ViewAttachments));
        respond(&mut app, id, Event::AttachmentsListed(Err(expired())));
        assert!(matches!(app.screen(), Screen::Attachments(_)));
        let failed = |app: &mut App| {
            app.update(Msg::SignIn(crate::signin::SignInEvent::Finished {
                account: mett_core::AccountId::new("me@contoso.com"),
                outcome: Err("expired".into()),
            }));
        };
        failed(&mut app);
        let (id, request) = sent(&act(&mut app, Action::Retry));
        assert_eq!(request, Request::ListAttachments(MessageId::new("m0")));
        respond(&mut app, id, Event::AttachmentsListed(Ok(vec![notes()])));
        let (id, _) = sent(&act(&mut app, Action::DisplayMessage));
        respond(&mut app, id, Event::AttachmentFetched(Err(expired())));
        failed(&mut app);
        let (_, request) = sent(&act(&mut app, Action::Retry));
        assert!(
            matches!(request, Request::FetchAttachment(m, a) if m.as_str() == "m0" && a.as_str() == "a0")
        );
    }

    #[test]
    fn the_pager_keys_scroll_a_text_attachment() {
        let mut app = menu();
        app.update(Msg::Resize {
            width: 80,
            height: 5,
        });
        app.screens.push(Screen::Text(TextView::new(
            "notes".into(),
            (0..10).map(|i| i.to_string()).collect(),
            80,
        )));
        let scroll = |app: &App| match app.screen() {
            Screen::Text(view) => view.scroll,
            _ => panic!("expected the text"),
        };
        act(&mut app, Action::NextEntry);
        assert_eq!(scroll(&app), 1);
        act(&mut app, Action::LastEntry);
        assert_eq!(scroll(&app), 8);
        act(&mut app, Action::PreviousEntry);
        assert_eq!(scroll(&app), 7);
        act(&mut app, Action::FirstEntry);
        assert_eq!(scroll(&app), 0);
        // Search isn't silently ignored.
        act(&mut app, Action::Search);
        assert!(app.status.contains("isn't supported"), "{}", app.status);
        assert!(app.input.is_none());
    }

    #[test]
    fn attached_items_are_named_by_their_kind() {
        let item = |name: &str, content_type: &str| {
            file_name(&attachment("i", name, content_type, AttachmentKind::Item))
        };
        assert_eq!(item("Budget v1.2", "message/rfc822"), "Budget v1.2.eml");
        assert_eq!(item("Lunch.ICS", "text/calendar"), "Lunch.ICS");
        assert_eq!(item("", "text/vcard"), "item.vcf");
        let file = |name: &str| {
            file_name(&attachment(
                "f",
                name,
                "application/pdf",
                AttachmentKind::File,
            ))
        };
        assert_eq!(file("C:report.pdf"), "report.pdf");
        assert_eq!(file("../a\\b/c.pdf"), "c.pdf");
        assert_eq!(item("Lunch", "Text/Calendar; charset=utf-8"), "Lunch.ics");
    }

    #[test]
    fn views_text_attachments_in_mett() {
        let mut app = menu();
        let (id, request) = sent(&act(&mut app, Action::ViewAttach));
        assert_eq!(
            request,
            Request::FetchAttachment(MessageId::new("m0"), AttachmentId::new("a0"))
        );
        respond(
            &mut app,
            id,
            Event::AttachmentFetched(Ok(b"hello\nworld".to_vec())),
        );
        let Screen::Text(view) = app.screen() else {
            panic!("expected text, got {:?}", app.screen());
        };
        assert_eq!(view.lines, ["hello", "world"]);
        act(&mut app, Action::Exit);
        assert!(matches!(app.screen(), Screen::Attachments(_)));
        act(&mut app, Action::Exit);
        assert_eq!(app.screen(), &Screen::Index);
    }

    #[test]
    fn opens_other_attachments_with_the_system() {
        let mut app = menu();
        act(&mut app, Action::NextEntry);
        let (id, _) = sent(&act(&mut app, Action::ViewAttach));
        let effects = respond(&mut app, id, Event::AttachmentFetched(Ok(b"%PDF".to_vec())));
        assert_eq!(
            effects,
            [Effect::OpenFile {
                name: "report.pdf".into(),
                data: b"%PDF".to_vec(),
            }]
        );
        // A link has nothing to fetch.
        act(&mut app, Action::NextEntry);
        assert!(act(&mut app, Action::ViewAttach).is_empty());
        assert!(app.status.contains("link"), "{}", app.status);
    }

    #[test]
    fn saves_attachments_asking_before_replacing_a_file() {
        let mut app = menu();
        act(&mut app, Action::SaveEntry);
        let Some(Input::Line { editor, .. }) = &app.input else {
            panic!("expected a question");
        };
        assert_eq!(editor.text(), "notes.txt");
        // `~` is the home directory.
        app.input = None;
        let (id, _) = sent(&app.answer_save_attachment("~/notes.txt".into()));
        let effects = respond(
            &mut app,
            id,
            Event::AttachmentFetched(Ok(b"hello".to_vec())),
        );
        let path = PathBuf::from("/home/alice/notes.txt");
        assert_eq!(
            effects,
            [Effect::SaveFile {
                op: FileOp(1),
                path: path.clone(),
                data: b"hello".to_vec(),
                overwrite: false,
            }]
        );
        app.update(Msg::FileSaved(
            FileOp(1),
            FileOutcome::Exists(path.clone(), b"hello".to_vec()),
        ));
        let effects = key(&mut app, KeyCode::Char('y'));
        assert_eq!(
            effects,
            [Effect::SaveFile {
                op: FileOp(2),
                path: path.clone(),
                data: b"hello".to_vec(),
                overwrite: true,
            }]
        );
        app.update(Msg::FileSaved(FileOp(2), FileOutcome::Done(path)));
        assert_eq!(app.status, "Saved to /home/alice/notes.txt.");
    }

    #[test]
    fn attachments_listed_under_the_help_screen_still_show() {
        let mut app = app();
        let (id, _) = sent(&act(&mut app, Action::ViewAttachments));
        // Keys wait for the listing.
        assert!(app.waiting());
        app.screens.push(Screen::Help(crate::app::Help {
            menu: crate::keymap::Menu::Attach,
            scroll: 0,
        }));
        respond(&mut app, id, Event::AttachmentsListed(Ok(vec![notes()])));
        assert!(!app.waiting());
        app.screens.pop();
        let Screen::Attachments(menu) = app.screen() else {
            panic!("expected the menu");
        };
        assert_eq!(menu.entries.len(), 1);
    }

    #[test]
    fn a_message_without_attachments_says_so() {
        let mut app = app();
        let (id, _) = sent(&act(&mut app, Action::ViewAttachments));
        respond(&mut app, id, Event::AttachmentsListed(Ok(Vec::new())));
        assert_eq!(app.screen(), &Screen::Index);
        assert_eq!(app.status, "This message has no attachments.");
    }

    #[test]
    fn attaches_and_detaches_files_in_the_compose_menu() {
        let mut app = app();
        app.screens.push(Screen::Compose(super::super::Compose::new(
            mett_core::Draft::default(),
            false,
        )));
        act(&mut app, Action::AttachFile);
        app.input = None;
        let effects = app.answer_attach_file("~/report.pdf".into());
        let path = PathBuf::from("/home/alice/report.pdf");
        assert_eq!(effects, [Effect::ReadFile(FileOp(1), path.clone())]);
        // Keys wait for it.
        assert!(key(&mut app, KeyCode::Char('q')).is_empty());
        assert!(matches!(app.screen(), Screen::Compose(_)));
        assert!(app.status.starts_with("Waiting for /home/alice/report.pdf"));
        app.update(Msg::FileRead(FileOp(1), path, Ok(b"%PDF".to_vec())));
        let Screen::Compose(compose) = app.screen() else {
            panic!("expected the compose menu");
        };
        assert_eq!(compose.draft.attachments[0].name, "report.pdf");
        assert_eq!(compose.draft.attachments[0].content_type, "application/pdf");
        assert_eq!(compose.selected, 1);
        // The text can't be detached; the file can.
        act(&mut app, Action::PreviousEntry);
        act(&mut app, Action::DetachFile);
        assert_eq!(app.status, "Only attached files can be detached.");
        act(&mut app, Action::NextEntry);
        act(&mut app, Action::DetachFile);
        let Screen::Compose(compose) = app.screen() else {
            panic!("expected the compose menu");
        };
        assert!(compose.draft.attachments.is_empty());
        // A file that can't be read is said.
        app.answer_attach_file("/nope".into());
        app.update(Msg::FileRead(
            FileOp(2),
            PathBuf::from("/nope"),
            Err("not found".into()),
        ));
        assert_eq!(app.status, "/nope: not found");
        // One given up on (^G) is left out, when it comes after all.
        app.answer_attach_file("/slow".into());
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.status, "Not waiting for /slow any more.");
        // Asked for again: only the new reading counts.
        app.answer_attach_file("/slow".into());
        app.update(Msg::FileRead(
            FileOp(3),
            PathBuf::from("/slow"),
            Ok(b"old".to_vec()),
        ));
        let Screen::Compose(compose) = app.screen() else {
            panic!("expected the compose menu");
        };
        assert!(compose.draft.attachments.is_empty());
        app.update(Msg::FileRead(
            FileOp(4),
            PathBuf::from("/slow"),
            Ok(b"new".to_vec()),
        ));
        let Screen::Compose(compose) = app.screen() else {
            panic!("expected the compose menu");
        };
        assert_eq!(compose.draft.attachments[0].data, b"new");
    }
}
