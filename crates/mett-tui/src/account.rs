//! Connecting to an Office365 account from the configuration: building the
//! sign-in settings and token store from the `office365_*` variables, and the
//! mailbox, through Microsoft Graph or Exchange Web Services (`use_ews`,
//! `ews_url`) (`docs/CONFIG-OFFICE365.md` §4).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use mett_core::{
    Address, Attachment, AttachmentId, Draft, FlagsUpdate, Folder, FolderChanges, FolderId,
    MailSender, MailStore, Message, MessageId, MessagePage, Page, Result as PortResult, SyncState,
    WellKnownFolder,
};
use mett_ews::EwsMailStore;
use mett_msgraph::{
    AuthFlow, AuthSettings, FileStore, GraphAuthenticator, GraphMailStore, IdentityCheck,
    KeyringStore, TokenStore, TokenStoreError,
};
use mett_sqlite::{Cache, CachedStore, Progress};
use url::Url;

use crate::config::mailbox::MailboxUrl;
use crate::config::{Config, Value};
use crate::signin::TuiPrompter;

/// Signs in to an Office365 account, prompting through the TUI's sign-in
/// window.
type Auth = GraphAuthenticator<AnyStore, TuiPrompter>;

/// The API an account is reached through (`use_ews`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Api {
    /// Microsoft Graph.
    #[default]
    Graph,
    /// Exchange Web Services.
    Ews,
}

impl Api {
    /// The API `config` chooses.
    pub fn from_config(config: &Config) -> Self {
        if config.flag("use_ews") {
            Self::Ews
        } else {
            Self::Graph
        }
    }

    /// Whether sending `draft` goes through a server draft, which a failed
    /// attempt may leave behind.
    pub fn needs_server_draft(self, draft: &Draft) -> bool {
        match self {
            Self::Graph => mett_msgraph::needs_server_draft(draft),
            Self::Ews => mett_ews::needs_server_draft(draft),
        }
    }

    /// Whether sending `draft` keeps a copy in Sent Items whatever `copy`
    /// says: Graph always keeps one of what it sends through a server
    /// draft.
    pub fn always_copies(self, draft: &Draft) -> bool {
        match self {
            Self::Graph => mett_msgraph::needs_server_draft(draft),
            Self::Ews => false,
        }
    }
}

/// An Office365 mailbox. It signs in when first used, prompting through
/// the TUI's sign-in window.
pub enum Mailbox {
    /// Through Microsoft Graph.
    Graph(GraphMailStore<Auth>),
    /// Through Exchange Web Services.
    Ews(EwsMailStore<Auth>),
}

/// `$body`, with `$store` bound to the mailbox's store, whichever API it
/// uses.
macro_rules! either {
    ($mailbox:expr, $store:ident => $body:expr) => {
        match $mailbox {
            Mailbox::Graph($store) => $body,
            Mailbox::Ews($store) => $body,
        }
    };
}

impl MailStore for Mailbox {
    async fn own_addresses(&self) -> PortResult<Vec<String>> {
        either!(self, s => s.own_addresses().await)
    }

    async fn search_people(&self, text: &str) -> PortResult<Vec<Address>> {
        either!(self, s => s.search_people(text).await)
    }

    async fn list_folders(&self) -> PortResult<Vec<Folder>> {
        either!(self, s => s.list_folders().await)
    }

    async fn child_folders(&self, parent: Option<&FolderId>) -> PortResult<Vec<Folder>> {
        either!(self, s => s.child_folders(parent).await)
    }

    async fn folder(&self, id: &FolderId) -> PortResult<Folder> {
        either!(self, s => s.folder(id).await)
    }

    async fn recent_unread(
        &self,
        folder: &FolderId,
        since: Option<DateTime<Utc>>,
    ) -> PortResult<Vec<(MessageId, DateTime<Utc>)>> {
        either!(self, s => s.recent_unread(folder, since).await)
    }

    async fn well_known_folder(&self, which: WellKnownFolder) -> PortResult<Folder> {
        either!(self, s => s.well_known_folder(which).await)
    }

    async fn list_messages(&self, folder: &FolderId, page: Page) -> PortResult<MessagePage> {
        either!(self, s => s.list_messages(folder, page).await)
    }

    async fn changed_messages(
        &self,
        folder: &FolderId,
        since: DateTime<Utc>,
        page: Page,
    ) -> PortResult<MessagePage> {
        either!(self, s => s.changed_messages(folder, since, page).await)
    }

    async fn fetch_message(&self, id: &MessageId) -> PortResult<Message> {
        either!(self, s => s.fetch_message(id).await)
    }

    async fn message_folder(&self, id: &MessageId) -> PortResult<FolderId> {
        either!(self, s => s.message_folder(id).await)
    }

    async fn list_attachments(&self, message: &MessageId) -> PortResult<Vec<Attachment>> {
        either!(self, s => s.list_attachments(message).await)
    }

    async fn fetch_attachment(
        &self,
        message: &MessageId,
        attachment: &AttachmentId,
    ) -> PortResult<Vec<u8>> {
        either!(self, s => s.fetch_attachment(message, attachment).await)
    }

    async fn update_flags(&self, id: &MessageId, update: FlagsUpdate) -> PortResult<()> {
        either!(self, s => s.update_flags(id, update).await)
    }

    async fn move_message(&self, id: &MessageId, to: &FolderId) -> PortResult<MessageId> {
        either!(self, s => s.move_message(id, to).await)
    }

    async fn copy_message(&self, id: &MessageId, to: &FolderId) -> PortResult<MessageId> {
        either!(self, s => s.copy_message(id, to).await)
    }

    async fn delete_message(&self, id: &MessageId) -> PortResult<()> {
        either!(self, s => s.delete_message(id).await)
    }

    async fn save_draft(&self, draft: &Draft) -> PortResult<MessageId> {
        either!(self, s => s.save_draft(draft).await)
    }

    fn can_sync(&self) -> bool {
        either!(self, s => s.can_sync())
    }

    fn can_watch(&self) -> bool {
        either!(self, s => s.can_watch())
    }

    async fn watch(&self, folders: &[FolderId]) -> PortResult<Vec<FolderId>> {
        either!(self, s => s.watch(folders).await)
    }

    async fn sync_folder(
        &self,
        folder: &FolderId,
        state: Option<&SyncState>,
    ) -> PortResult<FolderChanges> {
        either!(self, s => s.sync_folder(folder, state).await)
    }
}

/// The cache file of the account in `url` (mutt's `header_cache`): by
/// default `$XDG_CACHE_HOME/mett/<account>.sqlite` (`~/.cache/mett/...`
/// when `XDG_CACHE_HOME` is unset or relative). A `header_cache` that is a
/// directory (or ends with `/`) holds a file per account; another names
/// the file. Unset, nothing is cached.
pub fn cache_path(
    config: &Config,
    url: &MailboxUrl,
    xdg_cache_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let file = format!("{}.sqlite", url.account);
    if !config.was_set("header_cache") {
        // The XDG Base Directory spec says to ignore relative values.
        let cache = xdg_cache_home
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                home.filter(|h| !h.is_empty())
                    .map(|home| PathBuf::from(home).join(".cache"))
            })?;
        return Some(cache.join("mett").join(file));
    }
    match config.string("header_cache") {
        "" => None,
        path if path.ends_with('/') || Path::new(path).is_dir() => Some(Path::new(path).join(file)),
        path => Some(PathBuf::from(path)),
    }
}

/// `mailbox` with its cache (see [`cache_path`]), which tells `progress`
/// how far its syncs have got, and a warning if the cache can't be used:
/// mett then works without it.
pub fn cached(
    config: &Config,
    url: &MailboxUrl,
    mailbox: Mailbox,
    progress: Progress,
) -> (CachedStore<Mailbox>, Option<String>) {
    let path = cache_path(
        config,
        url,
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("HOME"),
    );
    let Some(path) = path else {
        return (CachedStore::new(mailbox, None), None);
    };
    match Cache::open(&path, &url.account) {
        Ok(cache) => (
            CachedStore::new(mailbox, Some(cache)).with_progress(progress),
            None,
        ),
        Err(e) => (
            CachedStore::new(mailbox, None),
            Some(format!("header_cache: {e}; not caching")),
        ),
    }
}

impl MailSender for Mailbox {
    async fn send(&self, draft: &Draft) -> PortResult<()> {
        either!(self, s => s.send(draft).await)
    }

    async fn discard_kept(&self, draft: &Draft) -> PortResult<()> {
        either!(self, s => s.discard_kept(draft).await)
    }

    fn keeps_draft(&self, draft: &Draft) -> bool {
        either!(self, s => s.keeps_draft(draft))
    }
}

/// The token store chosen by `office365_token_store`.
pub enum AnyStore {
    /// The OS keyring.
    Keyring(KeyringStore),
    /// A file (`office365_token_file`).
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
    let client_id = config.string("office365_client_id");
    if client_id.is_empty() {
        return Err(
            "office365_client_id is not set; see crates/mett-msgraph/README.md to register an app"
                .into(),
        );
    }
    let mut settings = AuthSettings::new(url.account.clone(), client_id);
    let tenant = config.string("office365_tenant");
    settings.tenant = (!tenant.is_empty()).then(|| tenant.to_owned());
    settings.cloud = config
        .string("office365_cloud")
        .parse()
        .map_err(|e| format!("office365_cloud: {e}"))?;
    settings.flow = match config.string("office365_auth_flow") {
        "auth-code" => AuthFlow::AuthCode {
            redirect_port: match config.get("office365_redirect_port") {
                Some(Value::Number(port)) => u16::try_from(*port)
                    .map_err(|_| format!("office365_redirect_port: {port} is not a port"))?,
                _ => 0,
            },
        },
        "device-code" => AuthFlow::DeviceCode,
        other => {
            return Err(format!(
                "office365_auth_flow: expected auth-code or device-code, got {other:?}"
            ));
        }
    };
    match Api::from_config(config) {
        Api::Graph => {
            settings.scopes = config
                .string("msgraph_scopes")
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            settings.graph_url = endpoint(config, "msgraph_endpoint")?;
        }
        // EWS takes one scope, of the cloud's Exchange Online. Graph doesn't
        // take its tokens, so they tell who signed in themselves.
        Api::Ews => {
            settings.scopes = vec![
                format!("{}EWS.AccessAsUser.All", settings.cloud.exchange_url()),
                "offline_access".into(),
            ];
            settings.identity = IdentityCheck::AccessToken;
        }
    }
    Ok(settings)
}

/// The endpoint the variable `name` sets, if any. The access token and
/// mail travel to it, so it must use HTTPS; plain HTTP is only for local
/// test servers.
fn endpoint(config: &Config, name: &str) -> Result<Option<Url>, String> {
    let endpoint = config.string(name);
    if endpoint.is_empty() {
        return Ok(None);
    }
    let url = Url::parse(endpoint).map_err(|e| format!("{name}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.cannot_be_a_base() {
        return Err(format!("{name}: {endpoint:?} is not an http(s) URL"));
    }
    if url.scheme() == "http" && !is_loopback(&url) {
        return Err(format!(
            "{name}: {endpoint:?} must use https (http is only allowed on this machine)"
        ));
    }
    Ok(Some(url))
}

/// The EWS endpoint: `ews_url`, or Exchange Online's in the account's
/// cloud.
pub fn ews_url(config: &Config, settings: &AuthSettings) -> Result<Url, String> {
    Ok(endpoint(config, "ews_url")?.unwrap_or_else(|| {
        settings
            .cloud
            .exchange_url()
            .join("EWS/Exchange.asmx")
            .expect("static path is valid")
    }))
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

/// The token store chosen by `office365_token_store`.
pub fn token_store(config: &Config, url: &MailboxUrl) -> Result<AnyStore, String> {
    match config.string("office365_token_store") {
        "keyring" => Ok(AnyStore::Keyring(KeyringStore)),
        "file" => {
            let path = match config.string("office365_token_file") {
                "" => FileStore::default_path(&url.account).ok_or(
                    "office365_token_file is not set and there is no home directory for the default",
                )?,
                path => path.into(),
            };
            Ok(AnyStore::File(FileStore::new(path)))
        }
        other => Err(format!(
            "office365_token_store: expected keyring or file, got {other:?}"
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
    if !config.string("office365_oauth_refresh_command").is_empty() {
        return Err("office365_oauth_refresh_command is not supported yet".into());
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
    Ok(match Api::from_config(config) {
        Api::Graph => Mailbox::Graph(GraphMailStore::new(http, &settings, auth)),
        Api::Ews => Mailbox::Ews(EwsMailStore::new(
            http,
            ews_url(config, &settings)?,
            url.account.clone(),
            auth,
        )),
    })
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
    fn builds_sign_in_settings_from_office365_variables() {
        let config = config(
            "set office365_client_id=abc office365_cloud=usgov office365_auth_flow=device-code\n\
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
        let settings = auth_settings(&config("set office365_client_id=abc\n"), &url()).unwrap();
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
                .contains("office365_client_id")
        );
        let bad = |s: &str| auth_settings(&config(s), &url()).unwrap_err();
        assert!(
            bad("set office365_client_id=a office365_cloud=mars\n").contains("office365_cloud")
        );
        assert!(bad("set office365_client_id=a office365_auth_flow=magic\n").contains("auth_flow"));
        assert!(bad("set office365_client_id=a office365_redirect_port=70000\n").contains("port"));
        assert!(
            bad("set office365_client_id=a msgraph_endpoint=mailto:x@example.com\n")
                .contains("not an http(s) URL")
        );
        assert!(
            bad("set office365_client_id=a msgraph_endpoint=http://proxy.example/\n")
                .contains("must use https")
        );
        // Plain HTTP is fine for a local test server.
        for local in [
            "http://127.0.0.1:8080/",
            "http://localhost/",
            "http://[::1]/",
        ] {
            let config = config(&format!(
                "set office365_client_id=a msgraph_endpoint={local}\n"
            ));
            assert!(auth_settings(&config, &url()).is_ok(), "{local}");
        }
    }

    #[test]
    fn ews_asks_for_exchange_online_and_trusts_its_token() {
        let config = config("set office365_client_id=abc use_ews office365_cloud=usgov\n");
        let settings = auth_settings(&config, &url()).unwrap();
        assert_eq!(
            settings.scopes,
            [
                "https://outlook.office365.us/EWS.AccessAsUser.All",
                "offline_access"
            ]
        );
        assert_eq!(settings.identity, IdentityCheck::AccessToken);
        assert_eq!(
            ews_url(&config, &settings).unwrap().as_str(),
            "https://outlook.office365.us/EWS/Exchange.asmx"
        );
        let graph = auth_settings(&self::config("set office365_client_id=abc\n"), &url()).unwrap();
        assert_eq!(graph.identity, IdentityCheck::Graph);
    }

    #[test]
    fn ews_url_overrides_the_endpoint() {
        let settings = |s: &str| {
            let config = config(s);
            let auth = auth_settings(&config, &url()).unwrap();
            ews_url(&config, &auth)
        };
        assert_eq!(
            settings("set office365_client_id=a use_ews ews_url=https://mail.contoso.com/EWS/Exchange.asmx\n")
                .unwrap()
                .as_str(),
            "https://mail.contoso.com/EWS/Exchange.asmx"
        );
        assert!(
            settings("set office365_client_id=a ews_url=http://mail.contoso.com/\n")
                .unwrap_err()
                .contains("ews_url")
        );
        // EWS never goes to Graph's endpoint.
        assert!(
            settings("set office365_client_id=a use_ews msgraph_endpoint=http://proxy.example/\n")
                .is_ok()
        );
    }

    #[test]
    fn only_graph_always_copies() {
        let reply = Draft {
            origin: Some(mett_core::DraftOrigin::Reply(MessageId::new("m"))),
            ..Draft::default()
        };
        assert!(Api::Graph.always_copies(&reply));
        assert!(!Api::Ews.always_copies(&reply));
        assert!(Api::Ews.needs_server_draft(&reply));
        assert!(!Api::Ews.needs_server_draft(&Draft::default()));
        let attached = Draft {
            attachments: vec![mett_core::OutgoingAttachment {
                name: "a.txt".into(),
                content_type: "text/plain".into(),
                data: b"a".to_vec(),
            }],
            ..Draft::default()
        };
        assert!(Api::Ews.needs_server_draft(&attached));
        let delegated = Draft {
            from: Some(Address::new("team@contoso.com")),
            ..Draft::default()
        };
        assert!(Api::Ews.needs_server_draft(&delegated));
        assert!(!Api::Ews.needs_server_draft(&Draft {
            save_copy: true,
            ..delegated
        }));
        assert!(!Api::Ews.always_copies(&attached));
    }

    #[test]
    fn the_cache_follows_header_cache() {
        let path = |source: &str, xdg: Option<&str>| {
            cache_path(
                &config(source),
                &url(),
                xdg.map(Into::into),
                Some("/home/a".into()),
            )
        };
        let file = "alice@contoso.com.sqlite";
        // By default, in the XDG cache directory (a relative one is
        // ignored).
        assert_eq!(
            path("", None),
            Some(PathBuf::from("/home/a/.cache/mett").join(file))
        );
        assert_eq!(
            path("", Some("/xdg")),
            Some(PathBuf::from("/xdg/mett").join(file))
        );
        assert_eq!(
            path("", Some("rel")),
            Some(PathBuf::from("/home/a/.cache/mett").join(file))
        );
        // A directory holds a file per account; a file is used as it is.
        assert_eq!(
            path("set header_cache=~/hc/\n", None),
            Some(PathBuf::from("/home/a/hc").join(file))
        );
        assert_eq!(
            path("set header_cache=~/hc.db\n", None),
            Some(PathBuf::from("/home/a/hc.db"))
        );
        // Unset, nothing is cached.
        assert_eq!(path("unset header_cache\n", None), None);
    }

    #[test]
    fn chooses_the_token_store() {
        assert!(matches!(
            token_store(&config(""), &url()),
            Ok(AnyStore::Keyring(_))
        ));
        assert!(matches!(
            token_store(
                &config("set office365_token_store=file office365_token_file=~/t\n"),
                &url()
            ),
            Ok(AnyStore::File(_))
        ));
        assert!(token_store(&config("set office365_token_store=vault\n"), &url()).is_err());
    }
}
