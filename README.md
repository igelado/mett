# mett

A clone of the [MUTT](http://www.mutt.org/) e-mail client for accessing
Office365 services.

## Goal

MUTT is a small, fast, text-based e-mail client for Unix-like systems.
`mett` aims to bring the same keyboard-driven, terminal-first experience to
Microsoft Office365, so that mail (and related services) hosted on
Office365 can be read, searched, composed and managed from the terminal.

## Status

Early stage. mett can sign in to an Office365 account and browse a folder,
mutt-style: a message index and a pager. Composing, replying and folder
management are still to come.

## Quick start

1. Register an app for mett in Microsoft Entra ID; see
   [crates/mett-msgraph/README.md](crates/mett-msgraph/README.md#1-register-an-app).
2. Create `~/.config/mett/mettrc`:

   ```muttrc
   set my_me = you@example.com
   set from = $my_me realname = "Your Name"
   set folder = msgraph://$my_me/
   set spoolfile = =@inbox
   set msgraph_client_id = <your app's client ID>
   ```

3. Run mett. The first time, it opens your browser to sign in (use
   `set msgraph_auth_flow = device-code` over SSH):

   ```sh
   cargo run -p mett-tui
   ```

`cargo run -p mett-tui -- --demo` shows the interface on a sample mailbox
without an account.

## Documentation

- [docs/DESIGN.md](docs/DESIGN.md): architecture and design patterns.
- [docs/CONFIG-OFFICE365.md](docs/CONFIG-OFFICE365.md): configuring mett.
- [docs/MUTTRC.md](docs/MUTTRC.md): the mutt configuration format mett
  builds on.
- Each crate's `README.md` explains what it does and how to test it.
