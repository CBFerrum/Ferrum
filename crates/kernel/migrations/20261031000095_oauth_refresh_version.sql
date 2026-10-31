-- A refresh token ends with the person's other sessions: it remembers the token version it was
-- issued under, and signing out everywhere, a new password or a deactivated account (each moves
-- the version on) refuses it. Tokens issued before take the version their person has now.
alter table oauth_refresh add column token_version int not null default 0;

update oauth_refresh r set token_version = u.token_version
  from app_user u
 where u.tenant_id = r.tenant_id and u.id = r.user_id;
