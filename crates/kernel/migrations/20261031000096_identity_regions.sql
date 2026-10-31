-- Ferrum accounts across regions (docs/plan/apps-ui.md §9.3): an account lives in one region; a
-- workspace in another trusts it. There, the directory keeps a visiting account for it, named by
-- its home region and its id at home: no password here (it signs in at home), its address as
-- home vouches for it.
alter table app_user add column home_region text check (home_region is null or length(home_region) between 1 and 40);
alter table app_user add column home_sub uuid;
create unique index app_user_home on app_user (tenant_id, home_region, home_sub) where home_region is not null;

-- A code for another region's accounts host is bound to that host's PKCE challenge (S256), so
-- only the server that asked for it can take it.
alter table host_code add column challenge text check (challenge is null or length(challenge) between 43 and 128);
