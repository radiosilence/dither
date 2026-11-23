-- Anonymous counts of what the public routes served, per UTC day (`day` is
-- days since the epoch). `target` is an image's id or an album's: an album's
-- rows count its own page and everything served of its images. No address,
-- user agent or visitor hash is kept; `visitors` is a count of distinct ones
-- seen that day. A deleted image or album takes its rows with it.
CREATE TABLE views (
    target TEXT NOT NULL,
    day INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('page', 'original', 'display', 'thumb', 'preview')),
    hits INTEGER NOT NULL DEFAULT 0,
    bytes INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (target, day, kind)
) STRICT, WITHOUT ROWID;

CREATE TABLE visitors (
    target TEXT NOT NULL,
    day INTEGER NOT NULL,
    visitors INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (target, day)
) STRICT, WITHOUT ROWID;

-- The host of the Referer header alone, never its path or query.
CREATE TABLE referrers (
    target TEXT NOT NULL,
    day INTEGER NOT NULL,
    host TEXT NOT NULL,
    hits INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (target, day, host)
) STRICT, WITHOUT ROWID;

-- Link previews' fetchers, by name: where something was shared. They are
-- never counted as views or visitors.
CREATE TABLE fetchers (
    target TEXT NOT NULL,
    day INTEGER NOT NULL,
    fetcher TEXT NOT NULL,
    hits INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (target, day, fetcher)
) STRICT, WITHOUT ROWID;
