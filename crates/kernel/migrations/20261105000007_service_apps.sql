-- Apps whose code runs on their publisher's servers: the webhook that carries what one hears of
-- is the app's, and goes with it.
alter table webhook add column app text;
create unique index webhook_app on webhook (tenant_id, app) where app is not null;

-- Plugins' accounts were made as members, so they took a seat and showed among people.
update app_user set kind = 'service' where kind = 'member' and email like '%@plugins.ferrum.invalid';

alter table app_release drop constraint app_release_runtime_check;
alter table app_release add constraint app_release_runtime_check check (runtime in ('native', 'wasm', 'meta', 'service'));
