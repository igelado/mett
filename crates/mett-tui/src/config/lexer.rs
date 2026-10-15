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
    /// The parts with the quotes as written (in double quotes, whichever
    /// were used), as mutt keeps them for an address list (`alias`):
    /// `"Doe, Jane"<jane@x.org>` is one address, not two.
    pub kept: Vec<Part>,
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

/// Adds `s` to the literal text at the end of `parts`.
fn push_literal(parts: &mut Vec<Part>, s: &str) {
    match parts.last_mut() {
        Some(Part::Literal(text)) => text.push_str(s),
        _ => parts.push(Part::Literal(s.to_owned())),
    }
}

impl Word {
    fn push_char(&mut self, c: char) {
        self.push_str(c.encode_utf8(&mut [0; 4]));
    }

    fn push_str(&mut self, s: &str) {
        push_literal(&mut self.parts, s);
        push_literal(&mut self.kept, s);
    }

    /// A variable or a command.
    fn push_part(&mut self, part: Part) {
        self.kept.push(part.clone());
        self.parts.push(part);
    }

    /// Starts or ends a quoted string.
    fn push_quote(&mut self) {
        push_literal(&mut self.kept, "\"");
    }

    /// A character inside quotes: kept with a backslash if it would end
    /// them.
    fn push_quoted_char(&mut self, c: char) {
        push_literal(&mut self.parts, c.encode_utf8(&mut [0; 4]));
        if matches!(c, '"' | '\\') {
            push_literal(&mut self.kept, "\\");
        }
        push_literal(&mut self.kept, c.encode_utf8(&mut [0; 4]));
    }

    /// The word's value: variables are replaced and backtick commands run
    /// (without their trailing newline).
    pub fn expand(&self, expander: &dyn Expander) -> Result<String, String> {
        expand_parts(&self.parts, expander)
    }

    /// [`Self::expand`] with the quotes as written (see [`Self::kept`]).
    /// What a variable or command gives inside quotes is escaped, so that
    /// it stays inside them.
    pub fn expand_kept(&self, expander: &dyn Expander) -> Result<String, String> {
        let mut out = String::new();
        let mut quoted = false;
        for part in &self.kept {
            let value = match part {
                Part::Literal(text) => {
                    let mut escaped = false;
                    for c in text.chars() {
                        match c {
                            _ if escaped => escaped = false,
                            '\\' => escaped = true,
                            '"' => quoted = !quoted,
                            _ => {}
                        }
                    }
                    out.push_str(text);
                    continue;
                }
                part => expand_parts(std::slice::from_ref(part), expander)?,
            };
            if quoted {
                out.push_str(&value.replace('\\', "\\\\").replace('"', "\\\""));
            } else {
                out.push_str(&value);
            }
        }
        Ok(out)
    }
}

/// `parts`, with variables replaced and backtick commands run.
fn expand_parts(parts: &[Part], expander: &dyn Expander) -> Result<String, String> {
    {
        let mut out = String::new();
        for part in parts {
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
}

impl Word {
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

    /// For a word starting with `=` (as in `set var =value`), the rest of
    /// the word after that `=`.
    pub fn strip_leading_equals(&self) -> Option<Word> {
        let Some(Part::Literal(text)) = self.parts.first() else {
            return None;
        };
        let rest = text.strip_prefix('=')?;
        let mut word = Word::default();
        if !rest.is_empty() {
            word.parts.push(Part::Literal(rest.to_owned()));
        }
        word.parts.extend(self.parts[1..].iter().cloned());
        Some(word)
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
    /// A lexer over `source`. Callers should convert CRLF line endings to
    /// LF first, so that line continuations work in either.
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

    /// The character the escape `\\c` stands for, reading what follows
    /// if it needs to (`\\Cx`).
    fn escaped(&mut self, c: char) -> Result<char, SyntaxError> {
        // `\C` needs a key on the same line: the newline ends the command.
        if matches!(c, 'c' | 'C') && matches!(self.chars.peek(), Some('\n') | None) {
            return Err(self.error("\\C needs a key"));
        }
        Ok(escape(c, &mut || self.bump()))
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
                        Some(c) => {
                            let c = self.escaped(c)?;
                            word.push_char(c);
                        }
                        None => return Err(self.error("backslash at end of file")),
                    }
                }
                '\'' => {
                    self.bump();
                    // A quoted string starts its own part, so that even `''`
                    // after `=` counts as an (empty) value.
                    word.parts.push(Part::Literal(String::new()));
                    word.push_quote();
                    loop {
                        match self.chars.peek() {
                            Some('\'') => {
                                self.bump();
                                word.push_quote();
                                break;
                            }
                            // As in mutt, a quote ends at the end of its line.
                            Some('\n') | None => {
                                return Err(self.error("unterminated single quote"));
                            }
                            Some(_) => {
                                let c = self.bump().expect("peeked");
                                word.push_quoted_char(c);
                            }
                        }
                    }
                }
                '"' => {
                    self.bump();
                    word.parts.push(Part::Literal(String::new()));
                    word.push_quote();
                    self.read_double_quoted(&mut word)?;
                }
                '`' => {
                    self.bump();
                    let command = self.read_backtick()?;
                    word.push_part(Part::Command(command));
                }
                '$' => {
                    self.bump();
                    self.read_variable(&mut word)?;
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
                Some('"') => {
                    word.push_quote();
                    return Ok(());
                }
                Some('\\') => match self.bump() {
                    Some('\n') => {}
                    Some(c) => {
                        let c = self.escaped(c)?;
                        word.push_quoted_char(c);
                    }
                    None => return Err(self.error("unterminated double quote")),
                },
                Some('`') => {
                    let command = self.read_backtick()?;
                    word.push_part(Part::Command(command));
                }
                Some('$') => self.read_variable(word)?,
                Some(c) => word.push_quoted_char(c),
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
                // An escaped character (such as \`) belongs to the shell
                // command; pass both characters through.
                Some('\\') => {
                    self.bump();
                    match self.chars.peek() {
                        // A trailing backslash continues the command on the
                        // next line, as elsewhere in a mettrc.
                        Some('\n') => {
                            self.bump();
                        }
                        None => return Err(self.error("unterminated backtick")),
                        Some(_) => {
                            command.push('\\');
                            command.extend(self.bump());
                        }
                    }
                }
                Some(_) => command.extend(self.bump()),
            }
        }
    }

    /// Reads a variable name after `$` (`$name` or `${name}`). A `$` not
    /// followed by a name is literal.
    fn read_variable(&mut self, word: &mut Word) -> Result<(), SyntaxError> {
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
        if braced && !name.is_empty() {
            if self.chars.peek() != Some(&'}') {
                return Err(self.error(format!("missing }} after ${{{name}")));
            }
            self.bump();
        }
        if name.is_empty() {
            word.push_str(if braced { "${" } else { "$" });
        } else {
            word.push_part(Part::Variable(name));
        }
        Ok(())
    }
}

/// The character a backslash escape stands for, as in mutt: `\\n`, `\\r`,
/// `\\t`, `\\f`, `\\e` (Esc) and `\\Cx` (Control-x); any other
/// character stands for itself.
fn escape(c: char, next: &mut dyn FnMut() -> Option<char>) -> char {
    match c {
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'f' => '\u{c}',
        'e' | 'E' => '\u{1b}',
        'c' | 'C' => match next() {
            // Control-x is x with bit 6 cleared: ^A is 1, ^[ is Esc; ^? is
            // DEL.
            Some('?') => '\u{7f}',
            Some(x) if x.is_ascii() && x.to_ascii_uppercase() as u8 >= b'@' => {
                char::from(x.to_ascii_uppercase() as u8 & 0x1f)
            }
            Some(x) => x,
            None => c,
        },
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
    fn escaped_backticks_belong_to_the_command() {
        let commands = lex(r"set x=`echo \`date\``");
        let words = &commands[0].as_ref().unwrap().words;
        assert_eq!(
            words[1].parts,
            [
                Part::Literal("x=".into()),
                Part::Command(r"echo \`date\`".into())
            ]
        );
    }

    #[test]
    fn backtick_commands_can_continue_on_the_next_line() {
        let commands = lex("set x=`printf %s \\\n  one`\nset y=2\n");
        assert_eq!(
            commands[0].as_ref().unwrap().words[1].parts,
            [
                Part::Literal("x=".into()),
                Part::Command("printf %s   one".into())
            ]
        );
        assert_eq!(commands[1].as_ref().unwrap().line, 3);
    }

    #[test]
    fn unterminated_braced_variables_are_errors() {
        let commands = lex("set realname=\"${HOME\"\nset x=${A}\n");
        assert!(matches!(&commands[0], Err(e) if e.message.contains("missing }")));
        assert!(commands[1].is_ok());
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

    #[test]
    fn control_escapes_cover_punctuation() {
        let control = |x: char| {
            let mut next = Some(x);
            escape('C', &mut || next.take())
        };
        assert_eq!(control('a'), '\u{1}');
        assert_eq!(control('['), '\u{1b}');
        assert_eq!(control('_'), '\u{1f}');
        assert_eq!(control('?'), '\u{7f}');
    }

    #[test]
    fn a_control_escape_at_the_end_of_a_line_spares_the_next() {
        let commands = lex("bind index \\C\nset resolve=no\n");
        assert!(matches!(&commands[0], Err(e) if e.line == 1 && e.message.contains("\\C")));
        assert!(
            matches!(&commands[1], Ok(c) if c.line == 2 && c.words.len() == 2),
            "{commands:?}"
        );
    }
}
