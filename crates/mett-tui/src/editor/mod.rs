//! The editor messages are written in: the user's own, as mutt runs it
//! (the text goes to a private temporary file, the editor runs on it, and
//! the file is read back and removed), or mett's built-in one
//! ([`builtin`]).

pub mod builtin;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The `editor` that asks for the built-in editor.
pub const BUILTIN: &str = "builtin";

/// The key bindings of the built-in editor (`editor_keys`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keys {
    /// vim's, with its modes.
    Vim,
    /// emacs's.
    Emacs,
    /// Those of Notepad and most graphical editors.
    Notepad,
}

impl Keys {
    /// The bindings `editor_keys` names, or, if it names none, those like
    /// the ones of the editor the user named (`named`).
    pub fn from_config(editor_keys: &str, named: Option<&str>) -> Result<Self, String> {
        match editor_keys.trim().to_ascii_lowercase().as_str() {
            "" => Ok(Self::like(named)),
            "vim" => Ok(Self::Vim),
            "emacs" => Ok(Self::Emacs),
            "notepad" => Ok(Self::Notepad),
            other => Err(format!(
                "editor_keys: {other:?} is not vim, emacs or notepad"
            )),
        }
    }

    /// The bindings the user of the editor `command` runs knows: vim's for
    /// vi and its kin, emacs's for emacs and its kin, notepad's for any
    /// other editor, or none.
    pub fn like(command: Option<&str>) -> Self {
        const VIM: &[&str] = &[
            "vi", "vim", "nvim", "gvim", "mvim", "vimx", "nvi", "elvis", "lvim", "neovide",
        ];
        const EMACS: &[&str] = &[
            "emacs",
            "emacsclient",
            "xemacs",
            "mg",
            "zile",
            "jove",
            "qemacs",
            "uemacs",
        ];
        let Some(program) = command.and_then(program) else {
            return Self::Notepad;
        };
        // `vim.basic`, `emacs-nox`, `emacs29`, `gvim.exe`: the name is
        // the letters it starts with.
        let name: String = program
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect::<String>()
            .to_ascii_lowercase();
        if VIM.contains(&name.as_str()) {
            Self::Vim
        } else if EMACS.contains(&name.as_str()) {
            Self::Emacs
        } else {
            Self::Notepad
        }
    }
}

/// The file name of the program a shell command runs: its first word,
/// past `env` and variable settings (`TERM=xterm`), without the
/// directories (`/usr/bin/vim`, `"C:\Program Files\Vim\gvim.exe"`).
fn program(command: &str) -> Option<&str> {
    let mut rest = command.trim_start();
    loop {
        let (word, after) = match rest.chars().next()? {
            quote @ ('"' | '\'') => {
                let end = rest[1..].find(quote).map_or(rest.len(), |i| i + 1);
                (&rest[1..end], rest.get(end + 1..).unwrap_or(""))
            }
            _ => {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                (&rest[..end], &rest[end..])
            }
        };
        rest = after.trim_start();
        if word == "env" || (word.contains('=') && !word.contains(['/', '\\'])) {
            continue;
        }
        return word
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.is_empty());
    }
}

/// The editor the user named: `editor` (unless it is [`BUILTIN`]), else
/// `$VISUAL`, else `$EDITOR`.
pub fn named(editor: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let editor = (editor.trim() != BUILTIN).then(|| editor.to_owned());
    [editor, env("VISUAL"), env("EDITOR")]
        .into_iter()
        .flatten()
        .find(|c| !c.trim().is_empty())
}

/// The keys of the built-in editor, if it is the one messages are written
/// in: always in the GUI (`gui`); in the terminal, when `editor` is
/// [`BUILTIN`] or no editor is named.
pub fn builtin(
    editor: &str,
    editor_keys: &str,
    gui: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Keys>, String> {
    let named = named(editor, env);
    let keys = Keys::from_config(editor_keys, named.as_deref())?;
    Ok((gui || editor.trim() == BUILTIN || named.is_none()).then_some(keys))
}

/// The editor command and where its temporary files go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Editor {
    /// A shell command; the file name is appended as an argument.
    command: String,
    /// The directory for temporary files.
    tmpdir: PathBuf,
}

impl Editor {
    /// The editor [`named`] by mutt's `editor` variable, `$VISUAL` or
    /// `$EDITOR` (none, when the built-in editor is used instead);
    /// temporary files go in `tmpdir`, else the system's temporary
    /// directory (`$TMPDIR`).
    pub fn new(editor: &str, tmpdir: &str, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let command = named(editor, env).unwrap_or_default();
        let tmpdir = if tmpdir.is_empty() {
            std::env::temp_dir()
        } else {
            PathBuf::from(tmpdir)
        };
        Self { command, tmpdir }
    }

    /// [`Editor::new`], with mett's own environment.
    pub fn from_env(editor: &str, tmpdir: &str) -> Self {
        Self::new(editor, tmpdir, &|name| std::env::var(name).ok())
    }

    /// Lets the user edit `text` and returns the result. The caller hands
    /// the terminal over first.
    ///
    /// If the editor fails, the file is kept, since it may hold what the
    /// user wrote, and the error says where it is.
    pub fn edit(&self, text: &str) -> Result<String, String> {
        let path = self.create(text)?;
        match self.run(&path) {
            Ok(text) => {
                let _ = fs::remove_file(&path);
                Ok(text)
            }
            Err(e) => Err(format!("{e}; the message is kept in {}", path.display())),
        }
    }

    /// Writes `text` to a new temporary file that only the user can read.
    fn create(&self, text: &str) -> Result<PathBuf, String> {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let mut options = OpenOptions::new();
        // Never reuse or follow an existing file: another user could have
        // planted it.
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        for _ in 0..100 {
            let n = COUNT.fetch_add(1, Ordering::Relaxed);
            let path = self
                .tmpdir
                .join(format!("mett-{}-{n}.txt", std::process::id()));
            match options.open(&path) {
                Ok(mut file) => {
                    return match file.write_all(text.as_bytes()) {
                        Ok(()) => Ok(path),
                        Err(e) => {
                            let _ = fs::remove_file(&path);
                            Err(format!("cannot write {}: {e}", path.display()))
                        }
                    };
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(format!(
                        "cannot create a temporary file in {}: {e}",
                        self.tmpdir.display()
                    ));
                }
            }
        }
        Err(format!(
            "cannot create a temporary file in {}",
            self.tmpdir.display()
        ))
    }

    fn run(&self, path: &Path) -> Result<String, String> {
        if self.command.trim().is_empty() {
            return Err("no editor is set".into());
        }
        // The command may have arguments (`vim +star`), so the shell runs
        // it.
        let status = crate::platform::editor(&self.command, path)
            .status()
            .map_err(|e| format!("cannot run the editor: {e}"))?;
        if !status.success() {
            return Err(format!("Error running \"{}\": {status}", self.command));
        }
        fs::read_to_string(path).map_err(|e| format!("cannot read the edited message: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of its own for test `name`.
    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mett-editor-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn chooses_the_editor_like_mutt() {
        let env = |name: &str| (name == "EDITOR").then(|| "nano".to_owned());
        assert_eq!(Editor::new("", "", &env).command, "nano");
        assert_eq!(Editor::new("emacs -nw", "", &env).command, "emacs -nw");
        assert_eq!(Editor::new("", "/x", &|_| None).command, "");
        assert_eq!(Editor::new("builtin", "", &env).command, "nano");
        assert_eq!(Editor::new("", "/x", &|_| None).tmpdir, PathBuf::from("/x"));
    }

    #[test]
    fn edits_through_a_private_temporary_file() {
        let dir = tmpdir("private");
        // An "editor" that appends a line, and records the file's mode.
        let editor = Editor::new(
            "f() { echo world >> \"$1\"; ls -l \"$1\" | cut -c1-10 > \"$1.mode\"; }; f",
            dir.to_str().unwrap(),
            &|_| None,
        );
        assert_eq!(editor.edit("hello\n").unwrap(), "hello\nworld\n");
        let modes: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        // The message file itself is gone; only the mode record is left.
        assert_eq!(modes.len(), 1, "{modes:?}");
        // (Windows has no such modes: the user's own temporary directory
        // keeps it private there.)
        #[cfg(unix)]
        assert_eq!(fs::read_to_string(&modes[0]).unwrap().trim(), "-rw-------");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_the_text_when_the_editor_fails() {
        let dir = tmpdir("failing");
        let editor = Editor::new(
            "f() { echo more >> \"$1\"; exit 3; }; f",
            dir.to_str().unwrap(),
            &|_| None,
        );
        let error = editor.edit("text\n").unwrap_err();
        assert!(error.contains("Error running"), "{error}");
        let kept = error.rsplit("kept in ").next().unwrap();
        assert_eq!(fs::read_to_string(kept).unwrap(), "text\nmore\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_builtin_editor_is_used_when_no_other_is_named() {
        let none = |_: &str| None;
        let vim = |name: &str| (name == "EDITOR").then(|| "vim".to_owned());
        assert_eq!(builtin("", "", false, &none), Ok(Some(Keys::Notepad)));
        assert_eq!(builtin("", "", false, &vim), Ok(None));
        assert_eq!(builtin("emacs -nw", "", false, &none), Ok(None));
        // `builtin` asks for it; the keys still follow `$EDITOR`.
        assert_eq!(builtin("builtin", "", false, &vim), Ok(Some(Keys::Vim)));
        // The GUI has only the built-in editor, with the keys of the one
        // named.
        assert_eq!(builtin("emacs -nw", "", true, &vim), Ok(Some(Keys::Emacs)));
        assert_eq!(builtin("", "Notepad", true, &vim), Ok(Some(Keys::Notepad)));
        assert!(builtin("", "ed", false, &none).is_err());
    }

    #[test]
    fn the_keys_follow_the_editor_named() {
        for (command, keys) in [
            ("vim", Keys::Vim),
            ("/usr/bin/vim.basic +star", Keys::Vim),
            ("nvim", Keys::Vim),
            ("env TERM=xterm-256color vi", Keys::Vim),
            ("\"C:\\Program Files\\Vim\\gvim.exe\" -f", Keys::Vim),
            ("'/opt/my editors/nvim-qt'", Keys::Vim),
            ("emacsclient -t", Keys::Emacs),
            ("emacs-nox", Keys::Emacs),
            ("emacs29 -nw", Keys::Emacs),
            ("mg", Keys::Emacs),
            ("nano", Keys::Notepad),
            ("code --wait", Keys::Notepad),
            ("notepad.exe", Keys::Notepad),
            ("hx", Keys::Notepad),
            ("visual-studio", Keys::Notepad),
            ("", Keys::Notepad),
        ] {
            assert_eq!(Keys::like(Some(command)), keys, "{command}");
        }
        assert_eq!(Keys::like(None), Keys::Notepad);
    }
}
