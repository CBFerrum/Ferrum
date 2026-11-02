-- What a consumer has done about an event, in the transaction that did it. Delivery is at least once, so
-- this is what makes an effect happen once, and the count is what a repeat leaves behind.
create table event_receipt (
  tenant_id uuid not null,
  consumer text not null check (length(consumer) between 1 and 120),
  event_id bigint not null,
  duplicates integer not null default 0,
  received_at timestamptz not null default now(),
  primary key (tenant_id, consumer, event_id)
);
create index event_receipt_age on event_receipt (received_at);
alter table event_receipt enable row level security;
alter table event_receipt force row level security;
create policy tenant_isolation on event_receipt using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
