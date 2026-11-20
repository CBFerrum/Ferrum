-- A send a provider marked simulated (TP-0): its answer or a report said so; never taken back.
alter table message_send add column simulated boolean not null default false;
