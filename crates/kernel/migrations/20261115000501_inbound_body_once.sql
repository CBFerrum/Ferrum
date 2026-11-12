-- A call signed without a timestamp (docs/builder/decisions.md BD-49): its replay guard is its delivery id, and the
-- digest of the body it signed, each taken once while the row is kept. A body signed once is refused again whatever
-- delivery id a header beside it claims, since the header is not signed.
alter table conn_inbound add column body_digest text;
create unique index conn_inbound_body_once on conn_inbound (tenant_id, conn_id, body_digest) where body_digest is not null;
