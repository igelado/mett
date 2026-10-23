# mett design

## 1. Overview

`mett` is a mutt-like, keyboard-driven terminal e-mail client for Office365.
It accesses mailboxes through the Microsoft Graph API and offers a terminal
UI (TUI) that mirrors mutt's workflow: an index of messages, a pager, a
compose flow that uses the user's `$EDITOR`, and configurable key bindings.

## 2. Goals and non-goals

### Goals

- Read, search, compose, reply to, forward, move and delete mail stored in
  Office365.
- Keep mutt's interaction model, with per-menu key bindings, macros and
  named functions.
- Keep the UI responsive at all times. Network I/O never blocks rendering.
- Keep the design clean, testable and extensible, built on well-established
  design patterns.

### Non-goals (for now)

- Supporting mail providers other than Office365. The design leaves room for
  them, but they are not planned.
- A graphical UI of its own, with widgets. `mett-gui` shows the terminal
  UI's screens, cell for cell, in a window (§5.2).
- Full compatibility with mutt's `muttrc`. mett uses the muttrc syntax but
  implements only part of mutt's commands and variables (see
  [CONFIG-OFFICE365.md](CONFIG-OFFICE365.md)).

## 3. Constraints

- Written in Rust, using the latest stable edition (Rust 2024).
- Terminal UI modelled on mutt.
- Clean design built on well-established design patterns.

## 4. Architecture

The system is layered following the hexagonal (ports and adapters) pattern.
The TUI follows the Elm Architecture. A background worker (actor) owns all
I/O and talks to the UI over channels.

```
┌──────────────── mett-tui (ratatui) ────────────┐
│  Elm Architecture: Model → update(Msg) → view  │
│  Commands: key → Action → update               │
│  Menus/screens: enum state machine             │
└──────────────┬─────────────────▲───────────────┘
        Request│ (channel)       │Event (channel)
┌──────────────▼─────────────────┴───────────────┐
│  Async worker (actor): owns network + cache    │
├────────────────────────────────────────────────┤
│  mett-core: domain model + ports (traits)      │
│  MailStore, Authenticator, MailSender, …       │
├───────────────┬──────────────┬─────────────────┤
│ mett-msgraph, │ mett-sqlite  │ mock (tests)    │
│ mett-ews,     │              │                 │
│ mett-imap,    │              │                 │
│ mett-smtp     │              │                 │
└───────────────┴──────────────┴─────────────────┘
```

### 4.1 Workspace layout

The project is a Cargo workspace, with each crate in `crates/<name>/`. The
crate boundaries enforce the layering:
a crate may only depend on crates below it in the diagram.

| Crate          | Responsibility                                                     | Depends on                     |
|----------------|--------------------------------------------------------------------|--------------------------------|
| `mett-core`    | Domain model (messages, folders, threads, drafts) and port traits. | none (no I/O crates)           |
| `mett-msgraph` | Microsoft Graph adapter: HTTP, OAuth2, paging, throttling, delta.  | `mett-core`                    |
| `mett-ews`     | Exchange Web Services adapter: SOAP, paging, throttling.           | `mett-core`                    |
| `mett-smtp`    | SMTP adapter (wraps `lettre`): sends mail, writes drafts as MIME.  | `mett-core`                    |
| `mett-imap`    | IMAP adapter (wraps `async-imap`): mailbox access; sends via SMTP. | `mett-core`, `mett-smtp`       |
| `mett-sqlite`  | Local cache and sync state, in SQLite.                             | `mett-core`                    |
| `mett-tui`     | The binary: TUI, key bindings, configuration, worker wiring.       | `mett-core`, adapters          |

`mett --demo` runs the UI on `mett-core`'s in-memory `MailStore`, without
an account.

`mett-tui` is the only crate that knows about every adapter. It wires them
together at startup, acting as the composition root.

## 5. Design patterns

### 5.1 Hexagonal architecture (ports and adapters)

`mett-core` defines the domain types and the traits (ports) that the rest of
the application depends on, for example:

- `MailStore`: list folders, list and fetch messages, move, delete and flag
  them, and save drafts.
- `MailSender`: send an outgoing message (a `Draft`, which may reply to or
  forward another message).
- `Authenticator`: obtain and refresh access tokens.
- `Renderer`: turn a message body (for example HTML) into terminal text.

Adapters implement these ports. `mett-msgraph` implements them against
Microsoft Graph, `mett-ews` against Exchange Web Services, and a mock
implementation is used in tests. `mett-ews` has no sign-in of its own: it
takes any `Authenticator`, and `mett-tui` gives it `mett-msgraph`'s, asking
for an Exchange Online token. `mett-tui` picks the adapter at startup
(`use_ews`); an enum of the two implements the ports by delegating, so the
worker stays generic over a single type.

`mett-core` holds the connection settings both mail adapters share
(`ServerSettings`, `Security`, `Login`). The two keep their own TLS stacks
(lettre's, and rustls through `async-imap`); both verify with the platform
verifier, except that with `verify_host` off lettre must check the chain
itself against the system's roots, without the platform's policy.

`mett-smtp` sends a `Draft` over SMTP (plain, STARTTLS or TLS; a password or
an OAuth2 token for the login, or none), wrapping the `lettre` crate. It also writes
the `Draft` as the MIME message (`compose`), which a mailbox that keeps its
own Sent and Drafts folders (IMAP) stores. Replies are threaded by the
`Message-ID:` and `References:` of the original, which the caller looks up
and gives as `Threading`. It leaves the Sent copy to the mailbox, so it is
not a `MailSender` itself.

`mett-msgraph` and `mett-ews` send replies, forwards and messages with
attachments (large ones, for Graph) through a server draft, whose bookkeeping (drafts
kept after a lost answer, the uploads of a recalled draft, what a retry takes
off) is shared: `mett-core`'s `DraftSession` holds it and runs the
algorithms, and each adapter implements only `DraftBackend`, the requests
that differ between Graph's REST and EWS's SOAP.

`mett-imap` implements `MailStore` against an IMAP server (wrapping
`async-imap` and `mail-parser`, with TLS by rustls), and `MailSender` on top
of an `SmtpSender`: it threads replies, carries the attachments of forwards
and recalled drafts, and appends the copy to the Sent folder. Folders are
named by their names on the server and messages by folder, `UIDVALIDITY` and
`UID`, so a message that moves is a new one, as with Graph. IMAP can't say
what changed in a folder as the Exchange APIs do, so the adapter doesn't
watch folders. It does sync them, from a state it keeps in the cache's own
state string (`UIDVALIDITY`, the next `UID`, the highest modification
sequence of `CONDSTORE`, the `UID`s the folder had), so the same
`mett-sqlite` cache serves it as serves Graph and EWS; a server without
`CONDSTORE` can't be synced, and isn't cached. Without the cache, it
remembers how each folder stood to find what is new; messages are ordered
by date with `SORT` where the server has it.
`mett-tui` picks IMAP when the `spoolfile` is an `imap://` or `imaps://` URL.

- **Pros:**
  - The core and UI can be tested without network access.
  - Graph-specific details (JSON shapes, paging, throttling) stay out of the
    UI.
  - Other backends can be added later without touching the UI.
- **Cons:**
  - More types and traits up front.
  - Async trait methods used through `dyn` need boxing. Prefer generics where
    the concrete type is known.
  - Office365-specific features (categories, Focused Inbox, delta tokens) may
    not map cleanly to a generic model.
- **Mitigation:** keep the ports shaped around what mett needs, not around a
  generic mail abstraction. Expose provider-specific features through optional
  extension traits.
- **Implementation:** port methods are declared as
  `fn …(&self, …) -> impl Future<Output = Result<…>> + Send`. Implementations
  write plain `async fn`s, the futures can run on a multi-threaded runtime,
  and no `async-trait` macro is needed. The worker uses the ports through
  generics, not `dyn`. `mett-core`'s `mock` feature provides an in-memory
  `MailStore` for tests in other crates.

### 5.2 Elm Architecture for the TUI

The UI holds a single `App` model. The only code that changes it is
`update(&mut App, Msg) -> Vec<Effect>`. `view(&App, &mut Frame)` renders it,
using `ratatui`.

`Msg` combines user input, already turned into `Action`s (§5.3), with
`Event`s coming back from the worker (§5.4). `Effect`s describe side
effects, such as sending a `Request` to the worker or suspending the TUI to
run `$EDITOR`. The main loop carries them out.

- **Pros:**
  - Every state change goes through a single function.
  - `update` can be unit-tested without a terminal.
  - There is no shared mutable widget state (`Rc<RefCell<…>>`).
- **Cons:**
  - The `Msg` enum becomes large.
  - Side effects take a round trip through messages.
- **Mitigation:** split `Msg` and `update` per screen, with a top-level
  dispatcher. Render only the visible rows of large lists.

**Grouped state.** State that belongs to the open folder lives in a
sub-struct of `App` that owns its `Default` and a `reset()`, so that opening
another folder resets it with one call and a new field can't be forgotten:
`IndexState` (the messages and how they are threaded, limited and listed)
and the folder's part of `Poll` (the check for new mail on its way, which
sits with the timers and the watch that carry on across folders). `FlagSync`
(flag changes on their way, saved or failed) forgets only what a listing
knew; the rest doesn't depend on the folder. `Throttle` (the requests the
server throttled and the countdown) belongs to no folder, so
`reset_folder` leaves it alone.

**Frontends.** The main loop is the same for the terminal and for
`mett-gui`'s window: it reads crossterm events from a channel, and draws on
a `Frontend`, which gives the screen's size, draws the `App`, copies to the
clipboard and runs the user's own editor.

- **The terminal** (`mett`): crossterm, with a thread reading its input;
  it hands the terminal over to `$EDITOR`.
- **The window** (`mett-gui`, the `gui` feature): winit's window, drawn by
  ratatui-wgpu. winit needs the main thread, so the app runs on a thread of
  its own. It draws each screen into cells (a ratatui backend that keeps
  them) and leaves the latest in a slot the window's thread reads, waking
  it (a window that draws slower than the app skips screens, rather than
  queueing them). That thread shows them, and
  sends keys back as the crossterm events a terminal would give. There is
  no terminal for `$EDITOR`, so messages are written in the built-in
  editor (`editor/builtin.rs`, on edtui).

The built-in editor is part of the model, like any screen. edtui lays its
text out while drawing, so the model holds it in a `RefCell`: the one
place the view changes anything, and only that layout.

The view runs for every frame and every visible row, so it keeps its
per-frame work small. Format strings (`index_format` and the others) are
compiled once into tokens, found again by their text (`format::compile`),
so a changed variable simply gets a new entry; `strftime` formats are
parsed once likewise. Rows are expanded in one pass into one buffer,
tracking the width as they go (the line is measured again only where a
combining character could join its neighbour), and plain ASCII lines
skip grapheme segmentation. The open folder's unread and flagged counts
(`App::folder_counts`) are counted once and shared by the status bar, the
sidebar and the sidebar's sort, and counted again after each `update` and
before each draw. A test keeps the old formatter as a reference to check
the compiled one against.

### 5.3 Command pattern for key bindings

Every user-visible operation is a named `Action` (for example `NextEntry`,
`Reply`, `SaveMessage`), mirroring mutt's functions such as `<next-entry>`.
A `Keymap` for each menu maps key sequences to actions. A macro is a list of
actions.

- **Pros:**
  - Keys can be rebound from the configuration file.
  - Macros are easy to support.
  - The help screen is generated from the keymap.
  - Behaviour can be tested without synthesising key events.
  - Undo is possible where it makes sense (for example, undeleting
    messages).
- **Cons:**
  - Adds a layer between input and behaviour.
  - Action names, documentation and default bindings have to be kept in sync.
- **Mitigation:** define actions in a single table (or with a macro) that
  produces the enum, the names, the help text and the default bindings.

### 5.4 Actor model for asynchronous I/O

A background `tokio` task, the worker, owns the `mett-msgraph` client, token
refresh and the local cache. The UI sends `Request`s over an `mpsc`
channel and receives `Event`s on another. Every request carries a
`RequestId`, so that the UI can match responses and drop stale ones.
Commands typed at the `:` prompt, which may `source` files or run
backticks, likewise run on a thread of their own, one at a time, and
report back to the main loop.

Requests are served concurrently, but reads (listing, fetching, checking)
only six calls at a time (a throttled call gives its turn up while it waits), the rest waiting their turn; writes (flags, moves,
sends, drafts) and the lookups the UI may give up on never wait behind them.
A read the UI no longer wants still runs once its turn comes (cancelling
stale fetches is not done yet).

A request that takes many Graph requests (moving many messages, checking
the `mailboxes`) makes a few at a time, keeping the order of its results.
A folder path is resolved one level at a time (`MailStore::child_folders`),
listing only the folders below each name on it rather than the whole tree,
and checking the `mailboxes` looks up each parent, well-known folder and
listing once for all of them.

- **Pros:**
  - The UI never blocks on I/O.
  - Each resource has a single owner, which fits Rust's ownership model.
  - Retries, throttling (`429`/`Retry-After`) and background sync are
    handled in one place.
- **Cons:**
  - Errors and cancellation must be modelled explicitly.
  - Control flow is harder to follow than direct `.await` calls.
- **Mitigation:** every `Request` has a matching outcome `Event`, which is
  either success or a typed error. The UI ignores outcomes whose `RequestId`
  is no longer relevant.

### 5.5 Repository with local cache and sync engine

The UI reads mailbox data from the cache, `mett-sqlite`, which keeps it up
to date by syncing each folder with the backend (`MailStore::sync_folder`:
Graph delta queries, or EWS `SyncFolderItems`).

- **Implementation:** `CachedStore` is a decorator: it implements the
  ports in front of the backend's store, so the app and the worker don't
  change. Listing a folder syncs it (once for all the calls of a request)
  and pages through the cache; changed and recent-unread queries read the
  cache after syncing; message bodies are kept once fetched; changes go to
  the server, then to the cache. A backend that can't sync, or
  `unset header_cache`, leaves everything to the server.
  A cache that fails for good (a full disk, corruption) is dropped for the
  session; one that is only busy (another mett writing) is skipped for
  that call alone.
  SQLite blocks (a busy wait, a large sync batch), so the database lives on
  a thread of its own, which runs the calls one at a time in the order
  they were made, and each is awaited: the async executor never waits on
  SQLite, and a call dropped meanwhile still finishes its transaction
  there (before any later one).

- **Pros:**
  - Browsing is fast, and the index opens instantly.
  - Search works offline.
  - Fewer Graph requests means less throttling.
- **Cons:**
  - Cache invalidation and conflict handling (for example, the same mailbox
    being changed from Outlook at the same time) are hard to get right.
  - The schema needs migrations.
- **Mitigation:** the first iteration may use online-only access behind the
  same `MailStore` port, adding the cache once the core flows work. The
  server is always treated as the source of truth.
- **New mail** is found by polling, and with backends that can push
  (`MailStore::watch`: EWS streaming notifications), as soon as a folder
  changes: the app keeps a watch open on the open folder and the
  `mailboxes`, and checks them when it says they changed. A timer (`Effect::Later`)
  has the app list the newest page of the open folder again, and the
  messages changed since the last check (`MailStore::changed_messages`,
  which finds mail that arrives with an old date), and merge them into
  the index: new messages join where their date puts them, messages
  within the newest page's dates that are missing from it have gone, and
  the others take in changes made elsewhere. Each check asks for changes
  from a while before the last one began, so that the clocks of mett's
  computer and the server may differ. Changes mett made while that page
  was on its way are laid over it, as for a listing. A page that is all
  new mail lists the folder again. The `mailboxes` are checked by reading
  their counts and their unread messages that arrived or changed since
  the user last saw them (`MailStore::recent_unread`): a mailbox has new mail when one of those
  is a message the user hasn't seen, newer than what they have, which
  reading mail can't disturb, unlike comparing counts. What the user saw
  is the unread mail that reached the index while the folder was open (and
  the changes they made to it), kept by folder so that it counts for any
  path naming the folder.

### 5.6 State machine for menus

mutt's menus (index, pager, compose, attachments, folder browser) are
modelled as an `enum Screen`, kept on a navigation stack in the `App` model.
Each screen has its own keymap, its own `update` logic and its own `view`.

- **Pros:**
  - The compiler checks that every screen and transition is handled.
  - Matches mutt's model of a keymap per menu.
- **Cons:**
  - The enum and its `match` arms grow with each feature.
  - State shared by several screens (for example, the current selection)
    needs a clear owner.
- **Mitigation:** shared state lives in the `App` model. Screens hold only
  state local to them.

The sidebar is not a screen either: it sits beside every screen, as in
mutt, and its state (its entries, its highlight, whether it has the
keys, and the folders listed for it) lives in the `App`. While it has
the keys, `App::menu` is its own, `sidebar`, whatever the screen beside
it. Its entries are made again only when what they show changes (the
mailboxes are checked or the folders listed, the settings change,
another folder opens), so
drawing it formats only the rows on screen. The app gives the menus the
width the sidebar leaves (`App::width`), so that what they lay out
(the pager's wrapping, say) fits beside it.

The message line itself is `App::status`: a text and a `StatusKind` (error,
loading, searching, ...). Code that reacts to a message (the view coloring an
error, a loaded folder clearing its "Loading..."), asks for the kind, never
compares words.

What failed for want of a sign-in (a folder change, a recall, an attachment
fetch, a message that wouldn't open, moves and purges) is kept as `Retry`
values in `App::retries` (`app/retry.rs`); the sign-in window's Retry takes
them in a fixed order, so the order they failed in doesn't matter.

Questions on the message line (`To:`, `Postpone this message?`) are not
screens: they sit over the current screen, as `App::input`, and take keys
as typed rather than through the key bindings. Writing a message is a
short sequence of such questions (`app/composing.rs`), then the external
editor, then the compose screen. Files are `Effect`s too
(saving an attachment, reading one to attach, opening one in the program
the system has for it): the main loop does them and sends back a `Msg`.
The editor is an `Effect`: the main loop
hands the terminal over to it, pausing the thread that reads input, and
sends the edited text back as a `Msg`, so the app itself does no I/O.

### 5.7 Strategy pattern for pluggable behaviour

The strategy pattern is used only where several implementations are
genuinely expected:

- **Authentication flow:** OAuth2 device code, or authorization code with
  PKCE.
- **HTML rendering:** built-in HTML-to-text conversion, or an external
  command (mailcap-style).
- **Credential storage:** OS keyring, or a plain file readable only by the
  user (mode `0600`, Unix only). The file is not encrypted.
- **Implementation:** in `mett-msgraph`, the sign-in flow is the `AuthFlow`
  setting, token storage is the `TokenStore` trait (`KeyringStore`,
  `FileStore`, and `MemoryStore` for tests), and the UI side of sign-in is
  the `Prompter` trait. The TUI's prompter forwards prompts to the main
  loop, which shows them in a sign-in window over the current screen, both
  at the first sign-in and when a token can no longer be refreshed. The
  prompter is also told when a sign-in ends, and can cancel it.
- **Pros:** each choice is configurable and testable in isolation.
- **Cons:** needless indirection when only one implementation exists, so the
  pattern is not applied speculatively.

### 5.8 Rust idioms: newtypes, typestate and builders

- **Newtypes:** `MessageId`, `FolderId`, `ConversationId` and similar types
  prevent different kinds of identifiers being mixed up. They share their
  text (`Arc<str>`), so cloning an identifier, which is constant, copies nothing.
- **Typestate:** an outgoing draft moves through `Draft<Editing>` to
  `Draft<Ready>`, and only `Draft<Ready>` can be passed to `Sender`.
- **Builders:** construct the configuration and outgoing messages.
- **Pros:** mistakes are caught at compile time, at no runtime cost.
- **Cons:** more verbose. Typestate is awkward when the state is only known at
  runtime, so it is used only at a few critical boundaries.

### 5.9 Rejected: observer or event bus with shared mutable widgets

The classic GUI approach, where widgets subscribe to events and mutate
themselves, leads to `Rc<RefCell<…>>` throughout, runtime borrow panics and
tangled lifetimes in Rust. The Elm Architecture plus channels (§5.2, §5.4)
covers the same needs more cleanly.

### 5.10 Meeting invitations

Exchange delivers an invitation as a native meeting item, not as an
iCalendar attachment, so mett takes what the server says and does not parse
`.ics` files. A `Message` has an optional `Invitation` (`mett-core`): what
the message is (request, update, cancellation, or an attendee's reply), when
and where the meeting is, who organizes and attends, how it repeats
(`Recurrence`, shown as text such as "Weekly on Tue, Thu"), and how the user
answered. Each adapter fills it from its own API (EWS `MeetingRequest`,
`MeetingCancellation` and `MeetingResponse` items; Graph `eventMessage`
with its `event`), and the cache stores it with the message body. The
pager shows it among the headers. The user answers it (accept, tentative,
decline) with `MailStore::answer_invitation`: the adapter sends the answer
through the server (EWS response objects; Graph's event actions), so
Exchange updates the calendar and tells the organizer. An answer covers the
series or one occurrence; for one occurrence, EWS finds the occurrence's
calendar item in a calendar view and Graph finds the series' instance. Times of a meeting of whole days are
midnights (UTC) of its dates, so they show as dates in any time zone.

### 5.11 Several accounts

mutt has none, so mett's own design: every account is a complete,
independent unit, and tabs only choose which is shown. The `account` command
(`config`) makes a configuration for each: a clone of the one so far, with
its file's commands on top. At start-up `mett-tui` builds a mailbox, a cache
and an `App` for each (an `Opened`), as it did for the single account, and
`run` keeps a `Slot` for each: the app, its worker (§5.4), its timers, its
file threads and its `:` command thread. Everything an account's worker,
sign-in, cache, timers and files say is forwarded into one channel, tagged
with the slot, by one task per account that takes them in a fixed order (a
failed save's sign-in failure before the save's response, as before); the
loop takes in that channel and the user's input, gives input to the account
shown, and everything else to the account it is for, so the others go on in
the background. Each app is given the `Tabs` (labels and whether new mail
is waiting) before the screen is drawn; the view takes a line for them
(`view::render_tabs`). Going to another account is an `Effect::Account` the
loop carries out. Quitting waits for every account's unsaved changes, with
one deadline. One account is the case of a single slot with no tabs.

- **Pros:** the apps know nothing of each other; every account keeps its own
  mailbox type, so Office365 and IMAP can be mixed; no shared mutable state.
- **Cons:** settings are copied into each configuration when the `account`
  command runs, and `:` commands change one account's copy; the key bindings
  are read once for all, and `:bind` changes the shown account's alone.

## 6. Example flow: opening a message

1. The user presses `Enter` in the index. The index keymap resolves it to
   `Action::DisplayMessage`.
2. `update` pushes `Screen::Pager` in a loading state, and returns
   `Effect::Request(FetchMessage { id, request_id })`.
3. The main loop forwards the request to the worker.
4. The worker serves the message from the cache if it is cached.
   Otherwise it fetches it through `mett-msgraph`, stores it and replies with
   `Event::MessageFetched { request_id, message }`.
5. `update` checks that `request_id` is still the one the pager is waiting
   for, uses the `Renderer` to turn the body into terminal text, and stores
   the result in the pager's state.
6. `view` draws the pager.

## 7. Error handling

- Library crates (`mett-core`, the adapters and `mett-sqlite`) define typed
  error enums.
- The binary (`mett-tui`) adds context to errors at the top level.
- Errors from the worker reach the UI as `Event`s and are shown in mutt's
  status line. They never cause a panic.
- Authentication errors move the UI into a re-authentication flow.
- Throttling (`Error::Throttled`) is waited out: the worker tries each
  throttled call again after the wait the server asks for (Graph's
  `Retry-After`, EWS's `BackOffMilliseconds`; without one, 2 s, doubling
  each time; at most 5 minutes), up to 5 times, before its failure is the
  answer. Throttled calls weren't carried out, so this is safe for sending
  too. It tells the UI of each wait with an interim `Event::Throttled`,
  and the message line counts the longest one down, second by second.

## 8. Configuration

mett reads its own `mettrc` files, never mutt's muttrc files. They use the
muttrc format ([MUTTRC.md](MUTTRC.md)), extended
for Office365 with a `msgraph://` mailbox URL scheme and `msgraph_*`
variables. The extensions are proposed in
[CONFIG-OFFICE365.md](CONFIG-OFFICE365.md). Parsing the configuration
belongs in `mett-tui`. The values specific to Microsoft Graph are passed to
`mett-msgraph` as a typed settings struct, and the EWS endpoint to
`mett-ews` as a URL, so that the adapters never parse muttrc themselves. mett ships no Azure AD app registration: each user provides
their own through `office365_client_id`.

## 9. Testing strategy

- **Unit tests:**
  - `update` functions, driven by `Action`s and `Event`s.
  - Domain logic in `mett-core`.
- **Adapter tests:**
  - `mett-msgraph` and `mett-ews` against recorded or mocked HTTP
    responses.
  - `mett-sqlite` in front of the in-memory store, with the clock paused.
- **UI snapshot tests:** render `view` into a `ratatui` test backend and
  compare the result against stored snapshots.
- **End-to-end tests:** the whole application wired to the mock `MailStore`.

## 10. Open questions

- Scope beyond mail: should calendar and contacts be included, and when?
- Default authentication flow.
- Should IMAP/SMTP with XOAUTH2 be supported as an alternative backend?
