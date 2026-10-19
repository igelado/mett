//! SQLite cache for mett.
//!
//! [`CachedStore`] sits in front of a backend's [`mett_core::MailStore`]
//! and keeps the indexes of the folders listed, and the messages fetched,
//! in a SQLite database (mutt's `header_cache`). Folders are kept up to
//! date by syncing them ([`mett_core::MailStore::sync_folder`]), so opening
//! one asks the server only for what changed (`docs/DESIGN.md` §5.5).

mod db;
mod store;

pub use store::{Cache, CacheError, CachedStore};
