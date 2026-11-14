-- One workspace moving from one installation to another (crates/kernel/src/workspace_move.rs; docs/operations.md, "Moving
-- one workspace"). One row per workspace on each side, kept after the move as its record. At the source: `leaving`, held
-- (no new background work) and then, from `frozen_at`, frozen; then `moved`, for good. At the target: `arriving`, frozen,
-- while the copy is made and checked; then `arrived`, an ordinary workspace again. The column is `workspace`, not
-- `tenant_id`: this row is about the workspace, and does not travel with it.
create table workspace_move (
  workspace uuid primary key,
  -- The same on both sides.
  move_id uuid not null,
  side text not null check (side in ('source', 'target')),
  state text not null check (state in ('leaving', 'moved', 'arriving', 'arrived')),
  frozen_at timestamptz,
  started_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  -- The other side's database, as an operator names it (host and database, never a password).
  peer text not null,
  -- At the source once moved: the host the workspace answers at, at the target.
  moved_to text,
  -- At the source: the target keeps its files in this one's storage, so finishing the move deletes rows only.
  shared_files boolean not null default false,
  -- At the target: what the copy read at the source, table by table (rows and a digest), to check against before the end.
  copied jsonb,
  -- At the source: when `finish_move` deleted what was left here.
  finished_at timestamptz
);

-- Whether a workspace takes changes in a kernel transaction, asked by every tenant transaction in its first round trip
-- (db.rs). PL/pgSQL, so that its query reads after its lock is granted: freezing takes ACCESS EXCLUSIVE on the table, and
-- a transaction that waited for it sees the freeze (as `ferrum_moving()`).
create function ferrum_workspace_frozen(w text) returns boolean language plpgsql volatile as $$
begin
  return w <> '' and exists (select 1 from workspace_move where workspace = w::uuid
    and (state in ('arriving', 'moved') or (state = 'leaving' and frozen_at is not null)));
end
$$;

-- Whether background work leaves a workspace alone: every claim across workspaces asks it.
create function ferrum_workspace_held(w uuid) returns boolean language sql stable as $$
  select exists (select 1 from workspace_move where workspace = w and state <> 'arrived' and finished_at is null)
$$;
