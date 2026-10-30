-- An API key for one use only. `dav`: the app password of calendar and contacts apps, sent over
-- HTTP Basic to /api/dav/... and refused everywhere else (docs/plan/collaboration.md, C5).
alter table api_key add column if not exists scope text check (scope in ('dav'));
