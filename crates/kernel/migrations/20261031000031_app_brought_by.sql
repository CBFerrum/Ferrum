-- The app whose installation brought this one in as something it requires (a suite's members, an
-- app's prerequisite); null when an administrator installed it for itself. Removing that app
-- offers to remove what it brought that nothing else needs (docs/plan/apps-ui.md §13).
alter table tenant_app add column brought_by text;
