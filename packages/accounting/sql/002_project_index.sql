-- Project profitability reads a project's ledger lines directly.
create index if not exists journal_line_project on journal_line (tenant_id, project_id, posting_date) where project_id is not null;
