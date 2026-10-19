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

## Install

Each [release](https://git.gelado.org/igelado/mett/releases) has packages
for x86_64 and aarch64:

- **Debian, Ubuntu:** `sudo apt install ./mett_<version>-1_<amd64|arm64>.deb`
- **Arch Linux:** `sudo pacman -U mett-<version>-1-<x86_64|aarch64>.pkg.tar.zst`
- **Other Linux:** the `.tar.gz` holds the `mett` binary, which needs
  nothing else.

To build from source, use `cargo install --path crates/mett-tui`.
[docs/RELEASING.md](docs/RELEASING.md) explains how releases are made.

## Quick start

1. Register an app for mett in Microsoft Entra ID; see
   [crates/mett-msgraph/README.md](crates/mett-msgraph/README.md#1-register-an-app).
2. Create `~/.config/mett/mettrc`:

   ```muttrc
   set my_me = you@example.com
   set from = $my_me realname = "Your Name"
   set folder = msgraph://$my_me/
   set spoolfile = =@inbox
   set office365_client_id = <your app's client ID>
   ```

3. Run mett. The first time, it opens your browser to sign in and shows the
   sign-in link in a window, in case the browser doesn't open (use
   `set office365_auth_flow = device-code` over SSH, and mett shows a code to
   enter instead):

   ```sh
   cargo run -p mett-tui
   ```

`cargo run -p mett-tui -- --demo` shows the interface on a sample mailbox
without an account.

To reach the account through Exchange Web Services instead of Microsoft
Graph, `set use_ews`; see
[crates/mett-ews/README.md](crates/mett-ews/README.md).

## Documentation

- [docs/DESIGN.md](docs/DESIGN.md): architecture and design patterns.
- [docs/CONFIG-OFFICE365.md](docs/CONFIG-OFFICE365.md): configuring mett.
- [docs/MUTTRC.md](docs/MUTTRC.md): the mutt configuration format mett
  builds on.
- [docs/RELEASING.md](docs/RELEASING.md): how releases are made.
- Each crate's `README.md` explains what it does and how to test it.

## License

mett is licensed under either of [Apache License, Version 2.0](LICENSE-APACHE)
or [MIT license](LICENSE-MIT), at your option.
