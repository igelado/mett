//! Signs in to Microsoft Graph and prints who you are.
//!
//! Use it to check an app registration before mett can read mail:
//!
//! ```sh
//! METT_ACCOUNT=alice@contoso.com METT_CLIENT_ID=<app-id> \
//!     cargo run -p mett-msgraph --example signin
//! ```
//!
//! Optional variables:
//! - `METT_TENANT`: overrides the tenant (default: the account's domain).
//! - `METT_FLOW=device`: uses the device code flow instead of the browser.
//! - `METT_TOKEN_FILE`: stores the refresh token in this file instead of the
//!   OS keyring.

use std::env;
use std::error::Error;
use std::sync::Arc;

use mett_core::Authenticator;
use mett_msgraph::{
    AuthFlow, AuthSettings, FileStore, GraphAuthenticator, KeyringStore, Prompter, SignInPrompt,
    TokenStore,
};

fn prompt(prompt: SignInPrompt) {
    match prompt {
        SignInPrompt::OpenBrowser { url } => {
            eprintln!("Opening your browser to sign in. If it doesn't open, visit:\n  {url}");
            if let Err(e) = open::that_detached(url.as_str()) {
                eprintln!("(could not open a browser: {e})");
            }
        }
        SignInPrompt::EnterCode { message, .. } => eprintln!("{message}"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let account = env::var("METT_ACCOUNT").map_err(|_| "set METT_ACCOUNT")?;
    let client_id = env::var("METT_CLIENT_ID").map_err(|_| "set METT_CLIENT_ID")?;
    let mut settings = AuthSettings::new(account.into(), client_id);
    settings.tenant = env::var("METT_TENANT").ok();
    if env::var("METT_FLOW").is_ok_and(|f| f == "device") {
        settings.flow = AuthFlow::DeviceCode;
    }

    match env::var_os("METT_TOKEN_FILE") {
        Some(path) => run(settings, FileStore::new(path), prompt).await,
        None => run(settings, KeyringStore, prompt).await,
    }
}

async fn run(
    settings: AuthSettings,
    store: impl TokenStore,
    prompter: impl Prompter,
) -> Result<(), Box<dyn Error>> {
    let me = settings.cloud.graph_url().join("v1.0/me")?;
    let auth = GraphAuthenticator::new(settings, reqwest::Client::new(), Arc::new(store), prompter);
    let token = auth.access_token().await?;
    eprintln!(
        "Signed in; the access token expires at {}.",
        token.expires_at()
    );

    let response = reqwest::Client::new()
        .get(me)
        .bearer_auth(token.secret())
        .send()
        .await?;
    let status = response.status();
    let body: serde_json::Value = response.json().await?;
    if !status.is_success() {
        return Err(format!("GET /me failed with {status}: {body}").into());
    }
    println!(
        "Hello, {} <{}>",
        body["displayName"].as_str().unwrap_or("?"),
        body["mail"]
            .as_str()
            .or(body["userPrincipalName"].as_str())
            .unwrap_or("?")
    );
    Ok(())
}
