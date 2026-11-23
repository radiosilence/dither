-- When each sign-in was made. A page load a day after it goes back through
-- the identity provider, which answers again for whether the person may
-- still sign in. Sign-ins from before have none, and are checked at once.
ALTER TABLE sessions ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;
-- And they last no longer than a sign-in made now, so one made under the
-- old thirty days does not keep the API open to someone no longer allowed.
UPDATE sessions
SET expires_at = MIN(expires_at, CAST(strftime('%s', 'now') AS INTEGER) * 1000 + 2 * 86400 * 1000);
