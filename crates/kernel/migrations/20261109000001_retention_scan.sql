-- Where a retention rule's scan stands (FEO-005). Each daily run continues from here, so records it cannot
-- remove (held, or failing) never keep it from the ones after them; a finished scan starts again from the
-- beginning, revisiting those. Null: at the beginning. Changing a rule starts its scan again.
alter table retention_rule add column scan_cursor text;
