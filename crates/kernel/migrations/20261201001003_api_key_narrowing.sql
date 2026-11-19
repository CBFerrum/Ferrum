-- A secret key narrowed to apps or operations (docs/builder/decisions.md BD-85) is a server's key for the capability door
-- alone: every other door refuses it. Nothing makes one yet; the columns are read so that no door takes one by mistake.
alter table api_key add column apps text[] check (apps is null or cardinality(apps) between 1 and 50);
alter table api_key add column operations text[] check (operations is null or cardinality(operations) between 1 and 200);
-- A narrowed key is a server's key for the capability door: no single-purpose scope beside it.
alter table api_key add constraint api_key_narrowed_whole check ((apps is null and operations is null) or scope is null);
