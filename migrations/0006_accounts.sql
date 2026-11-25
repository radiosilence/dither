-- Accounts are dither's own: koan-auth's tables, which its own migration
-- makes after these (`koan_auth::schema::migrate`), with an album's owner the
-- account's username. Nothing signed in through the identity provider is
-- kept: its images' copies are recorded as strays, which the hourly sweep
-- deletes from the store, and its albums, uploads and counts go.
INSERT OR REPLACE INTO strays (id, since) SELECT id, 0 FROM images;
DELETE FROM uploads;
DELETE FROM images;

-- Rebuilt to name its owner's account: SQLite adds a foreign key to no
-- existing column. An account cannot be deleted while it has albums, so its
-- albums go first, through the deletion that records their copies.
DROP TABLE albums;
CREATE TABLE albums (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES users (username) ON UPDATE CASCADE,
    title TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    cover TEXT,
    private INTEGER NOT NULL DEFAULT 0,
    next_position INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE INDEX albums_by_owner ON albums (owner, created_at DESC);

DELETE FROM views;
DELETE FROM visitors;
DELETE FROM referrers;
DELETE FROM fetchers;

DROP TABLE sessions;
DROP TABLE users;
