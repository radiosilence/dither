//! Anonymous counts of what the public routes serve, for an owner to see how
//! far something they shared travelled.
//!
//! Per image and per album, per UTC day: hits and bytes by what was served,
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
    /// What the response carries: a range's length, not the file's size.
    pub bytes: u64,
}

/// Who asked, as far as counting goes. Used once and dropped.
pub struct Visit<'a> {
    pub address: &'a str,
    pub agent: &'a str,
    /// The host of the page that linked here, when it is not dither's own.
    pub referrer: Option<String>,
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

/// Counts not yet in the database, each by target and day.
#[derive(Default, Clone, Debug)]
pub struct Pending {
    /// Hits and bytes, by kind.
    pub hits: HashMap<(String, i64, &'static str), (u64, u64)>,
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
        for (k, (h, b)) in other.hits {
            let e = self.hits.entry(k).or_default();
            e.0 += h;
            e.1 += b;
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
                s.pending.hits.entry((t.clone(), day, kind)).or_default().1 += c.bytes;
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
        for t in targets {
            let e = s.pending.hits.entry((t.clone(), day, kind)).or_default();
            e.0 += 1;
            e.1 += c.bytes;
            if s.seen.insert((t.clone(), visitor)) {
                *s.pending.visitors.entry((t.clone(), day)).or_default() += 1;
            }
            if let Some(host) = &v.referrer {
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
    /// Pages served.
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

/// The host `referer` names, unless it is one of `own`.
pub fn referrer(referer: &str, own: &[&str]) -> Option<String> {
    let url = url::Url::parse(referer).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    (!own.iter().any(|o| o.eq_ignore_ascii_case(&host))).then_some(host)
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
            bytes,
        }
    }

    fn visit<'a>(address: &'a str, agent: &'a str) -> Visit<'a> {
        Visit {
            address,
            agent,
            referrer: None,
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
        assert_eq!(p.hits[&("IMAGE00000".to_string(), 1, "page")], (3, 300));
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
        assert_eq!(p.hits[&("IMAGE00000".to_string(), 1, "preview")], (0, 50));
        assert_eq!(p.fetchers[&("ALBUM00000".to_string(), 1, "WhatsApp")], 1);
    }

    #[test]
    fn a_referrer_is_its_host_and_never_dithers_own() {
        let own = ["app.test", "img.test"];
        assert_eq!(
            referrer("https://Forum.example/t/123?page=2#p4", &own).as_deref(),
            Some("forum.example")
        );
        assert_eq!(referrer("https://img.test/a/x", &own), None);
        assert_eq!(referrer("not a url", &own), None);
    }
}
