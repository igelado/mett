# Configuring Office365 accounts in mett

Status: **partly implemented.** mett reads mettrc files (§1) with `set`,
`unset`, `toggle`, `reset` and `source`, and opens the `msgraph://` folder
in `spoolfile` (§3, without the Focused Inbox views), signing in with the
§4.1–4.3 variables (except `msgraph_oauth_refresh_command`). It sends mail
through the spoolfile's account and postpones it to that account's Drafts
(§5.2, without `record` and `postponed`). Folders of that account can be
opened, browsed, and used to save or copy messages, and purged messages go
to `trash` (§6). The open folder and the `mailboxes` of that account are
checked for new mail every `msgraph_sync_interval` seconds (§4.4).
Everything else is still a proposal.

This document proposes how mett is configured. mett adopts the muttrc
format described in [MUTTRC.md](MUTTRC.md) and adds the minimum extensions
needed to use an Office365 account through Microsoft Graph.

## 1. Principles

1. **muttrc syntax, unchanged.** The lexical rules, `set`/`unset`/`toggle`/
   `reset`, `source`, hooks, key bindings, macros, colors and patterns behave
   as in mutt. Users can reuse most of an existing muttrc.
2. **Extend, don't fork.** New behaviour is added through:
   - a new mailbox URL scheme (§3),
   - new variables with a `msgraph_` prefix (§4),
   - new meanings for existing mutt concepts where they map naturally (§6).

   No new commands are added unless no existing mechanism fits.
3. **Secrets stay out of the file.** OAuth2 tokens are never written in the
   configuration file. Like mutt, mett allows backticks, but its own token
   store (§4.3) is the default.
4. **Unknown is a warning, not an error.** Commands and variables that mett
   doesn't implement, such as POP, PGP, compressed folders and MuttLisp, are
   reported once as warnings and otherwise ignored. A muttrc that a mettrc
   `source`s therefore doesn't stop mett from starting.
5. **mett has its own files.** mett reads a `mettrc`, never mutt's muttrc
   files, so mutt never sees the `msgraph_*` variables. The search order
   mirrors mutt's:

   | Kind | Locations, in search order |
   |------|---------------------------|
   | System | `$sysconfdir/mettrc` (usually `/etc/mettrc`) |
   | User | `~/.mettrc`, then `~/.mett/mettrc`, then `$XDG_CONFIG_HOME/mett/mettrc` |

   Unset XDG base directories fall back to the defaults in the XDG Base
   Directory specification: `XDG_CONFIG_HOME` to `~/.config` and
   `XDG_STATE_HOME` to `~/.local/state`. This applies to every path in this
   document.

   Only the first user file found is read. `mett -F <file>` replaces the user
   file, and `mett -n` skips the system file. To share settings such as key
   bindings and colors with mutt, a mettrc can `source` a muttrc.
6. **Users bring their own Azure AD app.** mett ships no app registration
   and no default client ID. Each user or organisation registers an
   application in Microsoft Entra ID (Azure AD) and sets `msgraph_client_id`
   (§4.1).

## 2. Where plain muttrc falls short

| Need | Plain muttrc | Gap |
|------|--------------|-----|
| Identify a Graph mailbox | Paths and `imap://`/`pop://` URLs. | No way to name a Graph account or its folders. |
| OAuth2 | `*_authenticators=xoauth2`, plus an external `*_oauth_refresh_command` script. | No built-in OAuth2. Tenant, client ID, auth flow, scopes and token storage all live in an external script. |
| Sending | `smtp_url` or `sendmail`. | Graph sends mail with an API call (`sendMail`), not SMTP. |
| Standard folders | Folders addressed by name (`+Sent Items`). | Office365 folder names are localised (for example *Elementos enviados*). Graph exposes locale-independent *well-known names*. |
| Focused Inbox | None. | Office365 splits the Inbox into Focused and Other. |
| Categories | `X-Label` (`~y`, `<edit-label>`). | Office365 uses categories, which can hold several values per message. |
| Address lookup | `query_command`, an external script. | The organisation directory (GAL) is available through Graph. |
| Server-side search | `=b`/`=B`/`=h` patterns for IMAP. | Graph has its own `$search` and `$filter` syntax. |
| National clouds | n/a | Graph and login endpoints differ for the US Government and China clouds. |

Plain muttrc already covers multiple accounts (`account-hook`,
`folder-hook`), caching (`header_cache`, `message_cachedir`), identity
(`from`, `realname`) and the standard mailbox variables (`spoolfile`,
`record`, `postponed`, `trash`). mett reuses them.

## 3. The `msgraph://` mailbox URL scheme

```
msgraph://<account>/<folder-path>
```

- **`<account>`** is the user principal name (UPN), usually the e-mail
  address, for example `alice@contoso.com`. The `@` doesn't need escaping,
  because the URL has no other user-info part.
- **`<folder-path>`** is a `/`-separated path of folder display names below
  the mailbox root, for example `Inbox/Projects/mett`.
- **Well-known folders** are written `@name`, using the Graph well-known
  names. These include `@inbox`, `@drafts`, `@sentitems`, `@deleteditems`,
  `@junkemail`, `@archive` and `@outbox`. They work regardless of the
  mailbox's language.
- **Focused Inbox views:** `@inbox/@focused` and `@inbox/@other` are virtual
  folders that filter the Inbox by its Focused Inbox classification.

Because `$folder` can be a `msgraph://` URL, mutt's mailbox shortcuts work
unchanged:

```muttrc
set folder    = "msgraph://alice@contoso.com/"
set spoolfile = "=@inbox"
set record    = "=@sentitems"
set postponed = "=@drafts"
set trash     = "=@deleteditems"
mailboxes     =@inbox =@inbox/@focused "=Inbox/Projects/mett"
```

`account-hook` and `folder-hook` match against these URLs as they do against
`imaps://` URLs.

## 4. New variables

All new variables use the `msgraph_` prefix, following the `imap_`/`smtp_`
convention. Like other mutt variables, they can be set per account from an
`account-hook`.

### 4.1 Application and tenant

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `msgraph_client_id` | string | *(none, required)* | Application (client) ID of the user's Entra ID (Azure AD) app registration. mett refuses to open a `msgraph://` account without it. |
| `msgraph_tenant` | string | *(domain of the account)* | Tenant used to sign in: a domain, a tenant GUID, `organizations`, `common` or `consumers`. The default, for example `contoso.com` for `msgraph://alice@contoso.com/`, works for both single-tenant and multi-tenant apps, provided the domain is verified in the tenant. Single-tenant apps can't use `organizations` or `common`, because Entra ID rejects them (error AADSTS50194). |
| `msgraph_cloud` | string | `global` | `global`, `usgov`, `usgov-dod` or `china`. Selects the login and Graph endpoints. |
| `msgraph_endpoint` | string | *(from `msgraph_cloud`)* | Overrides the Graph base URL, for testing or proxies. Must be `https`, except on this machine (`localhost`, loopback addresses). |

#### Registering the app

The app registration needs:

- **Platform:** *Mobile and desktop applications*, with the redirect URI
  `http://localhost`. Entra ID accepts any port on a loopback redirect, which
  `msgraph_redirect_port` relies on.
- **Allow public client flows:** enabled. This is required for
  `device-code`, and mett never uses a client secret.
- **Supported account types:** single tenant is enough for one organisation.
  Choose multi-tenant only if the same registration serves several tenants.
  A single-tenant app signs in only through its own tenant, which is what
  the default `msgraph_tenant` provides.
- **API permissions:** the delegated Microsoft Graph permissions listed in
  `msgraph_scopes`. An administrator may need to grant consent, depending on
  the tenant's policy.

To check a registration before using mett, run the sign-in example. It signs
in and prints the signed-in user:

```sh
METT_ACCOUNT=alice@contoso.com METT_CLIENT_ID=<app-id> \
    cargo run -p mett-msgraph --example signin
```

Set `METT_FLOW=device` to use the device code flow instead of the browser,
and `METT_CLOUD` (`usgov`, `usgov-dod` or `china`) for a national cloud.
[crates/mett-msgraph/README.md](../crates/mett-msgraph/README.md) has step-by-step
instructions and troubleshooting.

### 4.2 Authentication

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `msgraph_auth_flow` | string | `auth-code` | `auth-code` (authorization code with PKCE, through the system browser and a loopback redirect) or `device-code` (for headless or SSH sessions). |
| `msgraph_redirect_port` | number | `0` | Loopback port for `auth-code`. `0` picks a free port. |
| `msgraph_scopes` | string | `Mail.ReadWrite Mail.Send User.Read User.ReadBasic.All People.Read offline_access` | Space-separated OAuth2 scopes. `User.ReadBasic.All` is needed only for directory search (`msgraph_query_directory`) and can be dropped when that is off. `User.Read` is required: after every sign-in and token refresh, mett reads the user principal name from Graph's `/me` and rejects the tokens unless it is this account. |
| `msgraph_oauth_refresh_command` | string | `""` | When set, mett runs this command to obtain an access token instead of using its built-in OAuth2. This mirrors `imap_oauth_refresh_command`, so existing helper scripts can be reused. *Not supported yet: mett refuses to start when it is set.* |

### 4.3 Token storage

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `msgraph_token_store` | string | `keyring` | `keyring` (the OS secret service) or `file`. `file` is supported only on Unix, where file permissions keep the tokens private. |
| `msgraph_token_file` | path | `$XDG_STATE_HOME/mett/<account>.tokens` (`~/.local/state/mett/<account>.tokens` when `XDG_STATE_HOME` is unset) | Used when the store is `file`. Created with mode `0600`. Several accounts may share one file; each keeps its own token. |

### 4.4 Behaviour

| Variable | Type | Default | Description |
|----------|------|---------|-------------|
| `msgraph_focused_inbox` | boolean | `no` | When set, `=@inbox` opens the Focused view, and a status-line indicator lets the user switch to Other. |
| `msgraph_query_directory` | boolean | `yes` | When `query_command` is unset, `<query>` searches the people the user works with (People API, `People.Read`) and the whole organisation directory (`User.ReadBasic.All`). |
| `msgraph_page_size` | number | `50` | Number of messages fetched per Graph request. |
| `msgraph_sync_interval` | number | `60` | Seconds between checks for new mail; `0` turns them off. This plays the role of `mail_check`, which mett uses only for local mailboxes. *Implemented so far:* each check lists the newest page of the open folder again (taking in what arrived, changed or went among those messages) and the messages changed since the last check (`lastModifiedDateTime`, which also finds mail delivered late or moved in with an old date), and reads the counts of the `mailboxes`; a mailbox has new mail when its unread count grew since it was last open (at first, when it has unread mail). |

## 5. Accounts and sending

### 5.1 Accounts

Like mutt, mett has no `account` command. An account is whatever a mailbox
URL names, and its settings come from hooks.

- **Discovering accounts:** at startup, mett collects the `<account>` part of
  every `msgraph://` URL in `folder`, `spoolfile`, `mailboxes`, `record` and
  `postponed`. The worker signs in to and syncs each of these accounts in the
  background. mutt does the equivalent when it polls `mailboxes`.
- **Connection settings (`account-hook`):** for each account, mett applies
  the `account-hook`s whose regex matches its URL to a private copy of the
  `msgraph_*` variables, and gives that copy to the worker. mutt is
  single-threaded, so it applies the hook to its global variables just before
  each connection. mett's worker serves several accounts at once, so each
  account needs its own copy. The result is the same as in mutt, but settings
  can't carry over from one account to another.
- **Identity (`folder-hook`):** `from`, `realname`, `record` and `postponed`
  are set by `folder-hook`s when a mailbox is opened, exactly as in mutt.

### 5.2 Sending

mett sends through the `msgraph://` account whose `<account>` matches the
`From:` address, which is normally set by a `folder-hook`. It uses Graph's
`sendMail` endpoint, and `smtp_url` and `sendmail` are ignored for that
message. If no `msgraph://` account matches, mett falls back to `smtp_url` or
`sendmail` as mutt does.

- **Saving a copy:** if `copy` allows a copy and `record` resolves to the same
  account's `@sentitems`, mett asks Graph to save the copy
  (`saveToSentItems=true`) instead of uploading a second one. Any other
  `record` value is honoured with an explicit save.
- **Postponing:** `postponed` pointing at `@drafts` stores drafts as server
  drafts, so they are visible in Outlook.
- **Replies and forwards** go through a server draft (Graph's
  `createReply` or `createForward`), so that they thread under the original
  and forwards keep its attachments. Graph always keeps a copy of a sent
  draft in Sent Items, so `copy` only applies to new messages. When a send
  or update of that draft has no clear outcome (the response was lost),
  the draft is kept, and sending or postponing the message again updates
  it rather than making another; leaving the message without postponing it
  deletes it (for good where Graph allows, in the global cloud; elsewhere
  into Deleted Items). (If the response to creating the draft is lost, mett can't
  tell which draft it made, and says one may be left in Drafts.)

*Implemented so far:* mail is sent through the account of `spoolfile`, and
a copy is kept in its Sent Items when `copy` allows. `record` and
`postponed` are not used yet: postponed messages always go to the
account's Drafts folder.

## 6. New meanings for existing mutt concepts

| mutt concept | Office365 meaning in mett |
|--------------|---------------------------|
| `~F`, `<flag-message>` | Follow-up flag (`flagged` / `notFlagged`). |
| `~y EXPR`, `<edit-label>`, `%y` | Categories. A message matches if any of its categories matches. `<edit-label>` edits the category list, separated by commas. |
| `=b`, `=B`, `=h` patterns | Translated to Graph `$search` where possible. Anything else is evaluated locally on cached messages. |
| `my_hdr Importance: high` | Sets the Graph `importance` property. |
| Threads (`sort=threads`) | Built from `References`/`In-Reply-To` as in mutt, with Graph's `conversationId` used when those headers are missing. |
| `header_cache`, `message_cachedir` | Location of the local cache. |
| `trash` | Deleting a message moves it to `trash`. With `trash` unset, mett deletes permanently, after the usual confirmation. |

## 7. Examples

### 7.1 One account

```muttrc
set from     = "alice@contoso.com"
set realname = "Alice Example"

set folder    = "msgraph://alice@contoso.com/"
set spoolfile = "=@inbox"
set record    = "=@sentitems"
set postponed = "=@drafts"
set trash     = "=@deleteditems"

set msgraph_client_id     = "00000000-0000-0000-0000-000000000000"
set msgraph_tenant        = "contoso.com"
set msgraph_focused_inbox = yes

mailboxes =@inbox =@inbox/@other =@archive "=Inbox/Projects/mett"
```

### 7.2 Two accounts

This uses the usual mutt `account-hook` and `folder-hook` idiom:

```muttrc
account-hook msgraph://alice@contoso.com/ 'set msgraph_client_id = "11111111-1111-1111-1111-111111111111"; set msgraph_tenant = "contoso.com"'
account-hook msgraph://alice@fabrikam.com/ 'set msgraph_client_id = "22222222-2222-2222-2222-222222222222"; set msgraph_tenant = "fabrikam.com"; set msgraph_auth_flow = "device-code"'

folder-hook msgraph://alice@contoso.com/  'set from = "alice@contoso.com";  set record = "msgraph://alice@contoso.com/@sentitems"'
folder-hook msgraph://alice@fabrikam.com/ 'set from = "alice@fabrikam.com"; set record = "msgraph://alice@fabrikam.com/@sentitems"'

set spoolfile = "msgraph://alice@contoso.com/@inbox"
mailboxes msgraph://alice@contoso.com/@inbox msgraph://alice@fabrikam.com/@inbox
```

## 8. Open questions

- **Shared mailboxes:** should delegated and shared mailboxes be addressed as
  `msgraph://alice@contoso.com/shared/team@contoso.com/@inbox`, or as
  accounts in their own right?
