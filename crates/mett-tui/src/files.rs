//! Files: reading, writing and opening them, off the main loop.

use crate::app;
use crate::app::Msg;
use tokio::sync::mpsc;

/// The largest file Office365 takes as an attachment.
const MAX_ATTACHMENT: u64 = 150 * 1024 * 1024;

/// Appends `text` to the file at `path` (created if missing), on a line of
/// its own even if the file doesn't end with a newline.
pub(crate) fn append_line(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    // The end of the file, to tell whether its last command is over.
    let length = file.metadata()?.len();
    let mut tail = vec![0; usize::try_from(length.min(64)).unwrap_or(64)];
    if !tail.is_empty() {
        file.seek(SeekFrom::End(-i64::try_from(tail.len()).unwrap_or(64)))?;
        file.read_exact(&mut tail)?;
    }
    let (line, newline) = match tail.strip_suffix(b"\n") {
        // (A CRLF file reads as LF.)
        Some(line) => (line.strip_suffix(b"\r").unwrap_or(line), true),
        None => (tail.as_slice(), false),
    };
    // An odd number of backslashes at the end of the line carries the
    // command on: an empty line ends it.
    let continued = line.iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 1;
    let separator = match (newline || tail.is_empty(), continued) {
        (true, false) => "",
        (true, true) | (false, false) => "\n",
        (false, true) => "\n\n",
    };
    file.write_all(format!("{separator}{text}").as_bytes())
}

/// Reads a file to attach, stopping past [`MAX_ATTACHMENT`]: one larger
/// (or without end, such as `/dev/zero`) can't be sent.
pub(crate) fn read_attachment(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read;

    let mut data = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_ATTACHMENT + 1).read_to_end(&mut data))
        .map_err(|e| e.to_string())?;
    if data.len() as u64 > MAX_ATTACHMENT {
        return Err(format!(
            "larger than {} MB, which Office365 doesn't take",
            MAX_ATTACHMENT / 1024 / 1024
        ));
    }
    Ok(data)
}

/// How many files are being saved, or written for a viewer and opened:
/// mett waits for them before exiting, so none is left half written (nor
/// a viewer without its file).
static SAVING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Waits (a while) for the files being saved.
pub(crate) fn wait_for_saves() {
    // However long it takes: cut short, a file would be left half written.
    while SAVING.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Writes `data` to `path`; a file there is replaced only if `overwrite`,
/// and only once the new one is written in full.
pub(crate) fn save_file(
    path: std::path::PathBuf,
    data: Vec<u8>,
    overwrite: bool,
) -> app::FileOutcome {
    let written = if overwrite {
        replace_file(&path, &data)
    } else {
        write_new(&path, &data, None)
    };
    match written {
        Ok(()) => app::FileOutcome::Done(path),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            app::FileOutcome::Exists(path, data)
        }
        Err(e) => {
            let why = format!("{}: {e}", path.display());
            app::FileOutcome::Failed(path, why)
        }
    }
}

/// Writes `data` to a new file at `path`, which is removed again if that
/// fails partway; with `permissions`, if given, given to the file before
/// anything is written to it (it is private until then).
fn write_new(
    path: &std::path::Path,
    data: &[u8],
    permissions: Option<std::fs::Permissions>,
) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if permissions.is_some() {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let written = permissions
        .map_or(Ok(()), |permissions| file.set_permissions(permissions))
        .and_then(|()| file.write_all(data));
    written.inspect_err(|_| {
        let _ = std::fs::remove_file(path);
    })
}

/// Writes `data` beside `path`, then puts it in its place: a failure
/// leaves the file there as it was.
fn replace_file(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    // A link is followed: the file it names is what is replaced.
    let resolved = std::fs::canonicalize(path)?;
    let path = resolved.as_path();
    static WRITTEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("not a file name"))?;
    let count = WRITTEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = path.with_file_name(format!(
        ".{}.mett-{}-{count}",
        name.to_string_lossy(),
        std::process::id()
    ));
    // The file keeps its permissions, from the start: a private one is
    // never readable by others meanwhile.
    let permissions = std::fs::metadata(path)?.permissions();
    write_new(&temporary, data, Some(permissions))?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// Writes an attachment to a directory of its own, readable only by the
/// user, and opens it with the program the system has for it.
pub(crate) fn open_file(name: &str, data: &[u8]) -> std::io::Result<()> {
    static OPENED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // Changed on Unix only.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    // A name no earlier run left behind: the process, the time, and a
    // count, tried again if taken all the same.
    let started = now_nanos();
    let dir = loop {
        let n = OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mett-{}-{started}-{n}", std::process::id()));
        match builder.create(&dir) {
            Ok(()) => break dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => continue,
            Err(e) => return Err(e),
        }
    };
    // Taken away by a later run of mett, once old (a viewer it was given
    // to may still need it after mett exits), or now if it can't be
    // opened.
    let path = dir.join(name);
    let opened = std::fs::write(&path, data).and_then(|()| open::that_detached(&path));
    if opened.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    opened
}

/// The time, in nanoseconds since the epoch.
fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// How old a copy of an attachment given to a viewer has to be to be
/// taken away: mett can't tell when the viewer is done with it.
const VIEWED_LEFT: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// How long another mett may keep a viewer's copy, where mett can't tell
/// whether it still runs.
const VIEWED_KEPT: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// Whether process `pid`, which made a copy `age` ago, may still be
/// running (another mett, whose viewers still use their copies). Without
/// `/proc` to tell, one made in the last week may be.
fn running(pid: &str, age: std::time::Duration) -> bool {
    let proc = std::path::Path::new("/proc");
    if proc.is_dir() {
        proc.join(pid).exists()
    } else {
        age < VIEWED_KEPT
    }
}

/// Removes the copies of attachments that earlier runs left for their
/// viewers, once they are old (named `mett-PID-NANOS-N`, by when).
pub(crate) fn remove_viewed_before() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let now = now_nanos();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix("mett-")) else {
            continue;
        };
        let parts: Vec<&str> = rest.split('-').collect();
        let [pid, started, count] = parts.as_slice() else {
            continue;
        };
        let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let Ok(started) = started.parse::<u128>() else {
            continue;
        };
        if numeric(pid)
            && numeric(count)
            && now.saturating_sub(started) > VIEWED_LEFT.as_nanos()
            && !running(
                pid,
                std::time::Duration::from_nanos(
                    u64::try_from(now.saturating_sub(started)).unwrap_or(u64::MAX),
                ),
            )
            && entry.file_type().is_ok_and(|t| t.is_dir())
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Runs `work` on a thread of its own, and sends the app what it has to
/// say, if anything. One `counted` among [`SAVING`] is waited for before
/// exiting, until it is done.
pub(crate) fn spawn_file(
    files: &mpsc::UnboundedSender<Msg>,
    counted: bool,
    work: impl FnOnce() -> Option<Msg> + Send + 'static,
) {
    let files = files.clone();
    if counted {
        SAVING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    std::thread::spawn(move || {
        let msg = work();
        if counted {
            SAVING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(msg) = msg {
            let _ = files.send(msg);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn attachments_stop_at_what_office365_takes() {
        let read = read_attachment(std::path::Path::new("/dev/zero"));
        assert!(read.is_err_and(|e| e.contains("150 MB")));
    }

    #[test]
    fn replacing_a_file_keeps_it_until_the_new_one_is_written() {
        let dir = std::env::temp_dir().join(format!("mett-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.txt");
        std::fs::write(&path, "old").unwrap();
        assert!(matches!(
            save_file(path.clone(), b"new".to_vec(), false),
            app::FileOutcome::Exists(..)
        ));
        assert!(matches!(
            save_file(path.clone(), b"new".to_vec(), true),
            app::FileOutcome::Done(_)
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            save_file(path.clone(), b"newer".to_vec(), true);
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Through a link, the file it names is replaced, and the link stays.
        #[cfg(unix)]
        {
            let link = dir.join("link.txt");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            save_file(link.clone(), b"linked".to_vec(), true);
            assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "linked");
            std::fs::remove_file(&link).unwrap();
        }
        // Nothing left beside it.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn appends_on_a_line_of_its_own() {
        let dir = std::env::temp_dir().join(format!("mett-append-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mettrc");
        std::fs::write(&path, "set a = b").unwrap();
        append_line(&path, "alias x x@y\n").unwrap();
        append_line(&path, "alias z z@y\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "set a = b\nalias x x@y\nalias z z@y\n"
        );
        // A line carried on is ended first.
        let continued = dir.join("continued");
        std::fs::write(&continued, "set a = b \\\n").unwrap();
        append_line(&continued, "alias x x@y\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&continued).unwrap(),
            "set a = b \\\n\nalias x x@y\n"
        );
        let crlf = dir.join("crlf");
        std::fs::write(&crlf, "set a = b \\\r\n").unwrap();
        append_line(&crlf, "alias x x@y\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&crlf).unwrap(),
            "set a = b \\\r\n\nalias x x@y\n"
        );
        let new = dir.join("new/mettrc");
        append_line(&new, "alias x x@y\n").unwrap();
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "alias x x@y\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saving_a_file_replaces_one_there_only_when_asked() {
        let dir = std::env::temp_dir().join(format!("mett-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            save_file(path.clone(), b"one".to_vec(), false),
            app::FileOutcome::Done(path.clone())
        );
        assert_eq!(
            save_file(path.clone(), b"two".to_vec(), false),
            app::FileOutcome::Exists(path.clone(), b"two".to_vec())
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
        save_file(path.clone(), b"two".to_vec(), true);
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
