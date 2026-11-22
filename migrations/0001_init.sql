-- Who has signed in, by the identity provider's subject. The login is the
-- provider's username at the last sign-in, shown to its owner only.
CREATE TABLE users (
    sub TEXT PRIMARY KEY,
    login TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

-- Sign-ins, keyed by the SHA-256 of the cookie's opaque id.
CREATE TABLE sessions (
    id_hash BLOB PRIMARY KEY,
    sub TEXT NOT NULL,
    expires_at INTEGER NOT NULL
) STRICT;

-- Every image is in exactly one album. `cover` names one of its images, or
-- NULL for the first. `next_position` is where the next image added goes,
-- reserved when an upload starts so a batch keeps the order it was picked in
-- whatever order its files finish.
CREATE TABLE albums (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    cover TEXT,
    private INTEGER NOT NULL DEFAULT 0,
    next_position INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE INDEX albums_by_owner ON albums (owner, created_at DESC);

-- Width and height are as shown, after the orientation. `display` says a
-- display copy exists; `name` is the file's name as uploaded, for its owner.
CREATE TABLE images (
    id TEXT PRIMARY KEY,
    album TEXT NOT NULL REFERENCES albums (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    caption TEXT NOT NULL DEFAULT '',
    kind TEXT NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    animated INTEGER NOT NULL DEFAULT 0,
    display INTEGER NOT NULL DEFAULT 0,
    sha256 BLOB NOT NULL,
    name TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX images_by_album ON images (album, position, created_at);

-- An upload in progress: its bytes land in the state directory, a chunk at a
-- time, until `received` reaches `size`. `image` or `error` records how it
-- ended, so a client that lost the answer can ask again.
CREATE TABLE uploads (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    album TEXT NOT NULL REFERENCES albums (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    name TEXT NOT NULL,
    size INTEGER NOT NULL,
    received INTEGER NOT NULL DEFAULT 0,
    image TEXT,
    error TEXT,
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX uploads_by_age ON uploads (created_at);

-- Copies in the object store that no image row claims: written for an upload
-- not yet recorded, or left by a deleted image until the store confirms them
-- gone. The sweep removes those an hour old, so it only ever deletes copies
-- this database knows it made, never ones it merely does not recognise: a
-- database restored from an older copy leaves newer images alone.
CREATE TABLE strays (
    id TEXT PRIMARY KEY,
    since INTEGER NOT NULL
) STRICT;

-- Which copy of the database this is, bumped before each copy to the store.
-- A database on disk behind the store's latest copy, which only an older run
-- can leave, is then told apart from one ahead of it.
CREATE TABLE generation (n INTEGER NOT NULL) STRICT;
INSERT INTO generation (n) VALUES (0);
