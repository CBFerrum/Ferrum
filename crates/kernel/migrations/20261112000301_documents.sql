-- Document templates (crates/kernel/src/documents.rs, docs/builder/decisions.md BD-21).

-- A workspace's own templates, and its edited copies of packages' defaults: `base` is the digest of the default an
-- edit started from, so that a release changing that default is told, never written over the edit.
create table document_template (
  tenant_id uuid not null references tenant (id) on delete cascade,
  key text not null check (length(key) between 1 and 100),
  def jsonb not null,
  base text,
  updated_by uuid,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, key)
);

alter table document_template enable row level security;
alter table document_template force row level security;
create policy tenant_isolation on document_template using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

-- The template a stored file was generated from, so a later generation can replace it.
alter table file_object add column document text;
create index file_object_document on file_object (tenant_id, entity, record_id, document) where document is not null;
