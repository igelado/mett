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
| | `signature` | path | Signature file. Only the default, `~/.signature`, may be missing; a missing file that was set is an error. |
| Mailboxes | `folder` | path | Base of the mailbox tree, often an `imaps://` URL. Default `~/Mail`. |
| | `spoolfile` | path | Incoming mailbox, opened at startup. |
| | `record` | path | Where copies of sent mail are saved. |
| | `postponed` | path | Where drafts are saved. |
| | `mbox` | path | Where read mail is moved when `move` is set. |
| | `trash` | path | Deleted mail is moved here instead of being purged. |
| | `copy` | quadoption | Whether to save a copy of outgoing mail in `record`. |
| IMAP | `imap_user`, `imap_login`, `imap_pass` | string | Credentials. |
| | `imap_authenticators` | string | Colon-separated methods, for example `oauthbearer:xoauth2`. |
| | `imap_oauth_refresh_command` | string | Command that prints a fresh OAuth2 access token. |
| | `imap_check_subscribed` | boolean | Poll subscribed folders. |
| SMTP | `smtp_url` | string | `smtp[s]://[user[:pass]@]host[:port]/` |
| | `smtp_pass`, `smtp_authenticators` | string | Credentials and methods. |
| | `smtp_oauth_refresh_command` | string | As for IMAP. |
| | `sendmail` | path | Local MTA, used when `smtp_url` is unset. |
| | `ssl_force_tls` | boolean | Require encrypted connections. |
| Caching | `header_cache` | path | Header cache database. |
| | `message_cachedir` | path | Body cache for IMAP and POP. |
| Composing | `editor` | path | Compose editor. Falls back to `$VISUAL`, `$EDITOR`, then `vi`. |
| | `include` | quadoption | Quote the message in replies. |
| | `attribution`, `indent_string` | string | The line before quoted text (`On %d, %n wrote:`), and the prefix of quoted lines (`> `). |
| | `forward_format` | string | Subject of forwarded messages (`[%a: %s]`). |
| | `reply_to` | quadoption | Reply to the `Reply-To:` address rather than the sender. |
| | `fast_reply`, `askcc`, `askbcc` | boolean | Skip the recipient and subject questions when replying; ask for `Cc:` and `Bcc:`. |
| | `metoo` | boolean | Keep one's own address in group replies. (A reply to one's own message goes to its other recipients, or to its one `Bcc:` recipient; several `Bcc:` recipients are never shown to each other.) |
| | `sig_dashes` | boolean | Put `-- ` before the signature. |
| | `abort_unmodified`, `abort_nosubject`, `postpone` | quadoption | Abort an unedited message or one without subject (asked again when sending without one, as in mutt); postpone when leaving the compose menu. |
| | `tmpdir` | path | Where the editor's temporary files go. |
| Behaviour | `mail_check` | number | Seconds between new-mail checks. |
| | `timeout` | number | Seconds to wait for input before checking mail. |
| | `sort`, `sort_aux` | sort order | Index sorting. |
| | `index_format`, `pager_format`, `status_format` | string | Display formats. |
| | `mailcap_path` | string | mailcap files used to view MIME parts. |
| | `query_command` | path | External address-book query. |

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

### 4.3 Addresses and identities

```
alias         [-group name [...]] key address [, address ...]
unalias       [ * | key ]
group         [-group name] [-rx EXPR ...] [-addr address ...]
ungroup       [-group name] [ * | [-rx EXPR ...] [-addr address ...] ]
alternates    [-group name] regexp [ ... ]
unalternates  [ * | regexp ... ]
```

### 4.4 Key bindings and macros

```
bind          map1,map2,... key function
macro         map key sequence [ description ]
push          string
exec          function [ ... ]
```

The maps are the menus: `generic`, `index`, `pager`, `compose`, `attach`,
`browser`, `alias`, `query`, `editor`, `postpone`, `pgp` and `smime`. Keys use
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

Remote mailboxes are URLs: `imap[s]://[user@]host[:port]/path` and
`pop[s]://…`.

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
