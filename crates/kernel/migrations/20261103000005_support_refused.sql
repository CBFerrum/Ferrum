-- Whether a request made with support access was turned away at the door, so a customer reading the list
-- can tell what was tried from what was reached.
alter table support_access add column refused boolean not null default false;
