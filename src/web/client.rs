//! Who a request is from and what it reached: the host it named, the
//! client's address behind the proxies, and the link preview fetcher its user
//! agent names.

use axum::http::{HeaderMap, header};

use crate::config::Config;

/// The host a request is for, with its port when it names one, as the
/// configured URLs do.
pub fn host(headers: &HeaderMap) -> &str {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

/// Whether a request reached the public pages' own domain.
pub fn on_public_host(config: &Config, headers: &HeaderMap) -> bool {
    config.split_hosts() && host(headers).eq_ignore_ascii_case(config.public_host())
}

/// Link previews' fetchers: what their user agents carry, and their names.
const FETCHERS: [(&str, &str); 10] = [
    ("WhatsApp", "WhatsApp"),
    ("facebookexternalhit", "Facebook"),
    ("Twitterbot", "X"),
    ("Slackbot", "Slack"),
    ("Discordbot", "Discord"),
    ("TelegramBot", "Telegram"),
    ("LinkedInBot", "LinkedIn"),
    ("redditbot", "Reddit"),
    ("Iframely", "Iframely"),
    ("Embedly", "Embedly"),
];

/// The name of the link preview's fetcher a user agent belongs to.
pub fn fetcher(agent: &str) -> Option<&'static str> {
    FETCHERS
        .iter()
        .find(|(token, _)| agent.contains(token))
        .map(|(_, name)| *name)
}

/// The client's address as `X-Forwarded-For` gives it: the last entry outside
/// the trusted proxies' ranges. Traefik appends the address it saw, and a CDN
/// in front (trusted) appends the client's before that, so whatever a client
/// sends sits to the left of both and is never chosen. Where the last entry is
/// a trusted proxy's, the address header it set comes first. `None` for a
/// request that came with no proxy.
pub fn forwarded(config: &Config, h: &HeaderMap) -> Option<String> {
    let trusted = |a: &str| {
        a.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| config.trusted_proxies.iter().any(|net| net.contains(&ip)))
    };
    let hops: Vec<&str> = h
        .get_all("x-forwarded-for")
        .iter()
        .rev()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.rsplit(','))
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .collect();
    let given = match (&config.address_header, hops.first()) {
        (Some(name), Some(last)) if trusted(last) => h
            .get_all(name)
            .iter()
            .next_back()
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|a| a.parse::<std::net::IpAddr>().is_ok()),
        _ => None,
    };
    given
        .or_else(|| hops.iter().copied().find(|a| !trusted(a)))
        .map(str::to_string)
}

/// The address a request came from, for limits on what one address may try.
/// What the proxies say ([`forwarded`]) is believed only from a peer that can
/// be a proxy in front of dither: a private or loopback address (an ingress
/// in the cluster, a tunnel on the host) or a trusted proxy's. Anyone else
/// could name any address in the header, and take a fresh allowance, or an
/// account's own network, with every try. Unspecified with no peer, as in a
/// test, which then shares one allowance.
pub fn client_ip(
    config: &Config,
    h: &HeaderMap,
    extensions: &axum::http::Extensions,
) -> std::net::IpAddr {
    let peer = extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip().to_canonical());
    let proxy = peer.is_some_and(|ip| {
        let internal = match ip {
            std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_loopback(),
            std::net::IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
        };
        internal || config.trusted_proxies.iter().any(|net| net.contains(&ip))
    });
    proxy
        .then(|| forwarded(config, h))
        .flatten()
        .and_then(|a| a.parse::<std::net::IpAddr>().ok())
        .or(peer)
        .map_or(
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            |ip| ip.to_canonical(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_proxys_address_is_believed_only_from_a_peer_that_can_be_one() {
        let mut config = Config::test(std::path::PathBuf::new());
        config.trusted_proxies = vec!["198.51.100.0/24".parse().unwrap()];
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "192.0.2.7".parse().unwrap());
        let from = |peer: Option<&str>| {
            let mut ext = axum::http::Extensions::new();
            if let Some(peer) = peer {
                ext.insert(axum::extract::ConnectInfo(std::net::SocketAddr::new(
                    peer.parse().unwrap(),
                    443,
                )));
            }
            client_ip(&config, &h, &ext).to_string()
        };
        // A client reaching dither directly names whatever it likes.
        assert_eq!(from(Some("203.0.113.9")), "203.0.113.9");
        // An ingress in the cluster, a tunnel on the host, a trusted proxy.
        assert_eq!(from(Some("10.42.0.7")), "192.0.2.7");
        assert_eq!(from(Some("127.0.0.1")), "192.0.2.7");
        assert_eq!(from(Some("198.51.100.5")), "192.0.2.7");
        assert_eq!(from(None), "0.0.0.0");
    }
}
