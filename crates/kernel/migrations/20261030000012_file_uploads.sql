-- Direct uploads in flight: the browser puts the bytes straight into the object store with a
-- presigned URL, then completes the upload, which turns the row into a `file_object`. Rows whose
-- upload never completed are swept with their objects once they expire.
create table file_upload (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  entity text,
  record_id uuid,
  file_name text not null,
  content_type text not null,
  size_bytes bigint not null check (size_bytes > 0),
  storage_key text not null,
  created_by uuid not null,
  expires_at timestamptz not null,
  primary key (tenant_id, id)
);
create index file_upload_expiry on file_upload (expires_at);

alter table file_upload enable row level security;
alter table file_upload force row level security;
create policy tenant_isolation on file_upload using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
