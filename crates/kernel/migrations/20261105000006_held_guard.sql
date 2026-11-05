-- A record under a legal hold is not deleted by any path (FERRUM-PRIVACY-004): the platform's own
-- deletes refuse it with their reason first, and this refuses the rest, an app's own SQL among
-- them, on every entity table the metadata sync makes (it names the entity as the argument). A
-- delete cascading from another (a workspace's deletion) is not stopped here.
create or replace function ferrum_held_guard() returns trigger
language plpgsql as $$
begin
  if pg_trigger_depth() = 1 and exists (
    select 1 from legal_hold h
    where h.tenant_id = old.tenant_id and h.released_at is null and h.entity = tg_argv[0] and h.record_id = old.id
  ) then
    raise exception 'ferrum: this % is under a legal hold; release it first', tg_argv[0] using errcode = 'P0001';
  end if;
  return old;
end $$;

-- What the guard asks on every delete, kept small: the holds in force on records.
create index legal_hold_records on legal_hold (tenant_id, entity, record_id) where released_at is null and record_id is not null;
