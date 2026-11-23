-- Processing is a queue of image rows. A row is added as `processing` when
-- its upload finishes, so the album shows it at once; a worker claims it,
-- makes its copies, and marks it `ready`, or `failed` with a key for why in
-- `failure`. Only a ready row has copies to serve, and only its owner sees
-- the others.
--
-- A claim is a lease: `lease_until` is when it lapses and `claimed_by` which
-- worker holds it, renewed while the worker works, so a worker that stops
-- leaves its job for the next to take. A job put back after a failure that
-- may pass waits until its `lease_until` too. `attempts` counts claims.
--
-- `duration_ms` is NULL for a picture, and `audio` says a video has sound.
ALTER TABLE images ADD COLUMN duration_ms INTEGER;
ALTER TABLE images ADD COLUMN audio INTEGER NOT NULL DEFAULT 0;
ALTER TABLE images ADD COLUMN state TEXT NOT NULL DEFAULT 'ready'
    CHECK (state IN ('ready', 'processing', 'failed'));
ALTER TABLE images ADD COLUMN failure TEXT NOT NULL DEFAULT '';
ALTER TABLE images ADD COLUMN lease_until INTEGER;
ALTER TABLE images ADD COLUMN claimed_by TEXT;
ALTER TABLE images ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;

CREATE INDEX images_queue ON images (created_at) WHERE state = 'processing';
