-- What an address of a workspace shows (docs/operations.md, "Addresses of a workspace"), for a `shell` address: the app
-- `/` lands on (`home`), the apps its bar lists (`apps`; empty: all of them) and its name in the bar (`label`). They
-- choose what a person sees first, never what the address admits: every app opens on every `shell` address, and a zone
-- stays the operator's. `base_name`: the name an administrator gave an address under the base domain,
-- `{base_name}--{slug}.{base}`; empty for every other address. An older build reads none of them, so such an address
-- lands on Home and lists every app there, which takes nothing away.
alter table tenant_host add column home text check (home ~ '^[a-z][a-z0-9-]{1,39}$');
alter table tenant_host add column apps text[] check (cardinality(apps) <= 50);
alter table tenant_host add column label text check (length(label) between 1 and 40);
alter table tenant_host add column base_name text check (base_name ~ '^[a-z0-9]([a-z0-9-]{0,28}[a-z0-9])?$' and base_name !~ '--');
