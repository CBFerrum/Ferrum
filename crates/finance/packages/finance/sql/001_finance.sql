-- Finance engine tables.
-- fin_subscription_period: one row per billed subscription period, written in the same transaction
-- as its invoice; the primary key makes invoice generation exactly-once per period.
-- fin_daily_run: the day each per-tenant daily upkeep task last ran, so the minute-by-minute
-- upkeep loop does the heavier scans (overdue customers, subscription statuses) once a day.

create table fin_subscription_period (
  tenant_id uuid not null references tenant(id) on delete cascade,
  subscription_id uuid not null,
  period_start date not null,
  period_end date not null,
  invoice_id uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, subscription_id, period_start),
  foreign key (tenant_id, subscription_id) references e_subscription (tenant_id, id) on delete cascade,
  foreign key (tenant_id, invoice_id) references e_sales_invoice (tenant_id, id) on delete set null (invoice_id),
  check (period_end >= period_start)
);

create table fin_daily_run (
  tenant_id uuid not null references tenant(id) on delete cascade,
  task text not null,
  day date not null,
  primary key (tenant_id, task)
);

do $$
declare t text;
begin
  foreach t in array array['fin_subscription_period', 'fin_daily_run'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;

-- A subscription's invoices (status refresh, history) without scanning every invoice.
create index if not exists fin_sales_invoice_subscription on e_sales_invoice (tenant_id, (ext->>'fin_subscription'))
  where ext ? 'fin_subscription';

-- Open purchase orders carry budget commitments; budget checks read only those.
create index if not exists fin_open_purchase_orders on e_purchase_order (tenant_id, company)
  where docstatus = 1 and status <> 'Closed';
