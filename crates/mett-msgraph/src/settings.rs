//! Sign-in settings: the `office365_*` (and `msgraph_*`) variables from
//! `mettrc`.
//!
//! See `docs/CONFIG-OFFICE365.md` §4. Parsing `mettrc` belongs to
//! `mett-tui`; this crate only receives the typed result.

use std::fmt;
use std::str::FromStr;

use mett_core::AccountId;
use url::Url;

/// Scopes requested when `msgraph_scopes` is not set.
pub const DEFAULT_SCOPES: &[&str] = &[
    "Mail.ReadWrite",
    "Mail.Send",
    "User.Read",
    "User.ReadBasic.All",
    "People.Read",
    "offline_access",
];

/// Scopes defined by OpenID Connect rather than by Microsoft Graph. They are
/// sent as they are, without a resource prefix.
const OIDC_SCOPES: &[&str] = &["openid", "profile", "email", "offline_access"];

/// The Microsoft cloud that hosts the tenant (`office365_cloud`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cloud {
    /// The global Microsoft cloud.
    #[default]
    Global,
    /// US Government (GCC High).
    UsGov,
    /// US Government DoD.
    UsGovDod,
    /// China, operated by 21Vianet.
    China,
}

impl Cloud {
    const ALL: [Self; 4] = [Self::Global, Self::UsGov, Self::UsGovDod, Self::China];

    /// The name used in `mettrc`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::UsGov => "usgov",
            Self::UsGovDod => "usgov-dod",
            Self::China => "china",
        }
    }

    /// The Microsoft identity platform (login) endpoint.
    pub fn login_url(self) -> Url {
        let url = match self {
            Self::Global => "https://login.microsoftonline.com/",
            Self::UsGov | Self::UsGovDod => "https://login.microsoftonline.us/",
            Self::China => "https://login.chinacloudapi.cn/",
        };
        Url::parse(url).expect("static URL is valid")
    }

    /// The Microsoft Graph resource, which also prefixes Graph scopes.
    pub fn graph_url(self) -> Url {
        let url = match self {
            Self::Global => "https://graph.microsoft.com/",
            Self::UsGov => "https://graph.microsoft.us/",
            Self::UsGovDod => "https://dod-graph.microsoft.us/",
            Self::China => "https://microsoftgraph.chinacloudapi.cn/",
        };
        Url::parse(url).expect("static URL is valid")
    }

    /// The Exchange Online resource, which hosts Exchange Web Services and
    /// prefixes their scope (`EWS.AccessAsUser.All`).
    pub fn exchange_url(self) -> Url {
        let url = match self {
            Self::Global => "https://outlook.office365.com/",
            Self::UsGov => "https://outlook.office365.us/",
            Self::UsGovDod => "https://webmail.apps.mil/",
            Self::China => "https://partner.outlook.cn/",
        };
        Url::parse(url).expect("static URL is valid")
    }
}

impl fmt::Display for Cloud {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when parsing an unknown cloud name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown Microsoft cloud: {0} (expected global, usgov, usgov-dod or china)")]
pub struct UnknownCloud(pub String);

impl FromStr for Cloud {
    type Err = UnknownCloud;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|c| c.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| UnknownCloud(s.to_owned()))
    }
}

/// How the user signs in interactively (`office365_auth_flow`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthFlow {
    /// Authorization code with PKCE, through the system browser and a
    /// loopback redirect on `redirect_port` (`0` picks a free port).
    AuthCode {
        /// `office365_redirect_port`.
        redirect_port: u16,
    },
    /// Device code, for headless or SSH sessions.
    DeviceCode,
}

impl Default for AuthFlow {
    fn default() -> Self {
        Self::AuthCode { redirect_port: 0 }
    }
}

/// How the authenticator confirms which user a sign-in was for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IdentityCheck {
    /// Microsoft Graph's `/me` says (it needs `User.Read`).
    #[default]
    Graph,
    /// The access token's `upn` claim says. For tokens that Graph doesn't
    /// take, such as those for Exchange Web Services; the token comes
    /// straight from the identity platform, so its claims can be trusted.
    AccessToken,
}

/// Everything needed to sign in to one account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthSettings {
    /// The account, as its user principal name.
    pub account: AccountId,
    /// `office365_client_id`: the user's own app registration.
    pub client_id: String,
    /// `office365_tenant`. `None` means the domain of [`AuthSettings::account`].
    pub tenant: Option<String>,
    /// `office365_cloud`.
    pub cloud: Cloud,
    /// `office365_auth_flow` and `office365_redirect_port`.
    pub flow: AuthFlow,
    /// `msgraph_scopes`, unqualified (for example `Mail.Read`).
    pub scopes: Vec<String>,
    /// How to confirm the user a sign-in was for.
    pub identity: IdentityCheck,
    /// Overrides the login endpoint of [`AuthSettings::cloud`]. Used by tests.
    pub login_url: Option<Url>,
    /// Overrides the Graph endpoint of [`AuthSettings::cloud`]. Used by tests.
    pub graph_url: Option<Url>,
}

impl AuthSettings {
    /// Settings with the documented defaults for `account` and `client_id`.
    pub fn new(account: AccountId, client_id: impl Into<String>) -> Self {
        Self {
            account,
            client_id: client_id.into(),
            tenant: None,
            cloud: Cloud::default(),
            flow: AuthFlow::default(),
            scopes: DEFAULT_SCOPES.iter().map(|s| (*s).to_owned()).collect(),
            identity: IdentityCheck::default(),
            login_url: None,
            graph_url: None,
        }
    }

    /// The tenant to sign in through: `office365_tenant` if set, otherwise the
    /// domain of the account (`alice@contoso.com` → `contoso.com`).
    pub fn tenant(&self) -> &str {
        match &self.tenant {
            Some(tenant) => tenant,
            None => self
                .account
                .as_str()
                .rsplit_once('@')
                .map_or("organizations", |(_, domain)| domain),
        }
    }

    /// The URL of an OAuth2 v2.0 endpoint, such as `token`.
    pub(crate) fn endpoint(&self, name: &str) -> Url {
        let base = self
            .login_url
            .clone()
            .unwrap_or_else(|| self.cloud.login_url());
        with_segments(base, &[self.tenant(), "oauth2", "v2.0", name])
    }

    /// The URL of a Graph v1.0 API path, such as `me`.
    pub(crate) fn graph_api(&self, path: &str) -> Url {
        let base = self
            .graph_url
            .clone()
            .unwrap_or_else(|| self.cloud.graph_url());
        with_segments(base, &["v1.0", path])
    }

    /// The `scope` parameter: Graph scopes are qualified with the cloud's
    /// Graph resource, so that national clouds get the right audience.
    pub(crate) fn scope_param(&self) -> String {
        let graph = self.cloud.graph_url();
        self.scopes
            .iter()
            .map(|scope| {
                if scope.contains('/') || OIDC_SCOPES.contains(&scope.as_str()) {
                    scope.clone()
                } else {
                    format!("{graph}{scope}")
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// `url` with `segments` added to its path (escaped, so IDs containing `/`
/// or `+` are safe).
pub(crate) fn with_segments(mut url: Url, segments: &[&str]) -> Url {
    url.path_segments_mut()
        .expect("HTTP URLs can be a base")
        .pop_if_empty()
        .extend(segments);
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> AuthSettings {
        AuthSettings::new("alice@contoso.com".into(), "client")
    }

    #[test]
    fn tenant_defaults_to_account_domain() {
        assert_eq!(settings().tenant(), "contoso.com");
        let explicit = AuthSettings {
            tenant: Some("organizations".into()),
            ..settings()
        };
        assert_eq!(explicit.tenant(), "organizations");
    }

    #[test]
    fn endpoints_use_tenant_and_cloud() {
        assert_eq!(
            settings().endpoint("token").as_str(),
            "https://login.microsoftonline.com/contoso.com/oauth2/v2.0/token"
        );
        let us = AuthSettings {
            cloud: Cloud::UsGov,
            ..settings()
        };
        assert_eq!(
            us.endpoint("devicecode").as_str(),
            "https://login.microsoftonline.us/contoso.com/oauth2/v2.0/devicecode"
        );
    }

    #[test]
    fn endpoints_honour_login_override_with_path() {
        let s = AuthSettings {
            login_url: Some(Url::parse("http://127.0.0.1:9/mock/").unwrap()),
            ..settings()
        };
        assert_eq!(
            s.endpoint("token").as_str(),
            "http://127.0.0.1:9/mock/contoso.com/oauth2/v2.0/token"
        );
    }

    #[test]
    fn graph_scopes_are_qualified_per_cloud() {
        let s = AuthSettings {
            scopes: vec!["Mail.Read".into(), "offline_access".into()],
            cloud: Cloud::China,
            ..settings()
        };
        assert_eq!(
            s.scope_param(),
            "https://microsoftgraph.chinacloudapi.cn/Mail.Read offline_access"
        );
        assert_eq!(
            s.graph_api("me").as_str(),
            "https://microsoftgraph.chinacloudapi.cn/v1.0/me"
        );
    }

    #[test]
    fn qualified_scopes_are_kept() {
        let s = AuthSettings {
            scopes: vec![
                format!("{}EWS.AccessAsUser.All", Cloud::Global.exchange_url()),
                "offline_access".into(),
            ],
            ..settings()
        };
        assert_eq!(
            s.scope_param(),
            "https://outlook.office365.com/EWS.AccessAsUser.All offline_access"
        );
    }

    #[test]
    fn clouds_parse_from_mettrc_names() {
        assert_eq!("usgov-dod".parse(), Ok(Cloud::UsGovDod));
        assert_eq!("Global".parse(), Ok(Cloud::Global));
        assert!("mars".parse::<Cloud>().is_err());
    }
}
