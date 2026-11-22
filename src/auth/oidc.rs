//! The authorization-code exchange against the identity provider.
//!
//! PKCE is used even though this client holds a secret: it costs one hash and
//! removes the attack where a leaked authorization code is enough on its own.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng as _;
use sha2::{Digest, Sha256};
use url::Url;

use crate::config::Oidc;
use crate::error::{AppError, AppResult};

/// The authorization endpoint URL to send the browser to.
pub fn authorize_url(oidc: &Oidc, redirect_uri: &str, csrf: &str, challenge: &str) -> String {
    let mut url = Url::parse(&format!("{}/oauth2/auth", oidc.issuer)).expect("issuer is a URL");
    url.query_pairs_mut()
        .append_pair("client_id", &oidc.client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid profile")
        .append_pair("state", csrf)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    url.into()
}

/// A fresh PKCE verifier and its S256 challenge.
pub fn pkce() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = challenge(&verifier);
    (verifier, challenge)
}

fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
}

/// Who signed in: the subject, which owns albums, and a name to show them.
#[derive(Debug, PartialEq, Eq)]
pub struct Identity {
    pub sub: String,
    pub login: String,
}

/// Exchange the code for tokens and return who signed in.
///
/// Nothing here calls an API on the person's behalf, so no access token is
/// kept: one retained past this point would be a credential held for nothing.
pub async fn exchange_code(
    http: &reqwest::Client,
    oidc: &Oidc,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> AppResult<Identity> {
    let response = http
        .post(&oidc.token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &oidc.client_id),
            ("client_secret", &oidc.client_secret),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|e| AppError::Upstream(e.to_string()))?;
    // Never repeat the body back: it can carry token material.
    if !response.status().is_success() {
        return Err(AppError::Upstream(format!(
            "token endpoint returned {}",
            response.status()
        )));
    }
    let token: TokenResponse = response
        .json()
        .await
        .map_err(|e| AppError::Upstream(e.to_string()))?;
    // The id_token came straight from the issuer over TLS, in answer to a
    // request made here: no untrusted party is in the path, so there is
    // nothing a signature check would catch. It exists for tokens that
    // travel through the browser.
    let id_token = token.id_token.ok_or(AppError::Upstream(
        "token response carried no id_token".into(),
    ))?;
    identity(&id_token)
}

fn identity(id_token: &str) -> AppResult<Identity> {
    let payload = id_token
        .split('.')
        .nth(1)
        .ok_or(AppError::Upstream("id_token is not a JWT".into()))?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| AppError::Upstream("id_token payload is not base64".into()))?;
    let claims: serde_json::Value = serde_json::from_slice(&payload)
        .map_err(|_| AppError::Upstream("id_token payload is not JSON".into()))?;
    let claim = |k: &str| {
        claims
            .get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    };
    let sub = claim("sub").ok_or(AppError::Upstream("id_token carried no sub".into()))?;
    let login = claim("preferred_username")
        .or_else(|| claim("name"))
        .unwrap_or(sub);
    Ok(Identity {
        sub: sub.to_string(),
        login: login.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oidc() -> Oidc {
        Oidc {
            issuer: "https://auth.example.com".into(),
            client_id: "dither".into(),
            client_secret: "secret".into(),
            token_url: "https://auth.example.com/oauth2/token".into(),
        }
    }

    /// RFC 7636 appendix B.
    #[test]
    fn pkce_challenge_matches_the_rfc_example() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn pkce_verifier_is_a_valid_rfc_verifier() {
        let (verifier, _) = pkce();
        assert!((43..=128).contains(&verifier.len()));
        assert!(
            verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c))
        );
    }

    #[test]
    fn authorize_url_carries_every_parameter_and_escapes_the_redirect() {
        let url = authorize_url(&oidc(), "https://d.example/auth/callback", "csrf", "chal");
        let parsed = Url::parse(&url).unwrap();
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs["client_id"], "dither");
        assert_eq!(pairs["redirect_uri"], "https://d.example/auth/callback");
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["state"], "csrf");
        assert_eq!(pairs["code_challenge"], "chal");
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert!(url.starts_with("https://auth.example.com/oauth2/auth?"));
        assert!(url.contains("%2F"));
    }

    fn jwt(claims: serde_json::Value) -> String {
        format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    #[test]
    fn the_identity_is_the_subject_with_the_username_beside_it() {
        let id = identity(&jwt(
            serde_json::json!({"sub": "github:1", "preferred_username": "ann"}),
        ))
        .unwrap();
        assert_eq!(
            id,
            Identity {
                sub: "github:1".into(),
                login: "ann".into()
            }
        );
        let id = identity(&jwt(serde_json::json!({"sub": "github:2"}))).unwrap();
        assert_eq!(id.login, "github:2");
        assert!(identity(&jwt(serde_json::json!({"name": "x"}))).is_err());
        assert!(identity("not a jwt").is_err());
    }
}
