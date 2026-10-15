# mett-tui

The `mett` binary: the mutt-like terminal UI. It follows the Elm
Architecture (`docs/DESIGN.md` §5.2):

- **`app.rs`:** the model and `update`. All UI logic lives here.
- **`view.rs`:** draws the model with [ratatui](https://ratatui.rs).
- **`action.rs` and `keymap.rs`:** mutt-style named actions, and key bindings
  for each menu (the Command pattern, §5.3).
- **`worker.rs`:** the background actor that owns all mailbox I/O (§5.4).
- **`config/`:** reads `mettrc`, mutt's configuration format with mett's
  extensions ([docs/CONFIG-OFFICE365.md](../../docs/CONFIG-OFFICE365.md)).

So far it can browse a mailbox: the message index and the pager. It opens
the Office365 folder set in `spoolfile`, signing in first if needed
(`account.rs`); `--demo` opens a built-in sample mailbox instead.

## Automated tests

```sh
cargo test -p mett-tui
```

- **`app` tests:** drive `App::update` with actions and worker responses,
  without a terminal. They cover navigation, paging, opening messages and
  ignoring stale responses.
- **`view` tests:** render into ratatui's `TestBackend` and compare the
  screen line by line, so a layout change shows up as a failing test.
- **`worker` test:** runs the worker against `mett-core`'s in-memory store.
- **`config` tests:** the lexer (quoting, escapes, expansion, backticks,
  line continuation), every form of `set`, `source` (including loops),
  file lookup, and `msgraph://` URLs with mutt's `=` shortcut.

## Configuration

mett reads `/etc/mettrc` (unless `-n`), then the first of `~/.mettrc`,
`~/.mett/mettrc` and `$XDG_CONFIG_HOME/mett/mettrc`, or the file given with
`-F`. A minimal configuration for an Office365 account:

```muttrc
set my_me = you@example.com
set from = $my_me realname = "Your Name"
set folder = msgraph://$my_me/
set spoolfile = =@inbox
set msgraph_client_id = <your app's client ID>
```

So far `set`, `unset`, `toggle`, `reset` and `source` are supported, with
mutt's quoting, `$` expansion, backticks and `my_` variables. Other mutt
commands and unknown variables are ignored with a warning, so a mettrc can
`source` an existing muttrc.

`mett -D` prints every setting after reading the configuration, which is a
quick way to check it:

```sh
cargo run -p mett-tui -- -D
```

## Trying it with an Office365 account

With an app registered (see `crates/mett-msgraph/README.md`) and a mettrc as
above:

```sh
cargo run -p mett-tui            # or: -- -n -F path/to/mettrc
```

The first time, mett opens a sign-in window over the index: it opens your
browser on the sign-in page and shows the link, or, with
`msgraph_auth_flow=device-code`, the code to enter. The window closes once
you have signed in, and later runs reuse the stored refresh token. Check
that the index lists the folder's messages, newest first, and that opening
an unread one marks it read in Outlook too.

The same window appears whenever mett needs a new interactive sign-in while
it runs, for example after the refresh token was revoked. To try that,
revoke your sessions (in the Microsoft 365 account settings, *Sign out
everywhere*) while mett is running, wait for the access token to expire
(about an hour) and open a message.

In the sign-in window, `o` opens the sign-in page in the browser (again),
and `q` or `Esc` cancels the sign-in. If signing in fails, the window shows
why: `Enter` tries again and `q` closes it. `Enter` in an empty index also
retries opening the folder.

## Trying it by hand

```sh
cargo run -p mett-tui -- --demo
```

The demo mailbox has 40 messages. The first four are unread, some are
flagged (`!`) or have attachments (`+`), and the first is HTML.

| Menu | Keys |
|------|------|
| Index | `j`/`k` or arrows: next or previous message. `z`/`Z`, PageDown/PageUp: next or previous page. `=`/`*`, Home/End: first or last message. `Enter`/`Space`: read the message. `q`: quit. |
| Pager | `Space`/`-`: next or previous page. `Enter`/`Backspace`: scroll one line. `j`/`k`: next or previous message. `i`/`q`: back to the index. |
| Sign-in window | `o`: open the sign-in page in a browser. `q`/`Esc`: cancel, or close after a failure. `Enter`: try again after a failure. `j`/`k` or arrows: scroll, when the terminal is too small to show it all. |

Check that:

- Opening an unread message (`N`) marks it read in the index and in the
  status bar's `New:` count.
- The pager's status bar shows `(all)`, a percentage, or `(end)` as you
  scroll.
- Resizing the terminal keeps the layout intact.
