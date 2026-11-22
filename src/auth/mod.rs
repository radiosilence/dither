//! An OAuth client of the estate's identity provider, and nothing more.
//!
//! Who may sign in is decided there (a GitHub allowlist in front of Hydra), so
//! anyone holding a code from that issuer has already passed it. There is no
//! second allowlist here: a copy would be one more thing to keep in step, and
//! the one that drifted would be this one. Everyone who signs in gets albums
//! of their own and sees nobody else's.

pub mod cookie;
pub mod extract;
pub mod oidc;
pub mod routes;
pub mod session;
