-- Erasing a person's data also blanks the values their records' audit history holds. An audit
-- event may change only its diff, and only while recording that redaction (`redacted` grows); its
-- hash and place in the chain stay, and verification skips the content of redacted events.
alter table audit_event add column redacted jsonb;

create or replace function ferrum_audit_redact_only() returns trigger language plpgsql as $$
begin
  if new.redacted is null
     or jsonb_typeof(new.redacted) <> 'array'
     or jsonb_array_length(new.redacted) <= coalesce(jsonb_array_length(old.redacted), 0)
     or (new.tenant_id, new.id, new.at, new.entity, new.record_id, new.action, new.hash)
        is distinct from (old.tenant_id, old.id, old.at, old.entity, old.record_id, old.action, old.hash)
     or new.actor is distinct from old.actor
     or new.version is distinct from old.version
     or new.request_id is distinct from old.request_id
     or new.prev_hash is distinct from old.prev_hash then
    raise exception 'audit events are append-only (only a recorded redaction may blank their values)';
  end if;
  return new;
end $$;

drop trigger audit_event_append_only on audit_event;
create trigger audit_event_no_delete before delete on audit_event
  for each row execute function ferrum_forbid_change();
create trigger audit_event_redact_only before update on audit_event
  for each row execute function ferrum_audit_redact_only();
