-- Universal journal: append-only, balanced at commit, period-locked, partitioned by year.

create table accounting_period (
  tenant_id uuid not null references tenant(id) on delete cascade,
  company_id uuid not null,
  period_start date not null,
  period_end date not null,
  status text not null default 'open' check (status in ('open', 'closed')),
  closed_by uuid,
  closed_at timestamptz,
  primary key (tenant_id, company_id, period_start),
  foreign key (tenant_id, company_id) references e_company (tenant_id, id) on delete cascade,
  check (period_end >= period_start),
  exclude using gist (tenant_id with =, company_id with =, daterange(period_start, period_end, '[]') with &&)
);

create table journal_entry (
  tenant_id uuid not null,
  id uuid not null,
  company_id uuid not null,
  posting_date date not null,
  source_entity text not null,
  source_id uuid not null,
  source_number text,
  reversal_of uuid,
  memo text,
  total_debit numeric(28,9) not null,
  total_credit numeric(28,9) not null,
  entry_hash bytea not null,
  created_by uuid,
  created_at timestamptz not null default now(),
  created_txid bigint not null default txid_current(),
  primary key (tenant_id, id),
  foreign key (tenant_id, company_id) references e_company (tenant_id, id),
  check (total_debit = total_credit and total_debit > 0)
);
create index journal_entry_source on journal_entry (tenant_id, source_entity, source_id);
create index journal_entry_date on journal_entry (tenant_id, company_id, posting_date);

create table journal_line (
  tenant_id uuid not null,
  entry_id uuid not null,
  line_no int not null check (line_no >= 0),
  company_id uuid not null,
  posting_date date not null,
  account_id uuid not null,
  customer_id uuid,
  supplier_id uuid,
  debit numeric(28,9) not null default 0 check (debit >= 0),
  credit numeric(28,9) not null default 0 check (credit >= 0),
  currency char(3) not null,
  debit_txn numeric(28,9) not null default 0 check (debit_txn >= 0),
  credit_txn numeric(28,9) not null default 0 check (credit_txn >= 0),
  exchange_rate numeric(28,9) not null default 1 check (exchange_rate > 0),
  cost_center_id uuid,
  project_id uuid,
  dims jsonb not null default '{}',
  against_entity text,
  against_id uuid,
  remarks text,
  primary key (tenant_id, posting_date, entry_id, line_no),
  check (not (debit > 0 and credit > 0)),
  check (num_nonnulls(customer_id, supplier_id) <= 1),
  foreign key (tenant_id, entry_id) references journal_entry (tenant_id, id),
  foreign key (tenant_id, account_id) references e_account (tenant_id, id),
  foreign key (tenant_id, customer_id) references e_customer (tenant_id, id),
  foreign key (tenant_id, supplier_id) references e_supplier (tenant_id, id),
  foreign key (tenant_id, cost_center_id) references e_cost_center (tenant_id, id),
  foreign key (tenant_id, project_id) references e_project (tenant_id, id)
) partition by range (posting_date);

create table journal_line_default partition of journal_line default;
create index journal_line_account on journal_line (tenant_id, company_id, account_id, posting_date);
create index journal_line_customer on journal_line (tenant_id, customer_id, posting_date) where customer_id is not null;
create index journal_line_supplier on journal_line (tenant_id, supplier_id, posting_date) where supplier_id is not null;
create index journal_line_against on journal_line (tenant_id, against_entity, against_id) where against_id is not null;
create index journal_line_entry on journal_line (tenant_id, entry_id);

-- Yearly partitions; years that already have rows in the default partition stay there.
create or replace function ferrum_ensure_journal_partitions(from_year int, to_year int) returns int
language plpgsql as $$
declare y int; created int := 0; name text;
begin
  for y in from_year..to_year loop
    name := format('journal_line_y%s', y);
    if to_regclass(name) is null and not exists (
      select 1 from journal_line_default where posting_date >= make_date(y, 1, 1) and posting_date < make_date(y + 1, 1, 1)
    ) then
      execute format('create table %I partition of journal_line for values from (%L) to (%L)', name, make_date(y, 1, 1), make_date(y + 1, 1, 1));
      execute format('alter table %I enable row level security', name);
      execute format('alter table %I force row level security', name);
      created := created + 1;
    end if;
  end loop;
  return created;
end $$;

select ferrum_ensure_journal_partitions(extract(year from now())::int - 2, extract(year from now())::int + 2);

create or replace function ferrum_journal_entry_guard() returns trigger
language plpgsql as $$
begin
  if exists (
    select 1 from accounting_period p
    where p.tenant_id = new.tenant_id and p.company_id = new.company_id
      and new.posting_date between p.period_start and p.period_end and p.status = 'closed'
  ) then
    raise exception 'ferrum: the accounting period containing % is closed', new.posting_date using errcode = 'P0001';
  end if;
  return new;
end $$;
create trigger journal_entry_guard before insert on journal_entry
  for each row execute function ferrum_journal_entry_guard();

create or replace function ferrum_journal_line_guard() returns trigger
language plpgsql as $$
declare e record; a record;
begin
  select company_id, posting_date, created_txid into e from journal_entry where tenant_id = new.tenant_id and id = new.entry_id;
  if not found then
    raise exception 'ferrum: journal entry % does not exist', new.entry_id using errcode = 'P0001';
  end if;
  if e.created_txid <> txid_current() then
    raise exception 'ferrum: journal lines can only be added while their entry is being posted' using errcode = 'P0001';
  end if;
  if new.company_id <> e.company_id or new.posting_date <> e.posting_date then
    raise exception 'ferrum: journal line company/date must match its entry' using errcode = 'P0001';
  end if;
  select company, is_group, frozen, disabled into a from e_account where tenant_id = new.tenant_id and id = new.account_id;
  if not found then
    raise exception 'ferrum: account % does not exist', new.account_id using errcode = 'P0001';
  end if;
  if a.company <> new.company_id then
    raise exception 'ferrum: account belongs to another company' using errcode = 'P0001';
  end if;
  if a.is_group then
    raise exception 'ferrum: cannot post to a group account' using errcode = 'P0001';
  end if;
  if a.frozen or a.disabled then
    raise exception 'ferrum: account is frozen or disabled' using errcode = 'P0001';
  end if;
  return new;
end $$;
create trigger journal_line_guard before insert on journal_line
  for each row execute function ferrum_journal_line_guard();

create or replace function ferrum_check_journal_balance() returns trigger
language plpgsql as $$
declare d numeric; c numeric; n int;
begin
  select coalesce(sum(debit), 0), coalesce(sum(credit), 0), count(*) into d, c, n
  from journal_line where tenant_id = new.tenant_id and entry_id = new.id and posting_date = new.posting_date;
  if n < 2 then
    raise exception 'ferrum: journal entry % needs at least two lines', new.id using errcode = 'P0001';
  end if;
  if d <> c or d <> new.total_debit or c <> new.total_credit then
    raise exception 'ferrum: journal entry % is not balanced (debit %, credit %)', new.id, d, c using errcode = 'P0001';
  end if;
  return null;
end $$;
create constraint trigger journal_entry_balanced after insert on journal_entry
  deferrable initially deferred for each row execute function ferrum_check_journal_balance();

create trigger journal_entry_append_only before update or delete on journal_entry
  for each row execute function ferrum_forbid_change();
create trigger journal_line_append_only before update or delete on journal_line
  for each row execute function ferrum_forbid_change();
create trigger journal_entry_no_truncate before truncate on journal_entry
  for each statement execute function ferrum_forbid_change();
create trigger journal_line_no_truncate before truncate on journal_line
  for each statement execute function ferrum_forbid_change();

-- Accounts receivable/payable open items and their (append-only) allocations.
create table open_item (
  tenant_id uuid not null,
  id uuid not null,
  company_id uuid not null,
  entity text not null,
  number text,
  party_kind text not null check (party_kind in ('customer', 'supplier')),
  customer_id uuid,
  supplier_id uuid,
  account_id uuid not null,
  posting_date date not null,
  due_date date,
  currency char(3) not null,
  exchange_rate numeric(28,9) not null default 1 check (exchange_rate > 0),
  amount numeric(28,9) not null check (amount <> 0),
  base_amount numeric(28,9) not null,
  outstanding numeric(28,9) not null,
  base_outstanding numeric(28,9) not null,
  status text not null default 'open' check (status in ('open', 'paid', 'cancelled')),
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  foreign key (tenant_id, company_id) references e_company (tenant_id, id),
  foreign key (tenant_id, customer_id) references e_customer (tenant_id, id),
  foreign key (tenant_id, supplier_id) references e_supplier (tenant_id, id),
  foreign key (tenant_id, account_id) references e_account (tenant_id, id),
  check ((party_kind = 'customer') = (customer_id is not null) and (party_kind = 'supplier') = (supplier_id is not null)),
  check (sign(outstanding) * sign(amount) >= 0 and abs(outstanding) <= abs(amount))
);
create index open_item_party on open_item (tenant_id, company_id, party_kind, customer_id, supplier_id, status, due_date);

create table allocation (
  tenant_id uuid not null,
  id uuid not null,
  company_id uuid not null,
  open_item_id uuid not null,
  payment_entity text not null,
  payment_id uuid not null,
  posting_date date not null,
  amount numeric(28,9) not null check (amount <> 0),
  base_amount numeric(28,9) not null,
  reversal_of uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id),
  foreign key (tenant_id, open_item_id) references open_item (tenant_id, id)
);
create index allocation_payment on allocation (tenant_id, payment_entity, payment_id);
create index allocation_item on allocation (tenant_id, open_item_id);
create trigger allocation_append_only before update or delete on allocation
  for each row execute function ferrum_forbid_change();

do $$
declare t text;
begin
  foreach t in array array['accounting_period', 'journal_entry', 'journal_line', 'journal_line_default', 'open_item', 'allocation'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
