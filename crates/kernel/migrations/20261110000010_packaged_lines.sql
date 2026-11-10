-- Lines a workspace adds to a package's entity (docs/builder/decisions.md BD-06, 10 November 2026): rows of
-- `custom_record` whose `parent_id` is a record of the package's own table. The foreign key from `parent_id` to
-- `custom_record` could not hold such a row, so it gives way to a trigger that does what its cascade did: when
-- custom records go, the custom records under them go too, at any depth. A package's record takes its lines through
-- the kernel (`store::delete_row`, and an uninstall that deletes the package's data); `packaged_lines_it` checks
-- that no line is left without its record.
--
-- Nothing is rewritten: the constraint is dropped and the trigger made, in one transaction, on any number of rows.
-- To go back, delete the lines whose parent is not a custom record, then
--   drop trigger custom_record_lines_cascade on custom_record;
--   alter table custom_record add constraint custom_record_parent
--     foreign key (tenant_id, parent_id) references custom_record (tenant_id, id) on delete cascade;
alter table custom_record drop constraint custom_record_parent;

create or replace function ferrum_custom_lines_cascade() returns trigger
language plpgsql as $$
begin
  -- A statement trigger runs for a delete of no rows too: stop there, or it would never end.
  if exists (select 1 from gone) then
    delete from custom_record c using gone g where c.tenant_id = g.tenant_id and c.parent_id = g.id;
  end if;
  return null;
end $$;

create trigger custom_record_lines_cascade after delete on custom_record
  referencing old table as gone for each statement execute function ferrum_custom_lines_cascade();
