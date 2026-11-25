//! Sessions. The cookie carries an opaque secret and nothing else; the
//! database keeps its SHA-256 (koan-auth's sessions), so a deploy signs no one
//! out and the table alone signs no one in. A session lasts thirty days and
//! ends sooner with signing out, a new password or the account's deletion,
//! and the account's role is read with it on every request.

use std::time::Duration;

use koan_auth::Role;
use koan_auth::users::UserRow;

/// How long a sign-in lasts.
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 3600);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub user_id: i64,
    /// The account's name: whose albums these are.
    pub username: String,
    pub role: Role,
}

impl From<UserRow> for Session {
    fn from(user: UserRow) -> Self {
        Self {
            user_id: user.id,
            username: user.username,
            role: user.role,
        }
    }
}

impl Session {
    /// Whether the account may change anything: a readonly one may not.
    pub fn writes(&self) -> bool {
        self.role.has_permission(Role::User)
    }

    pub fn admin(&self) -> bool {
        self.role == Role::Admin
    }
}
