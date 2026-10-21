//! Spell checking for the built-in editor, with Hunspell dictionaries
//! (spellbook reads them). en_US comes with mett; others are looked for
//! where the system keeps them, or in `spell_dict_path`. Words the user
//! adds go to `spell_words`, one a line.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// The en_US dictionary mett comes with (SCOWL's, MIT and BSD licensed).
const EN_US: (&str, &str) = (
    include_str!("../../assets/dictionaries/en_US.aff"),
    include_str!("../../assets/dictionaries/en_US.dic"),
);

/// How messages are spell checked: the settings that say so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// `spell_check`: whether misspelled words are underlined.
    pub check: bool,
    /// `spell_lang`: the dictionaries, such as `en_US`.
    pub langs: Vec<String>,
    /// `spell_dict_path`: where else to look for them.
    pub dirs: Vec<PathBuf>,
    /// `spell_words`: the user's own words.
    pub words: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            check: true,
            langs: vec!["en_US".into()],
            dirs: Vec::new(),
            words: None,
        }
    }
}

impl Settings {
    /// The settings from `spell_check`, `spell_lang` (comma-separated),
    /// `spell_dict_path` (as the system's `PATH`, `~` the `home`
    /// directory) and `spell_words`.
    pub fn new(check: bool, langs: &str, dirs: &str, words: &str, home: Option<&str>) -> Self {
        Self {
            check,
            langs: langs
                .split(',')
                .map(str::trim)
                .filter(|lang| !lang.is_empty())
                .map(str::to_owned)
                .collect(),
            dirs: std::env::split_paths(dirs)
                .filter(|dir| !dir.as_os_str().is_empty())
                .map(|dir| match (dir.strip_prefix("~"), home) {
                    (Ok(rest), Some(home)) => Path::new(home).join(rest),
                    _ => dir,
                })
                .collect(),
            words: Some(PathBuf::from(words)).filter(|path| !path.as_os_str().is_empty()),
        }
    }
}

/// The dictionaries, and the user's own words.
pub struct Speller {
    dictionaries: Vec<spellbook::Dictionary>,
    /// Words added: from `spell_words`, and since.
    own: RwLock<HashSet<String>>,
}

impl std::fmt::Debug for Speller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Speller")
            .field("dictionaries", &self.dictionaries.len())
            .finish_non_exhaustive()
    }
}

impl Speller {
    /// Loads the dictionaries `settings` names (en_US is mett's own unless
    /// one is found), and the user's words. It takes a moment: not on the
    /// UI's thread.
    pub fn load(settings: &Settings, home: Option<&str>) -> Result<Self, String> {
        // Without dictionaries, every word would do.
        if settings.langs.is_empty() {
            return Err("spell_lang: no dictionary named".into());
        }
        let mut dictionaries = Vec::new();
        for lang in &settings.langs {
            let found = dirs(settings, home)
                .iter()
                .find_map(|dir| read_dictionary(dir, lang).transpose())
                .transpose()?;
            let dictionary = match found {
                Some((aff, dic)) => spellbook::Dictionary::new(&aff, &dic),
                None if lang.eq_ignore_ascii_case("en_US") => {
                    spellbook::Dictionary::new(EN_US.0, EN_US.1)
                }
                None => return Err(format!("spell_lang: no dictionary for {lang}")),
            };
            dictionaries.push(dictionary.map_err(|e| format!("spell_lang: {lang}: {e}"))?);
        }
        let own = match &settings.words {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(text) => text
                    .lines()
                    .map(str::trim)
                    .filter(|word| !word.is_empty())
                    .map(str::to_owned)
                    .collect(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
                Err(e) => return Err(format!("spell_words: {}: {e}", path.display())),
            },
            None => HashSet::new(),
        };
        Ok(Self {
            dictionaries,
            own: RwLock::new(own),
        })
    }

    /// Whether `word` is spelled right.
    pub fn check(&self, word: &str) -> bool {
        self.dictionaries.is_empty()
            || self.dictionaries.iter().any(|d| d.check(word))
            || self.own.read().is_ok_and(|own| own.contains(word))
    }

    /// Words `word` may have been meant to be, best first.
    pub fn suggest(&self, word: &str) -> Vec<String> {
        let mut all = Vec::new();
        for dictionary in &self.dictionaries {
            let mut some = Vec::new();
            dictionary.suggest(word, &mut some);
            for suggestion in some {
                if !all.contains(&suggestion) {
                    all.push(suggestion);
                }
            }
        }
        all
    }

    /// Takes `word` as spelled right from now on.
    pub fn add(&self, word: &str) {
        if let Ok(mut own) = self.own.write() {
            own.insert(word.to_owned());
        }
    }

    /// Where `line`'s misspelled words are: their first and last
    /// characters' columns.
    pub fn misspelled(&self, line: &str) -> Vec<(usize, usize)> {
        let chars: Vec<char> = line.chars().collect();
        words(line)
            .into_iter()
            .filter(|&(start, end)| !self.check(&chars[start..=end].iter().collect::<String>()))
            .collect()
    }
}

/// Where dictionaries are looked for: `spell_dict_path`, then mett's own
/// directory, then the system's.
fn dirs(settings: &Settings, home: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = settings.dirs.clone();
    if let Some(home) = home {
        dirs.push(Path::new(home).join(".config/mett/dictionaries"));
        if cfg!(target_os = "macos") {
            dirs.push(Path::new(home).join("Library/Spelling"));
        }
    }
    if cfg!(target_os = "macos") {
        dirs.push("/Library/Spelling".into());
    } else if cfg!(unix) {
        dirs.extend(
            [
                "/usr/share/hunspell",
                "/usr/share/myspell/dicts",
                "/usr/share/myspell",
            ]
            .map(PathBuf::from),
        );
    }
    dirs
}

/// The dictionary for `lang` in `dir` (`en_US.aff` and `en_US.dic`; `en_us`
/// finds them too), as text, if it is there.
fn read_dictionary(dir: &Path, lang: &str) -> Result<Option<(String, String)>, String> {
    // en_us, as vim writes it, is en_US.
    let names = match lang.split_once('_') {
        Some((language, region)) => vec![
            lang.to_owned(),
            format!("{}_{}", language.to_lowercase(), region.to_uppercase()),
        ],
        None => vec![lang.to_owned()],
    };
    for name in names {
        let (aff, dic) = (
            dir.join(format!("{name}.aff")),
            dir.join(format!("{name}.dic")),
        );
        if !(aff.is_file() && dic.is_file()) {
            continue;
        }
        let read =
            |path: &Path| std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()));
        let (aff, dic) = (read(&aff)?, read(&dic)?);
        // Hunspell dictionaries name their encoding (`SET ISO8859-1`):
        // spellbook reads UTF-8.
        let encoding = String::from_utf8_lossy(&aff)
            .lines()
            .find_map(|line| line.strip_prefix("SET ").map(|name| name.trim().to_owned()))
            .and_then(|name| encoding_rs::Encoding::for_label(name.as_bytes()))
            .unwrap_or(encoding_rs::UTF_8);
        let decode = |bytes: &[u8]| encoding.decode(bytes).0.into_owned();
        return Ok(Some((decode(&aff), decode(&dic))));
    }
    Ok(None)
}

/// The words in `line` worth checking, as their first and last characters'
/// columns: letters, with apostrophes inside. Not in a web or mail address,
/// nor with digits in them, nor all capitals (`HTML`).
pub fn words(line: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    let mut words = Vec::new();
    let mut i = 0;
    // Where a token ends: a space, or punctuation around an address
    // (`word,bob@x.org`, `<https://x.org>`).
    let ends = |c: char| {
        c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '<' | '>' | '"' | '[' | ']')
    };
    while i < chars.len() {
        let start = i;
        while i < chars.len() && !ends(chars[i]) {
            i += 1;
        }
        let token = &chars[start..i];
        let text: String = token.iter().collect();
        // A web or mail address, or a path: with a scheme, `@`, a dot
        // between letters (`mett.dev/path`, past what ends a sentence), or
        // starting as a path does (`/usr`, `~/x`, `./x`). `and/or` is
        // words.
        let bare = text.trim_end_matches(['.', ',', '!', '?', ':', ';', ')']);
        let dotted = bare.char_indices().any(|(i, c)| {
            c == '.'
                && bare[..i]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric)
                && bare[i + 1..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric)
        });
        let path = ["/", "~/", "./", "../"].iter().any(|p| text.starts_with(p));
        let address = text.contains("://") || text.contains('@') || dotted || path;
        if !address {
            words.extend(words_in(token).map(|(s, e)| (start + s, start + e)));
        }
        while i < chars.len() && ends(chars[i]) {
            i += 1;
        }
    }
    words
}

/// The words in `token` (no spaces), as in [`words`].
fn words_in(token: &[char]) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < token.len() {
            if !token[i].is_alphanumeric() {
                i += 1;
                continue;
            }
            let start = i;
            while i < token.len()
                && (token[i].is_alphanumeric()
                    || (matches!(token[i], '\'' | '’')
                        && token.get(i + 1).is_some_and(|c| c.is_alphabetic())))
            {
                i += 1;
            }
            let word = &token[start..i];
            let digits = word.iter().any(char::is_ascii_digit);
            // (Scripts without capitals, such as Hebrew's, have no
            // words in capitals.)
            let capitals = word.len() > 1
                && word.iter().any(|c| c.is_uppercase())
                && word.iter().all(|c| !c.is_lowercase());
            if !digits && !capitals {
                return Some((start, i - 1));
            }
        }
        None
    })
}

/// What isn't checked in a message: quoted lines and the signature, as
/// the compose settings write them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unchecked {
    /// What quoted lines start with (`indent_string`, without the spaces
    /// after it); lines starting with `>` are quoted too.
    pub quote: String,
    /// The signature's lines, as they end the message (without `-- `,
    /// which marks it anyway).
    pub signature: Vec<String>,
}

impl Default for Unchecked {
    fn default() -> Self {
        Self {
            quote: ">".into(),
            signature: Vec::new(),
        }
    }
}

impl Unchecked {
    /// From `indent_string` and the signature.
    pub fn new(indent_string: &str, signature: Option<&str>) -> Self {
        Self {
            quote: indent_string.trim_end().to_owned(),
            signature: signature
                .map(|text| text.lines().map(str::to_owned).collect())
                .unwrap_or_default(),
        }
    }

    /// Whether each of `lines` is worth checking: not quoted, nor in the
    /// signature (after a `-- ` line, or the signature's own lines).
    pub fn rows(&self, lines: &[String]) -> Vec<bool> {
        let quoted = |line: &String| {
            line.trim_start().starts_with('>')
                || (!self.quote.is_empty() && line.starts_with(&self.quote))
        };
        let dashes = lines.iter().position(|line| line == "-- ");
        let n = self.signature.len();
        // The signature ends the message: only blank lines after it.
        let end = lines
            .iter()
            .rposition(|line| !line.trim().is_empty())
            .map_or(0, |last| last + 1);
        let own = (n > 0 && end >= n && lines[end - n..end] == self.signature[..]).then(|| end - n);
        let signature = [dashes, own]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(lines.len());
        lines
            .iter()
            .enumerate()
            .map(|(row, line)| row < signature && !quoted(line))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speller() -> Speller {
        Speller::load(&Settings::default(), None).unwrap()
    }

    #[test]
    fn finds_misspelled_words() {
        let speller = speller();
        assert!(speller.check("hello"));
        assert!(!speller.check("helo"));
        // "Thsi" (columns 0-3) and "mispeled" (13-20).
        assert_eq!(
            speller.misspelled("Thsi line is mispeled."),
            [(0, 3), (13, 20)]
        );
        assert!(speller.suggest("helo").iter().any(|s| s == "hello"));
        speller.add("mett");
        assert!(speller.check("mett"));
    }

    #[test]
    fn leaves_addresses_numbers_and_capitals_alone() {
        assert_eq!(
            words("See https://x.org/abcd or bob@x.org, 3rd HTML don't"),
            [(0, 2), (23, 24), (46, 50)]
        );
        assert_eq!(words("l’été"), [(0, 4)]);
        // Bare addresses, and words in scripts without capitals.
        assert_eq!(
            words("see mett.dev/path or ~/x and/or, end."),
            [(0, 2), (18, 19), (25, 27), (29, 30), (33, 35)]
        );
        assert_eq!(words("שלום עולם"), [(0, 3), (5, 8)]);
        // A word beside an address is still one.
        assert_eq!(
            words("Thsi,bob@x.org (see <https://x.org>)"),
            [(0, 3), (16, 18)]
        );
    }

    #[test]
    fn quotes_and_signatures_are_not_checked() {
        let lines = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let default = Unchecked::default();
        let text = lines(&["Hi", "> quoted", "", "-- ", "Me"]);
        assert_eq!(default.rows(&text), [true, false, true, false, false]);
        // Another indent_string, and a signature without dashes.
        let unchecked = Unchecked::new("| ", Some("Mee\nACME\n"));
        let text = lines(&["Hi", "| qoted", "Mee", "", "Mee", "ACME", ""]);
        assert_eq!(
            unchecked.rows(&text),
            [true, false, true, true, false, false, false]
        );
        // Written below it, it isn't the signature any more.
        let text = lines(&["Mee", "ACME", "PS: more"]);
        assert_eq!(unchecked.rows(&text), [true, true, true]);
    }

    #[test]
    fn settings_from_the_configuration() {
        let settings = Settings::new(true, "en_us, zz_ZZ", "", "", None);
        assert_eq!(settings.langs, ["en_us", "zz_ZZ"]);
        assert_eq!(settings.words, None);
        // en_us is mett's own en_US.
        assert!(Speller::load(&settings, None).is_err_and(|e| e.contains("zz_ZZ")));
        // No dictionaries: no checking, rather than every word right.
        assert!(Speller::load(&Settings::new(true, ", ,", "", "", None), None).is_err());
        let only = Settings::new(true, "en_us", "", "", None);
        // `~` in the paths is the home directory.
        let home = Settings::new(true, "en_US", "~/dicts", "", Some("/home/me"));
        assert_eq!(home.dirs, [PathBuf::from("/home/me/dicts")]);
        assert!(Speller::load(&only, None).is_ok());
    }

    #[test]
    fn reads_the_user_s_words_and_other_dictionaries() {
        let dir = std::env::temp_dir().join(format!("mett-spell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("words"), "Gelado\nmett\n").unwrap();
        // A tiny Latin-1 dictionary.
        std::fs::write(dir.join("xx_YY.aff"), "SET ISO8859-1\n").unwrap();
        std::fs::write(dir.join("xx_YY.dic"), b"1\ncaf\xe9\n").unwrap();
        let settings = Settings {
            langs: vec!["xx_yy".into()],
            dirs: vec![dir.clone()],
            words: Some(dir.join("words")),
            ..Settings::default()
        };
        let speller = Speller::load(&settings, None).unwrap();
        assert!(speller.check("café"));
        assert!(speller.check("Gelado"));
        assert!(!speller.check("hello"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
