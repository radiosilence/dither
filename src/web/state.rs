//! What every handler shares.

use std::sync::Arc;

use crate::config::Config;
use crate::db::Db;
use crate::store::Store;
use crate::upload::Uploads;

pub struct Inner {
    pub config: Config,
    pub db: Db,
    pub store: Store,
    /// Every password checked goes through this, with its ceiling on argon2
    /// and its budget of failures per account.
    pub verifier: koan_auth::password::Verifier,
    /// Posts to the sign-in pages, per address.
    pub sign_ins: koan_auth::limit::RateLimiter,
    /// Tries at changing a password, per account, whatever their outcome:
    /// each is two argon2 runs, one under the write lock.
    pub password_changes: koan_auth::limit::FailureLimiter<i64>,
    /// The code `/setup` asks for, logged at start while there is no
    /// account; `None` once there was one then.
    pub setup: Option<koan_auth::setup::SetupCode>,
    pub uploads: Arc<Uploads>,
    /// What the public routes served, counted anonymously.
    pub views: crate::views::Views,
    /// Becomes true when the server begins to stop: live streams end on it.
    pub stopping: tokio::sync::watch::Receiver<bool>,
}

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

impl AppState {
    pub fn new(inner: Inner) -> Self {
        Self(Arc::new(inner))
    }
}

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}
