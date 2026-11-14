-- A planned move of the whole installation (crates/kernel/src/moves.rs). `ferrum task plan_move` leaves one mark here:
-- first unfenced, while the work in hand finishes (nothing new that reaches outside starts), then fenced, and from then
-- every kernel transaction is read-only. A copy of this database (a dump restored, a physical copy promoted, a backup
-- recovered) finds the mark made on another database than itself, adopts its own identity without the restore hold, and
-- removes the mark. At most one row.
create table platform_move (
  id uuid primary key,
  one boolean not null default true unique check (one),
  made_at timestamptz not null default now(),
  -- A copy that starts later than this takes the restore hold as before.
  valid_until timestamptz not null,
  -- The database the move was planned on (as in platform_incarnation).
  system_identifier bigint not null,
  timeline integer not null,
  database_oid oid not null,
  reason text not null,
  -- Null while the work in hand finishes; set when the database stopped taking changes.
  fenced_at timestamptz
);

-- Asked at the start of every kernel transaction, in the same round trip as its settings. PL/pgSQL, so that its query
-- reads after its lock is granted: `plan_move` fences under ACCESS EXCLUSIVE on the table, and a transaction that waited
-- for it sees the fence (an inlined SQL function would read with the snapshot taken before the wait).
create function ferrum_moving() returns boolean language plpgsql volatile as $$
begin
  return exists (select 1 from platform_move where fenced_at is not null);
end
$$;
