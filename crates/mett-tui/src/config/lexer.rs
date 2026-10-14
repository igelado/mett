//! Splitting a mettrc into commands and words, with mutt's lexical rules
//! (`docs/MUTTRC.md` §2).
//!
//! Words are kept as parts (literal text, `$variables` and backtick
//! commands) and expanded only when their value is needed. A setting mett
//! ignores is therefore never expanded, so its backtick command never runs,
//! and `$variable` expansion sees the values set by earlier commands.

use std::fmt;

/// A piece of a word.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    /// Literal text, after quoting and escapes. An empty literal records an
    /// empty quoted string (`""`), which still counts as a value.
    Literal(String),
    /// `$name` or `${name}`.
    Variable(String),
    /// A backtick command.
    Command(String),
}

/// A word, as parts to expand.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Word {
    /// The parts, in order.
    pub parts: Vec<Part>,
}

/// A command: its words and where it starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The words; the first is the command name.
    pub words: Vec<Word>,
    /// The 1-based line the command starts on.
    pub line: usize,
}

/// A syntax error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxError {
    /// The 1-based line of the error.
    pub line: usize,
    /// What went wrong.
    pub message: String,
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

/// What expansion needs from its surroundings.
pub trait Expander {
    /// The value of `$name`: a configuration variable or, failing that, an
    /// environment variable.
    fn variable(&self, name: &str) -> Option<String>;
    /// The output of a backtick command.
    fn command(&self, command: &str) -> Result<String, String>;
}

impl Word {
    fn push_char(&mut self, c: char) {
        match self.parts.last_mut() {
            Some(Part::Literal(text)) => text.push(c),
            _ => self.parts.push(Part::Literal(c.to_string())),
        }
    }

    fn push_str(&mut self, s: &str) {
        match self.parts.last_mut() {
            Some(Part::Literal(text)) => text.push_str(s),
            _ => self.parts.push(Part::Literal(s.to_owned())),
        }
    }

    /// The word's value: variables are replaced and backtick commands run
    /// (without their trailing newline).
    pub fn expand(&self, expander: &dyn Expander) -> Result<String, String> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(text) => out.push_str(text),
                Part::Variable(name) => {
                    out.push_str(&expander.variable(name).unwrap_or_default());
                }
                Part::Command(command) => {
                    let output = expander
                        .command(command)
                        .map_err(|e| format!("`{command}` failed: {e}"))?;
                    out.push_str(output.trim_end_matches(['\n', '\r']));
                }
            }
        }
        Ok(out)
    }

    /// The word's text if it has no variables or commands.
    pub fn literal(&self) -> Option<String> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(text) => out.push_str(text),
                _ => return None,
            }
        }
        Some(out)
    }

    /// Splits `name=value` at the first literal `=`, if the name before it
    /// is literal text. The value is `None` for a bare `name=` (nothing
    /// after the `=`, not even `""`).
    pub fn split_assignment(&self) -> Option<(String, Option<Word>)> {
        let (index, offset) = self
            .parts
            .iter()
            .enumerate()
            .find_map(|(i, part)| match part {
                Part::Literal(text) => text.find('=').map(|offset| (i, offset)),
                _ => None,
            })?;
        let mut name = String::new();
        for part in &self.parts[..index] {
            match part {
                Part::Literal(text) => name.push_str(text),
                _ => return None,
            }
        }
        let Part::Literal(text) = &self.parts[index] else {
            unreachable!("found in a literal");
        };
        name.push_str(&text[..offset]);
        let mut value = Word::default();
        let rest = &text[offset + 1..];
        if !rest.is_empty() {
            value.parts.push(Part::Literal(rest.to_owned()));
        }
        value.parts.extend(self.parts[index + 1..].iter().cloned());
        let value = (!value.parts.is_empty()).then_some(value);
        Some((name, value))
    }
}

/// Reads commands from a mettrc source.
pub struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: usize,
}

impl<'a> Lexer<'a> {
    /// A lexer over `source`.
    pub fn new(source: &'a str) -> Self {
        Self {
            chars: source.chars().peekable(),
            line: 1,
        }
    }

    /// The next non-empty command, or `None` at the end of the source.
    pub fn next_command(&mut self) -> Option<Result<Command, SyntaxError>> {
        loop {
            let line = self.line;
            match self.read_command() {
                Ok(None) => return None,
                Ok(Some(words)) if words.is_empty() => continue,
                Ok(Some(words)) => return Some(Ok(Command { words, line })),
                Err(e) => {
                    // Skip the rest of the broken command.
                    self.skip_to_end_of_command();
                    return Some(Err(e));
                }
            }
        }
    }

    fn error(&self, message: impl Into<String>) -> SyntaxError {
        SyntaxError {
            line: self.line,
            message: message.into(),
        }
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn skip_to_end_of_command(&mut self) {
        while let Some(c) = self.bump() {
            if c == '\n' || c == ';' {
                break;
            }
        }
    }

    /// Reads one command's words, up to a newline or `;`. `Ok(None)` means
    /// the end of the source.
    fn read_command(&mut self) -> Result<Option<Vec<Word>>, SyntaxError> {
        if self.chars.peek().is_none() {
            return Ok(None);
        }
        let mut words = Vec::new();
        let mut word = Word::default();
        while let Some(&c) = self.chars.peek() {
            match c {
                '\n' | ';' => {
                    self.bump();
                    break;
                }
                ' ' | '\t' | '\r' => {
                    self.bump();
                    if !word.parts.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                }
                '#' => {
                    // A comment runs to the end of the line.
                    while self.chars.peek().is_some_and(|&c| c != '\n') {
                        self.bump();
                    }
                }
                '\\' => {
                    self.bump();
                    match self.bump() {
                        // A trailing backslash continues the command.
                        Some('\n') => {}
                        Some(c) => word.push_char(escaped(c)),
                        None => return Err(self.error("backslash at end of file")),
                    }
                }
                '\'' => {
                    self.bump();
                    // A quoted string starts its own part, so that even `''`
                    // after `=` counts as an (empty) value.
                    word.parts.push(Part::Literal(String::new()));
                    loop {
                        match self.chars.peek() {
                            Some('\'') => {
                                self.bump();
                                break;
                            }
                            // As in mutt, a quote ends at the end of its line.
                            Some('\n') | None => {
                                return Err(self.error("unterminated single quote"));
                            }
                            Some(_) => {
                                let c = self.bump().expect("peeked");
                                word.push_char(c);
                            }
                        }
                    }
                }
                '"' => {
                    self.bump();
                    word.parts.push(Part::Literal(String::new()));
                    self.read_double_quoted(&mut word)?;
                }
                '`' => {
                    self.bump();
                    let command = self.read_backtick()?;
                    word.parts.push(Part::Command(command));
                }
                '$' => {
                    self.bump();
                    self.read_variable(&mut word);
                }
                _ => {
                    self.bump();
                    word.push_char(c);
                }
            }
        }
        if !word.parts.is_empty() {
            words.push(word);
        }
        Ok(Some(words))
    }

    fn read_double_quoted(&mut self, word: &mut Word) -> Result<(), SyntaxError> {
        loop {
            // As in mutt, a quote ends at the end of its line (a trailing
            // backslash still continues it).
            if matches!(self.chars.peek(), Some('\n') | None) {
                return Err(self.error("unterminated double quote"));
            }
            match self.bump() {
                Some('"') => return Ok(()),
                Some('\\') => match self.bump() {
                    Some('\n') => {}
                    Some(c) => word.push_char(escaped(c)),
                    None => return Err(self.error("unterminated double quote")),
                },
                Some('`') => {
                    let command = self.read_backtick()?;
                    word.parts.push(Part::Command(command));
                }
                Some('$') => self.read_variable(word),
                Some(c) => word.push_char(c),
                None => return Err(self.error("unterminated double quote")),
            }
        }
    }

    /// Reads up to the closing backtick and returns the command.
    fn read_backtick(&mut self) -> Result<String, SyntaxError> {
        let mut command = String::new();
        loop {
            match self.chars.peek() {
                Some('`') => {
                    self.bump();
                    return Ok(command);
                }
                Some('\n') | None => return Err(self.error("unterminated backtick")),
                Some(_) => command.extend(self.bump()),
            }
        }
    }

    /// Reads a variable name after `$` (`$name` or `${name}`). A `$` not
    /// followed by a name is literal.
    fn read_variable(&mut self, word: &mut Word) {
        let braced = self.chars.peek() == Some(&'{');
        if braced {
            self.bump();
        }
        let mut name = String::new();
        while let Some(&c) = self.chars.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                name.push(c);
                self.bump();
            } else {
                break;
            }
        }
        if braced && self.chars.peek() == Some(&'}') {
            self.bump();
        }
        if name.is_empty() {
            word.push_str(if braced { "${" } else { "$" });
        } else {
            word.parts.push(Part::Variable(name));
        }
    }
}

/// The character a backslash escape stands for.
fn escaped(c: char) -> char {
    match c {
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        c => c,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct Fake {
        commands_run: Cell<usize>,
    }

    impl Expander for Fake {
        fn variable(&self, name: &str) -> Option<String> {
            HashMap::from([("HOME", "/home/a"), ("folder", "msgraph://a@b.c/")])
                .get(name)
                .map(|v| (*v).to_owned())
        }
        fn command(&self, command: &str) -> Result<String, String> {
            self.commands_run.set(self.commands_run.get() + 1);
            match command {
                "pass show work" => Ok("s3cret\n".into()),
                _ => Err("not found".into()),
            }
        }
    }

    fn lex(source: &str) -> Vec<Result<Command, SyntaxError>> {
        let mut lexer = Lexer::new(source);
        std::iter::from_fn(|| lexer.next_command()).collect()
    }

    fn words(source: &str) -> Vec<Vec<String>> {
        let fake = Fake::default();
        lex(source)
            .into_iter()
            .map(|c| {
                c.unwrap()
                    .words
                    .iter()
                    .map(|w| w.expand(&fake).unwrap())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn splits_commands_on_newlines_and_semicolons() {
        assert_eq!(
            words("set a=1\n\n  set b = 2 ; unset c # comment\n# only a comment\n"),
            [
                vec!["set", "a=1"],
                vec!["set", "b", "=", "2"],
                vec!["unset", "c"]
            ]
        );
    }

    #[test]
    fn handles_quotes_and_escapes() {
        assert_eq!(
            words(r#"set realname="Alice \"A\" Example" sig='$HOME/sig' x="""#),
            [vec![
                "set",
                r#"realname=Alice "A" Example"#,
                "sig=$HOME/sig",
                "x="
            ]]
        );
        assert_eq!(words(r"set a=one\ two"), [vec!["set", "a=one two"]]);
    }

    #[test]
    fn expands_variables_outside_single_quotes() {
        assert_eq!(
            words(r#"set a=$HOME/x b="${folder}@inbox" c=$UNSET d=$ e='$HOME'"#),
            [vec![
                "set",
                "a=/home/a/x",
                "b=msgraph://a@b.c/@inbox",
                "c=",
                "d=$",
                "e=$HOME"
            ]]
        );
    }

    #[test]
    fn runs_backticks_only_when_expanded() {
        let commands = lex(r#"set pass=`pass show work` quoted="x`pass show work`y""#);
        let fake = Fake::default();
        let words = &commands[0].as_ref().unwrap().words;
        assert_eq!(fake.commands_run.get(), 0, "lexing runs nothing");
        let expanded: Vec<_> = words.iter().map(|w| w.expand(&fake).unwrap()).collect();
        assert_eq!(expanded, ["set", "pass=s3cret", "quoted=xs3crety"]);
        assert_eq!(fake.commands_run.get(), 2);
    }

    #[test]
    fn splits_assignments_and_tells_empty_values_from_missing_ones() {
        let commands = lex(r#"set a=1 b="" c= d=$HOME"#);
        let fake = Fake::default();
        let split: Vec<_> = commands[0].as_ref().unwrap().words[1..]
            .iter()
            .map(|w| {
                let (name, value) = w.split_assignment().unwrap();
                (name, value.map(|v| v.expand(&fake).unwrap()))
            })
            .collect();
        assert_eq!(
            split,
            [
                ("a".to_owned(), Some("1".to_owned())),
                ("b".to_owned(), Some(String::new())),
                ("c".to_owned(), None),
                ("d".to_owned(), Some("/home/a".to_owned())),
            ]
        );
    }

    #[test]
    fn continues_lines_and_tracks_line_numbers() {
        let commands = lex("set a=1 \\\n  b=2\nset c=3\n");
        let lines: Vec<_> = commands.iter().map(|c| c.as_ref().unwrap().line).collect();
        assert_eq!(lines, [1, 3]);
        assert_eq!(words("set a=1 \\\n  b=2\n"), [vec!["set", "a=1", "b=2"]]);
    }

    #[test]
    fn reports_errors_and_carries_on() {
        // An unterminated quote ends at its line; the next line still runs.
        let commands = lex("set a=\"open\nset c=1\nset d='x\n");
        assert!(
            matches!(&commands[0], Err(e) if e.line == 1 && e.message.contains("unterminated"))
        );
        assert!(commands[1].is_ok());
        assert!(matches!(&commands[2], Err(e) if e.line == 3));
    }
}
