# mett-core

The centre of mett's hexagonal architecture (`docs/DESIGN.md` §5.1). It
defines the domain types (folders, messages, flags, typed IDs and errors) and
the ports that adapters implement:

- **`MailStore`:** list folders and messages, fetch, flag, move and delete,
  and save drafts. `mett-msgraph`'s `GraphMailStore` implements it for
  Microsoft Graph.
- **`MailSender`:** sends a `Draft`: new mail, or a reply or forward of
  another message (`DraftOrigin`). `GraphMailStore` implements it too.
- **`Authenticator`:** provides access tokens. Implemented by
  `mett-msgraph`'s `GraphAuthenticator`.

It performs no I/O and depends on no adapter.

## The `mock` feature

`mock` enables `mett_core::mock::MemoryStore`, an in-memory `MailStore` and
`MailSender` (it records what it sends), plus
helpers to build test folders and messages. Other crates use it in their
tests, and `mett-tui` uses it for `mett --demo`.

```toml
[dev-dependencies]
mett-core = { workspace = true, features = ["mock"] }
```

## Tests

```sh
cargo test -p mett-core --all-features
```

These are unit tests for the domain types, plus tests showing that
`MemoryStore` behaves like a real mailbox: pages are newest first and stay
stable while messages are moved or deleted, folder counts follow the
messages, and moved messages get new IDs. This crate has nothing to test by
hand.
