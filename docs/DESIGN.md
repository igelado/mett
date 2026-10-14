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
- Offering a graphical (non-terminal) UI.
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
│  MailStore, Authenticator, Sender, Renderer…   │
├───────────────┬──────────────┬─────────────────┤
│ mett-msgraph  │ cache crate¹ │ mock (tests)    │
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
| cache crate¹   | Local cache and sync state.                                        | `mett-core`                    |
| `mett-tui`     | The binary: TUI, key bindings, configuration, worker wiring.       | `mett-core`, adapters          |

¹ Named after its storage technology once that is chosen, for example
`mett-sqlite` or `mett-maildir` (see §10).

`mett-tui` is the only crate that knows about every adapter. It wires them
together at startup, acting as the composition root.

## 5. Design patterns

### 5.1 Hexagonal architecture (ports and adapters)

`mett-core` defines the domain types and the traits (ports) that the rest of
the application depends on, for example:

- `MailStore`: list folders, list and fetch messages, move, delete and flag
  them.
- `Sender`: submit an outgoing message.
- `Authenticator`: obtain and refresh access tokens.
- `Renderer`: turn a message body (for example HTML) into terminal text.

Adapters implement these ports. `mett-msgraph` implements them against
Microsoft Graph, and a mock implementation is used in tests.

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

The UI reads mailbox data from the cache crate. A sync component in the worker
keeps the store up to date with Graph delta queries.

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

### 5.7 Strategy pattern for pluggable behaviour

The strategy pattern is used only where several implementations are
genuinely expected:

- **Authentication flow:** OAuth2 device code, or authorization code with
  PKCE.
- **HTML rendering:** built-in HTML-to-text conversion, or an external
  command (mailcap-style).
- **Credential storage:** OS keyring, or an encrypted file.
- **Pros:** each choice is configurable and testable in isolation.
- **Cons:** needless indirection when only one implementation exists, so the
  pattern is not applied speculatively.

### 5.8 Rust idioms: newtypes, typestate and builders

- **Newtypes:** `MessageId`, `FolderId`, `ConversationId` and similar types
  prevent different kinds of identifiers being mixed up.
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

- Library crates (`mett-core`, `mett-msgraph` and the cache crate) define typed
  error enums.
- The binary (`mett-tui`) adds context to errors at the top level.
- Errors from the worker reach the UI as `Event`s and are shown in mutt's
  status line. They never cause a panic.
- Authentication errors move the UI into a re-authentication flow.

## 8. Configuration

mett reads its own `mettrc` files, never mutt's muttrc files. They use the
muttrc format ([MUTTRC.md](MUTTRC.md)), extended
for Office365 with a `msgraph://` mailbox URL scheme and `msgraph_*`
variables. The extensions are proposed in
[CONFIG-OFFICE365.md](CONFIG-OFFICE365.md). Parsing the configuration
belongs in `mett-tui`. The values specific to Microsoft Graph are passed to
`mett-msgraph` as a typed settings struct, so that the adapter never parses
muttrc itself. mett ships no Azure AD app registration: each user provides
their own through `msgraph_client_id`.

## 9. Testing strategy

- **Unit tests:**
  - `update` functions, driven by `Action`s and `Event`s.
  - Domain logic in `mett-core`.
- **Adapter tests:**
  - `mett-msgraph` against recorded or mocked HTTP responses.
  - The cache crate against a temporary store.
- **UI snapshot tests:** render `view` into a `ratatui` test backend and
  compare the result against stored snapshots.
- **End-to-end tests:** the whole application wired to the mock `MailStore`.

## 10. Open questions

- Scope beyond mail: should calendar and contacts be included, and when?
- Cache backend: SQLite or Maildir? The choice also names the cache crate.
- Default authentication flow.
- Should IMAP/SMTP with XOAUTH2 be supported as an alternative backend?
