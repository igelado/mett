//! mutt's patterns, for searching (`/`) and limiting (`l`) the index
//! (`docs/MUTTRC.md` §6).
//!
//! The supported subset works on what the index knows about a message:
//!
//! | Pattern | Matches messages |
//! |---------|------------------|
//! | `~f EXPR` | from a sender matching `EXPR` |
//! | `~t EXPR` | to a recipient matching `EXPR` |
//! | `~s EXPR` | with a subject matching `EXPR` |
//! | `~A` | all |
//! | `~N`, `~U` | new (unread) |
//! | `~R` | read |
//! | `~F` | flagged |
//! | `~D` | marked for deletion |
//!
//! Patterns are combined by writing them one after another (and), with `|`
//! (or) and `!` (not), and grouped with parentheses. `EXPR` is a regular
//! expression, which ignores case unless it has an upper-case letter, and
//! may be quoted. Text that isn't a pattern searches the sender and the
//! subject, as mutt's default `simple_search` does.

use std::fmt;

use mett_core::{Address, MessageSummary};
use regex::{Regex, RegexBuilder};

/// A parsed pattern.
#[derive(Clone, Debug)]
pub struct Pattern {
    expr: Expr,
}

/// Why a pattern couldn't be parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternError(pub String);

impl PatternError {
    /// Whether the pattern is one of mutt's that mett doesn't know (such
    /// as `~T`), rather than a mistake.
    pub fn unsupported(&self) -> bool {
        self.0.ends_with(UNSUPPORTED)
    }
}

/// How an error about a pattern mett doesn't know ends.
const UNSUPPORTED: &str = " is not supported";

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug)]
enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    From(Regex),
    To(Regex),
    Subject(Regex),
    All,
    New,
    Read,
    Flagged,
    Deleted,
}

/// What a pattern is matched against.
pub struct Candidate<'a> {
    /// The message.
    pub summary: &'a MessageSummary,
    /// Whether it is marked for deletion.
    pub deleted: bool,
}

impl Pattern {
    /// Parses `text`.
    pub fn parse(text: &str) -> Result<Self, PatternError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(PatternError("empty pattern".into()));
        }
        // Like mutt's `simple_search`: text without a `~X` pattern searches
        // the sender and the subject, as a regular expression (so
        // `invoice|receipt` finds either, and `budget ~ final` is text).
        // A `~X` operator starts a token: at the start, or after a space,
        // `!`, `|` or `(` (`billing~ops` is text).
        let bytes = text.as_bytes();
        let has_pattern = (0..bytes.len().saturating_sub(1)).any(|i| {
            bytes[i] == b'~'
                && bytes[i + 1].is_ascii_alphabetic()
                && (i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b'!' | b'|' | b'('))
        });
        if !has_pattern {
            let regex = regex(text)?;
            return Ok(Self {
                expr: Expr::Or(vec![Expr::From(regex.clone()), Expr::Subject(regex)]),
            });
        }
        let mut parser = Parser { text, position: 0 };
        let expr = parser.or()?;
        parser.skip_space();
        if parser.position < text.len() {
            return Err(PatternError(format!(
                "unexpected {:?} in pattern",
                &text[parser.position..]
            )));
        }
        Ok(Self { expr })
    }

    /// Whether the message matches.
    pub fn matches(&self, candidate: &Candidate<'_>) -> bool {
        self.expr.matches(candidate)
    }
}

impl Expr {
    fn matches(&self, c: &Candidate<'_>) -> bool {
        let summary = c.summary;
        match self {
            Expr::And(all) => all.iter().all(|e| e.matches(c)),
            Expr::Or(any) => any.iter().any(|e| e.matches(c)),
            Expr::Not(e) => !e.matches(c),
            Expr::From(regex) => summary.from.iter().any(|a| address_matches(a, regex)),
            Expr::To(regex) => summary.to.iter().any(|a| address_matches(a, regex)),
            Expr::Subject(regex) => regex.is_match(&summary.subject),
            Expr::All => true,
            Expr::New => !summary.flags.read,
            Expr::Read => summary.flags.read,
            Expr::Flagged => summary.flags.flagged,
            Expr::Deleted => c.deleted,
        }
    }
}

/// Whether the address matches, as shown (`Name <email>`), so that an
/// expression can span the name and the address.
fn address_matches(address: &Address, regex: &Regex) -> bool {
    regex.is_match(&address.to_string())
}

/// A regular expression with mutt's case rule: it ignores case unless it
/// has an upper-case letter.
pub fn regex(text: &str) -> Result<Regex, PatternError> {
    let ignore_case = !text.chars().any(char::is_uppercase);
    RegexBuilder::new(text)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| PatternError(format!("bad regular expression {text:?}: {e}")))
}

/// A recursive-descent parser: `or := and ('|' and)*`,
/// `and := unary+`, `unary := '!' unary | '(' or ')' | '~' letter [arg]`.
struct Parser<'a> {
    text: &'a str,
    position: usize,
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        &self.text[self.position..]
    }

    fn skip_space(&mut self) {
        let rest = self.rest();
        self.position += rest.len() - rest.trim_start().len();
    }

    fn eat(&mut self, c: char) -> bool {
        self.skip_space();
        if self.rest().starts_with(c) {
            self.position += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn or(&mut self) -> Result<Expr, PatternError> {
        let mut any = vec![self.and()?];
        while self.eat('|') {
            any.push(self.and()?);
        }
        Ok(if any.len() == 1 {
            any.remove(0)
        } else {
            Expr::Or(any)
        })
    }

    fn and(&mut self) -> Result<Expr, PatternError> {
        let mut all = Vec::new();
        loop {
            self.skip_space();
            if self.rest().is_empty() || self.rest().starts_with(['|', ')']) {
                break;
            }
            all.push(self.unary()?);
        }
        match all.len() {
            0 => Err(PatternError("missing pattern".into())),
            1 => Ok(all.remove(0)),
            _ => Ok(Expr::And(all)),
        }
    }

    fn unary(&mut self) -> Result<Expr, PatternError> {
        if self.eat('!') {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        if self.eat('(') {
            let expr = self.or()?;
            if !self.eat(')') {
                return Err(PatternError("missing )".into()));
            }
            return Ok(expr);
        }
        if !self.eat('~') {
            return Err(PatternError(format!(
                "expected a pattern such as ~s at {:?}",
                self.rest()
            )));
        }
        let mut chars = self.rest().chars();
        let Some(letter) = chars.next() else {
            return Err(PatternError("missing pattern letter after ~".into()));
        };
        self.position += letter.len_utf8();
        Ok(match letter {
            'f' => Expr::From(self.argument()?),
            't' => Expr::To(self.argument()?),
            's' => Expr::Subject(self.argument()?),
            'A' => Expr::All,
            'N' | 'U' => Expr::New,
            'R' => Expr::Read,
            'F' => Expr::Flagged,
            'D' => Expr::Deleted,
            other => {
                return Err(PatternError(format!("~{other}{UNSUPPORTED}")));
            }
        })
    }

    /// A pattern's argument: a quoted string, or text up to a space or
    /// one of `|()`.
    fn argument(&mut self) -> Result<Regex, PatternError> {
        self.skip_space();
        let rest = &self.text[self.position..];
        let (text, used) = match rest.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                // Up to the first quote not escaped with a backslash; an
                // escaped quote stands for itself.
                let mut text = String::new();
                let mut chars = rest[1..].char_indices();
                let end = loop {
                    match chars.next() {
                        Some((_, '\\')) => match chars.next() {
                            Some((_, c)) if c == quote => text.push(c),
                            Some((_, c)) => {
                                text.push('\\');
                                text.push(c);
                            }
                            None => text.push('\\'),
                        },
                        Some((i, c)) if c == quote => break i,
                        Some((_, c)) => text.push(c),
                        None => return Err(PatternError("missing closing quote".into())),
                    }
                };
                (std::borrow::Cow::Owned(text), end + 2)
            }
            _ => {
                let end = rest
                    .find(|c: char| c.is_whitespace() || "|()".contains(c))
                    .unwrap_or(rest.len());
                (std::borrow::Cow::Borrowed(&rest[..end]), end)
            }
        };
        if text.is_empty() {
            return Err(PatternError("missing text to search for".into()));
        }
        self.position += used;
        regex(&text)
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use mett_core::mock;

    use super::*;

    fn message(from: &str, subject: &str, read: bool, flagged: bool) -> MessageSummary {
        let mut message = mock::message("m", "in", subject, DateTime::UNIX_EPOCH).summary;
        message.from = Some(Address::new(from).with_name("Alice Example"));
        message.to = vec![Address::new("bob@fabrikam.com")];
        message.flags.read = read;
        message.flags.flagged = flagged;
        message
    }

    fn matches(pattern: &str, summary: &MessageSummary) -> bool {
        Pattern::parse(pattern).unwrap().matches(&Candidate {
            summary,
            deleted: false,
        })
    }

    #[test]
    fn matches_fields_and_flags() {
        let m = message("alice@contoso.com", "Budget review", false, true);
        assert!(matches("~f alice", &m));
        assert!(matches("~f Example", &m), "the name counts too");
        assert!(matches("~f 'Example.*contoso'", &m), "and both together");
        assert!(!matches("~f bob", &m));
        assert!(matches("~t fabrikam", &m));
        assert!(matches("~s budget", &m), "lower case ignores case");
        assert!(!matches("~s BUDGET", &m), "upper case doesn't");
        assert!(matches("~N ~F", &m));
        assert!(!matches("~R", &m));
        assert!(matches("~A", &m));
        assert!(!matches("~D", &m));
    }

    #[test]
    fn combines_patterns() {
        let m = message("alice@contoso.com", "Budget review", true, false);
        assert!(matches("~s nothing | ~f alice", &m));
        assert!(matches("!~N", &m));
        assert!(matches("~f alice !(~F | ~N)", &m));
        assert!(!matches("~f alice (~F | ~N)", &m));
        assert!(matches(r#"~s "budget rev""#, &m));
        assert!(matches("~s 'b.dget'", &m));
        let quoted = message("alice@contoso.com", r#"The "big" one"#, true, false);
        assert!(matches(r#"~s "\"big\"""#, &quoted), "escaped quotes");
        assert!(matches(r#"~s "\"big\" one" ~f alice"#, &quoted));
        assert!(matches(r#"~s "\d*""#, &m), "other escapes are the regex's");
    }

    #[test]
    fn plain_text_searches_sender_and_subject() {
        let m = message("alice@contoso.com", "Budget review!", true, false);
        assert!(matches("review!", &m));
        assert!(matches("invoice|budget", &m));
        let tilde = message("alice@contoso.com", "budget ~ final", true, false);
        assert!(matches("budget ~ final", &tilde));
        let joined = message("billing~ops@contoso.com", "Invoice", true, false);
        assert!(matches("billing~ops", &joined));
        assert!(matches("!~N", &joined), "an operator after ! is a pattern");
        assert!(matches("review", &m));
        assert!(matches("contoso", &m));
        assert!(!matches("fabrikam", &m), "not the recipients");
    }

    #[test]
    fn reports_bad_patterns() {
        for bad in [
            "",
            "~f",
            "~z",
            "(~N",
            "~N )",
            "~s (",
            "~f \"open",
            "|~N",
            "~s [",
            "[",
        ] {
            assert!(Pattern::parse(bad).is_err(), "{bad:?}");
        }
    }
}
