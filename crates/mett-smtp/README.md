# mett-smtp

The SMTP adapter for mett. It wraps the [`lettre`](https://crates.io/crates/lettre)
crate (with rustls):

- **`SmtpSender`** connects to an SMTP server and sends a `Draft`. It
  returns the copy to keep and leaves keeping it (and so `mett-core`'s
  `MailSender` port, which promises a copy in Sent) to the mailbox, IMAP. The
  connection is `Security::None` (plain text), `Security::StartTlsIfOffered`
  (upgraded if the server offers it), `Security::StartTls` (upgraded, and
  refused if the server won't) or `Security::Tls` (TLS from the first
  byte, port 465). The login is a password (`Login::Password`: PLAIN or
  LOGIN) or an OAuth2 token from any `Authenticator` (`Login::OAuth`: SASL
  XOAUTH2; a token the server turns down is replaced once; `None` logs in
  to nothing). `Login`, `Security` and `NoTokens` live in `mett-core`, as
  does `ServerSettings` (host, port, security, `verify_host`,
  `ca_certificates`, timeout), which `SmtpSettings` and IMAP's
  `ImapSettings` both embed as `server`. The server's
  certificate is checked against the system's roots and any `ca_certificates`
  given, and for the name it is made to unless `verify_host` is off. With
  `verify_host` off lettre can't use the platform verifier, so the chain is
  checked against the system's roots (read once per process) and the extra
  ones, without the platform's own policy (revocation and the like); IMAP,
  which uses the platform verifier throughout, does apply it and ignores
  only a name mismatch.
- **`compose`** writes a `Draft` as MIME: plain text UTF-8 and, with
  attachments, `multipart/mixed`. It returns the message as it goes out
  (without `Bcc:`) and as the sender keeps it (with), sharing a `Message-ID:`.
  A mailbox that keeps its own Sent and Drafts (IMAP) stores these.
- **`Threading`** is what threads a reply: the original's `Message-ID:` and
  `References:`, which the caller looks up.

Errors map onto `mett-core`'s: a refused login is `Error::Auth`, an answer
from the server (the message wasn't taken) is `Error::Backend`, a failure to
set up TLS or to write the message is `Error::NotSent`, and a lost connection
or timeout, which may have come after the server took the message, is
`Error::Network`.

Passwords and the mettrc variables that give them (`smtp_url`, `smtp_pass`,
…) are the caller's: this crate takes settings, not configuration.

## Automated tests

```sh
cargo test -p mett-smtp
```

The tests run against an SMTP server in the test process (plain, STARTTLS and
TLS with a generated certificate). They need no network access.
