-- Uploads sent through the server in parts (FERRUM-OPS-006): how much has arrived, so that after a
-- dropped connection the next part goes from there. Direct uploads to object storage leave both alone.
alter table file_upload add column parts boolean not null default false;
alter table file_upload add column received_bytes bigint not null default 0;
