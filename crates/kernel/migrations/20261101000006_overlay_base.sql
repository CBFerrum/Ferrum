-- What a customization replaced, when it was made: how an upgrade that changes the same thing is
-- noticed (crates/kernel/src/customize.rs). Older customizations have none, and are not compared.
alter table meta_overlay add column base jsonb;
