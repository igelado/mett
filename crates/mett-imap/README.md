# mett-imap

The IMAP adapter for mett. It wraps the
[`async-imap`](https://crates.io/crates/async-imap) crate (tokio, with TLS by
rustls) and [`mail-parser`](https://crates.io/crates/mail-parser), and
implements `mett-core`'s ports against any IMAP server:

- **`ImapMailStore`** implements `MailStore`: folders (their roles from the
  server's `SPECIAL-USE` attributes, or their usual names; counts from
  `STATUS`), messages (newest first, a page at a time by `UID`; threaded by
  `References:`), message bodies (HTML-only mail as text) and attachments,
  read and flagged state, move (`MOVE`, else copy and delete), copy and delete
  (`UID EXPUNGE` when the server has `UIDPLUS`), new-mail checks, and drafts
  (appended to the Drafts folder, with `\Draft`).
- With **`with_smtp(SmtpSender)`** it also implements `MailSender`: it sends
  through `mett-smtp`, threads replies from the original's `Message-ID:` and
  `References:`, carries the attachments of forwards and recalled drafts, and
  appends the copy to Sent.

The connection is plain, STARTTLS (required, or only if the server offers it)
or TLS from the first byte (`Security`), checked against the system's
certificate authorities and any more given. The login is a password (`LOGIN`)
or an OAuth2 token from any `Authenticator` (`Login::OAuth`, SASL XOAUTH2; a
token the server turns down is replaced once). It keeps one connection, which
it makes again if it is lost or has sat idle.

A folder is called by its name on the server. A message is called by its
folder, the folder's `UIDVALIDITY` and its `UID` (`<validity>.<uid>.<folder>`),
so a message that moves has a new identifier, as with Microsoft Graph, and
one of a folder the server made anew is gone.

Limits: IMAP can't say what changed in a folder as Exchange's APIs do (no
`QRESYNC`, no `IDLE` yet), so the adapter neither syncs nor watches, and mett
doesn't cache its mailbox; meeting invitations are attachments, not answered;
the folder listing counts messages with a `STATUS` for each folder.

The configuration (`imaps://` URLs, `imap_user`, `imap_pass`, `smtp_url`,
`ssl_*`) is described in [docs/MUTTRC.md](../../docs/MUTTRC.md) §4.13.

## Automated tests

```sh
cargo test -p mett-imap
```

The tests run every operation against an IMAP server in the test process
(in-memory mailboxes; plain, STARTTLS and TLS with a generated certificate) and
an SMTP one. They need no network access.
