//! Looking up addresses with the user's `query_command`.

use tokio::io::AsyncReadExt;

/// How much of a `query_command`'s output is read: 1 MiB.
const QUERY_OUTPUT_LIMIT: u64 = 1 << 20;

/// Runs `query_command` for `text`, as mutt does: `%s` is the text,
/// quoted for the shell; the first line of output is a message, and each
/// line after it an address, a tab, then a name.
pub(super) async fn run_query(
    command: &str,
    text: &str,
) -> std::result::Result<Vec<mett_core::Address>, String> {
    // Killed, with whatever it started, if the lookup is given up or mett
    // quits first: a command that hangs doesn't keep it from exiting.
    let mut shell = tokio::process::Command::from(crate::platform::sh(
        &query_script(command),
        "query_command",
    )?);
    shell
        .env("METT_QUERY", text)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // On Windows, KillGroup kills the shell with what it started: it
        // must still be there for that.
        .kill_on_drop(!cfg!(windows));
    #[cfg(unix)]
    shell.process_group(0);
    let child = shell.spawn().map_err(|e| format!("query_command: {e}"))?;
    let mut child = child;
    let mut group = KillGroup(child.id());
    // What the command prints is read up to a limit: a runaway command
    // doesn't fill the memory (the group is killed when this gives up).
    let mut stdout = Vec::new();
    if let Some(out) = child.stdout.take() {
        out.take(QUERY_OUTPUT_LIMIT + 1)
            .read_to_end(&mut stdout)
            .await
            .map_err(|e| format!("query_command: {e}"))?;
    }
    if stdout.len() as u64 > QUERY_OUTPUT_LIMIT {
        return Err("query_command: output over 1 MiB; ignored".to_owned());
    }
    let status = child
        .wait()
        .await
        .map_err(|e| format!("query_command: {e}"))?;
    group.0 = None;
    let stdout = String::from_utf8_lossy(&stdout);
    let mut lines = stdout.lines();
    let first = lines.next().unwrap_or_default();
    if !status.success() {
        return Err(if first.is_empty() {
            format!("query_command failed: {status}")
        } else {
            first.to_owned()
        });
    }
    Ok(lines
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let email = fields.next()?.trim();
            // One mailbox, and nothing else (not a list, nor a name).
            let parsed = mett_core::Address::parse_list(email).ok()?;
            let [address] = parsed.as_slice() else {
                return None;
            };
            if !email.contains('@') || address.name.is_some() || address.email != email {
                return None;
            }
            let address = mett_core::Address::new(email);
            Some(
                match fields.next().map(str::trim).filter(|n| !n.is_empty()) {
                    Some(name) => address.with_name(name),
                    None => address,
                },
            )
        })
        .collect())
}

/// Kills the process group of a query command when dropped while it runs
/// (the lookup was given up): `kill_on_drop` alone leaves what the shell
/// started. On Windows, which has no process groups, it kills the shell's
/// process tree.
struct KillGroup(Option<u32>);

impl Drop for KillGroup {
    fn drop(&mut self) {
        let Some(pid) = self.0 else {
            return;
        };
        let mut kill = if cfg!(windows) {
            let mut kill = std::process::Command::new("taskkill");
            kill.args(["/T", "/F", "/PID", &pid.to_string()]);
            kill
        } else {
            let mut kill = std::process::Command::new("kill");
            kill.args(["-TERM", "--", &format!("-{pid}")]);
            kill
        };
        // Not waited for here, in an async task: a thread reaps it.
        if let Ok(mut child) = kill
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// `command` with each `%s` standing for the text looked up, which the
/// shell gets in the environment (`$METT_QUERY`, which shell functions
/// see too): whatever it holds, it is never read
/// as shell syntax, inside quotes or out.
fn query_script(command: &str) -> String {
    /// Where in the shell's syntax the command is.
    #[derive(Clone, Copy, PartialEq)]
    enum Context {
        /// Unquoted, in the command or a `(...)`.
        Plain,
        /// In `'...'`.
        Single,
        /// In `"..."`.
        Double,
        /// In `$(...)` or `` `...` ``, quoted afresh whatever is around.
        Substitution,
        Backtick,
    }
    use Context::{Backtick, Double, Plain, Single, Substitution};
    let mut script = String::new();
    let mut stack = vec![Plain];
    // Without `%s`, the text goes at the end, as in mutt.
    let placed = command.replace("%%", "").contains("%s");
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        let context = *stack.last().expect("the command is there");
        match c {
            // `%%` is a `%`, as in mutt.
            '%' if chars.peek() == Some(&'%') => {
                chars.next();
                script.push('%');
                continue;
            }
            '%' if chars.peek() == Some(&'s') => {
                chars.next();
                script.push_str(match context {
                    Double => "${METT_QUERY}",
                    // Out of the quotes and back.
                    Single => "'\"${METT_QUERY}\"'",
                    Plain | Substitution | Backtick => "\"${METT_QUERY}\"",
                });
                continue;
            }
            '\\' if context != Single => {
                script.push(c);
                if let Some(next) = chars.next() {
                    script.push(next);
                }
                continue;
            }
            '\'' if context == Single => {
                stack.pop();
            }
            '\'' if context != Double => stack.push(Single),
            '"' if context == Double => {
                stack.pop();
            }
            '"' if context != Single => stack.push(Double),
            '$' if context != Single && chars.peek() == Some(&'(') => {
                script.push(c);
                script.extend(chars.next());
                stack.push(Substitution);
                continue;
            }
            '(' if matches!(context, Plain | Substitution | Backtick) => stack.push(Plain),
            ')' if matches!(context, Plain | Substitution) && stack.len() > 1 => {
                stack.pop();
            }
            '`' if context == Backtick => {
                stack.pop();
            }
            '`' if context != Single => stack.push(Backtick),
            _ => {}
        }
        script.push(c);
    }
    if !placed {
        script.push_str(" \"${METT_QUERY}\"");
    }
    script
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mett_core::mock::MemoryStore;
    use tokio::sync::mpsc;

    use super::super::{QuerySource, Request, RequestId, run};
    use super::*;

    #[tokio::test]
    async fn a_query_given_up_has_no_answer() {
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (response_tx, mut response_rx) = mpsc::unbounded_channel();
        tokio::spawn(run(Arc::new(MemoryStore::new()), request_rx, response_tx));
        let slow = Request::Query(QuerySource::Command("sleep 30".into()), "x".into());
        request_tx.send((RequestId(1), slow)).unwrap();
        request_tx
            .send((RequestId(2), Request::Cancel(RequestId(1))))
            .unwrap();
        request_tx
            .send((RequestId(3), Request::OwnAddresses))
            .unwrap();
        let first = response_rx.recv().await.unwrap();
        assert_eq!(first.id, RequestId(3));
        let more =
            tokio::time::timeout(std::time::Duration::from_millis(200), response_rx.recv()).await;
        assert!(more.is_err(), "the query answered: {more:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_query_command_printing_too_much_is_given_up() {
        let flood = run_query("yes x | head -c 3000000; echo", "a").await;
        assert_eq!(
            flood,
            Err("query_command: output over 1 MiB; ignored".into())
        );
        // Just under the limit is read.
        assert!(
            run_query("echo; head -c 1000000 /dev/zero | tr '\\0' x; echo", "a")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn runs_query_commands_like_mutt() {
        let found = run_query(
            "echo Searching for %s; printf 'bob@x.org\\tBob Smith\\tx\\nnot an address\\na@x.org, e@x.org\\n'",
            "it's",
        )
        .await
        .unwrap();
        assert_eq!(
            found,
            [mett_core::Address::new("bob@x.org").with_name("Bob Smith")]
        );
        assert_eq!(
            run_query("echo 'No such person'; exit 1", "x").await,
            Err("No such person".into())
        );
        // Without `%s`, the text comes last.
        let found = run_query("echo; echo", "c@d.org").await.unwrap();
        assert_eq!(found, [mett_core::Address::new("c@d.org")]);
        // In a command substitution, the text is still one word.
        let found = run_query("echo; echo \"$(set -- %s; echo n$#@x.org)\"", "a b")
            .await
            .unwrap();
        assert_eq!(found, [mett_core::Address::new("n1@x.org")]);
        // In a shell function too.
        let found = run_query("f() { echo; echo %s; }; f", "g@x.org")
            .await
            .unwrap();
        assert_eq!(found, [mett_core::Address::new("g@x.org")]);
        // `%%` is a `%`.
        let found = run_query("echo; printf '%%s\\n' %s", "a@b.org")
            .await
            .unwrap();
        assert_eq!(found, [mett_core::Address::new("a@b.org")]);
        // The text is never shell syntax, whatever quotes `%s` is in.
        for command in [
            "echo; echo %s",
            "echo; echo \"%s\"",
            "echo; echo '%s'",
            "echo; echo \"$(echo %s)\"",
            "echo; echo \"`echo %s`\"",
        ] {
            let found = run_query(command, "a$HOME`x`'q@y.org").await.unwrap();
            assert_eq!(found, [mett_core::Address::new("a$HOME`x`'q@y.org")]);
        }
    }
}
