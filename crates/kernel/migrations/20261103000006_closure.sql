-- A workspace can be closed: read-only, kept until a date, and deleted after that only by an operator.
alter table tenant drop constraint tenant_account_state_check;
alter table tenant add constraint tenant_account_state_check check (account_state in ('active', 'grace', 'restricted', 'closed'));
alter table tenant add column closed_at timestamptz;
alter table tenant add column retain_until date;

-- That a workspace was deleted, when, and what went, for whoever asks later. None of its data is here.
create table tenant_deletion (
  id uuid primary key,
  slug text not null,
  closed_at timestamptz not null,
  retained_until date not null,
  deleted_at timestamptz not null default now(),
  rows jsonb not null,
  files integer not null,
  note text not null
);
