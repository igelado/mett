//! Splitting a mettrc into commands and words, with mutt's lexical rules
//! (`docs/MUTTRC.md` §2).
//!
//! The source is read one command at a time, because `$variable`
//! expansion must see the values set by earlier commands.

use std::fmt;

/// A command: its words, after quoting and expansion, and where it starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The words; the first is the command name.
    pub words: Vec<String>,
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

/// What the lexer needs from its surroundings.
pub trait Expander {
    /// The value of `$name`: a configuration variable or, failing that, an
    /// environment variable.
    fn variable(&self, name: &str) -> Option<String>;
    /// The output of a backtick command.
    fn command(&self, command: &str) -> Result<String, String>;
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
    pub fn next_command(
        &mut self,
        expander: &dyn Expander,
    ) -> Option<Result<Command, SyntaxError>> {
        loop {
            let line = self.line;
            match self.read_command(expander) {
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
    fn read_command(
        &mut self,
        expander: &dyn Expander,
    ) -> Result<Option<Vec<String>>, SyntaxError> {
        if self.chars.peek().is_none() {
            return Ok(None);
        }
        let mut words = Vec::new();
        let mut word = String::new();
        // Whether `word` has started (a quoted empty string is a word).
        let mut in_word = false;
        while let Some(&c) = self.chars.peek() {
            match c {
                '\n' | ';' => {
                    self.bump();
                    break;
                }
                ' ' | '\t' | '\r' => {
                    self.bump();
                    if in_word {
                        words.push(std::mem::take(&mut word));
                        in_word = false;
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
                            word.push(escaped(c));
                            in_word = true;
                        }
                        None => return Err(self.error("backslash at end of file")),
                    }
                }
                '\'' => {
                    self.bump();
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
                            Some(_) => word.extend(self.bump()),
                        }
                    }
                    in_word = true;
                }
                '"' => {
                    self.bump();
                    self.read_double_quoted(&mut word, expander)?;
                    in_word = true;
                }
                '`' => {
                    self.bump();
                    word.push_str(&self.read_backtick(expander)?);
                    in_word = true;
                }
                '$' => {
                    self.bump();
                    word.push_str(&self.read_variable(expander));
                    in_word = true;
                }
                _ => {
                    self.bump();
                    word.push(c);
                    in_word = true;
                }
            }
        }
        if in_word {
            words.push(word);
        }
        Ok(Some(words))
    }

    fn read_double_quoted(
        &mut self,
        word: &mut String,
        expander: &dyn Expander,
    ) -> Result<(), SyntaxError> {
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
                    Some(c) => word.push(escaped(c)),
                    None => return Err(self.error("unterminated double quote")),
                },
                Some('`') => word.push_str(&self.read_backtick(expander)?),
                Some('$') => word.push_str(&self.read_variable(expander)),
                Some(c) => word.push(c),
                None => return Err(self.error("unterminated double quote")),
            }
        }
    }

    /// Reads up to the closing backtick and returns the command's output,
    /// without its trailing newline.
    fn read_backtick(&mut self, expander: &dyn Expander) -> Result<String, SyntaxError> {
        let mut command = String::new();
        loop {
            match self.chars.peek() {
                Some('`') => {
                    self.bump();
                    break;
                }
                Some('\n') | None => return Err(self.error("unterminated backtick")),
                Some(_) => command.extend(self.bump()),
            }
        }
        let output = expander
            .command(&command)
            .map_err(|e| self.error(format!("`{command}` failed: {e}")))?;
        Ok(output.trim_end_matches(['\n', '\r']).to_owned())
    }

    /// Reads a variable name after `$` (`$name` or `${name}`) and returns
    /// its value, or an empty string if it is unset. A `$` not followed by
    /// a name is literal.
    fn read_variable(&mut self, expander: &dyn Expander) -> String {
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
            return if braced { "${".into() } else { "$".into() };
        }
        expander.variable(&name).unwrap_or_default()
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
    use std::collections::HashMap;

    use super::*;

    struct Fake(HashMap<&'static str, &'static str>);

    impl Expander for Fake {
        fn variable(&self, name: &str) -> Option<String> {
            self.0.get(name).map(|v| (*v).to_owned())
        }
        fn command(&self, command: &str) -> Result<String, String> {
            match command {
                "pass show work" => Ok("s3cret\n".into()),
                _ => Err("not found".into()),
            }
        }
    }

    fn lex(source: &str) -> Vec<Result<Command, SyntaxError>> {
        let fake = Fake(HashMap::from([
            ("HOME", "/home/a"),
            ("folder", "msgraph://a@b.c/"),
        ]));
        let mut lexer = Lexer::new(source);
        std::iter::from_fn(|| lexer.next_command(&fake)).collect()
    }

    fn words(source: &str) -> Vec<Vec<String>> {
        lex(source).into_iter().map(|c| c.unwrap().words).collect()
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
    fn substitutes_backticks_without_trailing_newline() {
        assert_eq!(
            words(r#"set pass=`pass show work` quoted="x`pass show work`y""#),
            [vec!["set", "pass=s3cret", "quoted=xs3crety"]]
        );
    }

    #[test]
    fn continues_lines_and_tracks_line_numbers() {
        let commands = lex("set a=1 \\\n  b=2\nset c=3\n");
        let lines: Vec<_> = commands.iter().map(|c| c.as_ref().unwrap().line).collect();
        assert_eq!(lines, [1, 3]);
        assert_eq!(commands[0].as_ref().unwrap().words, ["set", "a=1", "b=2"]);
    }

    #[test]
    fn reports_errors_and_carries_on() {
        // An unterminated quote ends at its line; the next line still runs.
        let commands = lex("set a=\"open\nset c=1\nset d='x\n");
        assert!(
            matches!(&commands[0], Err(e) if e.line == 1 && e.message.contains("unterminated"))
        );
        assert_eq!(commands[1].as_ref().unwrap().words, ["set", "c=1"]);
        assert!(matches!(&commands[2], Err(e) if e.line == 3));
        let commands = lex("set b=`missing`\nset c=1\n");
        assert!(matches!(&commands[0], Err(e) if e.line == 1 && e.message.contains("failed")));
        assert_eq!(commands[1].as_ref().unwrap().words, ["set", "c=1"]);
    }
}
