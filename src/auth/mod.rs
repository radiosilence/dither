//! Accounts of dither's own, through koan-auth (kōan's accounts and sign-in,
//! shared as a crate).
//!
//! An account is a username, a password and a role. An admin manages the
//! accounts; a user has albums of their own and sees nobody else's; a
//! readonly account signs in and sees its albums, and changes nothing. The
//! first admin is made on `/setup` with a code the server logs while it has
//! no account; everyone else is invited by an admin, and chooses their own
//! password from the invite.

pub mod cookie;
pub mod extract;
pub mod session;
