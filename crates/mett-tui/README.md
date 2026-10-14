# mett-tui

The `mett` binary: the mutt-like terminal UI. It follows the Elm
Architecture (`docs/DESIGN.md` §5.2):

- **`app.rs`:** the model and `update`. All UI logic lives here.
- **`view.rs`:** draws the model with [ratatui](https://ratatui.rs).
- **`action.rs` and `keymap.rs`:** mutt-style named actions, and key bindings
  for each menu (the Command pattern, §5.3).
- **`worker.rs`:** the background actor that owns all mailbox I/O (§5.4).

So far it can browse a mailbox: the message index and the pager. It isn't
connected to Office365 yet; `--demo` opens a built-in sample mailbox.

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

Check that:

- Opening an unread message (`N`) marks it read in the index and in the
  status bar's `New:` count.
- The pager's status bar shows `(all)`, a percentage, or `(end)` as you
  scroll.
- Resizing the terminal keeps the layout intact.
