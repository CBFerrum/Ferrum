-- A reply that recorded a number's choice on a channel (a stop or start word, crates/kernel/src/messages.rs), by the
-- SHA-256 of the body its provider sent: taken once, so that the same call sent again by someone who captured it
-- cannot undo a later choice. Kept while the channel is.
create table message_reply_seen (
  tenant_id uuid not null,
  channel_id uuid not null,
  digest text not null,
  primary key (tenant_id, channel_id, digest),
  foreign key (tenant_id, channel_id) references message_channel (tenant_id, id) on delete cascade
);

alter table message_reply_seen enable row level security;
alter table message_reply_seen force row level security;
create policy tenant_isolation on message_reply_seen using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
