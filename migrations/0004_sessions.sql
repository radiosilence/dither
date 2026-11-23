-- When each sign-in was made. A page load a day after it goes back through
-- the identity provider, which answers again for whether the person may
-- still sign in. Sign-ins from before have none, and are checked at once.
ALTER TABLE sessions ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
