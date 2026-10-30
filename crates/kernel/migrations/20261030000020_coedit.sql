-- Documents people edit together live (docs/plan/collaboration.md, C3; the server's coedit.rs).
-- Each document's changes (Yjs updates) take the next number of its own sequence under the row
-- lock of `coedit_doc`, so they commit in order; every 500 are folded into the snapshot. The
-- package owning a document (its `DocHost`) keeps everything else about it.
create table coedit_doc (
  tenant_id uuid not null references tenant(id) on delete cascade,
  package text not null,
  id uuid not null,
  seq bigint not null default 0,
  snapshot bytea,
  snapshot_upto bigint not null default 0,
  -- The last change handed to the package as a save, when, and who changed it since.
  saved_seq bigint not null default 0,
  saved_at timestamptz,
  editors uuid[] not null default '{}',
  updated_at timestamptz not null default now(),
  primary key (tenant_id, package, id)
);

-- Documents changed but not handed to their package (a replica stopped first): saved by a sweep.
create index coedit_doc_unsaved on coedit_doc (updated_at) where saved_seq < seq;

create table coedit_update (
  tenant_id uuid not null,
  package text not null,
  doc uuid not null,
  seq bigint not null,
  data bytea not null,
  at timestamptz not null default now(),
  primary key (tenant_id, package, doc, seq),
  foreign key (tenant_id, package, doc) references coedit_doc (tenant_id, package, id) on delete cascade
);

alter table coedit_doc enable row level security;
alter table coedit_doc force row level security;
create policy tenant_isolation on coedit_doc
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
alter table coedit_update enable row level security;
alter table coedit_update force row level security;
create policy tenant_isolation on coedit_update
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
