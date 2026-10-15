//! Writing messages: the text and recipients of new mail, replies and
//! forwards, as mutt builds them. Everything here is pure, so the app's
//! compose flow is tested without an editor or a mailbox.

use chrono::Local;
use mett_core::{AccountId, Address, Message};

use crate::config::{Config, Quad};
use crate::text;

/// The settings that shape new messages, from the configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The `From:` address (`from` and `realname`), if set.
    pub from: Option<Address>,
    /// The user's own addresses, in lower case, left out of group replies
    /// (unless `metoo`).
    pub me: Vec<String>,
    /// mutt's `alternates`: regular expressions for the user's other
    /// addresses, such as a mail address that differs from the account's
    /// name. They match ignoring case.
    pub alternates: Vec<String>,
    /// The signature's text, if there is one.
    pub signature: Option<String>,
    /// `sig_dashes`: put `-- ` before the signature.
    pub sig_dashes: bool,
    /// `attribution`: the line before quoted text.
    pub attribution: String,
    /// `indent_string`: the prefix of quoted lines.
    pub indent_string: String,
    /// `forward_format`: the subject of forwarded messages.
    pub forward_format: String,
    /// `include`: quote the message in replies.
    pub include: Quad,
    /// `reply_to`: reply to the `Reply-To:` address.
    pub reply_to: Quad,
    /// `postpone`: postpone a message when leaving the compose menu.
    pub postpone: Quad,
    /// `abort_unmodified`: abort if the message wasn't edited.
    pub abort_unmodified: Quad,
    /// `abort_nosubject`: abort if the subject is empty.
    pub abort_nosubject: Quad,
    /// `copy`: keep a copy of sent messages.
    pub copy: Quad,
    /// `askcc`: ask for `Cc:` recipients.
    pub askcc: bool,
    /// `askbcc`: ask for `Bcc:` recipients.
    pub askbcc: bool,
    /// `fast_reply`: don't ask for recipients and subject when replying.
    pub fast_reply: bool,
    /// `metoo`: keep the user's own addresses in group replies.
    pub metoo: bool,
}

impl Default for Settings {
    /// mutt's defaults.
    fn default() -> Self {
        Self {
            from: None,
            me: Vec::new(),
            alternates: Vec::new(),
            signature: None,
            sig_dashes: true,
            attribution: "On %d, %n wrote:".into(),
            indent_string: "> ".into(),
            forward_format: "[%a: %s]".into(),
            include: Quad::AskYes,
            reply_to: Quad::AskYes,
            postpone: Quad::AskYes,
            abort_unmodified: Quad::Yes,
            abort_nosubject: Quad::AskYes,
            copy: Quad::Yes,
            askcc: false,
            askbcc: false,
            fast_reply: false,
            metoo: false,
        }
    }
}

impl Settings {
    /// How to write messages, from the configuration. `account` is the
    /// signed-in user, whose address counts as the user's own.
    pub fn from_config(config: &Config, account: &AccountId) -> Result<Self, String> {
        let realname = config.string("realname");
        let from = match config.string("from") {
            // Without `from`, the signed-in account is the sender; it still
            // takes `realname`.
            "" if realname.is_empty() => None,
            "" => Some(Address::new(account.as_str())),
            from => Some(Address::parse(from).map_err(|e| format!("from: {e}"))?),
        };
        let from = from.map(|mut address| {
            if address.name.is_none() && !realname.is_empty() {
                address.name = Some(realname.to_owned());
            }
            address
        });
        let mut me = vec![account.as_str().to_lowercase()];
        me.extend(from.iter().map(|a| a.email.to_lowercase()));
        // Without `use_from`, the address still counts as the user's own, but
        // messages carry no explicit sender: Graph uses the signed-in user.
        let from = from.filter(|_| config.flag("use_from"));
        let signature = match config.string("signature") {
            "" => None,
            path if path.ends_with('|') => {
                return Err(format!(
                    "signature: commands ({path}) are not supported yet; use a file"
                ));
            }
            path => match std::fs::read_to_string(path) {
                Ok(text) => Some(text),
                // mutt's default, ~/.signature, often doesn't exist.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("signature: cannot read {path}: {e}")),
            },
        };
        Ok(Settings {
            from,
            me,
            alternates: config.alternates().to_vec(),
            signature,
            sig_dashes: config.flag("sig_dashes"),
            attribution: config.string("attribution").to_owned(),
            indent_string: config.string("indent_string").to_owned(),
            forward_format: config.string("forward_format").to_owned(),
            include: config.quad("include"),
            reply_to: config.quad("reply_to"),
            postpone: config.quad("postpone"),
            abort_unmodified: config.quad("abort_unmodified"),
            abort_nosubject: config.quad("abort_nosubject"),
            copy: config.quad("copy"),
            askcc: config.flag("askcc"),
            askbcc: config.flag("askbcc"),
            fast_reply: config.flag("fast_reply"),
            metoo: config.flag("metoo"),
        })
    }

    fn is_me(&self, address: &Address) -> bool {
        let email = address.email.to_lowercase();
        self.me.contains(&email)
            || self.alternates.iter().any(|alternate| {
                regex::RegexBuilder::new(alternate)
                    .case_insensitive(true)
                    .build()
                    .is_ok_and(|regex| regex.is_match(&address.email))
            })
    }
}

/// What is being written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// New mail (`m`).
    New,
    /// A reply to the sender (`r`).
    Reply,
    /// A reply to everyone (`g`).
    GroupReply,
    /// A forward (`f`).
    Forward,
}

/// Expands mutt's `%` expandos for `message` in `format`: `%n` (the
/// author's name, or address), `%a` (the author's address), `%s` (the
/// subject), `%d` (the date) and `%%`. Unknown expandos are kept.
pub fn expand(format: &str, message: &Message) -> String {
    let summary = &message.summary;
    let mut out = String::new();
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push_str(
                summary
                    .from
                    .as_ref()
                    .map_or("", |a| a.name.as_deref().unwrap_or(&a.email)),
            ),
            Some('a') => out.push_str(summary.from.as_ref().map_or("", |a| a.email.as_str())),
            Some('s') => out.push_str(&summary.subject),
            Some('d') => out.push_str(
                &summary
                    .received
                    .with_timezone(&Local)
                    // mutt's default `date_format`.
                    .format("%a, %b %d, %Y at %I:%M:%S%p %Z")
                    .to_string(),
            ),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// The subject of a reply: `Re: ` before the original, unless it is
/// already there.
pub fn reply_subject(original: &str) -> String {
    let has_prefix = original
        .get(..3)
        .is_some_and(|p| p.eq_ignore_ascii_case("re:"));
    if has_prefix {
        original.to_owned()
    } else {
        format!("Re: {original}")
    }
}

/// The original message's text, quoted, after the attribution line.
pub fn quoted(message: &Message, settings: &Settings) -> String {
    let mut out = expand(&settings.attribution, message);
    out.push('\n');
    for line in text::body_lines(&message.body) {
        // Quoting an empty line adds no trailing space, as mutt does.
        let prefix = if line.is_empty() {
            settings.indent_string.trim_end()
        } else {
            &settings.indent_string
        };
        out.push_str(prefix);
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// The original message, inline, as mutt forwards it.
pub fn forwarded(message: &Message) -> String {
    let summary = &message.summary;
    let from = summary.from.as_ref().map(ToString::to_string);
    let mut out = format!(
        "----- Forwarded message from {} -----\n\n",
        from.as_deref().unwrap_or("unknown sender")
    );
    // The pager's headers, but never the blind recipients, then the
    // body.
    let lines = text::header_lines(message, false)
        .into_iter()
        .chain([String::new()])
        .chain(text::body_lines(&message.body));
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("\n----- End forwarded message -----\n");
    out
}

/// Appends the signature to `body`.
pub fn signed(mut body: String, settings: &Settings) -> String {
    if let Some(signature) = &settings.signature {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push('\n');
        if settings.sig_dashes {
            body.push_str("-- \n");
        }
        body.push_str(signature);
        if !signature.ends_with('\n') {
            body.push('\n');
        }
    }
    body
}

/// The recipients of a reply to `message`: `(to, cc)`.
///
/// A reply goes to the `Reply-To:` addresses if `use_reply_to` (and there
/// are any), otherwise to the sender; a reply to one's own message goes to
/// its recipients instead. A group reply also copies the original
/// recipients, except the user (unless `metoo`). Nobody appears twice.
pub fn reply_recipients(
    message: &Message,
    use_reply_to: bool,
    group: bool,
    settings: &Settings,
) -> (Vec<Address>, Vec<Address>) {
    let summary = &message.summary;
    let mut to: Vec<Address> = if use_reply_to && !message.reply_to.is_empty() {
        message.reply_to.clone()
    } else {
        summary.from.iter().cloned().collect()
    };
    // A reply to one's own message (by its sender, not its Reply-To,
    // which may point at the user) goes to its recipients instead.
    let from_me = summary.from.as_ref().is_some_and(|a| settings.is_me(a));
    if from_me && to.iter().all(|a| settings.is_me(a)) {
        // Leave the user out too (unless `metoo`): to the other To:
        // recipients, else the Cc: ones, else the Bcc: ones (a sent copy
        // has them), else back to the user.
        let others = |list: &[Address]| -> Vec<Address> {
            list.iter()
                .filter(|a| settings.metoo || !settings.is_me(a))
                .cloned()
                .collect()
        };
        to = [
            others(&summary.to),
            others(&message.cc),
            others(&message.bcc),
        ]
        .into_iter()
        .find(|list| !list.is_empty())
        .unwrap_or_else(|| summary.to.clone());
    }
    let mut cc = Vec::new();
    if group {
        let others = summary.to.iter().chain(&message.cc);
        cc.extend(
            others
                .filter(|a| settings.metoo || !settings.is_me(a))
                .cloned(),
        );
    }
    // Keep the first of any duplicates, To before Cc.
    let mut seen = Vec::new();
    let mut unique = |list: Vec<Address>| {
        list.into_iter()
            .filter(|a| {
                let email = a.email.to_lowercase();
                let new = !seen.contains(&email);
                seen.push(email);
                new
            })
            .collect::<Vec<_>>()
    };
    let to = unique(to);
    let cc = unique(cc);
    (to, cc)
}

/// Whether `message` asks for replies to go somewhere other than its
/// sender, so that mutt's `reply_to` question applies.
pub fn has_distinct_reply_to(message: &Message) -> bool {
    let from = message
        .summary
        .from
        .as_ref()
        .map(|a| a.email.to_lowercase());
    !message.reply_to.is_empty()
        && message
            .reply_to
            .iter()
            .any(|a| Some(a.email.to_lowercase()) != from)
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use mett_core::mock;

    use super::*;

    fn message() -> Message {
        // Mid-year at noon, so the local date is the same in any zone.
        let date = DateTime::parse_from_rfc3339("2020-06-15T12:00:00Z").unwrap();
        let mut message = mock::message("m1", "in", "Lunch", date.to_utc());
        message.summary.from = Some(Address::new("bob@x.org").with_name("Bob"));
        message.summary.to = vec![Address::new("me@contoso.com"), Address::new("carol@x.org")];
        message.cc = vec![Address::new("dave@x.org"), Address::new("BOB@x.org")];
        message.body.content = "Shall we?\n\nNoon.".into();
        message
    }

    fn settings() -> Settings {
        Settings {
            me: vec!["me@contoso.com".into()],
            ..Settings::default()
        }
    }

    fn config(source: &str) -> Config {
        let mut config = Config::new(Some("/nonexistent-home".into()));
        config.load_str(source, std::path::Path::new("mettrc"));
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        config
    }

    #[test]
    fn reads_the_settings_from_the_configuration() {
        let account = AccountId::new("Me@Contoso.com");
        let settings = Settings::from_config(
            &config("set from=other@x.org realname='Me Myself' include=no askcc\n"),
            &account,
        )
        .unwrap();
        assert_eq!(
            settings.from,
            Some(Address::new("other@x.org").with_name("Me Myself"))
        );
        assert_eq!(settings.me, ["me@contoso.com", "other@x.org"]);
        assert_eq!(settings.signature, None, "~/.signature doesn't exist");
        assert_eq!(settings.include, Quad::No);
        assert!(settings.askcc);
        assert_eq!(settings.attribution, "On %d, %n wrote:");

        // Without use_from, messages have no explicit sender.
        let settings =
            Settings::from_config(&config("set from=other@x.org nouse_from\n"), &account).unwrap();
        assert_eq!(settings.from, None);
        assert!(settings.me.contains(&"other@x.org".to_owned()));

        let settings = Settings::from_config(
            &config("set realname='Me Myself'\nalternates '^me@x\\.org$' -group work other\n"),
            &account,
        )
        .unwrap();
        assert_eq!(settings.alternates, ["^me@x\\.org$", "other"]);
        assert_eq!(
            settings.from,
            Some(Address::new("Me@Contoso.com").with_name("Me Myself"))
        );

        assert!(Settings::from_config(&config("set from=nobody\n"), &account).is_err());
        assert!(Settings::from_config(&config("set signature='sig.sh|'\n"), &account).is_err());
    }

    #[test]
    fn expands_attribution_expandos() {
        let message = message();
        assert_eq!(
            expand("%n <%a> wrote about %s, 100%%:", &message),
            "Bob <bob@x.org> wrote about Lunch, 100%:"
        );
        assert!(expand("%d", &message).contains("Jun 15, 2020"));
        assert_eq!(expand("%q %", &message), "%q %");
    }

    #[test]
    fn quotes_the_original() {
        let settings = Settings {
            attribution: "%n wrote:".into(),
            ..settings()
        };
        assert_eq!(
            quoted(&message(), &settings),
            "Bob wrote:\n> Shall we?\n>\n> Noon.\n"
        );
    }

    #[test]
    fn prefixes_reply_subjects_once() {
        assert_eq!(reply_subject("Lunch"), "Re: Lunch");
        assert_eq!(reply_subject("RE: Lunch"), "RE: Lunch");
        assert_eq!(reply_subject(""), "Re: ");
    }

    #[test]
    fn appends_the_signature() {
        let settings = Settings {
            signature: Some("Alice".into()),
            ..settings()
        };
        assert_eq!(signed(String::new(), &settings), "\n-- \nAlice\n");
        assert_eq!(signed("Hi".into(), &settings), "Hi\n\n-- \nAlice\n");
        let no_dashes = Settings {
            sig_dashes: false,
            ..settings
        };
        assert_eq!(signed("Hi\n".into(), &no_dashes), "Hi\n\nAlice\n");
        assert_eq!(signed("Hi\n".into(), &self::settings()), "Hi\n");
    }

    #[test]
    fn replies_go_to_the_sender_or_reply_to() {
        let mut message = message();
        let (to, cc) = reply_recipients(&message, true, false, &settings());
        assert_eq!(to, [Address::new("bob@x.org").with_name("Bob")]);
        assert!(cc.is_empty());

        message.reply_to = vec![Address::new("list@x.org")];
        assert!(has_distinct_reply_to(&message));
        let (to, _) = reply_recipients(&message, true, false, &settings());
        assert_eq!(to, [Address::new("list@x.org")]);
        let (to, _) = reply_recipients(&message, false, false, &settings());
        assert_eq!(to[0].email, "bob@x.org");
    }

    #[test]
    fn a_reply_to_address_of_mine_is_kept() {
        let mut message = message();
        message.reply_to = vec![Address::new("me@contoso.com")];
        let (to, _) = reply_recipients(&message, true, false, &settings());
        assert_eq!(to, [Address::new("me@contoso.com")]);
    }

    #[test]
    fn group_replies_copy_everyone_but_me_once() {
        let (to, cc) = reply_recipients(&message(), true, true, &settings());
        assert_eq!(to[0].email, "bob@x.org");
        let cc: Vec<_> = cc.iter().map(|a| a.email.as_str()).collect();
        assert_eq!(cc, ["carol@x.org", "dave@x.org"]);

        let metoo = Settings {
            metoo: true,
            ..settings()
        };
        let (_, cc) = reply_recipients(&message(), true, true, &metoo);
        assert_eq!(cc[0].email, "me@contoso.com");
    }

    #[test]
    fn alternates_count_as_me() {
        let settings = Settings {
            alternates: vec!["^isaac\\..*@contoso\\.com$".into()],
            ..settings()
        };
        let mut message = message();
        message.summary.from = Some(Address::new("Isaac.Gelado@contoso.com"));
        let (to, _) = reply_recipients(&message, true, false, &settings);
        assert!(to.iter().all(|a| a.email != "Isaac.Gelado@contoso.com"));
    }

    #[test]
    fn replying_to_my_own_message_goes_to_its_recipients() {
        let mut message = message();
        message.summary.from = Some(Address::new("Me@Contoso.com"));
        let (to, cc) = reply_recipients(&message, true, true, &settings());
        let to: Vec<_> = to.iter().map(|a| a.email.as_str()).collect();
        assert_eq!(to, ["carol@x.org"], "without me");
        let cc: Vec<_> = cc.iter().map(|a| a.email.as_str()).collect();
        assert_eq!(cc, ["dave@x.org", "BOB@x.org"]);
        // Sent to myself, copying others: to them.
        message.summary.to = vec![Address::new("me@contoso.com")];
        let (to, _) = reply_recipients(&message, true, false, &settings());
        assert_eq!(to[0].email, "dave@x.org");
        // Sent only to Bcc recipients: to them.
        message.cc.clear();
        message.bcc = vec![Address::new("eve@x.org")];
        let (to, _) = reply_recipients(&message, true, false, &settings());
        assert_eq!(to, [Address::new("eve@x.org")]);
        // A note to self goes back to self.
        message.bcc.clear();
        let (to, _) = reply_recipients(&message, true, false, &settings());
        assert_eq!(to, [Address::new("me@contoso.com")]);
    }

    #[test]
    fn forwards_never_show_blind_recipients() {
        let mut message = message();
        message.bcc = vec![Address::new("secret@x.org")];
        assert!(!forwarded(&message).contains("secret@x.org"));
        assert!(
            text::pager_lines(&message)
                .iter()
                .any(|l| l.contains("secret@x.org"))
        );
    }

    #[test]
    fn forwards_inline() {
        let text = forwarded(&message());
        assert!(text.starts_with("----- Forwarded message from Bob <bob@x.org> -----\n\n"));
        assert!(text.contains("Subject: Lunch\n\nShall we?\n\nNoon.\n"));
        assert!(text.ends_with("----- End forwarded message -----\n"));
    }
}
