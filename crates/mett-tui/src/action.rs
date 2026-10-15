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
