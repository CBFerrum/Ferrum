-- When each person became an administrator (docs/builder/decisions.md BD-83): an approval step whose approvers are
-- the workspace's administrators counts only those who have been one for seven days. Set by the database whenever the
-- flag turns on (a copied row keeps the time it carries) and cleared when it turns off. Administrators of before: this
-- migration's time.
alter table app_user add column admin_since timestamptz;
update app_user set admin_since = now() where is_admin;
create function ferrum_admin_since() returns trigger language plpgsql as $$
begin
  if not new.is_admin then
    new.admin_since := null;
  elsif tg_op = 'INSERT' then
    new.admin_since := coalesce(new.admin_since, now());
  elsif not old.is_admin then
    new.admin_since := now();
  end if;
  return new;
end $$;
create trigger app_user_admin_since before insert or update of is_admin on app_user
  for each row execute function ferrum_admin_since();
