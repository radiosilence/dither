-- Whether a video is kept as it was recorded, or converted to H.264 at its
-- own size, which plays everywhere: chosen when its upload begins, and copied
-- to its image's row for the worker. One copy is stored either way.
ALTER TABLE uploads ADD COLUMN as_recorded INTEGER NOT NULL DEFAULT 0;
ALTER TABLE images ADD COLUMN as_recorded INTEGER NOT NULL DEFAULT 0;
