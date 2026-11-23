-- Videos share the images table. A video's row is added as `processing` when
-- its upload finishes, so the album shows it at once; the worker that makes
-- its copies marks it `ready`, or `failed` with the reason in `failure`. Only
-- a ready row has copies to serve, and only its owner sees the others.
-- `duration_ms` is NULL for a picture, and `audio` says a video has sound.
ALTER TABLE images ADD COLUMN duration_ms INTEGER;
ALTER TABLE images ADD COLUMN audio INTEGER NOT NULL DEFAULT 0;
ALTER TABLE images ADD COLUMN state TEXT NOT NULL DEFAULT 'ready'
    CHECK (state IN ('ready', 'processing', 'failed'));
ALTER TABLE images ADD COLUMN failure TEXT NOT NULL DEFAULT '';

CREATE INDEX images_processing ON images (state) WHERE state = 'processing';
