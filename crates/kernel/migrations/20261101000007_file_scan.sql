-- Whether a stored file was scanned for malware (crates/kernel/src/scan.rs). What is found is never
-- stored, so there is no third value.
alter table file_object add column scan_status text not null default 'unscanned' check (scan_status in ('clean', 'unscanned'));
