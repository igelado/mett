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

[docs/RELEASING.md](https://git.gelado.org/igelado/mett/src/branch/main/docs/RELEASING.md) explains how releases are made.

### From source

mett needs Rust 1.89 or later, a C compiler (for the SQLite it builds
in) and Git; nothing else.

1. Install Rust with [rustup](https://rustup.rs/) (or `rustup update` an
   older one), then a C compiler and Git: `sudo apt install build-essential
   git` on Debian and Ubuntu, `sudo pacman -S base-devel git` on Arch.
2. Get the source and install the `mett` binary:

   ```sh
   git clone https://git.gelado.org/igelado/mett.git
   cd mett
   cargo install --locked --path crates/mett-tui
   ```

   It goes to `~/.cargo/bin/mett`; rustup puts that directory on your
   `PATH`. For another place, add `--root <dir>` (say `--root ~/.local`
   for `~/.local/bin/mett`), and make sure `<dir>/bin` is on your `PATH`
   too.
3. Check it: `mett -v` prints the version, and `mett --demo` opens a
   sample mailbox.

To update, `git pull` in the checkout and run the same `cargo install`
again. `cargo uninstall mett-tui` removes it (with the same `--root`, if
you gave one). The documentation isn't
installed this way: it is in the checkout's `docs/`.

## Quick start

1. Register an app for mett in Microsoft Entra ID; see
   [mett-msgraph's README](https://git.gelado.org/igelado/mett/src/branch/main/crates/mett-msgraph/README.md#1-register-an-app).
2. Create `~/.config/mett/mettrc`:

   ```muttrc
   set my_me = you@example.com
   set from = $my_me realname = "Your Name"
   set folder = msgraph://$my_me/
   set spoolfile = =@inbox
   set office365_client_id = <your app's client ID>
   ```

3. Run `mett` (installed from a package), or `cargo run -p mett-tui` (in a
   checkout of the source). The first time, it opens your browser to sign
   in and shows the sign-in link in a window, in case the browser doesn't
   open (use `set office365_auth_flow = device-code` over SSH, and mett shows
   a code to enter instead).

`mett --demo` shows the interface on a sample mailbox without an account.

To reach the account through Exchange Web Services instead of Microsoft
Graph, `set use_ews`; see
[mett-ews's README](https://git.gelado.org/igelado/mett/src/branch/main/crates/mett-ews/README.md).

## Documentation

The packages install `CONFIG-OFFICE365.md` and `MUTTRC.md` in
`/usr/share/doc/mett/`. In the source:

- [docs/DESIGN.md](https://git.gelado.org/igelado/mett/src/branch/main/docs/DESIGN.md): architecture and design patterns.
- [docs/CONFIG-OFFICE365.md](https://git.gelado.org/igelado/mett/src/branch/main/docs/CONFIG-OFFICE365.md): configuring
  mett.
- [docs/MUTTRC.md](https://git.gelado.org/igelado/mett/src/branch/main/docs/MUTTRC.md): the mutt configuration format
  mett builds on.
- [docs/RELEASING.md](https://git.gelado.org/igelado/mett/src/branch/main/docs/RELEASING.md): how releases are made.
- Each crate's `README.md` explains what it does and how to test it.

## License

mett is licensed under either of
[Apache License, Version 2.0](https://git.gelado.org/igelado/mett/src/branch/main/LICENSE-APACHE) or
[MIT license](https://git.gelado.org/igelado/mett/src/branch/main/LICENSE-MIT), at your option.
