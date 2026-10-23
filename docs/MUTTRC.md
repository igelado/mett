# The mutt configuration file format (muttrc)

This document summarises the format of mutt's configuration file, `muttrc`,
as the basis for mett's own configuration (see
[CONFIG-OFFICE365.md](CONFIG-OFFICE365.md)). It follows mutt 2.x. The
authoritative reference is mutt's manual and `muttrc(5)`.

## 1. File locations

At startup, mutt reads a system-wide file and then the user's own file.

| Kind | Locations, in search order |
|------|---------------------------|
| System | `$sysconfdir/Muttrc` (usually `/etc/Muttrc`) |
| User | `~/.muttrc`, then `~/.mutt/muttrc`, then `$XDG_CONFIG_HOME/mutt/muttrc` |

Only the first user file found is read. `mutt -F <file>` replaces the user
file, and `mutt -n` skips the system file. A configuration may include other
files with `source`.

## 2. Lexical rules

A muttrc is a sequence of commands, normally one per line.

- **Comments:** `#` starts a comment that runs to the end of the line.
- **Separators:** several commands may share a line, separated by `;`.
- **Line continuation:** a trailing `\` joins the next line, as long as the
  split doesn't fall inside a command name.
- **Single quotes** (`'…'`): a literal string. No expansion and no backslash
  escapes.
- **Double quotes** (`"…"`): the string is evaluated. Backslash escapes,
  `$` variable expansion and backtick substitution all apply.
- **Backslash:** quotes the next character. `\\` is a literal backslash, and
  `\n` and `\r` are line feed and carriage return.
- **Backticks** (`` `cmd` ``): replaced by the output of the shell command.
  Common for secrets, for example ``set imap_pass = "`pass show work`"``.
- **Variable expansion:** `$NAME` expands an environment variable. A
  configuration variable, including a user-defined `my_*` variable, can also
  be expanded, for example `set record = $my_sent`.

## 3. Variables

### 3.1 Setting variables

```
set     [no|inv|&|?]variable[=value] [ ... ]
unset   variable [ ... ]
toggle  variable [ ... ]
reset   variable [ ... ]
```

| Form | Meaning |
|------|---------|
| `set var` | Set a boolean to *yes*. |
| `set novar`, `unset var` | Set a boolean to *no*. |
| `set invvar`, `toggle var` | Invert a boolean. |
| `set var=value` | Assign a value (spaces around `=` are allowed). |
| `set ?var` | Show the current value. |
| `set &var`, `reset var` | Restore the default value. `reset all` restores every variable. |

User-defined variables must start with `my_`, for example
`set my_work = "imaps://…"`. mutt ignores them except when they are
expanded.

### 3.2 Variable types

| Type | Values |
|------|--------|
| boolean | `yes` / `no` |
| quadoption | `yes`, `no`, `ask-yes`, `ask-no`. The `ask-*` values prompt with a default answer. |
| number | Integer. |
| string | Free text. Many strings are printf-like formats with `%` expandos, such as `index_format`. |
| path | A file or mailbox path, with `~` and mailbox shortcuts expanded (§5). |
| regular expression | POSIX extended regex. |
| sort order | `date`, `from`, `subject`, `threads`, `size`, `score` and so on, optionally prefixed with `reverse-` or `last-`. |
| e-mail address | An RFC 5322 address. |

### 3.3 Variables relevant to mett

These are the variables most relevant to mett. mutt has several hundred more.

| Area | Variable | Type | Purpose |
|------|----------|------|---------|
| Identity | `from` | address | Default `From:` address. |
| | `realname` | string | Display name for outgoing mail. |
| | `use_from` | boolean | Generate the `From:` header. |
| | `hostname` | string | Fully qualified host name, used for Message-IDs. |
| | `signature` | path | Signature file. Left unset, the default `~/.signature` may be missing; a file set in the mettrc (even that one) must exist. |
| Mailboxes | `folder` | path | Base of the mailbox tree, often an `imaps://` URL. Default `~/Mail`. |
| | `spoolfile` | path | Incoming mailbox, opened at startup. |
| | `record` | path | Where copies of sent mail are saved. |
| | `postponed` | path | Where drafts are saved. |
| | `mbox` | path | Where read mail is moved when `move` is set. |
| | `trash` | path | Deleted mail is moved here instead of being purged. |
| | `delete` | quadoption | Whether to purge messages marked for deletion (on `$`, or when leaving the folder). |
| | `resolve` | boolean | Move to the next message after marking one (`d`, `u`). |
| | `copy` | quadoption | Whether to save a copy of outgoing mail in `record`. |
| IMAP | `imap_user`, `imap_pass` | string | Credentials (§4.13). mett doesn't know `imap_login`. |
| | `imap_authenticators` | string | Colon-separated methods, for example `oauthbearer:xoauth2`. (Not supported yet: mett logs in with `LOGIN`.) |
| | `imap_oauth_refresh_command` | string | Command that prints a fresh OAuth2 access token. (Not supported yet.) |
| | `imap_check_subscribed` | boolean | Poll subscribed folders. |
| SMTP | `smtp_url` | string | `smtp[s]://[user[:pass]@]host[:port]/` (§4.13). |
| | `smtp_pass` | string | The password (§4.13). |
| | `smtp_authenticators` | string | Methods. (Not supported yet: mett logs in with PLAIN or LOGIN.) |
| | `smtp_oauth_refresh_command` | string | As for IMAP. (Not supported yet.) |
| | `sendmail` | path | Local MTA, used when `smtp_url` is unset. (Not supported: with no `smtp_url`, mail can't be sent.) |
| | `ssl_starttls`, `ssl_force_tls` | boolean | Upgrade a plain connection with STARTTLS if the server offers it (`yes`); fail if it doesn't (`ssl_force_tls`, `no`). |
| | `ssl_verify_host` | boolean | Check that the server's certificate is for the server's name (`yes`). The certificate itself is always checked: for a self-signed one, give it in `ssl_ca_certificate_file`. |
| | `ssl_ca_certificate_file` | path | More certificate authorities to trust, as PEM, besides the system's. |
| Caching | `header_cache` | path | Header cache database. |
| | `message_cachedir` | path | Body cache for IMAP and POP. |
| Composing | `editor` | path | Compose editor. Falls back to `$VISUAL`, `$EDITOR`, then mett's built-in editor (§4.11), which `builtin` asks for. |
| | `editor_keys` | string | mett's own: the built-in editor's keys, `vim`, `emacs` or `notepad` (§4.11). |
| | `spell_check`, `spell_lang` | boolean, string | mett's own: whether the built-in editor underlines misspelled words (`yes`), and with which dictionaries, comma-separated (`en_US`) (§4.11). |
| | `spell_dict_path`, `spell_words` | string, path | mett's own: more directories to find dictionaries in (separated as in `PATH`), and the file of the words you added (`~/.config/mett/words`) (§4.11). |
| | `gui_font`, `gui_font_size` | path, number | mett's own, for `mett-gui`: the font its window shows text in (a monospaced TrueType or OpenType file; by default, the Cascadia Mono it comes with), and its size in points (`14`). Read when it starts. |
| | `include` | quadoption | Quote the message in replies. |
| | `attribution`, `indent_string` | string | The line before quoted text (`On %d, %n wrote:`), and the prefix of quoted lines (`> `). |
| | `forward_format` | string | Subject of forwarded messages (`[%a: %s]`). |
| | `reply_to` | quadoption | Reply to the `Reply-To:` address rather than the sender. (Not asked for one's own messages, whose replies go to their recipients.) |
| | `fast_reply`, `askcc`, `askbcc` | boolean | Skip the recipient and subject questions when replying; ask for `Cc:` and `Bcc:`. |
| | `metoo` | boolean | Keep one's own address in group replies. (A reply to one's own message goes to its other recipients, or to its one `Bcc:` recipient; several `Bcc:` recipients are never shown to each other.) |
| | `sig_dashes` | boolean | Put `-- ` before the signature. |
| | `abort_unmodified`, `abort_nosubject`, `postpone` | quadoption | Abort an unedited message or one without subject (asked again when sending without one, as in mutt); postpone when leaving the compose menu. |
| | `tmpdir` | path | Where the editor's temporary files go. |
| Behaviour | `mail_check` | number | Seconds between new-mail checks of an IMAP mailbox (`0` turns them off; Office365 has `office365_sync_interval`). |
| | `timeout` | number | Seconds to wait for input before checking mail. |
| | `sort`, `sort_aux` | sort order | Index sorting. |
| | `index_format`, `pager_format`, `status_format` | string | Display formats. |
| | `pager_index_lines` | number | Rows of a mini-index shown above the pager, one of them the index's status bar (default `0`, none). As in mutt, the open message is about a third of the way down it, and it takes only the rows the index needs. On a terminal too small for it and a few rows of the message, it shrinks, or is left out. |
| | `wrap` | number | Where the pager wraps text (also text attachments viewed in it, but not the help screen, as in mutt). `0` (the default) wraps at the pager's width (the terminal's, less the sidebar's, if shown); a positive number wraps at that many columns, or at the terminal's width if it is narrower; a negative number leaves that many columns empty on the right, or wraps at the terminal's width if that leaves no room. Changing it with `:set` wraps an open message again. |
| | `collapse_links` | boolean | mett's own: the pager shows a link written as its text followed by its URL in angle brackets (`docs<https://...>`, as Exchange writes links in text bodies, and as mett shows `<a href>` in HTML ones) as only its text, underlined (default `yes`). `<toggle-links>` (`L` in the pager) shows the open message's URLs, underlined, or hides them again. URLs on their own (with no text right before them) are always shown, underlined. Text bodies don't say where a link's text starts, so only the word right before the URL is underlined; HTML links are underlined whole. |
| | `sidebar_visible`, `sidebar_width`, `sidebar_format` | boolean, number, string | The sidebar (§4.10). |
| | `mailcap_path` | string | mailcap files used to view MIME parts. |
| | `query_command` | path | External address-book query. |
| | `alias_file` | path | Where aliases created with `a` (`<create-alias>`) are saved. Defaults to the user's own mettrc (`~/.config/mett/mettrc` for a user who has none yet). |

## 4. Commands

The synopses below follow `muttrc(5)`, grouped by purpose. `[!]` negates a
regular expression or pattern.

### 4.1 Configuration management

```
source        filename
set / unset / toggle / reset      (see §3.1)
setenv        [?]variable [ value ]
unsetenv      variable
cd            directory
echo          message
run           MuttLisp
```

### 4.2 Mailboxes and lists

```
mailboxes     [[-notify|-nonotify] [-poll|-nopoll] [[-label label]|-nolabel] mailbox] [ ... ]
unmailboxes   [ * | mailbox ... ]
subscribe     [-group name] regexp [ ... ]
unsubscribe   regexp [ ... ]
lists         [-group name] regexp [ ... ]
unlists       regexp [ ... ]
sidebar_whitelist   mailbox [ ... ]
unsidebar_whitelist [ * | mailbox ... ]
```

`sidebar_whitelist` names mailboxes that the sidebar shows even with
`sidebar_new_mail_only` (§4.10); `unsidebar_whitelist` forgets them.

### 4.3 Addresses and identities

```
alias         [-group name [...]] key address [, address ...]
unalias       [ * | key ]
group         [-group name] [-rx EXPR ...] [-addr address ...]
ungroup       [-group name] [ * | [-rx EXPR ...] [-addr address ...] ]
alternates    [-group name] regexp [ ... ]
unalternates  [ * | regexp ... ]
```

Besides `from` and `alternates`, mett counts as the user's own the
addresses the mailbox reports (with Office365: the account's mail address,
sign-in name and aliases).

### 4.4 Key bindings and macros

```
bind          map1,map2,... key function
macro         map key sequence [ description ]
push          string
exec          function [ ... ]
```

The maps are the menus: `generic`, `index`, `pager`, `compose`, `attach`,
`browser`, `alias`, `query`, `editor`, `postpone`, `pgp` and `smime`, and
mett's `sidebar` (§4.10). Keys use
names such as `<Enter>`, `<Esc>`, `\Cx` (Ctrl+x) and `<F1>`. Functions are
written as `<function-name>`, for example `<next-entry>` or `<reply>`.

### 4.5 Display

```
color         object [attribute ...] foreground background [ regexp ]
color index   [attribute ...] foreground background [ pattern ]
color compose composeobject [attribute ...] foreground background
uncolor       index pattern [ ... ]
mono          object attribute [ regexp ]
[un]ignore    header-prefix [ ... ]
hdr_order     header1 header2 [ ... ]
subjectrx     pattern replacement
unsubjectrx   [ * | pattern ]
```

### 4.6 Headers

```
my_hdr        "Header: value"
unmy_hdr      field
mailto_allow  header-field [ ... ]
unmailto_allow [ * | header-field ... ]
```

### 4.7 MIME handling

```
auto_view           type[/subtype] [ ... ]
unauto_view         type[/subtype] [ ... ]
alternative_order   type[/subtype] [ ... ]
unalternative_order [ * | type/subtype ... ]
mime_lookup         type[/subtype] [ ... ]
unmime_lookup       type[/subtype] [ ... ]
attachments         [+|-]disposition mime-type
unattachments       [+|-]disposition mime-type
charset-hook        alias charset
iconv-hook          charset local-charset
```

### 4.8 Hooks

Hooks run commands when an event matches a regular expression or a pattern
(§6).

```
account-hook   [!]regexp  command     # before connecting to a remote account (URL match)
folder-hook    [!]regexp  command     # on entering a mailbox
mbox-hook      [!]regexp  mailbox     # where read mail goes for a mailbox
message-hook   [!]pattern command     # before displaying a message
send-hook      [!]pattern command     # when composing (matched against recipients)
send2-hook     [!]pattern command     # whenever the draft changes
reply-hook     [!]pattern command     # when replying (matched against the original)
save-hook      [!]pattern mailbox     # default save location
fcc-hook       [!]pattern mailbox     # default Fcc location
fcc-save-hook  [!]pattern mailbox     # both of the above
crypt-hook     regexp     key-id
index-format-hook name [!]pattern format-string
open-hook / close-hook / append-hook  regexp "command"   # compressed mailboxes
unhook         [ * | hook-type ]
```

### 4.9 Scoring and spam

```
score         pattern value
unscore       [ * | pattern ... ]
spam          pattern format
nospam        pattern
```

### 4.10 The sidebar

mutt's sidebar lists the `mailboxes` in a column beside the menus, with
their counts. mett supports it as below; neomutt's additions are marked,
and so is mett's own. Without `mailboxes`, mett's sidebar lists every
folder instead, as a tree (`Inbox`, `Inbox/Projects`), with their
counts, as last listed: when a folder opens and at each check for new
mail.

| Variable | Default | Meaning |
|----------|---------|---------|
| `sidebar_visible` | `no` | Show the sidebar. |
| `sidebar_width` | `30` | Its width in columns, the divider included. |
| `sidebar_format` | `%B%*  %n` | Each entry: `%B` name, `%N` unread, `%S` messages, `%n` `N` with new mail, `%F` flagged, `%!` `!`, `!!` or `N!` for the flagged, and for the open folder `%d` deleted, `%L` shown by the limit, `%t` tagged; `%?X?…&…?`, `%>X`, `%*X` and `%\|X` as in other formats. |
| `sidebar_short_path` | `no` | Show only the last part of each path (after `sidebar_delim_chars`); for every folder, its name. |
| `sidebar_folder_indent` | `no` | Indent mailboxes below `folder` by their depth, with `sidebar_indent_string` (`"  "`); every folder, by its depth in the tree. |
| `sidebar_delim_chars` | `/.` | What separates the parts of a path. |
| `sidebar_divider_char` | `\|` | The line between the sidebar and the menus. |
| `sidebar_sort_method` | `unsorted` | `unsorted` (`mailbox-order`), `path`, `alpha` (`name`: the label, or the path), `count`, `unread` (`new`) or `flagged`, most first for counts; with `reverse-`. |
| `sidebar_new_mail_only` | `no` | Show only mailboxes with new, unread or flagged mail, the open one and those in `sidebar_whitelist`. |
| `sidebar_next_new_wrap` | `no` | `<sidebar-next-new>` and `<sidebar-prev-new>` go round the list. |
| `sidebar_on_right` | `no` | (neomutt) The sidebar on the right. |
| `sidebar_min_width` | `100` | (mett) On a terminal narrower than this, or one that would leave the menus fewer than 40 columns, the sidebar hides even if visible, and comes back once the terminal widens. `0` turns this off, as in mutt. |

The functions, unbound by default, work in the index and the pager:
`<sidebar-next>`, `<sidebar-prev>`, `<sidebar-first>`, `<sidebar-last>`,
`<sidebar-next-new>`, `<sidebar-prev-new>` (a mailbox with new or unread
mail), `<sidebar-page-down>`, `<sidebar-page-up>` move the sidebar's
highlight; `<sidebar-open>` opens its mailbox; and
`<sidebar-toggle-visible>` toggles `sidebar_visible`. For example:

```muttrc
bind index,pager \CP sidebar-prev
bind index,pager \CN sidebar-next
bind index,pager \CO sidebar-open
```

mett's `<sidebar-focus>`, on Tab in the index and the pager, gives the
sidebar the keys instead, in a menu of its own, `sidebar`: there `j` and
`k` (or the arrows) move its highlight, `=` and `*` (Home and End) go to
the first and the last mailbox, `z` and `Z` (Page Down and Page Up) a
page, and Enter or Space opens the mailbox. Opening one, Tab, `q` or Esc
gives the keys back. While the sidebar has them, its highlight is drawn
with `indicator`, then `sidebar_highlight`. For example, to put it on
the right, and show its folders' names indented:

```muttrc
set sidebar_visible sidebar_on_right
set sidebar_short_path sidebar_folder_indent
```

Its `color` objects are `sidebar_new` (new or unread mail),
`sidebar_flagged`, `sidebar_spoolfile` (if set), `sidebar_indicator` (the
open mailbox; `indicator` until set), `sidebar_highlight` (the highlight;
underlined by default), `sidebar_divider` and, from neomutt,
`sidebar_ordinary` (the others; `normal` until set).

### 4.12 Meeting invitations

A message that invites to a meeting shows it with the headers in the pager:
`Meeting:`, `When:` (in the local time zone), `Repeats:`, `Where:`,
`Organizer:`, `Attendees:` and `Your reply:`. mett's own functions, in the
index and the pager (the pager, once the message is open), answer it:

| Function | Default key | Answer |
|----------|-------------|--------|
| `<accept-invite>` | `^A` | Accept. |
| `<tentative-invite>` | `^T` | Accept tentatively. |
| `<decline-invite>` | `^D` | Decline. |

For a meeting that repeats, mett asks first whether the answer is for the
`[o]ccurrence` the message is about (the first, for an invitation to a new
series) or the `[s]eries`. If the organizer asked for an answer, mett then asks
for a comment (optional) and whether to send the answer; if not, it only
records it. The answer is always put in your calendar. The pager shows the new `Your reply:` once it is
sent. A cancellation, an attendee's answer, a request that is out of date
and a meeting you organize can't be answered.

With Graph, answering needs the `Calendars.ReadWrite` permission
(`msgraph_scopes`, see `CONFIG-OFFICE365.md`); EWS has it already.

### 4.11 The built-in editor

mett has an editor of its own, which writes messages in mett's screen. It
is used when `editor` is `builtin`, or when neither `editor`, `$VISUAL`
nor `$EDITOR` names an editor; and always in `mett-gui`'s window, which
has no terminal for another (`editor` is ignored there). In the window, it
copies to and pastes from the system's clipboard. `editor_keys` (mett's own) chooses its
keys: `vim`, `emacs` or `notepad`. Unset (the default), they are those of
the editor that `editor` (if not `builtin`), `$VISUAL` or `$EDITOR`
names: vim's for vi, vim, Neovim and their kin, emacs's for emacs,
emacsclient, mg and their kin, Notepad's for any other, or none.

As with any editor, the text goes back to the compose flow as last saved:
leaving without saving gives back the text as it was (an unmodified
message, which `abort_unmodified` aborts).

| | vim | emacs | notepad |
|---|---|---|---|
| Save and leave | `:wq`, `:x`, `ZZ` | `C-x C-c` | `^S` |
| Leave without saving | `:q!`, `ZQ` (`:q` when nothing changed) | `C-x k` (asks if something changed) | Esc (asks if something changed) |
| Save | `:w` | `C-x C-s` | |
| Search | `/` | `C-s` | `^F`, then F3 and Shift+F3 |
| Select | `v` | `C-Space` sets the mark; `C-w` cuts, `M-w` copies | Shift and the arrows, Home, End; `^A` selects all; `^X`, `^C`, `^V` |
| Undo, redo | `u`, `^R` | `C-/`, `C-u`; `C-r` | `^Z`, `^Y` |
| Spelling on, off | `:set spell`, `:set nospell`, `:set spell!` | `M-x flyspell-mode` | F7 |
| Dictionaries | `:set spelllang=en_us,es_es` | `M-x ispell-change-dictionary` | (`:set spell_lang` in mett) |
| Next, previous misspelled word | `]s`, `[s` | `M-g n`, `M-g p` | F8 (and its suggestions), Shift+F8 |
| Suggestions for the word | `z=` | `M-$` | `^.` |
| Add the word to yours | `zg` | `a` in the suggestions | `a` in the suggestions |

`M-x` runs `save-buffer`, `save-buffers-kill-terminal`, `kill-buffer`,
`flyspell-mode` and `ispell-change-dictionary`.

**Spelling.** With `spell_check` set (the default), the built-in editor
underlines misspelled words, in red where the terminal can: not in quoted
lines (`>`, or `indent_string`), the signature (after `-- `, or your
`signature` itself), web and mail addresses, words with digits, or
in capitals (`HTML`). Suggestions show on the bottom line: a digit puts one
in place of the word, `a` adds it to your words (`spell_words`, one a
line), any other key leaves it. Turning spelling on or off, or changing
the dictionaries, in the editor sets `spell_check` and `spell_lang` until
mett ends (the mettrc is left as it is).

The dictionaries are Hunspell's (`<lang>.aff` and `<lang>.dic`): mett has
`en_US`, from [SCOWL](http://wordlist.aspell.net/), and finds others in
`spell_dict_path`, `~/.config/mett/dictionaries/`, and the system's
(`/usr/share/hunspell/`, `/usr/share/myspell/`; on a Mac,
`~/Library/Spelling/` and `/Library/Spelling/`). Linux distributions
package them (`hunspell-es` on Debian, `hunspell-es_any` on Arch, say);
LibreOffice's are the same files. `en_us` finds `en_US` too.
The editing keys are those of [edtui](https://github.com/preiter93/edtui),
the editor it is built on: most of vim's motions and operators, and
emacs's movement and deletion. In the terminal, its clipboard is its
own: paste text from elsewhere through the terminal.

### 4.13 IMAP and SMTP accounts

mett reads and sends mail through any IMAP and SMTP server, besides
Office365 (`msgraph://`). It uses IMAP when `spoolfile` is an `imap://` or
`imaps://` URL:

```muttrc
set imap_user = "alice"
set imap_pass = "`pass show mail/example`"   # or a literal password
set folder    = "imaps://mail.example.com/"
set spoolfile = "=INBOX"
set record    = "=Sent"
set postponed = "=Drafts"
set trash     = "=Trash"

set smtp_url  = "smtps://smtp.example.com"   # smtp://host:587 for STARTTLS
set from      = "alice@example.com"
set realname  = "Alice Example"
```

- **The URL** is `imap[s]://[user[:password]@]host[:port]/folder`. The user
  and password may come from `imap_user` and `imap_pass` instead (escape
  `@`, `:` and `/` in a URL as `%40`, `%3A` and `%2F`). The account is named
  `user@host`, or the user if it is an address.
- **Connections.** `imaps://` and `smtps://` are TLS from the first byte
  (ports 993 and 465). `imap://` and `smtp://` (ports 143 and 587) upgrade with
  STARTTLS if the server offers it (`ssl_starttls`, on by default), fail
  without it if `ssl_force_tls` is set, and go on in plain text otherwise:
  use `ssl_force_tls` unless the network is yours. The server's certificate
  is checked against the system's authorities (and
  `ssl_ca_certificate_file`; put a self-signed certificate there), and for the
  server's name unless `ssl_verify_host` is off.
- **SMTP.** `smtp_url` names the server and, if it needs a login, the user
  (`smtps://alice@smtp.example.com`; as in mutt, a URL without a user is a
  relay that takes mail without one). Its password is in the URL, or
  `smtp_pass`, or the IMAP one if the user is the same. Without `smtp_url`,
  mail can't be sent.
  mett saves the copy of what it sends (`copy`) in the folder the server
  calls Sent, and postponed messages in Drafts, whatever `record` and
  `postponed` say (as with Office365). Replies keep the original's
  `Message-ID:` and `References:`.
- **Folders** are named as on the server, below `folder`: `=INBOX/Work`
  (with `/`, whatever the server's own separator is). `@inbox`, `@sentitems`,
  `@drafts`, `@deleteditems`, `@junkemail` and `@archive` find a folder by what
  the server says it is (the `\Sent`, `\Drafts`, `\Trash`, `\Junk` and `\Archive`
  attributes), or by its usual name (`Sent`, `Drafts`, `Trash`, `Spam`,
  `Archive`).
- **Changing them** with `:set` takes effect when mett starts again: the
  connections are made at start-up (mett says so).
- **Limits.** mett logs in with a password only (`LOGIN`; OAuth2 isn't
  wired to the configuration yet). IMAP can't tell what changed in a folder,
  so mett doesn't cache the mailbox and looks again every `mail_check`
  seconds rather than being told when mail arrives (flags that another client
  changes are noticed that way only where the server has `CONDSTORE`). The
  index is in the order of the messages' dates where the server has `SORT`,
  and of their arrival (UID, and they are dated by it) where it hasn't. Threads are those of
  `References:`. Meeting invitations aren't shown as such (their `text/calendar`
  part is an attachment) and can't be answered.

## 5. Mailbox shortcuts

Wherever a path is expected:

| Shortcut | Meaning |
|----------|---------|
| `=` or `+` | Relative to `$folder`, for example `=INBOX/Archive`. |
| `!` | `$spoolfile` |
| `>` | `$mbox` |
| `<` | `$record` |
| `^` | The current mailbox. |
| `-` or `!!` | The previously opened mailbox. |
| `~` | Home directory. |
| `@alias` | Default save folder for the alias's address. |

Remote mailboxes are URLs: `imap[s]://[user@]host[:port]/path` (§4.13) and
`msgraph://account/path`. mett doesn't read POP.

## 6. Patterns

Patterns select messages for limit, search, tag, hooks, scoring and color.
They are made of `~X` (or `=X`, `%X`) operators combined with:

- **AND:** adjacent operators, separated by spaces.
- **OR:** `|`
- **NOT:** `!`
- **Grouping:** parentheses.

| Operator | Matches |
|----------|---------|
| `~A` | All messages. |
| `~b EXPR` / `~B EXPR` | Body / whole message. |
| `~c`, `~C`, `~e`, `~f`, `~t`, `~L` | Cc / To or Cc / Sender / From / To / From or To. |
| `~s EXPR` | Subject. |
| `~h EXPR` | A header line. |
| `~i EXPR` | Message-ID. |
| `~d [MIN]-[MAX]`, `~r …` | Date sent / date received (absolute or relative, for example `~d <1w`). |
| `~N`, `~O`, `~U`, `~R` | New / old / unread / read. |
| `~F`, `~D`, `~T`, `~Q` | Flagged / deleted / tagged / replied. |
| `~p`, `~P` | Addressed to me / sent by me (see `alternates`). |
| `~l`, `~u` | Addressed to a known / subscribed mailing list. |
| `~x EXPR` | References and In-Reply-To. |
| `~y EXPR` | X-Label. |
| `~z [MIN]-[MAX]` | Size. |
| `~n [MIN]-[MAX]` | Score. |
| `~m [MIN]-[MAX]` | Message-number range. |
| `~(PATTERN)` | Messages in threads that contain a match. |
| `~v` | Messages in collapsed threads. |
| `~g`, `~G`, `~k`, `~V` | Signed / encrypted / has PGP key / verified. |

mett supports `~A`, `~f`, `~t`, `~s`, `~N`, `~U`, `~R`, `~F` and `~D`, with
`|`, `!` and parentheses, for `/` (search) and `l` (limit). As in mutt,
`EXPR` ignores case unless it has an upper-case letter, and text without a
pattern searches the sender and the subject.

`=b`, `=B` and `=h` are like the `~` forms, but on IMAP the search runs on the
server. `%X` matches against members of a group (`-group`) instead of a
regex.

## 7. Example: Office365 via IMAP and SMTP in plain mutt

Today, plain mutt can reach Office365 only through IMAP and SMTP with OAuth2.
It relies on an external helper script (mutt ships `contrib/mutt_oauth2.py`)
to obtain tokens:

```muttrc
set from      = "alice@contoso.com"
set realname  = "Alice Example"

set imap_user = "alice@contoso.com"
set folder    = "imaps://outlook.office365.com/"
set spoolfile = "+INBOX"
set record    = "+Sent Items"
set postponed = "+Drafts"
set trash     = "+Deleted Items"

set imap_authenticators        = "xoauth2"
set imap_oauth_refresh_command = "mutt_oauth2.py ~/.cache/mutt/alice.tokens"

set smtp_url                   = "smtp://alice%40contoso.com@smtp.office365.com:587/"
set smtp_authenticators        = "xoauth2"
set smtp_oauth_refresh_command = "mutt_oauth2.py ~/.cache/mutt/alice.tokens"
```
