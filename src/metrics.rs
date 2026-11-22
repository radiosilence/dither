//! Prometheus text on a port of its own: counters and gauges with a small,
//! fixed label set, for the estate's dashboard.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Clone, Copy)]
pub enum Served {
    Original,
    Thumb,
    Display,
    Page,
}

static SERVED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static SERVED_BYTES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static NOT_MODIFIED: AtomicU64 = AtomicU64::new(0);
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
static UPLOADS: AtomicU64 = AtomicU64::new(0);
static UPLOAD_BYTES: AtomicU64 = AtomicU64::new(0);
static REJECTED: AtomicU64 = AtomicU64::new(0);
static PROCESS_MS: AtomicU64 = AtomicU64::new(0);
static SNAPSHOTS: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_FAILURES: AtomicU64 = AtomicU64::new(0);
static SNAPSHOT_AT: AtomicU64 = AtomicU64::new(0);

pub fn served(kind: Served, bytes: u64) {
    SERVED[kind as usize].fetch_add(1, Relaxed);
    SERVED_BYTES[kind as usize].fetch_add(bytes, Relaxed);
}

pub fn not_modified() {
    NOT_MODIFIED.fetch_add(1, Relaxed);
}

pub fn cache(hit: bool) {
    if hit { &CACHE_HITS } else { &CACHE_MISSES }.fetch_add(1, Relaxed);
}

pub fn uploaded(bytes: u64, took: std::time::Duration) {
    UPLOADS.fetch_add(1, Relaxed);
    UPLOAD_BYTES.fetch_add(bytes, Relaxed);
    PROCESS_MS.fetch_add(took.as_millis() as u64, Relaxed);
}

pub fn rejected() {
    REJECTED.fetch_add(1, Relaxed);
}

pub fn snapshot(ok: bool) {
    if ok {
        SNAPSHOTS.fetch_add(1, Relaxed);
        SNAPSHOT_AT.store((crate::db::now() / 1000) as u64, Relaxed);
    } else {
        SNAPSHOT_FAILURES.fetch_add(1, Relaxed);
    }
}

pub struct Totals {
    pub albums: u64,
    pub images: u64,
    pub bytes: u64,
    pub free: Option<u64>,
}

pub fn render(t: &Totals) -> String {
    let mut out = String::new();
    let mut metric = |name: &str, kind: &str, help: &str, lines: &[(String, u64)]| {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
        for (labels, value) in lines {
            let _ = writeln!(out, "{name}{labels} {value}");
        }
    };
    let by_kind = |values: &[AtomicU64; 4]| -> Vec<(String, u64)> {
        ["original", "thumb", "display", "page"]
            .iter()
            .zip(values)
            .map(|(k, v)| (format!("{{kind=\"{k}\"}}"), v.load(Relaxed)))
            .collect()
    };
    let one = |v: u64| vec![(String::new(), v)];
    metric(
        "dither_served_total",
        "counter",
        "Responses served, by what was asked for.",
        &by_kind(&SERVED),
    );
    metric(
        "dither_served_bytes_total",
        "counter",
        "Body bytes served, by what was asked for.",
        &by_kind(&SERVED_BYTES),
    );
    metric(
        "dither_not_modified_total",
        "counter",
        "Conditional requests answered 304.",
        &one(NOT_MODIFIED.load(Relaxed)),
    );
    metric(
        "dither_cache_hits_total",
        "counter",
        "Store reads answered from memory.",
        &one(CACHE_HITS.load(Relaxed)),
    );
    metric(
        "dither_cache_misses_total",
        "counter",
        "Store reads that went to the store.",
        &one(CACHE_MISSES.load(Relaxed)),
    );
    metric(
        "dither_uploads_total",
        "counter",
        "Images stored.",
        &one(UPLOADS.load(Relaxed)),
    );
    metric(
        "dither_upload_bytes_total",
        "counter",
        "Bytes of originals stored.",
        &one(UPLOAD_BYTES.load(Relaxed)),
    );
    metric(
        "dither_uploads_rejected_total",
        "counter",
        "Uploads refused as unsupported, too big or damaged.",
        &one(REJECTED.load(Relaxed)),
    );
    metric(
        "dither_process_milliseconds_total",
        "counter",
        "Time spent cleaning and resizing uploads.",
        &one(PROCESS_MS.load(Relaxed)),
    );
    metric(
        "dither_snapshots_total",
        "counter",
        "Database snapshots written to the store.",
        &one(SNAPSHOTS.load(Relaxed)),
    );
    metric(
        "dither_snapshot_failures_total",
        "counter",
        "Database snapshots that failed.",
        &one(SNAPSHOT_FAILURES.load(Relaxed)),
    );
    metric(
        "dither_snapshot_timestamp_seconds",
        "gauge",
        "When the last snapshot was written.",
        &one(SNAPSHOT_AT.load(Relaxed)),
    );
    metric("dither_albums", "gauge", "Albums.", &one(t.albums));
    metric("dither_images", "gauge", "Images.", &one(t.images));
    metric(
        "dither_stored_bytes",
        "gauge",
        "Bytes of originals.",
        &one(t.bytes),
    );
    if let Some(free) = t.free {
        metric(
            "dither_disk_free_bytes",
            "gauge",
            "Bytes free under a file store.",
            &one(free),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_every_family_with_help_and_type() {
        served(Served::Thumb, 100);
        let text = render(&Totals {
            albums: 1,
            images: 2,
            bytes: 3,
            free: Some(4),
        });
        assert!(text.contains("# TYPE dither_served_total counter"));
        assert!(text.contains("dither_served_total{kind=\"thumb\"}"));
        assert!(text.contains("dither_images 2"));
        assert!(text.contains("dither_disk_free_bytes 4"));
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            assert!(line.starts_with("dither_"), "{line}");
        }
    }
}
