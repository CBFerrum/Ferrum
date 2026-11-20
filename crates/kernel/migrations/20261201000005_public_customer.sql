-- A role every portal person holds without being given it (docs/builder/decisions.md BD-89): an app's customer role,
-- whose rules reach only the person's own records. Only a portal role may be one.
alter table role add column portal_default boolean not null default false;
alter table role add constraint role_portal_default_is_portal check (portal or not portal_default);

-- A session opened on a site host (BD-89): it works on that host alone, as the person's customer self. Null: an
-- ordinary session, refused on every site host.
alter table user_session add column host text check (host is null or length(host) <= 253);
