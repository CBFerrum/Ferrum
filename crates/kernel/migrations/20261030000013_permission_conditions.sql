-- A permission rule may apply only to records whose fields hold given values (`where` in a role's
-- permissions). A role can then hold several rules for one entity, told apart by their condition.
alter table role_permission add column match_rule jsonb, add column rule_key text not null default '';
alter table role_permission drop constraint role_permission_pkey;
alter table role_permission add primary key (tenant_id, role, entity, if_owner, rule_key);
