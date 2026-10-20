//! What differs between the systems mett runs on: where the user's home
//! is, the default editor, and how commands are run.
//!
//! Commands from the mettrc (`editor`, `query_command`, backticks) are
//! shell commands, run with `sh -c` as mutt runs them. Windows has no `sh`
//! of its own, but Git for Windows (and MSYS2, Cygwin) brings one: mett
//! uses it when it finds it. Without it, only the editor can be run, with
//! `cmd`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

/// The editor used when neither `editor`, `$VISUAL` nor `$EDITOR` names
/// one.
pub const DEFAULT_EDITOR: &str = if cfg!(windows) { "notepad" } else { "vi" };

/// The user's home directory: `$HOME`, else (on Windows, where it is
/// usually unset) `%USERPROFILE%`.
pub fn home() -> Option<String> {
    let home = |name| std::env::var(name).ok().filter(|h| !h.is_empty());
    home("HOME").or_else(|| {
        if cfg!(windows) {
            home("USERPROFILE")
        } else {
            None
        }
    })
}

/// `sh -c script` (arguments added to it are `$0`, `$1`, ...), or an error
/// saying that `what` needs a shell, if there is none.
pub fn sh(script: &str, what: &str) -> Result<Command, String> {
    let sh = find_sh().ok_or_else(|| {
        format!("{what} needs a POSIX shell (sh), such as Git for Windows' on the PATH")
    })?;
    let mut command = Command::new(sh);
    command.arg("-c").arg(script);
    Ok(command)
}

/// The command that runs `editor` (a shell command, such as `vim +star`)
/// on `file`: with `sh` where there is one, else with `cmd`.
pub fn editor(editor: &str, file: &std::path::Path) -> Command {
    if let Some(sh) = find_sh() {
        // The file name is passed as a separate argument, so it needs no
        // quoting.
        let mut command = Command::new(sh);
        command
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("mett")
            .arg(file);
        return command;
    }
    cmd(&format!("{editor} \"{}\"", file.display()))
}

/// `cmd /C line`, given to `cmd` as it is (`cmd` doesn't parse its
/// arguments as other programs do).
#[cfg(windows)]
fn cmd(line: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut command = Command::new("cmd");
    command.arg("/C").raw_arg(line);
    command
}

/// Never needed: every other system has `sh`.
#[cfg(not(windows))]
fn cmd(line: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(line);
    command
}

/// The POSIX shell: `sh` itself everywhere but on Windows, where it is
/// looked for on the PATH, and else beside Git (`Git\cmd\git.exe` is often
/// on the PATH without `Git\bin\sh.exe`).
fn find_sh() -> Option<OsString> {
    if !cfg!(windows) {
        return Some("sh".into());
    }
    let path = std::env::var_os("PATH")?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    let on_path = |name: &str| dirs.iter().map(|dir| dir.join(name)).find(|p| p.is_file());
    on_path("sh.exe")
        .or_else(|| {
            let git = on_path("git.exe")?;
            let sh = git.parent()?.parent()?.join("bin").join("sh.exe");
            sh.is_file().then_some(sh)
        })
        .map(PathBuf::into_os_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_shell_commands() {
        let output = sh("printf '%s' \"$1\"", "test")
            .unwrap()
            .arg("mett")
            .arg("a b")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "a b");
    }
}
