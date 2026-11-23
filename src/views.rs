//! Anonymous counts of what the public routes serve, for an owner to see how
//! far something they shared travelled.
//!
//! Per image and per album, per UTC day: hits, views and bytes by what was
//! served, distinct visitors, the sites that linked to it (the Referer's host
//! alone) and the link previews' fetchers that asked for it. Nothing that
//! identifies a visitor is kept. A visitor is the SHA-256 of a salt, their
//! address and their user agent; the salt is random, held only here, and
//! replaced at UTC midnight, so a hash means nothing the next day and nothing
//! at all outside this process. The day's hashes stay in memory only to tell
//! a new visitor from one already counted; the database gets how many there
//! were. A restart draws a new salt, so someone seen before and after it
//! counts twice that day.
//!
//! A response is counted as its body is sent ([`Views::counting`]), so its
//! bytes are what went out, and a download abandoned part way counts what was
//! sent. Counts gather in memory and are added to the database every minute
//! in one transaction ([`Views::flush`]), and once more as the server stops.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::body::Body;
use futures_util::StreamExt as _;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::db::Db;
use crate::metrics::Served;

const DAY_MS: i64 = 86_400_000;

/// Visitors remembered in a day, across every target. Past it a day's new
/// visitors go uncounted rather than memory growing with whatever addresses
/// and user agents clients send.
const SEEN_MAX: usize = 500_000;

/// Referring hosts counted by name per target and day; the rest are counted
/// together under an empty host, which the pages call other sites.
const HOSTS_MAX: usize = 50;

/// Days since the epoch, in UTC.
pub fn today() -> i64 {
    crate::db::now().div_euclid(DAY_MS)
}

/// What a response served, set by its handler for the counting layer.
#[derive(Clone, Debug)]
pub struct Counted {
    pub album: String,
    /// `None` for an album's page.
    pub image: Option<String>,
    /// The album's owner, whose own requests are not counted.
    pub owner: String,
    pub kind: Served,
    /// Whether it is a view, as its handler sees it: a page, or a file
    /// asked for from its start. The counting layer drops a file that one
    /// of dither's own pages asked for, which the page's view counted.
    pub view: bool,
}

/// Who asked, as far as counting goes. Dropped once the response is sent.
pub struct Visit {
    pub address: String,
    pub agent: String,
    pub from: Source,
    /// The link preview's fetcher that asked, by name.
    pub fetcher: Option<&'static str>,
}

/// Where a request came from, by its `Referer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// No referrer, or one that is not a URL.
    Unknown,
    /// One of dither's own pages.
    Here,
    /// Another site, by its host alone.
    Site(String),
}

pub fn kind_name(kind: Served) -> &'static str {
    match kind {
        Served::Page => "page",
        Served::Original => "original",
        Served::Display => "display",
        Served::Thumb => "thumb",
        Served::Preview => "preview",
    }
}

/// Hits, views and bytes of one kind.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tally {
    pub hits: u64,
    pub views: u64,
    pub bytes: u64,
}

/// Counts not yet in the database, each by target and day.
#[derive(Default, Debug)]
pub struct Pending {
    /// By kind.
    pub hits: HashMap<(String, i64, &'static str), Tally>,
    /// Visitors new since the last flush.
    pub visitors: HashMap<(String, i64), u64>,
    /// By host; an empty host is the hosts past [`HOSTS_MAX`].
    pub referrers: HashMap<(String, i64, String), u64>,
    pub fetchers: HashMap<(String, i64, &'static str), u64>,
}

impl Pending {
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
            && self.visitors.is_empty()
            && self.referrers.is_empty()
            && self.fetchers.is_empty()
    }

    fn merge(&mut self, other: Pending) {
        for (k, t) in other.hits {
            let e = self.hits.entry(k).or_default();
            e.hits += t.hits;
            e.views += t.views;
            e.bytes += t.bytes;
        }
        for (k, n) in other.visitors {
            *self.visitors.entry(k).or_default() += n;
        }
        for (k, n) in other.referrers {
            *self.referrers.entry(k).or_default() += n;
        }
        for (k, n) in other.fetchers {
            *self.fetchers.entry(k).or_default() += n;
        }
    }
}

struct Inner {
    day: i64,
    salt: [u8; 32],
    /// Today's visitors of each target, as a hash of the visitor and the
    /// target.
    seen: HashSet<[u8; 16]>,
    /// Today's referring hosts by target, up to [`HOSTS_MAX`] each.
    hosts: HashMap<String, HashSet<String>>,
    pending: Pending,
}

struct Shared {
    inner: Mutex<Inner>,
    /// Held through a flush, so one that is under way finishes before the
    /// next begins: the last, as the server stops, waits for it.
    flushing: tokio::sync::Mutex<()>,
}

#[derive(Clone)]
pub struct Views(Arc<Shared>);

impl Default for Views {
    fn default() -> Self {
        Self(Arc::new(Shared {
            inner: Mutex::new(Inner {
                day: today(),
                salt: rand::random(),
                seen: HashSet::new(),
                hosts: HashMap::new(),
                pending: Pending::default(),
            }),
            flushing: tokio::sync::Mutex::new(()),
        }))
    }
}

impl Views {
    /// `body`, counted as it is sent: once it ends, or is dropped part way,
    /// the bytes that went out are recorded with the rest.
    pub fn counting(&self, body: Body, counted: Counted, visit: Visit) -> Body {
        struct Sending {
            views: Views,
            counted: Counted,
            visit: Visit,
            bytes: u64,
        }
        impl Sending {
            // A method, so the closure below takes the whole of it, and its
            // drop with it, rather than a copy of the count.
            fn sent(&mut self, n: usize) {
                self.bytes += n as u64;
            }
        }
        impl Drop for Sending {
            fn drop(&mut self) {
                self.views
                    .record_on(today(), &self.counted, &self.visit, self.bytes);
            }
        }
        let mut sending = Sending {
            views: self.clone(),
            counted,
            visit,
            bytes: 0,
        };
        Body::from_stream(body.into_data_stream().map(move |chunk| {
            if let Ok(chunk) = &chunk {
                sending.sent(chunk.len());
            }
            chunk
        }))
    }

    fn record_on(&self, day: i64, c: &Counted, v: &Visit, bytes: u64) {
        let mut guard = self.0.inner.lock();
        let s = &mut *guard;
        // Only forward: a request begun before midnight and ending after it
        // must not take the day back.
        if day > s.day {
            s.day = day;
            s.salt = rand::random();
            s.seen.clear();
            s.hosts.clear();
        }
        let day = s.day;
        let kind = kind_name(c.kind);
        let targets = std::iter::once(&c.album).chain(c.image.as_ref());
        if let Some(fetcher) = v.fetcher {
            for t in targets {
                *s.pending
                    .fetchers
                    .entry((t.clone(), day, fetcher))
                    .or_default() += 1;
                s.pending
                    .hits
                    .entry((t.clone(), day, kind))
                    .or_default()
                    .bytes += bytes;
            }
            return;
        }
        let visitor = Sha256::new()
            .chain_update(s.salt)
            .chain_update(v.address.as_bytes())
            .chain_update([0])
            .chain_update(v.agent.as_bytes());
        let view = c.view && v.from != Source::Here;
        for t in targets {
            let e = s.pending.hits.entry((t.clone(), day, kind)).or_default();
            e.hits += 1;
            e.views += u64::from(view);
            e.bytes += bytes;
            let seen: [u8; 32] = visitor
                .clone()
                .chain_update([0])
                .chain_update(t.as_bytes())
                .finalize()
                .into();
            let seen: [u8; 16] = seen[..16].try_into().expect("sixteen bytes");
            if s.seen.len() < SEEN_MAX && s.seen.insert(seen) {
                *s.pending.visitors.entry((t.clone(), day)).or_default() += 1;
            }
            if let Source::Site(host) = &v.from {
                let hosts = s.hosts.entry(t.clone()).or_default();
                let host = if hosts.contains(host) || hosts.len() < HOSTS_MAX {
                    hosts.insert(host.clone());
                    host.clone()
                } else {
                    String::new()
                };
                *s.pending
                    .referrers
                    .entry((t.clone(), day, host))
                    .or_default() += 1;
            }
        }
    }

    /// Add what has gathered to the database. What could not be written is
    /// kept for the next flush.
    pub async fn flush(&self, db: &Db) {
        let _flushing = self.0.flushing.lock().await;
        let pending = std::mem::take(&mut self.0.inner.lock().pending);
        if pending.is_empty() {
            return;
        }
        if let Err(failed) = db.add_views(pending).await {
            let (e, pending) = *failed;
            tracing::warn!(error = %e, "writing view counts failed; kept for the next try");
            if let Some(pending) = pending {
                self.0.inner.lock().pending.merge(pending);
            }
        }
    }
}

/// An image's or album's counts, as its owner sees them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub recent: Totals,
    pub all: Totals,
    /// Link previews' fetchers by name, most first.
    pub shared: Vec<(String, u64)>,
    /// The sites that linked here most; an empty host is other sites.
    pub referrers: Vec<(String, u64)>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    /// Pages, and files asked for on their own.
    pub views: u64,
    /// Distinct visitors, summed over days.
    pub visitors: u64,
    pub bytes: u64,
}

/// How many days "recent" covers, today included.
pub const RECENT_DAYS: i64 = 30;

/// A configured host without its port.
pub fn host_only(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((h, port)) if port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    }
}

/// Where a `referer` says a request came from, `own` being dither's hosts.
pub fn source(referer: &str, own: &[&str]) -> Source {
    let Some(host) = url::Url::parse(referer)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
    else {
        return Source::Unknown;
    };
    if own.iter().any(|o| o.eq_ignore_ascii_case(&host)) {
        Source::Here
    } else {
        Source::Site(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(image: Option<&str>, kind: Served) -> Counted {
        Counted {
            album: "ALBUM00000".into(),
            image: image.map(str::to_string),
            owner: "ann".into(),
            kind,
            view: kind != Served::Preview,
        }
    }

    fn visit(address: &str, agent: &str) -> Visit {
        Visit {
            address: address.into(),
            agent: agent.into(),
            from: Source::Unknown,
            fetcher: None,
        }
    }

    fn tally(hits: u64, views: u64, bytes: u64) -> Tally {
        Tally { hits, views, bytes }
    }

    #[test]
    fn the_same_visitor_is_one_a_day_and_a_new_day_starts_afresh() {
        let views = Views::default();
        let page = hit(Some("IMAGE00000"), Served::Page);
        let day = today();
        views.record_on(day, &page, &visit("192.0.2.1", "a browser"), 100);
        views.record_on(day, &page, &visit("192.0.2.1", "a browser"), 100);
        views.record_on(day, &page, &visit("192.0.2.2", "a browser"), 100);
        views.record_on(day + 1, &page, &visit("192.0.2.1", "a browser"), 100);
        // A request that began before midnight counts in the new day.
        views.record_on(day, &page, &visit("192.0.2.1", "a browser"), 100);
        let p = &views.0.inner.lock().pending;
        assert_eq!(p.visitors[&("IMAGE00000".to_string(), day)], 2);
        assert_eq!(p.visitors[&("ALBUM00000".to_string(), day)], 2);
        assert_eq!(p.visitors[&("IMAGE00000".to_string(), day + 1)], 1);
        assert_eq!(
            p.hits[&("IMAGE00000".to_string(), day, "page")],
            tally(3, 3, 300)
        );
        assert_eq!(
            p.hits[&("IMAGE00000".to_string(), day + 1, "page")],
            tally(2, 2, 200)
        );
    }

    #[test]
    fn a_fetcher_is_counted_as_itself_and_never_as_a_view() {
        let views = Views::default();
        let v = Visit {
            fetcher: Some("WhatsApp"),
            ..visit("192.0.2.1", "WhatsApp/2")
        };
        let day = today();
        views.record_on(day, &hit(Some("IMAGE00000"), Served::Preview), &v, 50);
        let p = &views.0.inner.lock().pending;
        assert!(p.visitors.is_empty());
        assert_eq!(
            p.hits[&("IMAGE00000".to_string(), day, "preview")],
            tally(0, 0, 50)
        );
        assert_eq!(p.fetchers[&("ALBUM00000".to_string(), day, "WhatsApp")], 1);
    }

    #[test]
    fn a_file_is_a_view_on_its_own_but_not_as_part_of_a_page() {
        let views = Views::default();
        let file = hit(Some("IMAGE00000"), Served::Original);
        let day = today();
        views.record_on(day, &file, &visit("192.0.2.1", "a browser"), 10);
        let embedded = Visit {
            from: Source::Here,
            ..visit("192.0.2.1", "a browser")
        };
        views.record_on(day, &file, &embedded, 10);
        let p = &views.0.inner.lock().pending;
        assert_eq!(
            p.hits[&("IMAGE00000".to_string(), day, "original")],
            tally(2, 1, 20)
        );
    }

    #[test]
    fn referring_hosts_past_the_limit_are_counted_together() {
        let views = Views::default();
        let page = hit(None, Served::Page);
        let day = today();
        for n in 0..=HOSTS_MAX {
            let v = Visit {
                from: Source::Site(format!("site{n}.example")),
                ..visit("192.0.2.1", "a browser")
            };
            views.record_on(day, &page, &v, 0);
        }
        let again = Visit {
            from: Source::Site("site0.example".into()),
            ..visit("192.0.2.1", "a browser")
        };
        views.record_on(day, &page, &again, 0);
        let p = &views.0.inner.lock().pending;
        let album = |host: &str| p.referrers[&("ALBUM00000".to_string(), day, host.to_string())];
        assert_eq!(album("site0.example"), 2);
        assert_eq!(album(""), 1);
        assert_eq!(p.referrers.len(), HOSTS_MAX + 1);
    }

    #[tokio::test]
    async fn a_body_abandoned_part_way_counts_what_was_sent() {
        let views = Views::default();
        let chunks = futures_util::stream::iter([
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"12345")),
            Ok(bytes::Bytes::from_static(b"67890")),
        ]);
        let body = views.counting(
            Body::from_stream(chunks),
            hit(Some("IMAGE00000"), Served::Original),
            visit("192.0.2.1", "a browser"),
        );
        let mut sent = body.into_data_stream();
        sent.next().await.unwrap().unwrap();
        drop(sent);
        let day = views.0.inner.lock().day;
        let p = &views.0.inner.lock().pending;
        assert_eq!(
            p.hits[&("IMAGE00000".to_string(), day, "original")],
            tally(1, 1, 5)
        );
    }

    #[test]
    fn a_referrer_is_its_host_and_never_dithers_own() {
        let own = ["app.test", "img.test"];
        assert_eq!(
            source("https://Forum.example/t/123?page=2#p4", &own),
            Source::Site("forum.example".into())
        );
        assert_eq!(source("https://img.test/a/x", &own), Source::Here);
        assert_eq!(source("not a url", &own), Source::Unknown);
    }
}
