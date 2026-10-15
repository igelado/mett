# mett-tui

The `mett` binary: the mutt-like terminal UI. It follows the Elm
Architecture (`docs/DESIGN.md` §5.2):

- **`app/`:** the model and `update`. All UI logic lives here;
  `app/composing.rs` has the steps of writing a message.
- **`compose.rs`:** how replies and forwards are built (quoting,
  attribution, recipients, signature); **`lineedit.rs`:** the line editor
  for questions such as `To:`; **`editor.rs`:** runs the external editor;
  **`pattern.rs`:** mutt's patterns, for searching and limiting.
- **`view.rs`:** draws the model with [ratatui](https://ratatui.rs).
- **`action.rs` and `keymap.rs`:** mutt-style named actions, and key bindings
  for each menu (the Command pattern, §5.3).
- **`worker.rs`:** the background actor that owns all mailbox I/O (§5.4).
- **`config/`:** reads `mettrc`, mutt's configuration format with mett's
  extensions ([docs/CONFIG-OFFICE365.md](../../docs/CONFIG-OFFICE365.md)).

So far it can browse a mailbox (the message index, the pager, and the
folder browser), mark, flag, purge, move and copy messages, and write
mail: new messages, replies, group replies and forwards, which are sent or
postponed from mutt's compose menu. It checks for new mail every
`msgraph_sync_interval` seconds, in the open folder and in the folders
named by `mailboxes`. It opens the Office365 folder set in
`spoolfile`, signing in when needed (`account.rs`); `--demo` opens a
built-in sample mailbox instead.

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

So far `set`, `unset`, `toggle`, `reset`, `source`, `alternates` (the
user's other addresses, for replies), `mailboxes` and `unmailboxes` (with
`-label`, `-notify`/`-nonotify` and `-poll`/`-nopoll`), `color`, `mono`,
`uncolor` and `unmono`, `bind`, `macro`, `push` and `exec` are supported, with mutt's quoting, `$` expansion, backticks and `my_`
variables. `bind` and `macro` take mutt's menus (`generic`, `index`,
`pager`, `compose`, `browser`) plus mett's `signin` and `help`, and mutt's
key notation (`\Cx`, `^x`, `\e`, `<PageDown>`, `<F1>`; `^M`, `^I` and
`^[` are Enter, Tab and Esc; `^J` and `\n` are Control-J, which does what
Enter does unless bound); a macro's
`<function>` runs that function. Binding a sequence of several keys is not
supported yet. Other mutt
commands and unknown variables are ignored with a warning, so a mettrc can
`source` an existing muttrc.

### Display

The index is ordered by `sort` (mett's default is `reverse-date`, newest
first, where mutt's is `date`), with `sort_aux` breaking ties: `date`
(also `date-sent`), `date-received` (when it reached the mailbox), `from`, `subject`, `to`,
`unsorted` (as listed) or, for `sort`, `threads`, each with `reverse-`;
with threads, `sort_aux` orders each thread's messages, and with `last-`,
orders the threads by their latest message. Threads are drawn with line
characters, or with `ascii_chars`, with `|`, `` ` ``, `-` and `>`.

`index_format`, `pager_format` and `status_format` take mutt's format
strings: `%-15.15L` widths, `%?X?then&else?` conditionals, `%>X`, `%*X` and
`%|X` fills, and dates as `%{fmt}`, `%[fmt]` or `%(fmt)` (all in local
time; `date_format` is used by `%d` and `%D`). The expandos mett knows:

| Format | Expandos |
|--------|----------|
| `index_format`, `pager_format` | `%M` messages folded into a collapsed thread, `%a` sender's address, `%C` number, `%d`/`%D` date, `%F`/`%L` sender (or `To` recipient, for the user's own mail), `%f` sender with address, `%m` messages in the folder, `%n` sender's name, `%s` subject (after the thread's lines, and left out when it is its parent's), `%S` status (`D`, `!`, `N`), `%t` first recipient, `%u` sender's user name, `%v` sender's first name, `%X` attachments (1 or 0), `%Z` flags (`D`/`N`, `!`, `+` for attachments); in `pager_format`, `%P` how far down. |
| `status_format` | `%b` mailboxes with new mail, `%d` marked for deletion, `%F` flagged, `%f` folder, `%h` host name, `%M` shown by the limit, `%m` messages, `%n`/`%u` unread, `%P` how far down, `%r` `*` with changes waiting (`-` otherwise), `%s`/`%S` `sort` and `sort_aux`, `%V` the limit, `%v` mett's version, and mett's `%I`, set when part of the folder couldn't be listed. |

Expandos mett can't fill yet (such as sizes and line counts) are empty, so
`%?l?...?` conditionals take their `else` part.

`color OBJECT [ATTRIBUTE ...] FOREGROUND BACKGROUND [EXPRESSION]` (an
expression with spaces quoted, as in mutt) sets the
colors of `normal`, `indicator`, `status` (and the help line), `error`,
`message`, `prompt`, `search`, `hdrdefault`, `quoted` to `quoted9` (by
`quote_regex`), `signature` and `attachment`; `index` takes a pattern (the
last that matches counts), and `header` and `body` a regular expression.
Colors are `default`, the eight basic colors (with `bright` for the light
ones), `colorN` and `"#rrggbb"` (quoted, as `#` otherwise starts a comment); attributes are `bold`, `underline`,
`reverse`, `standout`, `blink`, `italic` and `none`. `uncolor` removes
`index`, `header` and `body` rules (`*` for all) or sets an object back to
its default. As in mutt, `mono` and `unmono` set the attributes used
instead on a terminal without colors (`TERM` such as `vt100` or `dumb`,
or `NO_COLOR` set). Objects and patterns mett doesn't do yet (such as
`color compose ...` or `color index ... ~T`) are ignored with a warning.
`tree`, `tilde` and `markers` are accepted but not drawn yet.

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

To test the mailbox operations, use a test folder: `s` a message to it
(`Tab` lists the folders), check in Outlook that it moved, `C` it back,
and `d` then `$` it. With `set trash = =@deleteditems` it lands in
Deleted Items; without `trash` it is deleted for good. `F` and `N` should
show up in Outlook as the flag and the unread state.

To test new mail, add `mailboxes =@inbox =Work` (any other folder) and
start mett with `set msgraph_sync_interval=15`. Send yourself a message:
within a check or two it joins the index with `New mail in this
mailbox.`. Move a message into `Work` from Outlook: the status line says
`New mail in =Work.`, the status bar counts it in `Inc:`, and `c` offers
`=Work`. Reading or deleting a recent message in Outlook shows up in mett
at the next check too.

To test sending, send yourself a message (`m`, your own address), then
reply to it (`r`) and forward it (`f`) to yourself. In Outlook, check that
the reply is threaded under the original, the forward keeps any
attachments, the quoted text and signature look right, and each message is
in Sent Items. `P` in the compose menu saves the message in Drafts instead.

The same window appears whenever mett needs a new interactive sign-in while
it runs, for example after the refresh token was revoked. To try that,
revoke your sessions (in the Microsoft 365 account settings, *Sign out
everywhere*) while mett is running, wait for the access token to expire
(about an hour) and open a message.

The link is printed so that the terminal wraps it, so selecting it copies
it in one piece. In the sign-in window, `c` copies it to the clipboard, and
`o` opens the sign-in page in the browser (again),
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
| Index | `Esc v`/`Esc V`: collapse or open the thread, or all of them (with `sort=threads`). `j`/`k` or arrows: next or previous message. `z`/`Z`, PageDown/PageUp: next or previous page. `=`/`*`, Home/End: first or last message. `Enter`/`Space`: read the message. `q`: quit. |
| Pager | `Space`/`-`: next or previous page. `Enter`/`Backspace`: scroll one line. `j`/`k`: next or previous message. `i`/`q`: back to the index. |
| Everywhere but the sign-in window | `?`: the keys of the current menu. `:`: run a mettrc command, such as `set sig_dashes=no` or `bind index J last-entry` (`msgraph_` settings other than `msgraph_page_size` and `msgraph_sync_interval` take effect when mett starts again). |
| Index and pager | `m`: new message. `r`: reply. `g`: reply to all. `f`: forward. `/`: search with a pattern, such as `~f alice ~N` (in the pager, the text). `n`: the next match. `l` (index): show only the messages matching a pattern; `all` shows them all again. `d`/`u`: mark or unmark for deletion. `$`: purge the marked messages (to `trash`, or for good if it is unset). `F`: flag. `N`: toggle new. `^N`/`^P`: next or previous thread. `s`/`C`: move or copy to another folder. `c`: open another folder (offers a mailbox with new mail; `Space` offers the next). `y`: the folder browser. |
| Folder browser | `j`/`k`, arrows, `z`/`Z`: move. `Enter`: choose. `q`: back. `Tab`: switch between the `mailboxes` (listed first, if there are any) and every folder. In a folder question (`s`, `C`, `c`), `Tab` or `?` opens the browser, and mutt's shortcuts work: `=`/`+` (under `folder`), `!` (`spoolfile`), `>` (`mbox`), `<` (`record`), `^` (this folder), `-` or `!!` (the one before). |
| Compose menu | `y`: send. `q`: leave (asks whether to postpone). `P`: postpone to Drafts. `e`: edit the text again. `t`/`c`/`b`/`s`: edit To, Cc, Bcc, Subject. |
| Questions | Type the answer and press `Enter`; `Esc` or `^G` gives up. The usual line-editing keys work (`^A`, `^E`, `^U`, `^K`, `^W`, arrows). Yes/no questions take `y`, `n`, or `Enter` for the suggested answer. |
| Sign-in window | `o`: open the sign-in page in a browser. `c`: copy the link (or the device code) to the clipboard, in terminals that support OSC 52 (in tmux, `set -g set-clipboard on`). `q`/`Esc`: cancel, or close after a failure. `Enter`: try again after a failure. `j`/`k` or arrows: scroll, when the terminal is too small to show it all. |

Check that:

- Opening an unread message (`N`) marks it read in the index and in the
  status bar's `New:` count.
- `d` marks a message `D` and counts it in `Del:`; `$` asks to purge it and
  takes it out of the index. `s` with `=Archive` moves a message there,
  `y` lists the folders (Archive now has it), and `c` opens another one.
- Changes to the mailbox (flags, moves, copies, purges) are made one at a
  time, in the order given: `F` then `s` saves the flag before the move,
  and `C` then `$` copies a marked message before purging it.
- `m` asks for the recipients and subject, starts your editor (`editor`,
  `$VISUAL` or `$EDITOR`), then shows the compose menu; `y` reports
  `Mail sent.`. `r` on a message asks whether to include it, and quotes it
  in the editor. Leaving the editor without changes aborts the message, as
  in mutt.
- `:set sort=threads` groups the replies under the messages they answer,
  `Esc v` folds a thread into its first message and `^N` moves to the next
  thread; `:set sort=reverse-threads sort_aux=last-date` puts the thread
  with the latest message first.
- The pager's status bar shows `(all)`, a percentage, or `(end)` as you
  scroll.
- Resizing the terminal keeps the layout intact.
