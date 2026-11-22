//! Server-side sessions and login flows, keyed by opaque ids.
//!
//! Cookies carry an id and nothing else: no JWT, nothing forgeable, nothing
//! claim-bearing on the client. Sessions are rows keyed by the id's SHA-256,
//! so a deploy signs no one out and the table alone signs no one in. A login
//! flow lasts minutes and is used once, so it stays in memory: a restart
//! mid-login costs one retry.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use parking_lot::Mutex;
use rand::Rng as _;
use sha2::{Digest, Sha256};

use crate::db::Db;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// The identity provider's subject: whose albums these are.
    pub sub: String,
}

/// A login round-trip in progress: the PKCE verifier and the CSRF state, held
/// server-side so neither travels through the browser.
#[derive(Clone, Debug)]
pub struct Flow {
    pub verifier: String,
    pub csrf: String,
    /// The local path to return to once signed in.
    pub next: String,
    pub expires: Instant,
}

#[derive(Clone)]
pub struct Sessions {
    db: Db,
    flows: Arc<Mutex<HashMap<String, Flow>>>,
}

fn hash(id: &str) -> Vec<u8> {
    Sha256::digest(id.as_bytes()).to_vec()
}

impl Sessions {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            flows: Arc::default(),
        }
    }

    /// A URL-safe opaque id with 128 bits behind it.
    pub fn token() -> String {
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        URL_SAFE_NO_PAD.encode(bytes)
    }

    pub async fn create(&self, sub: &str, ttl: Duration) -> crate::db::Result<String> {
        let id = Self::token();
        self.db
            .create_session(hash(&id), sub, crate::db::now() + ttl.as_millis() as i64)
            .await?;
        Ok(id)
    }

    /// `None` for an unknown or expired id, and when the database cannot be
    /// asked: a sign-in page is the safe answer to not knowing.
    pub async fn get(&self, id: &str) -> Option<Session> {
        self.db
            .session(hash(id))
            .await
            .inspect_err(|e| tracing::warn!(error = %e, "session lookup failed"))
            .ok()
            .flatten()
            .map(|sub| Session { sub })
    }

    pub async fn delete(&self, id: &str) {
        if let Err(e) = self.db.delete_session(hash(id)).await {
            tracing::warn!(error = %e, "session delete failed");
        }
    }

    pub fn begin_flow(&self, verifier: &str, csrf: &str, next: &str, ttl: Duration) -> String {
        let id = Self::token();
        let flow = Flow {
            verifier: verifier.to_string(),
            csrf: csrf.to_string(),
            next: next.to_string(),
            expires: Instant::now() + ttl,
        };
        // Anyone can start a login and most never finish one: expired flows
        // go, and past a cap the oldest make room.
        const MAX_FLOWS: usize = 1024;
        let mut flows = self.flows.lock();
        let now = Instant::now();
        flows.retain(|_, f| f.expires > now);
        while flows.len() >= MAX_FLOWS {
            let Some(oldest) = flows
                .iter()
                .min_by_key(|(_, f)| f.expires)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            flows.remove(&oldest);
        }
        flows.insert(id.clone(), flow);
        id
    }

    /// One-shot: taking a flow consumes it, so a callback cannot be replayed.
    pub fn take_flow(&self, id: &str) -> Option<Flow> {
        let flow = self.flows.lock().remove(id)?;
        (Instant::now() < flow.expires).then_some(flow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn take_flow_returns_once_then_none() {
        let sessions = Sessions::new(Db::temp().await);
        let id = sessions.begin_flow("verifier", "csrf", "/", Duration::from_secs(60));
        assert!(sessions.take_flow(&id).is_some());
        assert!(sessions.take_flow(&id).is_none());
    }

    #[tokio::test]
    async fn an_expired_flow_is_not_taken() {
        let sessions = Sessions::new(Db::temp().await);
        let id = sessions.begin_flow("verifier", "csrf", "/", Duration::ZERO);
        assert!(sessions.take_flow(&id).is_none());
    }

    #[tokio::test]
    async fn a_created_session_is_found_and_a_deleted_one_is_not() {
        let sessions = Sessions::new(Db::temp().await);
        let id = sessions
            .create("someone", Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(sessions.get(&id).await.unwrap().sub, "someone");
        assert!(sessions.get("not-a-session").await.is_none());
        sessions.delete(&id).await;
        assert!(sessions.get(&id).await.is_none());
    }

    #[test]
    fn two_tokens_differ() {
        assert_ne!(Sessions::token(), Sessions::token());
    }
}
