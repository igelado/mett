# mett-ews

The Exchange Web Services (EWS) adapter for mett. It implements
`mett-core`'s ports against EWS, the SOAP API of Exchange Online and
Exchange Server, as an alternative to Microsoft Graph (`mett-msgraph`):

- **`EwsMailStore`** implements the `MailStore` port on the signed-in
  user's mailbox. It lists folders (the mail folders of the whole tree in
  one deep `FindFolder`, or those below one), lists messages (newest first,
  a page at a time by offset, with `FindItem` for the order and `GetItem`
  for the fields), fetches messages (with Exchange converting HTML bodies to
  text), updates read and flagged state, moves, copies and deletes
  messages, and fetches attachments. It also implements `MailSender`: new
  mail without attachments goes out with one `CreateItem`, while replies,
  forwards and messages with attachments are created as drafts
  (`ReplyToItem`, `ForwardItem`), filled in, given their attachments
  (`CreateAttachment`) and sent with `SendItem`, so that Outlook threads
  them, forwards keep their attachments, and attachments are on a saved
  message, as Exchange requires. Postponed messages are saved in Drafts;
  a recalled one is updated in place and sent as itself, and is never
  deleted when an attempt fails. A draft whose send has no clear outcome is
  kept for the next attempt, as with Graph.
  The folder roles are looked up by each full folder listing, and other
  calls reuse them for 15 minutes.

It has no sign-in of its own: it takes any `Authenticator`. mett gives it
`mett-msgraph`'s, asking for the Exchange Online scope
`EWS.AccessAsUser.All`. The configuration (`use_ews`, `ews_url`) is
described in [docs/CONFIG-OFFICE365.md](../../docs/CONFIG-OFFICE365.md)
§4.4.

Microsoft is retiring EWS in Exchange Online, starting in October 2026.
Check that your tenant still allows it before relying on this adapter.

## Automated tests

```sh
cargo test -p mett-ews
```

The tests run every mailbox operation against a mock EWS endpoint
([wiremock](https://crates.io/crates/wiremock)). They need no network access
and no Microsoft account.

## Testing with a real account

1. Use the app registration from
   [mett-msgraph's README](../mett-msgraph/README.md#1-register-an-app), and
   under **API permissions** add the *delegated* permission
   **Office 365 Exchange Online** › `EWS.AccessAsUser.All` (it is listed
   under *APIs my organization uses*). An administrator usually has to grant
   consent for it.
2. Turn EWS on in `~/.config/mett/mettrc`:

   ```muttrc
   set use_ews
   ```

3. Run mett and sign in as usual:

   ```sh
   cargo run -p mett-tui
   ```

   The Inbox listing checks mailbox access. Check that `c` lists the
   folders, that `<query>` finds people, and that a reply to yourself
   arrives threaded under the original.

If sign-in succeeds but every request fails with `ErrorAccessDenied` or
HTTP 401/403, the tenant has probably turned EWS off for the account or the
app.
