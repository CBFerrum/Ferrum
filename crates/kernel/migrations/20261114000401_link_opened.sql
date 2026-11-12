-- Signed action links (docs/builder/decisions.md BD-38): when a visitor first acted on its page (`opened_at`, from
-- the page's own first POST-back, which link-preview fetchers and mail scanners never send), and the columns of
-- each table field it shows (`columns`, `{field: [column, ...]}`).
alter table action_link add column opened_at timestamptz;
alter table action_link add column columns jsonb not null default '{}';
-- A record's `opened` event: automations may answer it.
alter table automation drop constraint automation_event_check;
alter table automation add constraint automation_event_check
  check (event in ('created', 'updated', 'submitted', 'cancelled', 'deleted', 'transitioned', 'scheduled', 'opened'));
