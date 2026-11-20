-- Each shared count says when its window ends, so that a window longer than an hour is swept by its own length rather
-- than an hour after it began (docs/builder/decisions.md BD-86). Counts kept before say nothing: swept an hour after
-- they began, as before.
alter table rate_window add column ends_at timestamptz;
