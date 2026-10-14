//! Where refresh tokens are kept between runs (`msgraph_token_store`).
//!
//! Only the refresh token is stored. Access tokens are short-lived and are
//! obtained from it on start-up.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mett_core::AccountId;
use serde::{Deserialize, Serialize};

/// The service name refresh tokens are stored under in the OS keyring.
const KEYRING_SERVICE: &str = "mett";

/// Errors from a [`TokenStore`].
#[derive(Debug, thiserror::Error)]
pub enum TokenStoreError {
    /// The OS keyring failed.
    #[error("keyring: {0}")]
    Keyring(#[from] keyring::Error),
    /// Reading or writing the token file failed.
    #[error("token file {path}: {source}")]
    File {
        /// The token file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
}

impl From<TokenStoreError> for mett_core::Error {
    fn from(e: TokenStoreError) -> Self {
        Self::Auth(format!("cannot access stored credentials: {e}"))
    }
}

/// Persistent storage for refresh tokens, one per account.
///
/// The methods are blocking; callers on an async runtime should run them
/// with `spawn_blocking`.
pub trait TokenStore: Send + Sync + 'static {
    /// Returns the stored refresh token, if any.
    fn load(&self, account: &AccountId) -> Result<Option<String>, TokenStoreError>;
    /// Stores a refresh token, replacing any previous one.
    fn save(&self, account: &AccountId, refresh_token: &str) -> Result<(), TokenStoreError>;
    /// Removes the stored refresh token, if any.
    fn clear(&self, account: &AccountId) -> Result<(), TokenStoreError>;
}

/// Stores refresh tokens in the OS keyring (`msgraph_token_store = keyring`).
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringStore;

impl KeyringStore {
    fn entry(account: &AccountId) -> Result<keyring::Entry, TokenStoreError> {
        Ok(keyring::Entry::new(KEYRING_SERVICE, account.as_str())?)
    }
}

impl TokenStore for KeyringStore {
    fn load(&self, account: &AccountId) -> Result<Option<String>, TokenStoreError> {
        match Self::entry(account)?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, account: &AccountId, refresh_token: &str) -> Result<(), TokenStoreError> {
        Ok(Self::entry(account)?.set_password(refresh_token)?)
    }

    fn clear(&self, account: &AccountId) -> Result<(), TokenStoreError> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Stores one account's refresh token in a file readable only by the user
/// (`msgraph_token_store = file`).
#[derive(Debug, Clone)]
pub struct FileStore {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct TokenFile {
    refresh_token: String,
}

impl FileStore {
    /// A store backed by `path` (`msgraph_token_file`).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The default `msgraph_token_file` for `account`:
    /// `$XDG_STATE_HOME/mett/<account>.tokens`, where `XDG_STATE_HOME`
    /// defaults to `~/.local/state`. Returns `None` if neither
    /// `XDG_STATE_HOME` nor `HOME` is set.
    pub fn default_path(account: &AccountId) -> Option<PathBuf> {
        let state = std::env::var_os("XDG_STATE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|home| Path::new(&home).join(".local/state"))
            })?;
        Some(state.join("mett").join(format!("{account}.tokens")))
    }

    fn error(&self, source: io::Error) -> TokenStoreError {
        TokenStoreError::File {
            path: self.path.clone(),
            source,
        }
    }
}

impl TokenStore for FileStore {
    fn load(&self, _account: &AccountId) -> Result<Option<String>, TokenStoreError> {
        let data = match fs::read(&self.path) {
            Ok(data) => data,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(self.error(e)),
        };
        let file: TokenFile = serde_json::from_slice(&data)
            .map_err(|e| self.error(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        Ok(Some(file.refresh_token))
    }

    fn save(&self, _account: &AccountId, refresh_token: &str) -> Result<(), TokenStoreError> {
        let json = serde_json::to_vec(&TokenFile {
            refresh_token: refresh_token.to_owned(),
        })
        .map_err(|e| self.error(io::Error::other(e)))?;
        write_private(&self.path, &json).map_err(|e| self.error(e))
    }

    fn clear(&self, _account: &AccountId) -> Result<(), TokenStoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(self.error(e)),
        }
    }
}

/// Atomically replaces `path` with `data`, creating it (and its directory)
/// readable only by the user.
fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    // A new, uniquely named file: `create_new` guarantees that the 0600 mode
    // applies, which reusing an existing file would not.
    let random = crate::pkce::random_token().map_err(io::Error::other)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.{random}.tmp"));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);

    let result = options.open(&tmp).and_then(|mut file| {
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    });
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Keeps refresh tokens in memory only. For tests.
#[derive(Debug, Default)]
pub struct MemoryStore {
    tokens: Mutex<HashMap<AccountId, String>>,
}

impl MemoryStore {
    fn tokens(&self) -> std::sync::MutexGuard<'_, HashMap<AccountId, String>> {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl TokenStore for MemoryStore {
    fn load(&self, account: &AccountId) -> Result<Option<String>, TokenStoreError> {
        Ok(self.tokens().get(account).cloned())
    }

    fn save(&self, account: &AccountId, refresh_token: &str) -> Result<(), TokenStoreError> {
        self.tokens()
            .insert(account.clone(), refresh_token.to_owned());
        Ok(())
    }

    fn clear(&self, account: &AccountId) -> Result<(), TokenStoreError> {
        self.tokens().remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mett-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn file_store_round_trips_privately() {
        let dir = temp_dir("filestore");
        let store = FileStore::new(dir.join("sub/alice.tokens"));
        let account = AccountId::new("alice@contoso.com");

        assert_eq!(store.load(&account).unwrap(), None);
        store.save(&account, "r1").unwrap();
        store.save(&account, "r2").unwrap();
        assert_eq!(store.load(&account).unwrap().as_deref(), Some("r2"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join("sub/alice.tokens"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        store.clear(&account).unwrap();
        store.clear(&account).unwrap();
        assert_eq!(store.load(&account).unwrap(), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn file_store_ignores_stale_world_readable_temp_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("stale-tmp");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("alice.tokens");
        // Leftovers from an older version or another program.
        for stale in [path.with_extension("tmp"), dir.join(".alice.tokens.tmp")] {
            fs::write(&stale, "old").unwrap();
            fs::set_permissions(&stale, fs::Permissions::from_mode(0o644)).unwrap();
        }

        let account = AccountId::new("alice@contoso.com");
        FileStore::new(&path).save(&account, "r1").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn default_path_is_under_the_state_dir() {
        let path = FileStore::default_path(&"alice@contoso.com".into()).unwrap();
        assert!(path.ends_with("mett/alice@contoso.com.tokens"));
    }
}
