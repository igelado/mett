# mett-msgraph

The Microsoft Graph adapter for mett. It implements `mett-core`'s ports
against Microsoft Graph. So far it provides sign-in: `GraphAuthenticator`
implements the `Authenticator` port with OAuth2 (authorization code with
PKCE, or device code), stores refresh tokens, and checks that the signed-in
user is the configured account.

The configuration it implements (`msgraph_*` variables) is described in
[docs/CONFIG-OFFICE365.md](../../docs/CONFIG-OFFICE365.md) §4.

## Automated tests

```sh
cargo test -p mett-msgraph
```

The tests run every sign-in flow against a mock identity platform and a mock
Graph ([wiremock](https://crates.io/crates/wiremock)). They need no network
access and no Microsoft account.

## Testing sign-in with a real account

A real sign-in needs an app registration of your own, because mett ships
none.

### 1. Register an app

In the [Microsoft Entra admin center](https://entra.microsoft.com), go to
**App registrations** › **New registration**:

1. **Supported account types:** *Accounts in this organizational directory
   only* (single tenant) is enough.
2. **Redirect URI:** choose the platform *Public client/native (mobile &
   desktop)* and enter `http://localhost`.
3. After creating it, under **Authentication**, set **Allow public client
   flows** to *Yes*. This is needed for the device code flow.
4. Under **API permissions**, add these *delegated* Microsoft Graph
   permissions: `Mail.ReadWrite`, `Mail.Send`, `User.Read`,
   `User.ReadBasic.All`, `People.Read` and `offline_access`. Depending on
   your tenant's policy, an administrator may need to grant consent.
5. Copy the **Application (client) ID** from the **Overview** page.

[docs/CONFIG-OFFICE365.md](../../docs/CONFIG-OFFICE365.md) explains why each
setting is needed.

### 2. Run the sign-in example

```sh
METT_ACCOUNT=you@example.com METT_CLIENT_ID=<application-id> \
    cargo run -p mett-msgraph --example signin
```

The example opens your browser to sign in. When it succeeds, it prints:

```
Signed in; the access token expires at <time>.
Hello, Your Name <you@example.com>
```

`METT_ACCOUNT` must be your user principal name (UPN). Usually that's your
e-mail address. If you sign in as a different user, the example fails with
`signed in as …, but the account is …`.

Optional variables:

| Variable | Effect |
|----------|--------|
| `METT_FLOW=device` | Use the device code flow: the example prints a code to enter at `https://microsoft.com/devicelogin`. Use this over SSH or without a browser. |
| `METT_TENANT` | Sign in through this tenant (a domain or tenant ID) instead of the account's domain. |
| `METT_CLOUD` | `usgov`, `usgov-dod` or `china` for national clouds (default `global`). |
| `METT_TOKEN_FILE` | Store the refresh token in this file instead of the OS keyring. |

### 3. Check that the refresh token is reused

Run the example a second time. It should print the greeting without asking
you to sign in, because the stored refresh token is used.

To start over, remove the stored token:

- **Keyring (the default):** delete the entry for service `mett` and your
  account. On Linux with the Secret Service:

  ```sh
  secret-tool clear service mett username you@example.com
  ```

- **File (`METT_TOKEN_FILE`):** delete the file.

### Troubleshooting

| Error | Cause and fix |
|-------|---------------|
| `AADSTS700038: … is not a valid application identifier` | `METT_CLIENT_ID` is wrong. Copy the Application (client) ID again. |
| `AADSTS50194: Application … is not configured as a multi-tenant application` | The app is single tenant, but sign-in went through `organizations` or `common`. Leave `METT_TENANT` unset, or set it to your tenant's domain or ID. |
| `AADSTS50011: The redirect URI … does not match` | Add `http://localhost` under the *Mobile and desktop applications* platform. |
| `AADSTS7000218: The request body must contain … client_assertion or client_secret` | Set **Allow public client flows** to *Yes*. |
| `AADSTS65001: The user or administrator has not consented` | Grant the API permissions, or ask an administrator to grant consent. |
| `could not confirm that the sign-in was for …` | Graph's `/me` failed. Check that the app has the `User.Read` permission. |
