//! The session cookie and its hardened `Set-Cookie` values.
//!
//! `SameSite=Lax` keeps the session off cross-site posts, and every write also
//! needs a header a cross-site form cannot set. The `__Host-` prefix makes
//! browsers refuse the cookie unless it is `Secure`, has `Path=/` and names no
//! `Domain`, so a sibling subdomain cannot set or shadow it, and a public
//! image domain of its own never receives it.

pub const SESSION_COOKIE: &str = "__Host-dither-session";

pub fn set_cookie(name: &str, value: &str, max_age_secs: i64) -> String {
    format!("{name}={value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={max_age_secs}")
}

pub fn clear_cookie(name: &str) -> String {
    format!("{name}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0")
}

pub fn read_cookie<'a>(cookie_header: &'a str, name: &str) -> Option<&'a str> {
    cookie_header.split(';').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.trim() == name).then(|| v.trim())
    })
}

/// The session cookie's value from a request's headers.
pub fn session_id(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| read_cookie(h, SESSION_COOKIE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_named_cookie_from_a_header_of_several() {
        let h = "other=1; __Host-dither-session=abc123; another=2";
        assert_eq!(read_cookie(h, SESSION_COOKIE), Some("abc123"));
        assert_eq!(read_cookie(h, "absent"), None);
    }

    #[test]
    fn cookies_are_host_only_secure_and_unreadable_by_scripts() {
        let c = set_cookie(SESSION_COOKIE, "v", 60);
        for part in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
            assert!(c.contains(part), "{c}");
        }
        assert!(!c.contains("Domain"));
    }
}
