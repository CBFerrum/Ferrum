-- Purchase, receipt, invoice and stock entry lines carry the material request / supplier quotation line they
-- fulfil in `ext` (package overlay fields). Fulfilment tracking and the procurement tracker look lines up by them.
create index if not exists proc_po_item_mr_line on e_purchase_order__items (tenant_id, (ext->>'proc_mr_line')) where ext ? 'proc_mr_line';
create index if not exists proc_po_item_sq_line on e_purchase_order__items (tenant_id, (ext->>'proc_sq_line')) where ext ? 'proc_sq_line';
create index if not exists proc_pr_item_mr_line on e_purchase_receipt__items (tenant_id, (ext->>'proc_mr_line')) where ext ? 'proc_mr_line';
create index if not exists proc_pi_item_mr_line on e_purchase_invoice__items (tenant_id, (ext->>'proc_mr_line')) where ext ? 'proc_mr_line';
create index if not exists proc_se_item_mr_line on e_stock_entry__items (tenant_id, (ext->>'proc_mr_line')) where ext ? 'proc_mr_line';
