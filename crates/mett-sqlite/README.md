# mett-sqlite

mett's local cache, in SQLite. `CachedStore` sits in front of a backend's
`MailStore` (Microsoft Graph or Exchange Web Services) and keeps the index
of each folder listed, and each message read, in a database file: mutt's
`header_cache`, by default `~/.cache/mett/<account>.sqlite`.

- **Syncing:** listing a folder first syncs it with the backend's
  `sync_folder` (EWS `SyncFolderItems`, Graph delta queries), so only what
  arrived, changed or left since the last time comes from the server. The
  first time, the whole folder comes, a batch at a time; each batch is kept
  as it arrives, so a sync cut short carries on where it stopped. A folder
  synced in the last two seconds isn't synced again, so the calls of one
  check for new mail share a sync. When the server no longer knows where
  the sync stood, the folder is synced from the start, keeping what is
  cached until the sync says it is gone.
- **Listing:** the index pages, the messages changed since a time, and the
  unread mail of the `mailboxes` come from the cache. The folders are kept
  as last seen too, so offline, a folder seen before is found, and one
  synced to the end is listed, as it was.
- **Messages:** a message's body is kept once it is read, and goes with
  its message. One that changes on the server (EWS lets bodies be edited)
  is fetched again; drafts' bodies aren't kept, as drafts change.
- **Changes:** flags, moves and deletions go to the server first; the
  cache follows at once, and the next sync confirms them. What mett puts
  elsewhere (a move, a copy, a draft, a sent message) makes folders sync
  again at once (only the folder a copy or a watch names, where the change
  is known to be in one). The server is always the source of truth.
- **Failures:** if the cache fails (a full disk, a corrupt file, say), mett
  leaves it alone for the rest of the session and asks the server for
  everything. A cache that is only busy (another mett is writing to it)
  isn't given up on: the server answers for ten seconds (SQLite already
  waited five), and then the cache serves again.
- **Threads:** SQLite's calls block (waiting for another mett's lock, a
  large sync batch), so `CachedStore` runs each on a blocking thread
  (`spawn_blocking`) and awaits it: the executor, and the UI sharing it,
  keep running. A call dropped meanwhile still runs to its end there, its
  transaction finished (or rolled back) and the cache's state noted. The
  `Db` itself needs no runtime.
- **Storage:** a message's row keeps what is sorted, filtered or changed
  alone (dates, read, flagged) in columns, with indexes that page by date
  and by change (and one for unread mail), and the rest of its summary as
  JSON. The database is in WAL mode with `synchronous=NORMAL`: a crash may
  lose the last changes, which the next sync brings back.
- **Privacy:** the cache holds mail, so on Unix its directory is created
  with mode `0700` and its file with mode `0600` (an existing one is made
  so), and a directory others can write to is refused. On Windows, mett
  sets no permissions: the cache relies on its folder's access rules
  (the user profile's, by default, which others can't read). Everywhere,
  a symbolic link, or a database that isn't mett's cache, is refused.

A cache of another schema version is dropped and filled again: it holds
nothing the server doesn't. A cache holds one account's mailbox: another
account's mett doesn't use it (with a warning), as the account's own mett
may have it open.

## Automated tests

```sh
cargo test -p mett-sqlite
```

The tests run the cache in front of `mett-core`'s in-memory store, with
the clock paused, and need no network access.

## Manual testing

Run mett twice on a large folder. The first start syncs the whole folder;
the second lists it from the cache at once and asks the server only for
what changed. Deleting `~/.cache/mett/<account>.sqlite` (or
`unset header_cache`) starts afresh.
