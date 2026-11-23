//! Anonymous counts of what the public routes serve, for an owner to see how
//! far something they shared travelled.
//!
//! Per image and per album, per UTC day: hits, views and bytes by what was
//! served,
//! distinct visitors, the sites that linked to it (the Referer's host alone)
//! and the link previews' fetchers that asked for it. Nothing that identifies
//! a visitor is kept. A visitor is the SHA-256 of a salt, their address and
//! their user agent; the salt is random, held only here, and replaced at UTC
//! midnight, so a hash means nothing the next day and nothing at all outside
//! this process. The day's hashes stay in memory only to tell a new visitor
//! from one already counted; the database gets how many there were. A
//! restart draws a new salt, so someone seen before and after it counts
//! twice that day.
//!
//! Counts gather in memory and are added to the database every minute in one
//! transaction ([`Views::flush`]), and once more as the server stops.

use std::collections::{HashMap, HashSet};

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::db::Db;
use crate::metrics::Served;

const DAY_MS: i64 = 86_400_000;

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
    /// What the response carries: a range's length, not the file's size.
    pub bytes: u64,
}

/// Who asked, as far as counting goes. Used once and dropped.
pub struct Visit<'a> {
    pub address: &'a str,
    pub agent: &'a str,
    pub from: Source,
    /// The link preview's fetcher that asked, by name.
    pub fetcher: Option<&'static str>,
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

/// Hits, views and bytes of one kind.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tally {
    pub hits: u64,
    pub views: u64,
    pub bytes: u64,
}

/// Counts not yet in the database, each by target and day.
#[derive(Default, Clone, Debug)]
pub struct Pending {
    /// By kind.
    pub hits: HashMap<(String, i64, &'static str), Tally>,
    /// Visitors new since the last flush.
    pub visitors: HashMap<(String, i64), u64>,
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
    /// Today's visitors, by target.
    seen: HashSet<(String, [u8; 32])>,
    pending: Pending,
}

pub struct Views(Mutex<Inner>);

impl Default for Views {
    fn default() -> Self {
        Self(Mutex::new(Inner {
            day: today(),
            salt: rand::random(),
            seen: HashSet::new(),
            pending: Pending::default(),
        }))
    }
}

impl Views {
    pub fn record(&self, c: &Counted, v: &Visit) {
        self.record_on(today(), c, v);
    }

    fn record_on(&self, day: i64, c: &Counted, v: &Visit) {
        let mut s = self.0.lock();
        if s.day != day {
            s.day = day;
            s.salt = rand::random();
            s.seen.clear();
        }
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
                    .bytes += c.bytes;
            }
            return;
        }
        let visitor: [u8; 32] = Sha256::new()
            .chain_update(s.salt)
            .chain_update(v.address.as_bytes())
            .chain_update([0])
            .chain_update(v.agent.as_bytes())
            .finalize()
            .into();
        let view = c.view && v.from != Source::Here;
        for t in targets {
            let e = s.pending.hits.entry((t.clone(), day, kind)).or_default();
            e.hits += 1;
            e.views += u64::from(view);
            e.bytes += c.bytes;
            if s.seen.insert((t.clone(), visitor)) {
                *s.pending.visitors.entry((t.clone(), day)).or_default() += 1;
            }
            if let Source::Site(host) = &v.from {
                *s.pending
                    .referrers
                    .entry((t.clone(), day, host.clone()))
                    .or_default() += 1;
            }
        }
    }

    /// Add what has gathered to the database. What could not be written is
    /// kept for the next flush.
    pub async fn flush(&self, db: &Db) {
        let pending = std::mem::take(&mut self.0.lock().pending);
        if pending.is_empty() {
            return;
        }
        if let Err(e) = db.add_views(pending.clone()).await {
            tracing::warn!(error = %e, "writing view counts failed; kept for the next try");
            self.0.lock().pending.merge(pending);
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
    /// The sites that linked here most.
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

    fn hit(image: Option<&str>, kind: Served, bytes: u64) -> Counted {
        Counted {
            album: "ALBUM00000".into(),
            image: image.map(str::to_string),
            owner: "ann".into(),
            kind,
            view: kind != Served::Preview,
            bytes,
        }
    }

    fn visit<'a>(address: &'a str, agent: &'a str) -> Visit<'a> {
        Visit {
            address,
            agent,
            from: Source::Unknown,
            fetcher: None,
        }
    }

    #[test]
    fn the_same_visitor_is_one_a_day_and_a_new_day_starts_afresh() {
        let views = Views::default();
        let page = hit(Some("IMAGE00000"), Served::Page, 100);
        views.record_on(1, &page, &visit("192.0.2.1", "a browser"));
        views.record_on(1, &page, &visit("192.0.2.1", "a browser"));
        views.record_on(1, &page, &visit("192.0.2.2", "a browser"));
        views.record_on(2, &page, &visit("192.0.2.1", "a browser"));
        let p = &views.0.lock().pending;
        assert_eq!(p.visitors[&("IMAGE00000".to_string(), 1)], 2);
        assert_eq!(p.visitors[&("ALBUM00000".to_string(), 1)], 2);
        assert_eq!(p.visitors[&("IMAGE00000".to_string(), 2)], 1);
        let tally = |hits, views, bytes| Tally { hits, views, bytes };
        assert_eq!(
            p.hits[&("IMAGE00000".to_string(), 1, "page")],
            tally(3, 3, 300)
        );
    }

    #[test]
    fn a_fetcher_is_counted_as_itself_and_never_as_a_view() {
        let views = Views::default();
        let v = Visit {
            fetcher: Some("WhatsApp"),
            ..visit("192.0.2.1", "WhatsApp/2")
        };
        views.record_on(1, &hit(Some("IMAGE00000"), Served::Preview, 50), &v);
        let p = &views.0.lock().pending;
        assert!(p.visitors.is_empty());
        assert_eq!(
            p.hits[&("IMAGE00000".to_string(), 1, "preview")],
            Tally {
                hits: 0,
                views: 0,
                bytes: 50
            }
        );
        assert_eq!(p.fetchers[&("ALBUM00000".to_string(), 1, "WhatsApp")], 1);
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

    #[test]
    fn a_file_is_a_view_on_its_own_but_not_as_part_of_a_page() {
        let views = Views::default();
        let file = hit(Some("IMAGE00000"), Served::Original, 10);
        views.record_on(1, &file, &visit("192.0.2.1", "a browser"));
        let embedded = Visit {
            from: Source::Here,
            ..visit("192.0.2.1", "a browser")
        };
        views.record_on(1, &file, &embedded);
        let p = &views.0.lock().pending;
        assert_eq!(
            p.hits[&("IMAGE00000".to_string(), 1, "original")],
            Tally {
                hits: 2,
                views: 1,
                bytes: 20
            }
        );
    }
}
