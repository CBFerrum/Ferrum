-- Portal roles (docs/builder/decisions.md BD-02): a role may be marked for the portal. Portal people
-- hold only such roles, and such a role holds only own-record rules. A rule may reach its holder's own
-- records through a path (`via`) ending at their email, rather than by who made them.
alter table role add column portal boolean not null default false;
alter table role_permission add column via text check (via is null or length(via) between 1 and 200);
-- The entity's actions a rule's holders may run on the records it reaches, whatever the action's own
-- permission asks: a patient cancels their visit without being able to edit it.
alter table role_permission add column actions text[] not null default '{}';
