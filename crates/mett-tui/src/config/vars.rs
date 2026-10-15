//! The variables mett knows, with their types and defaults.
//!
//! These are the mutt variables mett uses and the `msgraph_*` variables
//! from `docs/CONFIG-OFFICE365.md` §4. Unknown variables produce a warning,
//! not an error (principle 4 of that document).

use std::fmt;

/// A variable's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `yes` or `no`.
    Bool,
    /// An integer.
    Number,
    /// Free text.
    String,
    /// A file or mailbox path: a leading `~` is expanded when it is set.
    Path,
    /// A quad-option: `yes`, `no`, `ask-yes` or `ask-no`.
    Quad,
}

/// A quad-option's value: do it, don't, or ask with a default answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quad {
    /// Always.
    Yes,
    /// Never.
    No,
    /// Ask, suggesting yes.
    AskYes,
    /// Ask, suggesting no.
    AskNo,
}

impl Quad {
    /// The same option with the opposite answer, as `toggle` does.
    pub fn toggled(self) -> Self {
        match self {
            Quad::Yes => Quad::No,
            Quad::No => Quad::Yes,
            Quad::AskYes => Quad::AskNo,
            Quad::AskNo => Quad::AskYes,
        }
    }
}

impl fmt::Display for Quad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Quad::Yes => "yes",
            Quad::No => "no",
            Quad::AskYes => "ask-yes",
            Quad::AskNo => "ask-no",
        })
    }
}

/// A variable's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// A boolean.
    Bool(bool),
    /// A number.
    Number(i64),
    /// A string or path.
    Str(String),
    /// A quad-option.
    Quad(Quad),
}

impl fmt::Display for Value {
    /// Formats the value the way mutt writes it in `set` commands.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Bool(true) => f.write_str("yes"),
            Value::Bool(false) => f.write_str("no"),
            Value::Number(n) => write!(f, "{n}"),
            Value::Quad(q) => write!(f, "{q}"),
            // Escaped so that reading it back gives the same text: no
            // expansion, no command substitution, and newlines kept.
            Value::Str(s) => {
                f.write_str("\"")?;
                for c in s.chars() {
                    match c {
                        '\\' | '"' | '$' | '`' => write!(f, "\\{c}")?,
                        '\n' => f.write_str("\\n")?,
                        '\r' => f.write_str("\\r")?,
                        '\t' => f.write_str("\\t")?,
                        c => write!(f, "{c}")?,
                    }
                }
                f.write_str("\"")
            }
        }
    }
}

/// A known variable.
#[derive(Clone, Copy, Debug)]
pub struct Var {
    /// Its name.
    pub name: &'static str,
    /// Its type.
    pub kind: Kind,
    /// Its default, as it would be written in a mettrc.
    pub default: &'static str,
}

const fn var(name: &'static str, kind: Kind, default: &'static str) -> Var {
    Var {
        name,
        kind,
        default,
    }
}

/// Every variable mett knows, sorted by name.
pub const VARS: &[Var] = &[
    var("abort_nosubject", Kind::Quad, "ask-yes"),
    var("abort_unmodified", Kind::Quad, "yes"),
    var("askbcc", Kind::Bool, "no"),
    var("askcc", Kind::Bool, "no"),
    var("attribution", Kind::String, "On %d, %n wrote:"),
    var("copy", Kind::Quad, "yes"),
    var(
        "date_format",
        Kind::String,
        "!%a, %b %d, %Y at %I:%M:%S%p %Z",
    ),
    var("delete", Kind::Quad, "ask-yes"),
    var("editor", Kind::Path, ""),
    var("fast_reply", Kind::Bool, "no"),
    var("folder", Kind::Path, "~/Mail"),
    var("forward_format", Kind::String, "[%a: %s]"),
    var("from", Kind::String, ""),
    var("include", Kind::Quad, "ask-yes"),
    var("indent_string", Kind::String, "> "),
    var("index_format", Kind::String, "%4C %Z %{%b %d} %-15.15L  %s"),
    var("mail_check", Kind::Number, "5"),
    var("mbox", Kind::Path, "~/mbox"),
    var("metoo", Kind::Bool, "no"),
    var("msgraph_auth_flow", Kind::String, "auth-code"),
    var("msgraph_client_id", Kind::String, ""),
    var("msgraph_cloud", Kind::String, "global"),
    var("msgraph_endpoint", Kind::String, ""),
    var("msgraph_focused_inbox", Kind::Bool, "no"),
    var("msgraph_oauth_refresh_command", Kind::String, ""),
    var("msgraph_page_size", Kind::Number, "50"),
    var("msgraph_query_directory", Kind::Bool, "yes"),
    var("msgraph_redirect_port", Kind::Number, "0"),
    var(
        "msgraph_scopes",
        Kind::String,
        "Mail.ReadWrite Mail.Send User.Read User.ReadBasic.All People.Read offline_access",
    ),
    var("msgraph_sync_interval", Kind::Number, "60"),
    var("msgraph_tenant", Kind::String, ""),
    var("msgraph_token_file", Kind::Path, ""),
    var("msgraph_token_store", Kind::String, "keyring"),
    var(
        "pager_format",
        Kind::String,
        "-%Z- %C/%m: %-20.20n   %s%*  -- (%P)",
    ),
    var("postpone", Kind::Quad, "ask-yes"),
    var("postponed", Kind::Path, "~/postponed"),
    var("quote_regex", Kind::String, "^([ \t]*[|>:}#])+"),
    var("realname", Kind::String, ""),
    var("record", Kind::Path, "~/sent"),
    var("reply_to", Kind::Quad, "ask-yes"),
    var("resolve", Kind::Bool, "yes"),
    var("sig_dashes", Kind::Bool, "yes"),
    var("signature", Kind::Path, "~/.signature"),
    var("spoolfile", Kind::Path, ""),
    var(
        "status_format",
        Kind::String,
        "-%r-mett: %f [Msgs:%?M?%M/?%m%?n? New:%n?%?d? Del:%d?%?F? Flag:%F?%?b? Inc:%b?]%?V? [Limit: %V]?%?I? [incomplete]?---%>-(%P)---",
    ),
    var("timeout", Kind::Number, "600"),
    var("tmpdir", Kind::Path, ""),
    var("trash", Kind::Path, ""),
    var("use_from", Kind::Bool, "yes"),
];

/// Looks up a known variable.
pub fn find(name: &str) -> Option<&'static Var> {
    VARS.binary_search_by(|v| v.name.cmp(name))
        .ok()
        .map(|i| &VARS[i])
}

/// Parses `text` as a value for `var`. Paths expand a leading `~` using
/// `home`.
pub fn parse(var: &Var, text: &str, home: Option<&str>) -> Result<Value, String> {
    match var.kind {
        Kind::Bool => match text.to_ascii_lowercase().as_str() {
            "yes" | "true" | "on" => Ok(Value::Bool(true)),
            "no" | "false" | "off" => Ok(Value::Bool(false)),
            _ => Err(format!("{}: expected yes or no, got {text:?}", var.name)),
        },
        Kind::Number => text
            .trim()
            .parse()
            .map(Value::Number)
            .map_err(|_| format!("{}: expected a number, got {text:?}", var.name)),
        Kind::String => Ok(Value::Str(text.to_owned())),
        Kind::Path => Ok(Value::Str(expand_home(text, home))),
        Kind::Quad => match text.to_ascii_lowercase().as_str() {
            "yes" | "true" | "on" => Ok(Value::Quad(Quad::Yes)),
            "no" | "false" | "off" => Ok(Value::Quad(Quad::No)),
            "ask-yes" => Ok(Value::Quad(Quad::AskYes)),
            "ask-no" => Ok(Value::Quad(Quad::AskNo)),
            _ => Err(format!(
                "{}: expected yes, no, ask-yes or ask-no, got {text:?}",
                var.name
            )),
        },
    }
}

/// Expands a leading `~` or `~/` to `home`.
fn expand_home(path: &str, home: Option<&str>) -> String {
    match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            format!("{home}{rest}")
        }
        _ => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_are_sorted_and_defaults_parse() {
        for pair in VARS.windows(2) {
            assert!(
                pair[0].name < pair[1].name,
                "{} is out of order",
                pair[1].name
            );
        }
        for var in VARS {
            assert!(parse(var, var.default, Some("/h")).is_ok(), "{}", var.name);
        }
        assert_eq!(find("msgraph_cloud").unwrap().default, "global");
        assert!(find("no_such_variable").is_none());
    }

    #[test]
    fn parses_values_by_kind() {
        let bool_var = find("use_from").unwrap();
        assert_eq!(parse(bool_var, "No", None), Ok(Value::Bool(false)));
        assert!(parse(bool_var, "maybe", None).is_err());
        let number = find("timeout").unwrap();
        assert_eq!(parse(number, "30", None), Ok(Value::Number(30)));
        assert!(parse(number, "soon", None).is_err());
        let quad = find("include").unwrap();
        assert_eq!(parse(quad, "Ask-No", None), Ok(Value::Quad(Quad::AskNo)));
        assert!(parse(quad, "maybe", None).is_err());
        let path = find("signature").unwrap();
        assert_eq!(
            parse(path, "~/.sig", Some("/home/a")),
            Ok(Value::Str("/home/a/.sig".into()))
        );
        // `~user` and non-leading tildes are left alone.
        assert_eq!(
            parse(path, "~bob/x", Some("/home/a")),
            Ok(Value::Str("~bob/x".into()))
        );
    }

    #[test]
    fn values_display_like_mutt() {
        assert_eq!(Value::Bool(true).to_string(), "yes");
        assert_eq!(Value::Number(5).to_string(), "5");
        assert_eq!(Value::Str(r#"a "b""#.into()).to_string(), r#""a \"b\"""#);
        assert_eq!(
            Value::Str("$HOME `x`\n".into()).to_string(),
            r#""\$HOME \`x\`\n""#
        );
    }
}
