//! Running the user's editor on a message, as mutt does: the text goes to
//! a private temporary file, the editor runs on it, and the file is read
//! back and removed.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The editor command and where its temporary files go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Editor {
    /// A shell command; the file name is appended as an argument.
    command: String,
    /// The directory for temporary files.
    tmpdir: PathBuf,
}

impl Editor {
    /// The editor from mutt's `editor` variable, else `$VISUAL`, else
    /// `$EDITOR`, else `vi` (Notepad on Windows); temporary files go in
    /// `tmpdir`, else the
    /// system's temporary directory (`$TMPDIR`).
    pub fn new(editor: &str, tmpdir: &str, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let command = [Some(editor.to_owned()), env("VISUAL"), env("EDITOR")]
            .into_iter()
            .flatten()
            .find(|c| !c.trim().is_empty())
            .unwrap_or_else(|| crate::platform::DEFAULT_EDITOR.to_owned());
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
        assert_eq!(
            Editor::new("", "/x", &|_| None).command,
            crate::platform::DEFAULT_EDITOR
        );
        assert_eq!(Editor::new("", "/x", &|_| None).tmpdir, PathBuf::from("/x"));
    }

    #[test]
    fn edits_through_a_private_temporary_file() {
        let dir = tmpdir("private");
        // An "editor" that appends a line, and records the file's mode.
        let editor = Editor::new(
            "f() { echo world >> \"$1\"; stat -c %a \"$1\" > \"$1.mode\"; }; f",
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
        assert_eq!(fs::read_to_string(&modes[0]).unwrap().trim(), "600");
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
}
