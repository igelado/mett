# mett-imap

The IMAP adapter for mett. It wraps the
[`async-imap`](https://crates.io/crates/async-imap) crate (tokio, with TLS by
rustls) and [`mail-parser`](https://crates.io/crates/mail-parser), and
implements `mett-core`'s ports against any IMAP server:

- **`ImapMailStore`** implements `MailStore`: folders (their roles from the
  server's `SPECIAL-USE` attributes, or their usual names; counts from
  `LIST-STATUS` or `STATUS`), messages (newest first by their dates where the server has
  `SORT`, by `UID` (and so by arrival) where it hasn't; a page at a time; threaded by
  `References:`), message bodies (HTML-only mail as HTML) and attachments,
  read and flagged state, move (`MOVE`, else copy and delete), copy and delete
  (`UID EXPUNGE`: a server without `UIDPLUS` can't delete one message without
  deleting every one marked deleted, so mett refuses), new-mail checks, and drafts
  (appended to the Drafts folder, with `\Draft`).
- With **`with_smtp(SmtpSender)`** it also implements `MailSender`: it sends
  through `mett-smtp`, threads replies from the original's `Message-ID:` and
  `References:`, carries the attachments of forwards and recalled drafts, and
  appends the copy to Sent.

The connection is plain, STARTTLS (required, or only if the server offers it)
or TLS from the first byte (`Security`), checked against the system's
certificate authorities and any more given, and for the name it is made
to unless `verify_host` is off (with the platform's verifier throughout:
only a name mismatch is let through; see `mett-smtp` for how SMTP differs).
`Login`, `Security` and `ServerSettings` (embedded in `ImapSettings` as
`server`) are `mett-core`'s. The login is a password (`LOGIN`)
or an OAuth2 token from any `Authenticator` (`Login::OAuth`, SASL XOAUTH2; a
token the server turns down is replaced once). It keeps one connection, which
it makes again if it is lost or has sat idle.

A folder is called by its name on the server. A message is called by its
folder, the folder's `UIDVALIDITY` and its `UID` (`<validity>.<uid>.<folder>`),
so a message that moves has a new identifier, as with Microsoft Graph, and
one of a folder the server made anew is gone.

**Syncing.** With `CONDSTORE` (nearly every server), `ImapMailStore` also
implements `sync_folder`, so `mett-sqlite`'s cache works in front of it as it
does in front of Graph and EWS (`can_sync` is true once connected). A folder's
sync state holds its `UIDVALIDITY`, the next `UID`, the highest modification
sequence and the `UID`s it had: changed messages are those `CHANGEDSINCE` the
sequence, and the new ones are among them (those from the next `UID`: a new
message's sequence is above all others, RFC 7162; a folder whose sequence
didn't move is asked with `UID SEARCH` instead), and the messages that went
are those missing from the `UID`s the folder has now (asked for only when
its message count doesn't add up). A sync of many messages is made 100 at a
time. A folder made anew, or a state that isn't understood, is `StaleSync`: the cache
starts that folder over. Servers without `CONDSTORE` can't be synced (flags
changed by other clients couldn't be found), so mett doesn't cache them. On a
server that can't `SORT`, messages are dated by arrival; the cache orders
those that arrived in the same second by their ids, not by `UID`. A sync of
more than 250 new messages takes them in newest first, so the new-mail check
(which is limited to 250 unread messages) may report some older ones in
place of the newest of such a burst.

Limits: IMAP can't say what changed in a folder as Exchange's APIs do (no
`QRESYNC`, no `IDLE` yet), so the adapter doesn't watch. To find what is new
without the cache it remembers, for each folder,
the `UID` the next message would have and, with `CONDSTORE`, the modification
sequence: mail copied in with an old date and flags that another client
changed are found that way; without `CONDSTORE`, a flag another client
changed is seen when the folder is opened again. Meeting invitations are
attachments, not answered. The folder listing counts messages with the
listing itself where the server has `LIST-STATUS`, and else with a `STATUS` for
each folder, sent together without waiting for each answer (in rounds of 50).

The configuration (`imaps://` URLs, `imap_user`, `imap_pass`, `smtp_url`,
`ssl_*`) is described in [docs/MUTTRC.md](../../docs/MUTTRC.md) §4.13.

## Automated tests

```sh
cargo test -p mett-imap
```

The tests run every operation against an IMAP server in the test process
(in-memory mailboxes; plain, STARTTLS and TLS with a generated certificate) and
an SMTP one. They need no network access.
