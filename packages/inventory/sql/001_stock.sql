-- Append-only stock ledger, current balances and FIFO cost layers.
-- Valuation is computed once, at posting time, from the state at that moment. A backdated entry
-- is valued with the current state and is never followed by a cascade of re-valuations.

create table stock_move (
  tenant_id uuid not null,
  id uuid not null,
  company_id uuid not null,
  item_id uuid not null,
  warehouse_id uuid not null,
  posting_date date not null,
  posting_ts timestamptz not null,
  qty numeric(28,9) not null check (qty <> 0),
  valuation_rate numeric(28,12) not null check (valuation_rate >= 0),
  value numeric(28,9) not null,
  qty_after numeric(28,9) not null,
  value_after numeric(28,9) not null,
  batch_no text,
  serial_no text,
  source_entity text not null,
  source_id uuid not null,
  source_line_id uuid,
  reversal_of uuid,
  seq bigint generated always as identity,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id),
  foreign key (tenant_id, company_id) references e_company (tenant_id, id),
  foreign key (tenant_id, item_id) references e_item (tenant_id, id),
  foreign key (tenant_id, warehouse_id) references e_warehouse (tenant_id, id),
  check (sign(value) * sign(qty) >= 0)
);
create index stock_move_item on stock_move (tenant_id, item_id, warehouse_id, posting_ts, seq);
create index stock_move_source on stock_move (tenant_id, source_entity, source_id);
create index stock_move_date on stock_move (tenant_id, company_id, posting_date);
create index stock_move_batch on stock_move (tenant_id, item_id, batch_no) where batch_no is not null;
create index stock_move_serial on stock_move (tenant_id, item_id, serial_no) where serial_no is not null;

create trigger stock_move_append_only before update or delete on stock_move
  for each row execute function ferrum_forbid_change();
create trigger stock_move_no_truncate before truncate on stock_move
  for each statement execute function ferrum_forbid_change();

create table stock_balance (
  tenant_id uuid not null,
  item_id uuid not null,
  warehouse_id uuid not null,
  company_id uuid not null,
  qty numeric(28,9) not null default 0,
  value numeric(28,9) not null default 0,
  valuation_rate numeric(28,12) not null default 0 check (valuation_rate >= 0),
  reserved_qty numeric(28,9) not null default 0 check (reserved_qty >= 0),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, item_id, warehouse_id),
  foreign key (tenant_id, item_id) references e_item (tenant_id, id),
  foreign key (tenant_id, warehouse_id) references e_warehouse (tenant_id, id)
);

create table cost_layer (
  tenant_id uuid not null,
  id uuid not null,
  item_id uuid not null,
  warehouse_id uuid not null,
  posting_ts timestamptz not null,
  qty_in numeric(28,9) not null check (qty_in > 0),
  qty_remaining numeric(28,9) not null check (qty_remaining >= 0 and qty_remaining <= qty_in),
  unit_cost numeric(28,12) not null check (unit_cost >= 0),
  source_move_id uuid not null,
  seq bigint generated always as identity,
  primary key (tenant_id, id),
  foreign key (tenant_id, source_move_id) references stock_move (tenant_id, id)
);
create index cost_layer_open on cost_layer (tenant_id, item_id, warehouse_id, posting_ts, seq) where qty_remaining > 0;

-- Serial numbers: exactly one location (or none when delivered) per serial.
create table serial_no (
  tenant_id uuid not null,
  item_id uuid not null,
  serial text not null,
  warehouse_id uuid,
  status text not null check (status in ('in_stock', 'delivered', 'consumed')),
  last_move_id uuid,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, item_id, serial),
  foreign key (tenant_id, item_id) references e_item (tenant_id, id),
  foreign key (tenant_id, warehouse_id) references e_warehouse (tenant_id, id),
  check ((status = 'in_stock') = (warehouse_id is not null))
);

do $$
declare t text;
begin
  foreach t in array array['stock_move', 'stock_balance', 'cost_layer', 'serial_no'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
