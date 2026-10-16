//! Connecting to an Office365 account from the configuration: building the
//! sign-in settings and token store from the `msgraph_*` variables
//! (`docs/CONFIG-OFFICE365.md` §4).

use std::sync::Arc;

use mett_msgraph::{
    AuthFlow, AuthSettings, FileStore, GraphAuthenticator, GraphMailStore, KeyringStore,
    TokenStore, TokenStoreError,
};
use url::Url;

use crate::config::mailbox::MailboxUrl;
use crate::config::{Config, Value};
use crate::signin::TuiPrompter;

/// An Office365 mailbox. It signs in when first used, prompting through
/// the TUI's sign-in window.
pub type Mailbox = GraphMailStore<GraphAuthenticator<AnyStore, TuiPrompter>>;

/// The token store chosen by `msgraph_token_store`.
pub enum AnyStore {
    /// The OS keyring.
    Keyring(KeyringStore),
    /// A file (`msgraph_token_file`).
    File(FileStore),
}

impl AnyStore {
    /// The store chosen.
    fn inner(&self) -> &dyn TokenStore {
        match self {
            Self::Keyring(s) => s,
            Self::File(s) => s,
        }
    }
}

impl TokenStore for AnyStore {
    fn load(&self, account: &mett_core::AccountId) -> Result<Option<String>, TokenStoreError> {
        self.inner().load(account)
    }

    fn save(&self, account: &mett_core::AccountId, token: &str) -> Result<(), TokenStoreError> {
        self.inner().save(account, token)
    }

    fn clear(&self, account: &mett_core::AccountId) -> Result<(), TokenStoreError> {
        self.inner().clear(account)
    }

    fn clear_if(
        &self,
        account: &mett_core::AccountId,
        token: &str,
    ) -> Result<Option<String>, TokenStoreError> {
        self.inner().clear_if(account, token)
    }
}

/// The sign-in settings for the account in `url`.
pub fn auth_settings(config: &Config, url: &MailboxUrl) -> Result<AuthSettings, String> {
    let client_id = config.string("msgraph_client_id");
    if client_id.is_empty() {
        return Err(
            "msgraph_client_id is not set; see crates/mett-msgraph/README.md to register an app"
                .into(),
        );
    }
    let mut settings = AuthSettings::new(url.account.clone(), client_id);
    let tenant = config.string("msgraph_tenant");
    settings.tenant = (!tenant.is_empty()).then(|| tenant.to_owned());
    settings.cloud = config
        .string("msgraph_cloud")
        .parse()
        .map_err(|e| format!("msgraph_cloud: {e}"))?;
    settings.flow = match config.string("msgraph_auth_flow") {
        "auth-code" => AuthFlow::AuthCode {
            redirect_port: match config.get("msgraph_redirect_port") {
                Some(Value::Number(port)) => u16::try_from(*port)
                    .map_err(|_| format!("msgraph_redirect_port: {port} is not a port"))?,
                _ => 0,
            },
        },
        "device-code" => AuthFlow::DeviceCode,
        other => {
            return Err(format!(
                "msgraph_auth_flow: expected auth-code or device-code, got {other:?}"
            ));
        }
    };
    settings.scopes = config
        .string("msgraph_scopes")
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let endpoint = config.string("msgraph_endpoint");
    if !endpoint.is_empty() {
        let url = Url::parse(endpoint).map_err(|e| format!("msgraph_endpoint: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") || url.cannot_be_a_base() {
            return Err(format!(
                "msgraph_endpoint: {endpoint:?} is not an http(s) URL"
            ));
        }
        // The access token and mail travel to this endpoint, so it must be
        // encrypted; plain HTTP is only for local test servers.
        if url.scheme() == "http" && !is_loopback(&url) {
            return Err(format!(
                "msgraph_endpoint: {endpoint:?} must use https (http is only allowed on this machine)"
            ));
        }
        settings.graph_url = Some(url);
    }
    Ok(settings)
}

/// Whether `url` points at this machine.
fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// The token store chosen by `msgraph_token_store`.
pub fn token_store(config: &Config, url: &MailboxUrl) -> Result<AnyStore, String> {
    match config.string("msgraph_token_store") {
        "keyring" => Ok(AnyStore::Keyring(KeyringStore)),
        "file" => {
            let path = match config.string("msgraph_token_file") {
                "" => FileStore::default_path(&url.account).ok_or(
                    "msgraph_token_file is not set and there is no home directory for the default",
                )?,
                path => path.into(),
            };
            Ok(AnyStore::File(FileStore::new(path)))
        }
        other => Err(format!(
            "msgraph_token_store: expected keyring or file, got {other:?}"
        )),
    }
}

/// The mailbox of the account in `url`, from the configuration. Nothing is
/// sent until it is used: the first request signs in, prompting through
/// `prompter`.
pub fn mailbox(
    config: &Config,
    url: &MailboxUrl,
    prompter: TuiPrompter,
) -> Result<Mailbox, String> {
    if !config.string("msgraph_oauth_refresh_command").is_empty() {
        return Err("msgraph_oauth_refresh_command is not supported yet".into());
    }
    let settings = auth_settings(config, url)?;
    let store = token_store(config, url)?;
    let http = reqwest::Client::new();
    let auth = Arc::new(GraphAuthenticator::new(
        settings.clone(),
        http.clone(),
        Arc::new(store),
        prompter,
    ));
    Ok(GraphMailStore::new(http, &settings, auth))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use mett_msgraph::Cloud;

    use super::*;

    fn config(source: &str) -> Config {
        let mut config = Config::new(Some("/home/a".into()));
        config.load_str(source, Path::new("mettrc"));
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        config
    }

    fn url() -> MailboxUrl {
        MailboxUrl::parse("msgraph://alice@contoso.com/@inbox").unwrap()
    }

    #[test]
    fn builds_sign_in_settings_from_msgraph_variables() {
        let config = config(
            "set msgraph_client_id=abc msgraph_cloud=usgov msgraph_auth_flow=device-code\n\
             set msgraph_scopes='Mail.Read offline_access' msgraph_endpoint=https://proxy.example/\n",
        );
        let settings = auth_settings(&config, &url()).unwrap();
        assert_eq!(settings.account.as_str(), "alice@contoso.com");
        assert_eq!(settings.client_id, "abc");
        assert_eq!(settings.tenant(), "contoso.com");
        assert_eq!(settings.cloud, Cloud::UsGov);
        assert_eq!(settings.flow, AuthFlow::DeviceCode);
        assert_eq!(settings.scopes, ["Mail.Read", "offline_access"]);
        assert_eq!(
            settings.graph_url.unwrap().as_str(),
            "https://proxy.example/"
        );
    }

    #[test]
    fn defaults_follow_the_documentation() {
        let settings = auth_settings(&config("set msgraph_client_id=abc\n"), &url()).unwrap();
        assert_eq!(settings.cloud, Cloud::Global);
        assert_eq!(settings.flow, AuthFlow::AuthCode { redirect_port: 0 });
        assert!(settings.scopes.contains(&"Mail.ReadWrite".to_owned()));
        assert_eq!(settings.graph_url, None);
    }

    #[test]
    fn reports_bad_settings() {
        assert!(
            auth_settings(&config(""), &url())
                .unwrap_err()
                .contains("msgraph_client_id")
        );
        let bad = |s: &str| auth_settings(&config(s), &url()).unwrap_err();
        assert!(bad("set msgraph_client_id=a msgraph_cloud=mars\n").contains("msgraph_cloud"));
        assert!(bad("set msgraph_client_id=a msgraph_auth_flow=magic\n").contains("auth_flow"));
        assert!(bad("set msgraph_client_id=a msgraph_redirect_port=70000\n").contains("port"));
        assert!(
            bad("set msgraph_client_id=a msgraph_endpoint=mailto:x@example.com\n")
                .contains("not an http(s) URL")
        );
        assert!(
            bad("set msgraph_client_id=a msgraph_endpoint=http://proxy.example/\n")
                .contains("must use https")
        );
        // Plain HTTP is fine for a local test server.
        for local in [
            "http://127.0.0.1:8080/",
            "http://localhost/",
            "http://[::1]/",
        ] {
            let config = config(&format!(
                "set msgraph_client_id=a msgraph_endpoint={local}\n"
            ));
            assert!(auth_settings(&config, &url()).is_ok(), "{local}");
        }
    }

    #[test]
    fn chooses_the_token_store() {
        assert!(matches!(
            token_store(&config(""), &url()),
            Ok(AnyStore::Keyring(_))
        ));
        assert!(matches!(
            token_store(
                &config("set msgraph_token_store=file msgraph_token_file=~/t\n"),
                &url()
            ),
            Ok(AnyStore::File(_))
        ));
        assert!(token_store(&config("set msgraph_token_store=vault\n"), &url()).is_err());
    }
}
