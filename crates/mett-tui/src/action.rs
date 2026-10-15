//! Named user actions: the Command pattern (`docs/DESIGN.md` §5.3).
//!
//! Every user-visible operation is an [`Action`] with a mutt-style name, such
//! as `next-entry`. Keys map to actions through a [`crate::keymap::Keymap`],
//! so bindings, macros and the help line all work in terms of actions.

/// Defines [`Action`] together with its names and descriptions, so the
/// three can't drift apart.
macro_rules! actions {
    ($($variant:ident => $name:literal, $description:literal;)*) => {
        /// A user-visible operation.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Action {
            $(
                #[doc = $description]
                $variant,
            )*
        }

        // The names are for `bind` and `macro` in mettrc, which comes next.
        #[allow(dead_code)]
        impl Action {
            /// Every action.
            pub const ALL: &[Action] = &[$(Action::$variant),*];

            /// The mutt-style function name, as used in `bind` (without the
            /// angle brackets).
            pub fn name(self) -> &'static str {
                match self {
                    $(Action::$variant => $name,)*
                }
            }

            /// A short description, for help screens.
            pub fn description(self) -> &'static str {
                match self {
                    $(Action::$variant => $description,)*
                }
            }

            /// Looks an action up by its mutt-style name.
            pub fn from_name(name: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|a| a.name() == name)
            }
        }
    };
}

actions! {
    Quit => "quit", "Quit mett";
    NextEntry => "next-entry", "Move to the next entry";
    PreviousEntry => "previous-entry", "Move to the previous entry";
    FirstEntry => "first-entry", "Move to the first entry";
    LastEntry => "last-entry", "Move to the last entry";
    NextPage => "next-page", "Move to the next page";
    PreviousPage => "previous-page", "Move to the previous page";
    DisplayMessage => "display-message", "Display the selected message";
    Exit => "exit", "Leave the current menu";
    NextLine => "next-line", "Scroll down one line";
    PreviousLine => "previous-line", "Scroll up one line";
    OpenBrowser => "open-browser", "Open the sign-in page in a browser";
    Retry => "retry", "Try signing in again";
    CopyLink => "copy-link", "Copy the sign-in link or code to the clipboard";
    Mail => "mail", "Compose a new message";
    Reply => "reply", "Reply to the message";
    GroupReply => "group-reply", "Reply to all recipients";
    Forward => "forward-message", "Forward the message";
    SendMessage => "send-message", "Send the message";
    PostponeMessage => "postpone-message", "Save the message to send later";
    EditMessage => "edit-message", "Edit the message";
    EditTo => "edit-to", "Edit the To: list";
    EditCc => "edit-cc", "Edit the Cc: list";
    EditBcc => "edit-bcc", "Edit the Bcc: list";
    EditSubject => "edit-subject", "Edit the subject";
    DeleteMessage => "delete-message", "Mark the message for deletion";
    UndeleteMessage => "undelete-message", "Unmark the message for deletion";
    SyncMailbox => "sync-mailbox", "Purge the messages marked for deletion";
    FlagMessage => "flag-message", "Toggle the message's follow-up flag";
    ToggleNew => "toggle-new", "Toggle whether the message is new";
    SaveMessage => "save-message", "Move the message to another folder";
    CopyMessage => "copy-message", "Copy the message to another folder";
    ChangeFolder => "change-folder", "Open another folder";
    BrowseMailboxes => "browse-mailboxes", "Choose a folder from a list";
    ToggleMailboxes => "toggle-mailboxes", "Switch between the mailboxes and every folder";
    NextThread => "next-thread", "Move to the next thread";
    PreviousThread => "previous-thread", "Move to the previous thread";
    CollapseThread => "collapse-thread", "Collapse or uncollapse the current thread";
    CollapseAll => "collapse-all", "Collapse or uncollapse every thread";
    Query => "query", "Find addresses with query_command or the directory";
    CreateAlias => "create-alias", "Save the sender as an alias";
    ViewAttachments => "view-attachments", "Show the message's attachments";
    ViewAttach => "view-attach", "View the attachment";
    SaveEntry => "save-entry", "Save the attachment to a file";
    AttachFile => "attach-file", "Attach a file to the message";
    DetachFile => "detach-file", "Take the attached file off the message";
    Search => "search", "Search with a pattern (in the pager, the text)";
    SearchNext => "search-next", "Find the next match of the last search";
    Limit => "limit", "Show only the messages matching a pattern";
    Help => "help", "Show the key bindings";
    EnterCommand => "enter-command", "Run a mettrc command";
    Noop => "noop", "Do nothing (unbinds a key)";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_round_trip() {
        for action in Action::ALL {
            assert_eq!(Action::from_name(action.name()), Some(*action));
            let same_name = Action::ALL.iter().filter(|a| a.name() == action.name());
            assert_eq!(same_name.count(), 1, "duplicate name {}", action.name());
        }
        assert_eq!(Action::from_name("no-such-function"), None);
    }
}
