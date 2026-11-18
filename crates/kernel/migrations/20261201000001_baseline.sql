-- Ferrum kernel schema: the platform's own tables, functions and policies, in one file. Business entity tables are
-- generated from package metadata. Until the first release this file is edited in place.
--
-- Every tenant-owned table carries tenant_id, has RLS forced on, and is isolated by the per-transaction setting
-- `ferrum.tenant_id`; system jobs set `ferrum.system = on`; the vault's tables answer only to `ferrum.vault = on`.
-- The policies are made in the loops at the end of the file.

create extension if not exists btree_gist;
create extension if not exists citext;
create schema if not exists vault;

-- Whether this transaction is the system's (`ferrum.system = on`): jobs, token endpoints, the operator's tasks.
create function ferrum_is_system() returns boolean language sql stable parallel safe as $$
  select coalesce(current_setting('ferrum.system', true), '') = 'on'
$$;

-- The workspace this transaction works in (`ferrum.tenant_id`, set per transaction), or null.
create function ferrum_tenant() returns uuid language sql stable parallel safe as $$
  select nullif(current_setting('ferrum.tenant_id', true), '')::uuid
$$;

-- ---- Workspaces, people and roles ------------------------------------------------------------------------------------

create table tenant (
  id uuid primary key,
  slug text not null unique check (slug ~ '^[a-z0-9][a-z0-9-]{1,62}$'),
  name text not null check (length(name) between 1 and 200),
  status text not null default 'active' check (status in ('active', 'suspended')),
  meta_version bigint not null default 1,
  settings jsonb not null default '{}',
  created_at timestamptz not null default now(),
  -- Where a workspace's account stands (crates/kernel/src/account.rs). Nothing here touches its data.
  account_state text not null default 'active' check (account_state in ('active', 'grace', 'restricted', 'closed')),
  account_note text,
  quotas jsonb not null default '{}',
  closed_at timestamptz,
  retain_until date,
  -- A sandbox: a copy of a live workspace under a new id, which never reaches outside (crates/kernel/src/sandbox.rs;
  -- docs/operations.md, "A sandbox of a workspace"). Set in the same transaction as the sandbox's row, before any of its
  -- rows arrives, and never changed after: no setting or task turns a sandbox into a live workspace.
  sandbox_of uuid,
  sandbox_from text,
  sandbox_until date,
  constraint tenant_sandbox_whole check (((sandbox_of is null) = (sandbox_until is null)) and ((sandbox_of is null) = (sandbox_from is null)))
);
create index tenant_sandbox_of on tenant (sandbox_of) where sandbox_of is not null;

create function ferrum_forbid_change() returns trigger language plpgsql as $$
begin
  raise exception 'ferrum: % on % is not allowed (append-only table)', tg_op, tg_table_name
    using errcode = 'P0001';
end $$;

-- One slug at a time: a rename retiring a slug and a new workspace claiming it cannot interleave.
create function ferrum_tenant_slug_guard() returns trigger language plpgsql as $$
begin
  -- One slug at a time: a rename retiring a slug and a new workspace claiming it cannot interleave.
  if tg_op = 'DELETE' or (tg_op = 'UPDATE' and new.slug is distinct from old.slug) then
    perform pg_advisory_xact_lock(hashtextextended('ferrum-slug:' || old.slug, 0));
    insert into tenant_slug_retired (slug, tenant_id) values (old.slug, old.id) on conflict (slug) do nothing;
  end if;
  if tg_op = 'INSERT' or (tg_op = 'UPDATE' and new.slug is distinct from old.slug) then
    perform pg_advisory_xact_lock(hashtextextended('ferrum-slug:' || new.slug, 0));
    if exists (select 1 from tenant_slug_retired r where r.slug = new.slug and r.tenant_id <> new.id) then
      raise exception using
        errcode = 'unique_violation',
        constraint = 'tenant_slug_retired_pkey',
        message = format('the slug %s belonged to another workspace and is never used again', new.slug);
    end if;
  end if;
  if tg_op = 'DELETE' then
    return old;
  end if;
  return new;
end $$;
create trigger tenant_sandbox_kept before update of sandbox_of, sandbox_from on tenant
  for each row
  when ((old.sandbox_of is distinct from new.sandbox_of) or (old.sandbox_from is distinct from new.sandbox_from))
  execute function ferrum_forbid_change();
create trigger tenant_slug_guard before insert or delete or update of slug on tenant
  for each row
  execute function ferrum_tenant_slug_guard();

alter table tenant enable row level security;
alter table tenant force row level security;
create policy tenant_self on tenant using (id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check ((select ferrum_is_system()) or id = (select ferrum_tenant()));

create table app_user (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  email citext not null check ((length((email)::text) <= 254) and (email operator(~) '^[^@\s]+@[^@\s]+$'::citext)),
  full_name text not null default '' check (length(full_name) <= 200),
  password_hash text,
  is_active boolean not null default true,
  is_admin boolean not null default false,
  token_version int not null default 1,
  failed_logins int not null default 0,
  locked_until timestamptz,
  last_login_at timestamptz,
  settings jsonb not null default '{}',
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  -- Two-factor sign-in with an authenticator app (TOTP): whether a secret was made (`totp_set`; the secret is kept in the
  -- vault), the last time step accepted (each code works once), and single-use recovery codes kept as hashes.
  totp_set boolean not null default false,
  totp_enabled_at timestamptz,
  totp_last_step bigint,
  sso_subject text,
  -- Members use the workspace's apps; guests reach only what was shared with them; portal people
  -- see the workspace's portal; service accounts have no interface.
  kind text not null default 'member' check (kind in ('member', 'portal', 'guest', 'service', 'support')),
  -- One person, many workspaces (docs/plan/apps-ui.md §9.3, §12.3; U6). In shapes B and C a person
  -- signs in once, at the accounts host, as an identity: a person of the cluster's directory, the
  -- reserved workspace `accounts`, so password, two-step sign-in, passkeys and browser sessions are
  -- those any person has. A membership (`app_user` of a workspace) points at the identity it was
  -- added to after the person proved it; the workspace's own password stays as it was, unused while
  -- the cluster signs in through accounts.
  identity_id uuid,
  -- When a directory person's address was proven with a code sent to it.
  email_verified_at timestamptz,
  -- Ferrum accounts across regions (docs/plan/apps-ui.md §9.3): an account lives in one region; a
  -- workspace in another trusts it. There, the directory keeps a visiting account for it, named by
  -- its home region and its id at home: no password here (it signs in at home), its address as
  -- home vouches for it.
  home_region text check ((home_region is null) or (length(home_region) between 1 and 40)),
  home_sub uuid,
  -- SCIM provisioning (RFC 7643, 7644; docs/identity-lifecycle.md). A person keeps their row when a
  -- provider deletes them: their name stays on what they did, and `scim_deleted_at` hides them from
  -- SCIM until the provider creates them again. Groups are the provider's: they grant the roles an
  -- administrator maps them to, and `via_scim` marks the grants that leave when the membership does,
  -- so a role given by hand survives.
  scim_external_id text check (length(scim_external_id) <= 255),
  scim_deleted_at timestamptz,
  primary key (tenant_id, id),
  unique (tenant_id, email)
);
-- The memberships an address has across workspaces (signing in at accounts, adding a workspace).
create index app_user_email on app_user (email);
create unique index app_user_home on app_user (tenant_id, home_region, home_sub) where home_region is not null;
create unique index app_user_identity on app_user (tenant_id, identity_id) where identity_id is not null;
create index app_user_identity_of on app_user (identity_id) where identity_id is not null;
create unique index app_user_scim_external on app_user (tenant_id, scim_external_id) where scim_external_id is not null;
create unique index app_user_sso_subject on app_user (tenant_id, sso_subject) where sso_subject is not null;

-- A workspace's people quota (`tenant.quotas->>'people'`, the operator's or its plan's) holds on
-- every path that makes someone an active member: added by an administrator, by SCIM or single
-- sign-on, joining by invitation, turned on again. Service accounts and people turned off never
-- count. Adds to one workspace wait for each other, so two at once cannot both take the last seat.
create function ferrum_people_quota() returns trigger language plpgsql as $$
declare
  cap bigint;
  used bigint;
begin
  if new.kind <> 'member' or not new.is_active then
    return new;
  end if;
  if tg_op = 'UPDATE' and old.is_active and old.kind = 'member' then
    return new;
  end if;
  select (quotas->>'people')::bigint into cap from tenant where id = new.tenant_id;
  if cap is null then
    return new;
  end if;
  perform pg_advisory_xact_lock(hashtext('ferrum_people_quota'), hashtext(new.tenant_id::text));
  select count(*) into used from app_user where tenant_id = new.tenant_id and is_active and kind = 'member' and id <> new.id;
  if used >= cap then
    raise exception 'ferrum: this workspace has room for % people on its plan: turn someone off, or ask for a larger plan', cap;
  end if;
  return new;
end $$;

create function ferrum_person_identifier() returns trigger language plpgsql as $$
begin
  insert into person_identifier (tenant_id, user_id, email) values (old.tenant_id, old.id, old.email) on conflict do nothing;
  return new;
end $$;
create trigger app_user_email_history after update of email on app_user
  for each row
  when (old.email is distinct from new.email)
  execute function ferrum_person_identifier();
create trigger app_user_people_quota before insert or update of is_active, kind on app_user
  for each row
  execute function ferrum_people_quota();

create table role (
  tenant_id uuid not null references tenant(id) on delete cascade,
  name text not null check (length(name) between 1 and 100),
  description text,
  package text,
  -- Portal roles (docs/builder/decisions.md BD-02): a role may be marked for the portal. Portal people
  -- hold only such roles, and such a role holds only own-record rules. A rule may reach its holder's own
  -- records through a path (`via`) ending at their email, rather than by who made them.
  portal boolean not null default false,
  -- Outside people signing themselves up to a workspace's portal (docs/api.md, "Public sign-up"; BD-02).
  -- A portal role's `public_signup` turns it on: {domains: [...], passes: [...]}; null (the default) leaves it off.
  public_signup jsonb,
  primary key (tenant_id, name)
);

create table user_role (
  tenant_id uuid not null,
  user_id uuid not null,
  role text not null,
  via_scim boolean not null default false,
  -- A role may be given until a moment (FERRUM-IDENTITY-009): a contractor for the length of their
  -- contract, a stand-in while someone is away. From then the person no longer holds it: their
  -- context leaves it out (and a cached one lapses at that moment), and the hourly upkeep removes the
  -- grant, so whatever reads who holds a role stops finding them.
  expires_at timestamptz,
  primary key (tenant_id, user_id, role),
  foreign key (tenant_id, role) references role(tenant_id, name) on update cascade on delete cascade,
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);

-- One person's grants are checked one at a time, so two arriving at once cannot both slip past.
-- A grant already held is no new grant: an identity provider re-syncing someone who held both roles
-- of a pair before it was declared (their insert of a role they hold becomes an update) is not
-- refused. Granting either afresh still is.
create function user_role_conflict() returns trigger language plpgsql as $$
declare
  c record;
begin
  perform 1 from app_user where tenant_id = new.tenant_id and id = new.user_id for no key update;
  if tg_op = 'INSERT' and exists (select 1 from user_role where tenant_id = new.tenant_id and user_id = new.user_id and role = new.role) then
    return new;
  end if;
  select rc.role_a, rc.role_b, rc.reason into c
    from role_conflict rc
    join user_role held on held.tenant_id = rc.tenant_id and held.user_id = new.user_id
     and held.role = case when rc.role_a = new.role then rc.role_b else rc.role_a end
     and (held.expires_at is null or held.expires_at > now())
   where rc.tenant_id = new.tenant_id and new.role in (rc.role_a, rc.role_b)
   limit 1;
  if found then
    raise exception 'ferrum: % and % are not held by one person: %', c.role_a, c.role_b, c.reason;
  end if;
  return new;
end $$;
create trigger user_role_conflict before insert or update of role, user_id on user_role
  for each row
  execute function user_role_conflict();

create table role_permission (
  tenant_id uuid not null,
  role text not null,
  entity text not null,
  can_read boolean not null default false,
  can_write boolean not null default false,
  can_create boolean not null default false,
  can_delete boolean not null default false,
  can_submit boolean not null default false,
  can_cancel boolean not null default false,
  can_export boolean not null default false,
  can_import boolean not null default false,
  if_owner boolean not null default false,
  read_groups text[] not null default '{}',
  write_groups text[] not null default '{}',
  -- A permission rule may apply only to records whose fields hold given values (`where` in a role's
  -- permissions). A role can then hold several rules for one entity, told apart by their condition.
  match_rule jsonb,
  rule_key text not null default '',
  via text check ((via is null) or (length(via) between 1 and 200)),
  -- The entity's actions a rule's holders may run on the records it reaches, whatever the action's own
  -- permission asks: a patient cancels their visit without being able to edit it.
  actions text[] not null default '{}',
  -- What a portal role's rule lets its holders see of a record's conversation: `read` the shared comments and
  -- files, `add` those and their own. Null: nothing.
  shared text check ((shared is null) or (shared in ('read', 'add'))),
  -- Fields a permission rule neither shows nor accepts (docs/builder/decisions.md BD-40), and whose a rule row is:
  -- `packaged` true, still as its package wrote it, so its package's later releases rewrite it; false, an
  -- administrator's (their edit makes it theirs), never rewritten; null, of unknown origin: only its `hide` follows the
  -- package while it is empty.
  hide text[] not null default '{}',
  packaged boolean,
  -- Which package wrote a packaged rule row (docs/builder/decisions.md BD-40, corrected): a packaged row follows the
  -- releases of that package only. Two packages declaring a rule for the same role and entity do not overwrite one
  -- another: the first row stays. Rows written packaged without a package are of unknown
  -- origin and no package claims them.
  package text,
  primary key (tenant_id, role, entity, if_owner, rule_key),
  foreign key (tenant_id, role) references role(tenant_id, name) on update cascade on delete cascade
);

-- Record-level scopes ("user permissions"): user may only see records linked to these values.
create table user_scope (
  tenant_id uuid not null,
  user_id uuid not null,
  entity text not null,
  value uuid not null,
  is_default boolean not null default false,
  primary key (tenant_id, user_id, entity, value),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);

-- A records key (records:Customer=read;SalesInvoice=read+create): its owner's rights narrowed to
-- the entities and actions it names, on the records API only (crate::auth::records_scope).
create table api_key (
  tenant_id uuid not null,
  id uuid not null,
  user_id uuid not null,
  name text not null check (length(name) between 1 and 100),
  prefix text not null unique,
  secret_hash text not null,
  created_at timestamptz not null default now(),
  expires_at timestamptz,
  revoked_at timestamptz,
  last_used_at timestamptz,
  -- An API key for one use only. `dav`: the app password of calendar and contacts apps, sent over
  -- HTTP Basic to /api/dav/... and refused everywhere else (docs/plan/collaboration.md, C5).
  scope text check ((scope in ('dav', 'scim', 'support')) or (scope ~ '^records:[A-Za-z0-9_]+=[a-z+]+(;[A-Za-z0-9_]+=[a-z+]+)*$')),
  -- A key's own networks (docs/operations.md, "Staff networks"): it works only from an address inside one of them.
  -- Empty: from anywhere.
  networks text[],
  primary key (tenant_id, id),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);

-- A workspace's slug names addresses: its apps' origins of their own ({app}--{slug}.{apps base},
-- docs/plan/apps-ui.md U8), whose browser storage must never reach another workspace. So once a
-- workspace has used a slug, no other workspace ever does: renaming or deleting a workspace retires
-- its old slug for good. Enforced here, whatever code changes slugs.
--
-- No row-level security: the guard must see every retired slug from any context (a rename made in
-- the workspace's own context included), and the table holds slugs and ids only.
create table tenant_slug_retired (
  slug text primary key,
  tenant_id uuid not null,
  retired_at timestamptz not null default now()
);

create table scim_group (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  display_name text not null check (length(display_name) between 1 and 200),
  external_id text check (length(external_id) <= 255),
  roles text[] not null default '{}',
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  unique (tenant_id, display_name)
);
create unique index scim_group_external on scim_group (tenant_id, external_id) where external_id is not null;

create table scim_group_member (
  tenant_id uuid not null,
  group_id uuid not null,
  user_id uuid not null,
  primary key (tenant_id, group_id, user_id),
  foreign key (tenant_id, group_id) references scim_group(tenant_id, id) on delete cascade,
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);
create index scim_group_member_user on scim_group_member (tenant_id, user_id);

-- Support access a workspace gives, and every use of it (crates/kernel/src/support.rs).
create table support_grant (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  level text not null check (level in ('health', 'diagnostics', 'assistance', 'incident')),
  grantee text not null check (length(grantee) between 1 and 200),
  entities text[] not null default '{}',
  can_write boolean not null default false,
  reason text not null check (length(reason) between 1 and 500),
  user_id uuid not null,
  created_by uuid,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  revoked_at timestamptz,
  revoked_by uuid,
  primary key (tenant_id, id)
);
create unique index support_grant_user on support_grant (tenant_id, user_id);

create table support_access (
  tenant_id uuid not null,
  grant_id uuid not null,
  seq bigint generated always as identity,
  at timestamptz not null default now(),
  method text not null,
  path text not null,
  -- Whether a request made with support access was turned away at the door, so a customer reading the list
  -- can tell what was tried from what was reached.
  refused boolean not null default false,
  primary key (tenant_id, seq),
  foreign key (tenant_id, grant_id) references support_grant(tenant_id, id) on delete cascade
);
create index support_access_grant on support_access (tenant_id, grant_id, seq desc);

-- Usage as it stood at the end of each day, so a bill can be explained later.
create table usage_day (
  tenant_id uuid not null references tenant(id) on delete cascade,
  day date not null,
  meter text not null,
  quantity bigint not null,
  primary key (tenant_id, day, meter)
);

-- A workspace can be closed: read-only, kept until a date, and deleted after that only by an operator.
-- That a workspace was deleted, when, and what went, for whoever asks later. None of its data is here.
create table tenant_deletion (
  id uuid primary key,
  slug text not null,
  closed_at timestamptz not null,
  retained_until date not null,
  deleted_at timestamptz not null default now(),
  rows jsonb not null,
  files int not null,
  note text not null
);

-- Segregation of duties (FERRUM-IDENTITY-014): pairs of roles one person may not hold together, as
-- an administrator declares them (raising suppliers and paying them, say). The database refuses a
-- grant that would join them, on every path a grant takes: by hand, through a SCIM group, from an
-- app. People who held both before the pair was declared are listed beside it until one goes.
create table role_conflict (
  tenant_id uuid not null,
  role_a text not null,
  role_b text not null,
  reason text not null check (length(reason) between 1 and 500),
  created_by uuid,
  created_at timestamptz not null default now(),
  check (role_a < role_b),
  primary key (tenant_id, role_a, role_b),
  foreign key (tenant_id, role_a) references role(tenant_id, name) on update cascade on delete cascade,
  foreign key (tenant_id, role_b) references role(tenant_id, name) on update cascade on delete cascade
);

-- Who stands in for someone while they are away (crates/kernel/src/process.rs, FEO-034): a task handed to
-- them may be decided by their substitute between these days, as they could have, never on the
-- substitute's own request.
create table person_away (
  tenant_id uuid not null references tenant(id) on delete cascade,
  user_id uuid not null,
  starts_on date not null,
  ends_on date not null,
  substitute uuid not null,
  reason text not null check (length(reason) between 1 and 500),
  created_by uuid,
  created_at timestamptz not null default now(),
  check (ends_on >= starts_on),
  constraint person_away_check1 check (substitute <> user_id),
  primary key (tenant_id, user_id, starts_on)
);
create index person_away_substitute on person_away (tenant_id, substitute, ends_on);

-- ---- Signing in ------------------------------------------------------------------------------------------------------

-- OAuth 2.1 for MCP and other API clients: self-registered public clients, one-time PKCE codes,
-- rotating refresh tokens. Secrets are stored as SHA-256 hashes only.
create table oauth_client (
  id text primary key,
  name text not null,
  redirect_uris text[] not null check (cardinality(redirect_uris) between 1 and 10),
  created_at timestamptz not null default now(),
  last_used_at timestamptz
);

create table oauth_code (
  code_hash text primary key,
  client_id text not null references oauth_client(id) on delete cascade,
  tenant_id uuid not null references tenant(id) on delete cascade,
  user_id uuid not null,
  redirect_uri text not null,
  code_challenge text not null,
  scope text not null,
  expires_at timestamptz not null,
  used_at timestamptz
);

create table oauth_refresh (
  token_hash text primary key,
  client_id text not null references oauth_client(id) on delete cascade,
  tenant_id uuid not null references tenant(id) on delete cascade,
  user_id uuid not null,
  scope text not null,
  expires_at timestamptz not null,
  revoked_at timestamptz,
  created_at timestamptz not null default now(),
  -- A refresh token ends with the person's other sessions: it remembers the token version it was
  -- issued under, and signing out everywhere, a new password or a deactivated account (each moves
  -- the version on) refuses it. Tokens issued before take the version their person has now.
  token_version int not null default 0
);
create index oauth_refresh_owner on oauth_refresh (client_id, tenant_id, user_id) where revoked_at is null;

create table user_recovery_code (
  tenant_id uuid not null,
  user_id uuid not null,
  code_hash text not null,
  used_at timestamptz,
  created_at timestamptz not null default now(),
  primary key (tenant_id, user_id, code_hash),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);

-- Single sign-on through OpenID Connect providers (Microsoft Entra ID, Okta, Google Workspace,
-- Keycloak, Auth0…), set up per workspace with the client secret kept in the vault (`has_client_secret`); the
-- sign-ins on their way through a provider (state, nonce and PKCE verifier, ten minutes); and each
-- person's link to their identity at the provider, so a reused email address cannot take over an
-- account.
create table sso_provider (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  label text not null check (length(label) between 1 and 60),
  issuer text not null,
  client_id text not null,
  has_client_secret boolean not null default false,
  domains text[] not null default '{}',
  auto_create boolean not null default false,
  default_roles text[] not null default '{}',
  enabled boolean not null default true,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  -- SAML 2.0 identity providers beside OpenID Connect ones (crates/kernel/src/sso.rs, crates/saml). A
  -- SAML provider keeps its metadata (the XML, or the https address it is fetched from at each sign-in)
  -- in place of a client; its entity ID stands in the issuer column. A SAML sign-in comes back as a
  -- response the provider posts to the server, which checks it and keeps who it names on the flow,
  -- under a one-time code the browser then finishes with, as with OpenID Connect.
  protocol text not null default 'oidc' check (protocol in ('oidc', 'saml')),
  saml_metadata text,
  -- A SAML provider's own addresses and single logout. `saml_sp_base`: the origin its entity ID and addresses are
  -- under, kept from when it was saved, so they never change under it; null: PUBLIC_URL's. `slo_out` (logout requests
  -- sent from here) and `slo_in` (taken from the provider): null when the provider has none.
  saml_sp_base text,
  slo_out boolean,
  slo_in boolean,
  constraint sso_provider_saml_metadata check ((protocol = 'saml') = (saml_metadata is not null)),
  primary key (tenant_id, id)
);

-- Ways to confirm it is you (docs/operations.md, "Asking again before sensitive work"): besides the password, a passkey
-- and signing in again at the identity provider. Each proof is bound to the browser session and the person it is for,
-- and is used once.
-- A passkey's challenge for a step-up names the session it is for.
--
-- Confirming it is you with a passkey of one's Ferrum account, at the accounts host, for a workspace's address that may
-- not use it (docs/operations.md, "Asking again before sensitive work"). Its request, the window's way back and the
-- one-use proof are a step-up's flow, as signing in again at a provider is; such a flow has no provider.
create table sso_flow (
  state_hash text primary key,
  tenant_id uuid not null,
  provider_id uuid,
  nonce text not null,
  verifier text not null,
  redirect_uri text not null,
  binding_hash text not null,
  created_at timestamptz not null default now(),
  code_hash text,
  answer jsonb,
  -- Signing in again at the identity provider: the session and person it is for, and the page on the host they work on
  -- that hands the proof to their dialog. Such a flow never signs anyone in (crates/kernel/src/sso.rs), and a sign-in's
  -- flow (these columns empty) never confirms anyone.
  step_up_session uuid,
  step_up_user uuid,
  step_up_back text,
  foreign key (tenant_id, provider_id) references sso_provider(tenant_id, id) on delete cascade
);
create index sso_flow_created on sso_flow (created_at);

-- The app shell (docs/plan/apps-ui.md): browser sessions per device, host names that lead to a
-- workspace, kinds of people, and which interface release each workspace runs.
-- One row per signed-in browser; the session cookie's token names it (`sid`). Revoking a row
-- signs that device out; `app_user.token_version` still signs out every device at once.
-- `actor` is who really acts when someone acts as `user_id` (support access, impersonation);
-- `step_up_at` is when the person last proved themselves again for a sensitive action.
create table user_session (
  tenant_id uuid not null,
  id uuid not null,
  user_id uuid not null,
  actor uuid,
  created_at timestamptz not null default now(),
  last_seen_at timestamptz not null default now(),
  step_up_at timestamptz,
  expires_at timestamptz not null,
  revoked_at timestamptz,
  user_agent text not null default '' check (length(user_agent) <= 300),
  ip text not null default '' check (length(ip) <= 64),
  -- Zones (docs/operations.md, "Zones"): the zone a browser session was opened in, shown where sessions are listed.
  -- Empty: opened without zones, or before them.
  zone text,
  -- What a SAML sign-in's logout needs, kept on its session and going with it: `{provider, name_id, format,
  -- session_index}`.
  saml jsonb,
  -- How a browser session's person last proved who they are (an ID token's `amr`): the steps of the sign-in that opened
  -- it, or the way of its last step-up. Null: its ID tokens say nothing of it.
  proof text[],
  primary key (tenant_id, id),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);
create index user_session_expired on user_session (expires_at);
create index user_session_live on user_session (tenant_id, user_id) where revoked_at is null;
create index user_session_saml on user_session (tenant_id, ((saml ->> 'provider')), ((saml ->> 'name_id'))) where (saml is not null) and (revoked_at is null);

-- Codes sent by email to sign in with (docs/plan/apps-ui.md §9.1): portal people have no password.
-- Only a hash is kept. A code lasts ten minutes, takes five wrong tries and works once; sending a
-- new one ends the ones before.
create table sign_in_code (
  id uuid default gen_random_uuid() primary key,
  tenant_id uuid not null,
  user_id uuid not null,
  code_hash text not null,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  attempts int not null default 0,
  used_at timestamptz,
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);
create index sign_in_code_expiry on sign_in_code (expires_at);
create index sign_in_code_user on sign_in_code (tenant_id, user_id, created_at desc);

-- Passkeys (WebAuthn; docs/plan/apps-ui.md §12.3, U6): credentials a person's devices keep, to sign
-- in without a password. The relying party is the host the passkey was made on (`rp_id`). A
-- credential belongs to one account in the cluster, so signing in finds the account by it.
create table passkey (
  tenant_id uuid not null,
  id uuid not null,
  user_id uuid not null,
  credential_id bytea not null unique check (length(credential_id) between 16 and 1023),
  rp_id text not null check (length(rp_id) between 1 and 253),
  -- ES256: the uncompressed P-256 point; RS256: n's length (2 bytes), n, then e; EdDSA: 32 bytes.
  public_key bytea not null,
  alg int not null check (alg = any (array['-7'::integer, '-8'::integer, '-257'::integer])),
  sign_count bigint not null default 0,
  name text not null default 'Passkey' check (length(name) between 1 and 100),
  transports text[] not null default '{}',
  created_at timestamptz not null default now(),
  last_used_at timestamptz,
  primary key (tenant_id, id),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);
create index passkey_user on passkey (tenant_id, user_id);

-- A challenge is handed out once and taken once (five minutes): a signed answer cannot be replayed.
create table webauthn_challenge (
  id uuid primary key,
  challenge bytea not null,
  purpose text not null check (purpose in ('register', 'sign_in', 'step_up')),
  tenant_id uuid,
  user_id uuid,
  rp_id text not null,
  expires_at timestamptz not null,
  session_id uuid
);
create index webauthn_challenge_expiry on webauthn_challenge (expires_at);

-- Codes sent to an address to prove it is the person's: to set a password at accounts, to finish
-- making an identity, to add a workspace. Only the newest works; five wrong tries spend it.
create table email_code (
  id uuid default gen_random_uuid() primary key,
  email citext not null check (length((email)::text) <= 254),
  code_hash text not null,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  attempts int not null default 0,
  used_at timestamptz
);
create index email_code_email on email_code (email, created_at desc);
create index email_code_expiry on email_code (expires_at);

-- A sign-in carried from the accounts host to a workspace's host: minted for one membership and
-- one host, taken once within a minute. `identity_id` is empty when the workspace's single sign-on
-- proved the person rather than an identity.
create table host_code (
  code_hash text primary key,
  tenant_id uuid not null,
  user_id uuid not null,
  identity_id uuid,
  host text not null check (length(host) <= 253),
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  used_at timestamptz,
  -- A code for another region's accounts host is bound to that host's PKCE challenge (S256), so
  -- only the server that asked for it can take it.
  challenge text check ((challenge is null) or (length(challenge) between 43 and 128)),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);
create index host_code_expiry on host_code (expires_at);

-- Web Push (docs/plan/apps-ui.md §12.5; the kernel's webpush.rs): the server's own keys, each
-- device's subscription with its browser's push service, and "push" as a way a kind of
-- notification reaches people (in the bar and on the devices where they turned it on).
-- Keys the server makes for itself, the private half kept in the vault (the installation's). `vapid` signs what it
-- sends to push services (RFC 8292); `public` is the half browsers subscribe with.
create table server_key (
  name text primary key check (length(name) between 1 and 40),
  public text not null default '' check (length(public) <= 200),
  created_at timestamptz not null default now()
);

-- OpenID Connect back-channel logout (crates/kernel/src/sso.rs). `sso_sid` says whose a provider's
-- session is, for logout tokens that name only the session; `sso_logout_seen` keeps a token from
-- being used twice.
create table sso_sid (
  tenant_id uuid not null,
  provider_id uuid not null,
  sid text not null check (length(sid) between 1 and 255),
  user_id uuid not null,
  created_at timestamptz not null default now(),
  primary key (tenant_id, provider_id, sid),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);
create index sso_sid_created on sso_sid (created_at);

create table sso_logout_seen (
  tenant_id uuid not null references tenant(id) on delete cascade,
  provider_id uuid not null,
  jti text not null check (length(jti) between 1 and 255),
  seen_at timestamptz not null default now(),
  primary key (tenant_id, provider_id, jti)
);
create index sso_logout_seen_at on sso_logout_seen (seen_at);

-- Each workspace's own SAML key: it signs the workspace's requests and opens assertions encrypted to
-- it; its certificate is in the workspace's metadata. Made the first time it is wanted, the private
-- half kept in the vault.
create table saml_sp_key (
  tenant_id uuid primary key references tenant(id) on delete cascade,
  certificate text not null,
  created_at timestamptz not null default now()
);

-- Failed sign-ins of a known account, counted by the hour (docs/operations.md, "Staff networks"): the
-- audit trail records the first few of an hour one by one, then one summary with how many more there were.
create table sign_in_failure (
  tenant_id uuid not null,
  user_id uuid not null,
  hour timestamptz not null,
  n int not null default 1,
  primary key (tenant_id, user_id, hour),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);
create index sign_in_failure_hour on sign_in_failure (hour);

-- A sign-in from a new device (docs/operations.md, "Staff networks"): the devices each person has signed in
-- from, by the browser's user agent with its version numbers taken out. Kept with the account, gone with it.
create table known_device (
  tenant_id uuid not null,
  user_id uuid not null,
  device text not null,
  first_seen timestamptz not null default now(),
  primary key (tenant_id, user_id, device),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);

-- SAML: encrypted assertions and single logout (crates/kernel/src/sso.rs and saml_logout.rs, crates/saml).
-- The keys a workspace's SAML assertions may be encrypted to, apart from the key its requests are signed with. Made
-- the first time wanted (by whichever replica gets there first: one current key at most), the private half kept in the
-- vault. A replaced key keeps being published and accepted until `retires_at`.
create table saml_enc_key (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  certificate text not null,
  created_at timestamptz not null default now(),
  retires_at timestamptz,
  primary key (tenant_id, id)
);
create unique index saml_enc_key_current on saml_enc_key (tenant_id) where retires_at is null;

-- A logout this server starts, for ten minutes: the ticket the browser carries to send the request, then the
-- request's ID until the provider answers, and where the browser lands after.
create table saml_logout (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  ticket_hash text unique,
  request_id text unique,
  provider_id uuid not null,
  saml jsonb not null,
  return_to text not null,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);

-- Ferrum as an OpenID Connect provider for the programs that run beside it (crates/kernel/src/oidc.rs; docs/api.md,
-- "Signing in to programs beside Ferrum"; docs/plan/stateless.md, D8).
--
-- Every key here is unique within its workspace, never across the installation (the issuer a request comes to says
-- which workspace it is for), so that a copy of a workspace into this database that takes every table with a
-- `tenant_id` (the copier's rule, unless a table is named to be left out) never meets the original's keys. A sandbox
-- has no companion client: this build copies none, and takes away any such a copy brought (crates/kernel/src/oidc.rs).
-- (The build before makes no sandbox on a database this one migrated: it is a guest there.)
-- A program an installed app runs beside Ferrum (its companion), declared by the app's own code for a workspace: a
-- confidential client whose secret the vault made as the app's own. Only the secret's SHA-256 digest is here.
create table oauth_companion (
  client_id text not null,
  tenant_id uuid not null references tenant(id) on delete cascade,
  app text not null,
  name text not null,
  redirect_uris text[] not null check (cardinality(redirect_uris) between 1 and 10),
  secret_hash text not null,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, client_id),
  unique (tenant_id, app, name)
);

-- One sign-in to a companion, from its authorization request on: waiting while its person confirms it is them, then a
-- one-time code (a minute), then the access token's digest (five minutes). Codes and tokens are stored only as SHA-256
-- digests. Kept a day after it ends, so that a client never has a nonce taken twice.
create table oidc_code (
  code_hash text not null,
  client_id text not null,
  tenant_id uuid not null,
  user_id uuid not null,
  session_id uuid not null,
  token_version int not null,
  issuer text not null,
  redirect_uri text not null,
  code_challenge text not null,
  scope text not null,
  nonce text,
  state text not null default '',
  max_age int,
  prompt_login boolean not null default false,
  waiting boolean not null default false,
  auth_time timestamptz,
  amr text[],
  access_hash text,
  access_expires_at timestamptz,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  used_at timestamptz,
  primary key (tenant_id, code_hash),
  unique (tenant_id, access_hash),
  foreign key (tenant_id, client_id) references oauth_companion(tenant_id, client_id) on delete cascade
);
create unique index oidc_code_nonce on oidc_code (tenant_id, client_id, nonce) where nonce is not null;

-- The subject a workspace's ID tokens name a person by: random, so that it says nothing else about them, and kept, so
-- that it never changes.
create table oidc_subject (
  tenant_id uuid not null,
  user_id uuid not null,
  subject text not null,
  primary key (tenant_id, user_id),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);

-- Which browser session signed in to which companion, until its app has been told that it ended
-- (`ferrum.companion.signed_out`), and a week after. It outlives its session's row, so that a session deleted (an
-- erasure) is still told.
create table oidc_sign_in (
  tenant_id uuid not null,
  session_id uuid not null,
  client_id text not null,
  user_id uuid not null,
  token_version int not null,
  signed_in_at timestamptz not null default now(),
  ended_at timestamptz,
  primary key (tenant_id, session_id, client_id),
  foreign key (tenant_id, client_id) references oauth_companion(tenant_id, client_id) on delete cascade
);
create index oidc_sign_in_live on oidc_sign_in (signed_in_at) where ended_at is null;

-- ---- Addresses and the interface -------------------------------------------------------------------------------------

-- Per-user interface preferences (list columns and sort, saved filters, collapsed panels): small
-- JSON documents keyed by a scope such as `list:SalesInvoice`, versioned so two open tabs never
-- silently overwrite each other.
create table ui_pref (
  tenant_id uuid not null references tenant(id) on delete cascade,
  user_id uuid not null,
  scope text not null check (scope ~ '^[A-Za-z0-9:_.-]{1,120}$'),
  value jsonb not null,
  version bigint not null default 1,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, user_id, scope)
);

-- Hosts the operator vouches for (`ferrum task add_host`, docs/operations.md, "Zones"): names that only resolve inside a
-- private network, which the server cannot verify over the internet. Served as verified (their `verified_at` is set
-- when they are added), never checked by the verifier, which looks at `custom` hosts alone.
-- Host names that lead to a workspace: its subdomain of the cluster's base domain, or a custom
-- domain the workspace proved it owns (a TXT record carrying `verify_token`). `target` is what
-- the host serves: `shell` (the shell and every app), `portal` (portal pages only) or
-- `site:{app}` (an app's public site at the root); or `mail`: a domain of the workspace's own
-- claimed for mail, proven by its DNS record alone, which serves nothing and is never a main host.
-- The host is the key, so a domain is one workspace's, whatever it is used for. One primary host
-- per workspace and target carries links and redirects. A released host (renamed or deleted
-- workspace) redirects for 90 days, then nobody may claim it for a year. Read before any session
-- exists, so it is a cluster table without row-level security.
create table tenant_host (
  host text primary key check ((host ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)+$') and (length(host) <= 253)),
  tenant_id uuid not null references tenant(id) on delete cascade,
  kind text not null check (kind in ('subdomain', 'custom', 'operator')),
  target text not null default 'shell' check ((target in ('shell', 'portal', 'mail')) or (target ~ '^site:[a-z][a-z0-9-]{1,39}$')),
  is_primary boolean not null default false,
  verify_token text not null,
  verified_at timestamptz,
  checked_at timestamptz,
  failures int not null default 0,
  released_at timestamptz,
  created_at timestamptz not null default now(),
  -- Zones (docs/operations.md, "Zones"): the zone a host's requests are in, on replicas not pinned to one. Empty: the
  -- caller's network or the default zone decides. Only `ferrum task add_host` and `set_host_zone` set it, never a
  -- workspace's administrator.
  zone text check (zone ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
  -- A released host's leaving period (docs/operations.md, "Changing or removing an address"): until this moment it
  -- still serves the people already signed in there, so that what their browsers keep for it can be sent; then it
  -- redirects as a released host does. Empty: released with no leaving period.
  leaving_until timestamptz,
  -- What an address of a workspace shows (docs/operations.md, "Addresses of a workspace"), for a `shell` address: the app
  -- `/` lands on (`home`), the apps its bar lists (`apps`; empty: all of them) and its name in the bar (`label`). They
  -- choose what a person sees first, never what the address admits: every app opens on every `shell` address, and a zone
  -- stays the operator's. `base_name`: the name an administrator gave an address under the base domain,
  -- `{base_name}--{slug}.{base}`; empty for every other address.
  home text check (home ~ '^[a-z][a-z0-9-]{1,39}$'),
  apps text[] check (cardinality(apps) <= 50),
  label text check (length(label) between 1 and 40),
  base_name text check ((base_name ~ '^[a-z0-9]([a-z0-9-]{0,28}[a-z0-9])?$') and (base_name !~ '--')),
  -- A domain of a workspace's own proven by its owner (docs/operations.md, "Custom domains"): when it became verified, its
  -- DNS held `_ferrum.{host}` TXT `ferrum-domain={verify_token}`, a token no other workspace's row ever has. Empty: never
  -- proven by a record. One once proven is verified again by pointing here alone.
  proved_at timestamptz,
  -- A forwarding address (docs/operations.md, "Addresses of a workspace"): a `shell` address that serves no workspace and
  -- sends each page on to the workspace's main address (`/` to its `home` there). One that served before it forwards keeps
  -- serving those signed in there until `leaving_until`, as a removed address does.
  forward boolean not null default false,
  check ((target <> 'mail') or ((kind = 'custom') and (not is_primary)))
);
create unique index tenant_host_primary on tenant_host (tenant_id, target) where is_primary and (released_at is null);
create index tenant_host_tenant on tenant_host (tenant_id);

-- Interface releases: one row per (app, digest), the files of which live in object storage
-- under `ui/{app}/{digest}/`. `entry` is the app's index.html split around where the shell goes.
-- Kept for 30 days after no workspace uses them, so open tabs keep loading their chunks after an
-- upgrade.
create table ui_release (
  app text not null check (app ~ '^[a-z][a-z0-9-]{1,39}$'),
  digest text not null check (digest ~ '^[0-9a-f]{64}$'),
  files jsonb not null,
  entry jsonb not null,
  created_at timestamptz not null default now(),
  last_used_at timestamptz not null default now(),
  primary key (app, digest)
);

-- Which release an app version runs. Versions that did not change the interface share one.
create table ui_release_version (
  app text not null,
  version text not null check (length(version) <= 64),
  digest text not null,
  created_at timestamptz not null default now(),
  primary key (app, version, digest),
  foreign key (app, digest) references ui_release(app, digest) on delete cascade
);

-- A workspace pinned to an interface release other than the installed one (rollback, canary).
create table tenant_ui_pin (
  tenant_id uuid not null references tenant(id) on delete cascade,
  app text not null,
  digest text not null,
  pinned_at timestamptz not null default now(),
  pinned_by uuid,
  primary key (tenant_id, app),
  foreign key (app, digest) references ui_release(app, digest)
);

-- Origins that may frame one of an app's public pages (docs/builder/decisions.md BD-24): a form embedded in a
-- website. None by default; a page with no row stays unframeable.
create table public_frame (
  tenant_id uuid not null references tenant(id) on delete cascade,
  app text not null,
  path text not null check (length(path) between 1 and 300),
  origins text[] not null check (cardinality(origins) between 1 and 10),
  primary key (tenant_id, app, path)
);

-- The addresses of a workspace that left this database (deleted, or moved away and finished), kept from every other
-- workspace until `until`, as a released address is (`HOST_QUARANTINE_DAYS`): the workspace's rows go, so the names are
-- kept here. No workspace column, so nothing that deletes, copies or moves a workspace's rows reaches it.
create table host_quarantine (
  host text primary key,
  until timestamptz not null,
  -- Whose addresses the kept hosts were when that workspace moved away (`finish_move`) rather than being deleted: it may
  -- come back, and takes back those it holds again; every other workspace, one moving in among them, is refused them until
  -- `until` (crates/kernel/src/workspace_move.rs). Empty for a deleted workspace, which never comes back, so its id is not
  -- kept. Not `tenant_id` and no reference to `tenant`, so nothing that deletes, copies or moves a workspace's rows reaches it.
  workspace uuid
);

-- ---- Packages, apps and plugins --------------------------------------------------------------------------------------

create table installed_package (
  tenant_id uuid not null references tenant(id) on delete cascade,
  name text not null,
  version text not null,
  installed_at timestamptz not null default now(),
  primary key (tenant_id, name)
);

create table meta_overlay (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  seq bigint generated always as identity,
  entity text not null,
  op jsonb not null,
  is_active boolean not null default true,
  note text,
  created_by uuid,
  created_at timestamptz not null default now(),
  -- What a customization replaced, when it was made: how an upgrade that changes the same thing is
  -- noticed (crates/kernel/src/customize.rs). A customization with none is not compared.
  base jsonb,
  primary key (tenant_id, id)
);
create index meta_overlay_seq on meta_overlay (tenant_id, seq);

create table naming_counter (
  tenant_id uuid not null references tenant(id) on delete cascade,
  key text not null,
  value bigint not null check (value >= 0),
  primary key (tenant_id, key)
);

-- Cluster-level bookkeeping (not tenant data, no RLS).
create table cluster_package_sql (
  package text not null,
  file text not null,
  checksum text not null,
  applied_at timestamptz not null default now(),
  primary key (package, file)
);

create table cluster_state (
  key text primary key,
  value text not null,
  updated_at timestamptz not null default now()
);

-- Tenant plugins: sandboxed WebAssembly modules bound to document hooks.
create table plugin (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  name text not null check (name ~ '^[a-z][a-z0-9_-]{1,62}$'),
  version text not null default '0.1.0',
  description text,
  sha256 text not null,
  wasm bytea not null check (octet_length(wasm) <= 8388608),
  -- [{"entity": "SalesInvoice", "stage": "validate"}, ...]
  hooks jsonb not null default '[]' check (jsonb_typeof(hooks) = 'array'),
  fuel_limit bigint not null default 50000000 check (fuel_limit between 10000 and 2000000000),
  memory_limit_mb int not null default 32 check (memory_limit_mb between 1 and 256),
  enabled boolean not null default true,
  created_by uuid,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  -- Plugins as platforms: a manifest (routes, subscriptions, schedules, sockets, permissions, outbound
  -- hosts, entities), admin-set configuration, a service user to act as, private key-value storage,
  -- deduplicated event delivery and multi-replica-safe schedules.
  manifest jsonb not null default '{}' check (jsonb_typeof(manifest) = 'object'),
  config jsonb not null default '{}' check (jsonb_typeof(config) = 'object'),
  user_id uuid,
  primary key (tenant_id, id),
  unique (tenant_id, name)
);

create table plugin_kv (
  tenant_id uuid not null references tenant(id) on delete cascade,
  plugin_id uuid not null,
  key text not null check (length(key) between 1 and 200),
  value jsonb not null check (pg_column_size(value) <= 262144),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, plugin_id, key),
  foreign key (tenant_id, plugin_id) references plugin(tenant_id, id) on delete cascade
);

create table plugin_event_run (
  tenant_id uuid not null references tenant(id) on delete cascade,
  plugin_id uuid not null,
  outbox_id bigint not null,
  ran_at timestamptz not null default now(),
  -- A plugin's run for an event is recorded when it starts and again when it ends. A row that never
  -- finished means the run may have done its work: the retry is told so, and how many came before it.
  attempts int not null default 1 check (attempts >= 1),
  finished_at timestamptz,
  primary key (tenant_id, plugin_id, outbox_id),
  foreign key (tenant_id, plugin_id) references plugin(tenant_id, id) on delete cascade
);

create table plugin_schedule (
  tenant_id uuid not null references tenant(id) on delete cascade,
  plugin_id uuid not null,
  name text not null,
  every_secs int not null check (every_secs >= 60),
  next_run_at timestamptz not null default now(),
  last_run_at timestamptz,
  last_error text,
  primary key (tenant_id, plugin_id, name),
  foreign key (tenant_id, plugin_id) references plugin(tenant_id, id) on delete cascade
);
create index plugin_schedule_due on plugin_schedule (next_run_at);

-- Apps. A release made available on the cluster contributes packages to the catalog (tables are
-- created for every workspace to use); workspaces install it separately. Bundles themselves live
-- in object storage; everything a replica needs to rebuild its catalog is here.
create table app_release (
  id uuid default gen_random_uuid() primary key,
  app_id text not null check (app_id ~ '^[a-z][a-z0-9-]{1,39}$'),
  version text not null,
  runtime text not null check (runtime in ('native', 'wasm', 'meta', 'service')),
  manifest jsonb not null,
  digest text not null,
  signatures jsonb,
  source text not null,
  bundle_key text,
  -- package name -> {relative path -> text}
  packages jsonb not null default '{}' check (jsonb_typeof(packages) = 'object'),
  plugin_sha256 text,
  status text not null default 'active' check (status in ('active', 'superseded', 'revoked', 'failed')),
  status_reason text,
  -- Workspace-private uploads (sandboxed code only, no tables) are visible to that workspace alone.
  private_tenant uuid references tenant(id) on delete cascade,
  created_by uuid,
  created_at timestamptz not null default now()
);
create unique index app_release_active on app_release (app_id) where (status = 'active') and (private_tenant is null);
create unique index app_release_private_active on app_release (app_id, private_tenant) where (status = 'active') and (private_tenant is not null);
create unique index app_release_private_version on app_release (app_id, version, private_tenant) where private_tenant is not null;
create unique index app_release_version on app_release (app_id, version) where private_tenant is null;

create table tenant_app (
  tenant_id uuid not null references tenant(id) on delete cascade,
  app_id text not null,
  version text not null,
  enabled boolean not null default true,
  consent_digest text,
  plugin_id uuid,
  installed_by uuid,
  installed_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  -- The app whose installation brought this one in as something it requires (a suite's members, an
  -- app's prerequisite); null when an administrator installed it for itself. Removing that app
  -- offers to remove what it brought that nothing else needs (docs/plan/apps-ui.md §13).
  brought_by text,
  -- An app installed but waiting for its required resources: what it waits for, until it has it.
  waiting_for text[],
  primary key (tenant_id, app_id)
);

-- What each app store last told this cluster: when its newest applied revocation list was
-- generated, so an older (stale or replayed) list is ignored instead of silently holding back
-- newer revocations. Shared by every replica.
create table app_store_state (
  store text primary key,
  revocations_at timestamptz not null,
  updated_at timestamptz not null default now()
);

-- What has been established about one exact artifact. A label is of a digest, so a new build of the
-- same app starts with none, however much it resembles the last one.
create table app_label (
  app_id text not null,
  digest text not null,
  dimension text not null check (dimension in ('community', 'security_reviewed', 'operationally_verified', 'provider_qualified', 'production_supported')),
  profile text not null,
  evidence text not null check (length(trim(both from evidence)) > 0),
  reviewer text not null check (length(trim(both from reviewer)) > 0),
  granted_at timestamptz not null default now(),
  expires_at timestamptz,
  primary key (app_id, digest, dimension, profile)
);

-- Licences of apps sold in a store (ferrum-store's GET /v1/licenses/{license}): the one a workspace's
-- administrator gave for each app, and what the store last said of it. A sold app installs or
-- upgrades only with a live one.
create table app_licence (
  tenant_id uuid not null references tenant(id) on delete cascade,
  app_id text not null,
  store text not null,
  licence uuid not null,
  live boolean not null,
  period_end timestamptz,
  checked_at timestamptz not null default now(),
  last_error text,
  primary key (tenant_id, app_id)
);

-- Backfills packages declare (FERRUM-PLATFORM-010): one row per workspace and backfill, from when it
-- started. The last row a batch reached and how many it changed are written with the batch, so a
-- restart picks up there; when it is done, how many rows still needed it.
create table backfill_run (
  tenant_id uuid not null references tenant(id) on delete cascade,
  package text not null,
  name text not null,
  reached uuid,
  changed bigint not null default 0,
  remaining bigint,
  started_at timestamptz not null default now(),
  finished_at timestamptz,
  -- The installed version of its package a backfill was last queued under (docs/builder/decisions.md BD-39): one whose
  -- job failed is queued again once when its package changes version, the release that fixes it.
  version text,
  primary key (tenant_id, package, name)
);

-- Packs installed in a workspace (docs/api.md, "Packs"; docs/builder/decisions.md BD-07): which pack, its
-- version and the digest of what was installed, who installed it and when, and what the install did to
-- each section (created, changed, left alone).
create table pack_install (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  name text not null check (length(name) between 1 and 64),
  version bigint not null check (version >= 1),
  digest text not null,
  installed_by uuid,
  installed_at timestamptz not null default now(),
  summary jsonb not null,
  primary key (tenant_id, id)
);

-- Floors of the two-version window (crates/kernel/src/window.rs; docs/operations.md, "Protections an older build would
-- not enforce"): one row per protection in use, with the date of the first build that enforces it. The recorded
-- `schema_compatible_from` is never earlier than the latest of them, so a rollback to a build that would drop one of
-- these protections is refused at start. A temporary one (a move under way) has a row per holder and goes with the
-- last; a lasting one has the holder ''.
create table window_floor (
  feature text not null,
  holder text not null default '',
  since timestamptz not null,
  what text not null,
  temporary boolean not null default false,
  switched_on_at timestamptz not null default now(),
  switched_on_by timestamptz not null,
  primary key (feature, holder)
);

-- ---- Records ---------------------------------------------------------------------------------------------------------

-- Tenant-defined entities live here; every field is in `ext`.
create table custom_record (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  entity text not null,
  number text,
  docstatus smallint not null default 0 check (docstatus between 0 and 2),
  version int not null default 1 check (version >= 1),
  workflow_state text,
  amended_from uuid,
  created_at timestamptz not null default now(),
  created_by uuid,
  updated_at timestamptz not null default now(),
  updated_by uuid,
  ext jsonb not null default '{}',
  -- Custom entities that hold a business document (docs/builder/decisions.md BD-06): lines and unique keys,
  -- by configuration, with no table per workspace.
  --
  -- Lines: a custom entity's table rows are custom records of its child entity, under their parent.
  -- Each custom child entity is the lines of one table field, so its name tells one table's rows from another's.
  parent_id uuid,
  idx int,
  primary key (tenant_id, id)
);
create index custom_record_entity on custom_record (tenant_id, entity, created_at desc);
create index custom_record_lines on custom_record (tenant_id, parent_id, idx) where parent_id is not null;
create unique index custom_record_number on custom_record (tenant_id, entity, number) where number is not null;

-- Lines a workspace adds to a package's entity (docs/builder/decisions.md BD-06, 10 November 2026): rows of
-- `custom_record` whose `parent_id` is a record of the package's own table. The foreign key from `parent_id` to
-- `custom_record` could not hold such a row, so it gives way to a trigger that does what its cascade did: when
-- custom records go, the custom records under them go too, at any depth. A package's record takes its lines through
-- the kernel (`store::delete_row`, and an uninstall that deletes the package's data); `packaged_lines_it` checks
-- that no line is left without its record.
create function ferrum_custom_lines_cascade() returns trigger language plpgsql as $$
begin
  -- A statement trigger runs for a delete of no rows too: stop there, or it would never end.
  if exists (select 1 from gone) then
    delete from custom_record c using gone g where c.tenant_id = g.tenant_id and c.parent_id = g.id;
  end if;
  return null;
end $$;
create trigger custom_record_lines_cascade after delete on custom_record referencing old table as gone
  for each statement
  execute function ferrum_custom_lines_cascade();

-- Collaboration on any record: comments, assignments (to-dos), tags and followers. Access follows
-- the record itself (checked by the kernel): whoever can read a record can see and add these.
create table record_comment (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  entity text not null,
  record_id uuid not null,
  body text not null check (length(body) between 1 and 20000),
  created_by uuid not null,
  created_at timestamptz not null default now(),
  edited_at timestamptz,
  -- Comments and files shared with outside people (docs/builder/decisions.md BD-22): a portal person sees only what staff
  -- mark, and what outside people write themselves.
  shared boolean not null default false,
  primary key (tenant_id, id)
);
create index record_comment_record on record_comment (tenant_id, entity, record_id, created_at);

create table record_assignment (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  entity text not null,
  record_id uuid not null,
  user_id uuid not null,
  assigned_by uuid not null,
  note text check (length(note) <= 2000),
  due_date date,
  priority text not null default 'Medium' check (priority in ('Low', 'Medium', 'High')),
  status text not null default 'Open' check (status in ('Open', 'Done', 'Cancelled')),
  created_at timestamptz not null default now(),
  closed_at timestamptz,
  closed_by uuid,
  primary key (tenant_id, id),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);
-- One open assignment per user and record.
create unique index record_assignment_open on record_assignment (tenant_id, entity, record_id, user_id) where status = 'Open';
create index record_assignment_record on record_assignment (tenant_id, entity, record_id);
create index record_assignment_user on record_assignment (tenant_id, user_id, status, due_date);

create table record_tag (
  tenant_id uuid not null references tenant(id) on delete cascade,
  entity text not null,
  record_id uuid not null,
  tag text not null check (length(tag) between 1 and 60),
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, entity, record_id, tag)
);
create index record_tag_tag on record_tag (tenant_id, entity, tag);

create table record_follower (
  tenant_id uuid not null references tenant(id) on delete cascade,
  entity text not null,
  record_id uuid not null,
  user_id uuid not null,
  created_at timestamptz not null default now(),
  primary key (tenant_id, entity, record_id, user_id),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);
create index record_follower_user on record_follower (tenant_id, user_id);

-- Which secret fields of which records are set (passwords, API keys): each value is a secret of its owner in the vault,
-- bound to its tenant, entity, record and field. Never part of documents, events, the audit trail or exports.
create table record_secret (
  tenant_id uuid not null references tenant(id) on delete cascade,
  entity text not null,
  record_id uuid not null,
  field text not null,
  updated_at timestamptz not null default now(),
  updated_by uuid,
  primary key (tenant_id, entity, record_id, field)
);

-- Documents people edit together live (docs/plan/collaboration.md, C3; the server's coedit.rs).
-- Each document's changes (Yjs updates) take the next number of its own sequence under the row
-- lock of `coedit_doc`, so they commit in order; every 500 are folded into the snapshot. The
-- package owning a document (its `DocHost`) keeps everything else about it.
create table coedit_doc (
  tenant_id uuid not null references tenant(id) on delete cascade,
  package text not null,
  id uuid not null,
  seq bigint not null default 0,
  snapshot bytea,
  snapshot_upto bigint not null default 0,
  -- The last change handed to the package as a save, when, and who changed it since.
  saved_seq bigint not null default 0,
  saved_at timestamptz,
  editors uuid[] not null default '{}',
  updated_at timestamptz not null default now(),
  primary key (tenant_id, package, id)
);
-- Documents changed but not handed to their package (a replica stopped first): saved by a sweep.
create index coedit_doc_unsaved on coedit_doc (updated_at) where saved_seq < seq;

create table coedit_update (
  tenant_id uuid not null,
  package text not null,
  doc uuid not null,
  seq bigint not null,
  data bytea not null,
  at timestamptz not null default now(),
  primary key (tenant_id, package, doc, seq),
  foreign key (tenant_id, package, doc) references coedit_doc(tenant_id, package, id) on delete cascade
);

-- A record merged into another (crates/kernel/src/merge.rs). Its row stays: an append-only ledger may
-- name it for ever, by a foreign key. This says what it became.
create table record_alias (
  tenant_id uuid not null references tenant(id) on delete cascade,
  entity text not null,
  alias_id uuid not null,
  canonical_id uuid not null,
  reason text not null check (length(reason) between 1 and 500),
  merged_by uuid,
  merged_at timestamptz not null default now(),
  primary key (tenant_id, entity, alias_id)
);
create index record_alias_canonical on record_alias (tenant_id, entity, canonical_id);

-- A list view a team shares (crates/kernel/src/views.rs). A view is a question, not an answer: what
-- each person gets from it is still what they may read.
create table shared_view (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  entity text not null,
  name text not null check (length(name) between 1 and 80),
  value jsonb not null default '{}',
  created_by uuid not null,
  version int not null default 1,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
create unique index shared_view_name on shared_view (tenant_id, entity, lower(name));

-- Unique keys: one row per value a record holds of one of its entity's keys (`key` names the fields,
-- `value` is their values as a JSON array). The primary key enforces uniqueness; empty values and
-- cancelled documents hold none.
create table custom_key (
  tenant_id uuid not null references tenant(id) on delete cascade,
  entity text not null,
  key text not null,
  value text not null,
  record_id uuid not null,
  primary key (tenant_id, entity, key, value),
  foreign key (tenant_id, record_id) references custom_record(tenant_id, id) on delete cascade
);
create index custom_key_record on custom_key (tenant_id, record_id);

-- Service targets (docs/api.md, "Service targets"; docs/builder/decisions.md BD-04): one row per record and
-- target, kept by the record's saves. `round` counts the rounds begun; a round not started has no
-- `started_at`. At a pause `worked_secs` grows by the open time since `resumed_at`; at a resume `resumed_at`
-- is now and `due_at` the remaining time from now. `breached_at` is set once per round, by an update that
-- only succeeds while it is empty.
create table record_target (
  tenant_id uuid not null references tenant(id) on delete cascade,
  entity text not null,
  record_id uuid not null,
  target text not null,
  round int not null default 0,
  started_at timestamptz,
  resumed_at timestamptz,
  paused_at timestamptz,
  worked_secs bigint not null default 0,
  within_secs bigint not null default 0,
  due_at timestamptz,
  met_at timestamptz,
  breached_at timestamptz,
  -- What a round was last worked out from (docs/builder/decisions.md BD-41): the calendar it counts on ('' plain
  -- time) and the record's start moment it was counted from. A change of either on an open round works its due moment
  -- out again once. `anchored` false: a round whose anchors are recorded at its next save
  -- without working anything out again.
  calendar text,
  start_from timestamptz,
  anchored boolean not null default false,
  primary key (tenant_id, entity, record_id, target)
);
-- What the minute loop sweeps: running rounds past their due moment, not yet breached.
create index record_target_due on record_target (due_at) where (started_at is not null) and (met_at is null) and (paused_at is null) and (breached_at is null);

-- Document templates (crates/kernel/src/documents.rs, docs/builder/decisions.md BD-21).
-- A workspace's own templates, and its edited copies of packages' defaults: `base` is the digest of the default an
-- edit started from, so that a release changing that default is told, never written over the edit.
create table document_template (
  tenant_id uuid not null references tenant(id) on delete cascade,
  key text not null check (length(key) between 1 and 100),
  def jsonb not null,
  base text,
  updated_by uuid,
  updated_at timestamptz not null default now(),
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, key)
);

-- Each request to sign up, kept for the limits (per address and per network address, across replicas) and, when it
-- sent a code, until the code is confirmed: nobody is made a person of the workspace before that.
create table portal_signup (
  id uuid primary key,
  tenant_id uuid not null references tenant(id) on delete cascade,
  email citext not null,
  full_name text not null,
  role text not null,
  passed jsonb not null default '{}',
  network text not null,
  code_hash text,
  attempts int not null default 0,
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  used_at timestamptz
);
create index portal_signup_email on portal_signup (tenant_id, email, created_at desc);
create index portal_signup_expiry on portal_signup (created_at);
create index portal_signup_network on portal_signup (tenant_id, network, created_at desc);

-- Signed action links (docs/builder/decisions.md BD-23): "a person with this link may do this one thing to this one
-- record". The link's token is 256 random bits handed out once; only its SHA-256 is kept.
create table action_link (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  digest text not null,
  entity text not null,
  record_id uuid not null,
  -- The entity's actions it may run (`view` alone: it only shows).
  actions text[] not null check (cardinality(actions) between 1 and 8),
  -- The fields its page shows, an allow-list.
  show text[] not null default '{}',
  -- Parameters the issuer fixed; the visitor cannot change them.
  params jsonb not null default '{}',
  max_uses int not null default 1 check (max_uses between 1 and 100),
  uses int not null default 0,
  expires_at timestamptz not null,
  -- An address or a number a one-time code goes to before the action runs.
  code_to text,
  -- Who it was issued to, as the record's history names the guest.
  recipient text check ((recipient is null) or (length(recipient) <= 200)),
  frame_origins text[] not null default '{}',
  issued_by uuid,
  created_at timestamptz not null default now(),
  revoked_at timestamptz,
  -- Signed action links (docs/builder/decisions.md BD-38): when a visitor first acted on its page (`opened_at`, from
  -- the page's own first POST-back, which link-preview fetchers and mail scanners never send), and the columns of
  -- each table field it shows (`columns`, `{field: [column, ...]}`).
  opened_at timestamptz,
  columns jsonb not null default '{}',
  primary key (tenant_id, id),
  unique (tenant_id, digest)
);
create index action_link_record on action_link (tenant_id, entity, record_id);

-- Print formats (crates/kernel/src/documents.rs and layout.rs, docs/builder/decisions.md BD-28).
-- The workspace's chosen default document per entity: a choice of its own, so that making a package's format the
-- default does not make an edited copy of it.
create table document_default (
  tenant_id uuid not null references tenant(id) on delete cascade,
  entity text not null,
  key text not null check (length(key) between 1 and 100),
  updated_by uuid,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, entity)
);

-- Every saved version of a workspace's template, the last 50 per key kept; a restore is a save of an old one.
create table document_template_version (
  tenant_id uuid not null references tenant(id) on delete cascade,
  key text not null,
  n int not null,
  def jsonb not null,
  saved_by uuid,
  saved_at timestamptz not null default now(),
  primary key (tenant_id, key, n)
);

-- Views and votes on published record pages (docs/builder/decisions.md BD-42). No personal data: a visitor is a
-- keyed digest of a first-party cookie's random value (under the workspace's own key, `published_key`), a view's
-- digest taken with its day, so one day's cannot be matched to another's, a vote's with its record; an address is
-- a keyed digest with its hour, kept two hours to limit how much one address counts.
create table published_key (
  tenant_id uuid primary key references tenant(id) on delete cascade,
  key bytea not null
);

create table published_view (
  tenant_id uuid not null references tenant(id) on delete cascade,
  list text not null,
  record_id uuid not null,
  day date not null,
  visitor bytea not null,
  primary key (tenant_id, list, record_id, day, visitor)
);
create index published_view_day on published_view (tenant_id, day);

create table published_vote (
  tenant_id uuid not null references tenant(id) on delete cascade,
  list text not null,
  record_id uuid not null,
  visitor bytea not null,
  up boolean not null,
  primary key (tenant_id, list, record_id, visitor)
);

create table published_hit (
  tenant_id uuid not null references tenant(id) on delete cascade,
  address bytea not null,
  hour timestamptz not null,
  n int not null,
  primary key (tenant_id, address, hour)
);

-- ---- Files -----------------------------------------------------------------------------------------------------------

create table file_object (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  entity text,
  record_id uuid,
  file_name text not null,
  content_type text not null,
  size_bytes bigint not null check (size_bytes >= 0),
  storage_key text not null,
  sha256 text not null,
  created_by uuid,
  created_at timestamptz not null default now(),
  -- Files published for anonymous viewing (storefront images, public documents). Only images and
  -- PDFs, only when attached to a record the uploader can edit.
  is_public boolean not null default false,
  -- Whether a stored file was scanned for malware (crates/kernel/src/scan.rs). What is found is never
  -- stored, so there is no third value.
  scan_status text not null default 'unscanned' check (scan_status in ('clean', 'unscanned')),
  -- The template a stored file was generated from, so a later generation can replace it.
  document text,
  shared boolean not null default false,
  primary key (tenant_id, id)
);
create index file_object_document on file_object (tenant_id, entity, record_id, document) where document is not null;
create index file_object_public on file_object (id) where is_public;

-- Direct uploads in flight: the browser puts the bytes straight into the object store with a
-- presigned URL, then completes the upload, which turns the row into a `file_object`. Rows whose
-- upload never completed are swept with their objects once they expire.
create table file_upload (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  entity text,
  record_id uuid,
  file_name text not null,
  content_type text not null,
  size_bytes bigint not null check (size_bytes > 0),
  storage_key text not null,
  created_by uuid not null,
  expires_at timestamptz not null,
  -- Uploads sent through the server in parts (FERRUM-OPS-006): how much has arrived, so that after a
  -- dropped connection the next part goes from there. Direct uploads to object storage leave both alone.
  parts boolean not null default false,
  received_bytes bigint not null default 0,
  primary key (tenant_id, id)
);
create index file_upload_expiry on file_upload (expires_at);

-- Object storage only (docs/plan/stateless.md, D6; docs/operations.md, "Object storage").
--
-- `file_store`: where the installation's files are, once a server of this release has seen them in object storage
-- (one row). 'moving': the store takes every write, and reads look there first and in the local directory second,
-- until `ferrum task files_move` has copied the directory and confirmed it. 'store': the directory is no longer read.
-- No row: an installation on a local directory, or one that has only ever used a store.
create table file_store (
  id boolean default true primary key check (id),
  state text not null check (state in ('moving', 'store')),
  -- When files began to move (a replica started before then may still write to the directory).
  since timestamptz not null default now(),
  confirmed_at timestamptz,
  updated_at timestamptz not null default now()
);

-- Each file of the directory `files_move` has copied to the store and read back the same (its length and SHA-256):
-- a run after a crash, or a second run, skips them.
create table file_move (
  key text primary key,
  size_bytes bigint not null check (size_bytes >= 0),
  sha256 text not null,
  copied_at timestamptz not null default now()
);

-- Objects whose deletion waits (`files_delete_after_days`), so that a database restored to any moment within the
-- backups' reach finds every file it names. A write of the same key takes it off.
create table object_release (
  key text primary key,
  due_at timestamptz not null,
  requested_at timestamptz not null default now()
);
create index object_release_due on object_release (due_at);

-- ---- Events, jobs, automations and messages --------------------------------------------------------------------------

create table audit_event (
  tenant_id uuid not null,
  id bigint generated always as identity,
  at timestamptz not null default clock_timestamp(),
  actor uuid,
  entity text not null,
  record_id uuid not null,
  action text not null,
  version int,
  diff jsonb,
  request_id text,
  prev_hash bytea,
  hash bytea not null,
  -- Erasing a person's data also blanks the values their records' audit history holds. An audit
  -- event may change only its diff, and only while recording that redaction (`redacted` grows); its
  -- hash and place in the chain stay, and verification skips the content of redacted events.
  redacted jsonb,
  -- Sending the audit log elsewhere (docs/operations.md): each audit event names the transaction that wrote it, so
  -- the sender reads events in an order no late commit can step back into.
  -- A default, not a value from the writer.
  xid xid8 default pg_current_xact_id(),
  -- Sending the audit log elsewhere (docs/operations.md): what kind of event it is, decided where it is written
  -- (`sign_in`, `administration` by the kernel's own changes, `records` by a record's). Empty for events written
  -- before this, and by a build from before it: those are told apart by name, as before.
  category text,
  -- `at` is in the key because the table is partitioned by it. `id` is generated, so (tenant_id, id) stays unique
  -- without a constraint: a move keeps a workspace's numbers, into a workspace that has none here yet.
  primary key (tenant_id, id, at)
) partition by range (at);
create index audit_event_at on audit_event (tenant_id, at);
create index audit_event_record on audit_event (tenant_id, entity, record_id, id desc);
create index audit_event_xid on audit_event (xid, id) where xid is not null;

create function ferrum_audit_redact_only() returns trigger language plpgsql as $$
begin
  if new.redacted is null
     or jsonb_typeof(new.redacted) <> 'array'
     or jsonb_array_length(new.redacted) <= coalesce(jsonb_array_length(old.redacted), 0)
     or (new.tenant_id, new.id, new.at, new.entity, new.record_id, new.action, new.hash)
        is distinct from (old.tenant_id, old.id, old.at, old.entity, old.record_id, old.action, old.hash)
     or new.actor is distinct from old.actor
     or new.version is distinct from old.version
     or new.request_id is distinct from old.request_id
     or new.prev_hash is distinct from old.prev_hash then
    raise exception 'audit events are append-only (only a recorded redaction may blank their values)';
  end if;
  return new;
end $$;
create trigger audit_event_no_delete before delete on audit_event
  for each row
  execute function ferrum_forbid_change();
create trigger audit_event_no_truncate before truncate on audit_event
  for each statement
  execute function ferrum_forbid_change();
create trigger audit_event_redact_only before update on audit_event
  for each row
  execute function ferrum_audit_redact_only();

-- The one way to make a partition of a table, an app's included: `name`, in the parent's schema, for the range [lower,
-- upper) (literals of the partition key's type). Made only if missing (true when made). Read by its own name a partition
-- is a table of its own, which the parent's policies do not reach: it is given the parent's, with row-level security
-- forced, so such a read sees what the same read of the parent would. A partition without them reads as empty to the
-- system too, and a dump (deploy/dump.sh reads each partition by its name) would hold none of its rows without a word;
-- the kernel refuses to start while one exists (ferrum_partitions_without_policies). It is made apart and attached,
-- which locks the parent against other changes of its partitions only, never against reads or writes (a partition made
-- in place would wait for every reader, and every writer would wait behind it).
create function ferrum_create_partition(parent regclass, name text, lower text, upper text) returns boolean
language plpgsql as $$
declare
  schema_oid oid := (select relnamespace from pg_class where oid = parent);
  p record;
begin
  -- Read as rows, not from the catalog's cache: a partition another session made while this one waited is seen.
  if exists (select 1 from pg_class where relnamespace = schema_oid and relname = name) then
    return false;
  end if;
  perform pg_advisory_xact_lock(hashtext('ferrum_create_partition'), parent::oid::int);
  if exists (select 1 from pg_class where relnamespace = schema_oid and relname = name) then
    return false;
  end if;
  execute format('create table %s.%I (like %s including constraints)', schema_oid::regnamespace, name, parent);
  execute format('alter table %s.%I enable row level security, force row level security', schema_oid::regnamespace, name);
  for p in select * from pg_policy where polrelid = parent loop
    execute format('create policy %I on %s.%I as %s for %s to %s%s%s', p.polname, schema_oid::regnamespace, name,
      case when p.polpermissive then 'permissive' else 'restrictive' end,
      case p.polcmd when 'r' then 'select' when 'a' then 'insert' when 'w' then 'update' when 'd' then 'delete' else 'all' end,
      (select coalesce(string_agg(case when r = 0 then 'public' else quote_ident(pg_get_userbyid(r)) end, ', '), 'public') from unnest(p.polroles) r),
      coalesce(' using (' || pg_get_expr(p.polqual, parent) || ')', ''), coalesce(' with check (' || pg_get_expr(p.polwithcheck, parent) || ')', ''));
  end loop;
  execute format('alter table %s attach partition %s.%I for values from (%L) to (%L)', parent, schema_oid::regnamespace, name, lower, upper);
  return true;
end $$;

-- Partitions of a table with row-level security policies that have none of their own: made some other way than
-- ferrum_create_partition, they read as empty to the system and a dump leaves their rows out.
create function ferrum_partitions_without_policies() returns setof text language sql stable as $$
  select c.oid::regclass::text from pg_class c join pg_inherits i on i.inhrelid = c.oid
  where c.relispartition and exists (select 1 from pg_policy where polrelid = i.inhparent)
    and not exists (select 1 from pg_policy where polrelid = c.oid)
  order by 1
$$;

-- The months of a table partitioned by month on a time, `<parent>_yYYYYmMM` (UTC months), from that of `since` to that of
-- `until`: those it lacks are made (ferrum_create_partition), and how many is returned. A row whose month has no partition
-- fails its statement, so the kernel makes the audit trail's at every start and every hour, three months ahead, and a copy
-- makes those of the rows it brings. There is no default partition: one holding rows of a month would refuse that month's
-- partition later. More than 600 months at once is refused: a clock or a row dated decades off, not history.
create function ferrum_ensure_month_partitions(parent regclass, since timestamptz, until timestamptz) returns int
language plpgsql as $$
declare
  month timestamp := date_trunc('month', since at time zone 'UTC');
  base text := (select relname from pg_class where oid = parent);
  made int := 0;
begin
  if month + interval '600 months' <= until at time zone 'UTC' then
    raise exception 'ferrum: % would need a partition for every month from % to %, more than 600: a date that far off is not history',
      base, since, until using errcode = 'P0001';
  end if;
  while month <= until at time zone 'UTC' loop
    if ferrum_create_partition(parent, base || to_char(month, '"_y"YYYY"m"MM'), (month at time zone 'UTC')::text,
                               ((month + interval '1 month') at time zone 'UTC')::text) then
      made := made + 1;
    end if;
    month := month + interval '1 month';
  end loop;
  return made;
end $$;

create table audit_anchor (
  tenant_id uuid not null references tenant(id) on delete cascade,
  day date not null,
  events bigint not null,
  root_hash bytea not null,
  prev_root bytea,
  created_at timestamptz not null default now(),
  primary key (tenant_id, day)
);

-- The outbox does not wake its dispatcher with NOTIFY from the writing transaction: PostgreSQL serializes the
-- commits of every transaction that notifies (a global lock on the notification queue), so under concurrent writes it
-- would cap throughput. Dispatchers poll the pending index instead (every 250 ms when idle).
create table outbox (
  id bigint generated always as identity primary key,
  tenant_id uuid not null,
  topic text not null,
  entity text,
  record_id uuid,
  payload jsonb not null,
  created_at timestamptz not null default now(),
  available_at timestamptz not null default now(),
  attempts int not null default 0,
  last_error text,
  processed_at timestamptz,
  dead boolean not null default false,
  -- Events say where they came from and how urgent they are (docs/events.md). The trace columns
  -- default to what the writing transaction was given (db.rs, Trace), so no caller has to pass them.
  -- current_setting is stable, so existing rows are not rewritten.
  priority smallint not null default coalesce(((nullif(current_setting('ferrum.priority', true), ''))::smallint)::integer, 1),
  correlation_id text default nullif(current_setting('ferrum.correlation', true), ''),
  causation_id bigint default (nullif(current_setting('ferrum.causation', true), ''))::bigint,
  traceparent text default nullif(current_setting('ferrum.traceparent', true), ''),
  actor uuid,
  legal_entity uuid,
  source_version int,
  schema_version smallint not null default 1,
  -- How many events lie between an event and the change that started its chain: work done while handling an
  -- event writes events one further (db.rs, Trace). A chain too long, or one change followed by too many events,
  -- is stopped and kept as a dead event saying why (FLC-HOOK-008).
  depth smallint not null default coalesce(((nullif(current_setting('ferrum.depth', true), ''))::smallint)::integer, 0),
  -- The two-version window (crates/kernel/src/window.rs): events and jobs say which build wrote them, and a server takes
  -- only those of its own build or an older one, so what a newer build wrote waits for a newer replica. Empty: written
  -- by SQL; anyone takes it.
  build_date timestamptz,
  zone text default nullif(current_setting('ferrum.zone', true), '')
);
-- Claiming walks the workspaces that have work, then each one's most urgent events.
create index outbox_due on outbox (tenant_id, priority, id) where (processed_at is null) and (not dead);
-- A record's events, which its erasure finds (crate::privacy).
create index outbox_record on outbox (tenant_id, record_id) where record_id is not null;
create index outbox_tenant on outbox (tenant_id, created_at desc);

create table idempotency_key (
  tenant_id uuid not null,
  key text not null check (length(key) between 8 and 200),
  request_hash text not null,
  status_code int,
  response jsonb,
  created_at timestamptz not null default now(),
  -- A receipt belongs to the person who claimed its key (FEO-003): anyone else sending the same key is refused,
  -- whatever the request. Null for the system.
  actor uuid,
  primary key (tenant_id, key)
);
create index idempotency_key_created on idempotency_key (created_at);

create table webhook (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  name text not null,
  url text not null check (url ~ '^https?://'),
  topics text[] not null,
  is_active boolean not null default true,
  created_by uuid,
  created_at timestamptz not null default now(),
  -- Apps whose code runs on their publisher's servers: the webhook that carries what one hears of
  -- is the app's, and goes with it.
  app text,
  primary key (tenant_id, id)
);
create unique index webhook_app on webhook (tenant_id, app) where app is not null;

create table webhook_delivery (
  tenant_id uuid not null,
  id uuid not null,
  webhook_id uuid not null,
  outbox_id bigint not null,
  topic text not null,
  payload jsonb not null,
  -- The record a delivery is about, which its erasure finds (crate::privacy), also once its event is gone.
  record_id text generated always as ((payload ->> 'id')) stored,
  status text not null default 'pending' check (status in ('pending', 'delivered', 'dead')),
  attempts int not null default 0,
  next_attempt_at timestamptz not null default now(),
  response_code int,
  last_error text,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  unique (tenant_id, webhook_id, outbox_id),
  foreign key (tenant_id, webhook_id) references webhook(tenant_id, id) on delete cascade
);
create index webhook_delivery_due on webhook_delivery (next_attempt_at) where status = 'pending';
create index webhook_delivery_record on webhook_delivery (tenant_id, record_id) where payload ?| array['doc', 'was'];

create table automation (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  name text not null,
  entity text not null,
  event text not null check (event in ('created', 'updated', 'submitted', 'cancelled', 'deleted', 'transitioned', 'scheduled', 'opened')),
  condition text,
  actions jsonb not null,
  is_active boolean not null default true,
  created_by uuid,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  -- Automations that fire on time (docs/api.md, Automations): an automation may carry a schedule in place
  -- of a record event, and its event is then `scheduled`. A loop in every replica looks at the window from
  -- `scanned_until` to now; a large entity is worked through a page at a time, `scan_to` holding the end of
  -- the window being worked through and `scan_cursor` the last record done. Each (record, due moment) fires
  -- once: `automation_occurrence`'s primary key says so, however many replicas look.
  schedule jsonb,
  scanned_until timestamptz,
  scan_to timestamptz,
  scan_cursor uuid,
  -- Automations a package ships (docs/api.md, Automations): installed in each workspace as its defaults, found again by
  -- `package` and `package_key`. `base` is the default as last installed: a row that differs from it is the
  -- workspace's copy, kept when a release changes the default, which is then flagged.
  package text,
  package_key text,
  base jsonb,
  constraint automation_schedule_check check ((event = 'scheduled') = (schedule is not null)),
  primary key (tenant_id, id)
);
create unique index automation_package on automation (tenant_id, package, package_key) where package is not null;
create index automation_scheduled on automation (tenant_id, scanned_until) where (event = 'scheduled') and is_active;

create table automation_run (
  tenant_id uuid not null,
  automation_id uuid not null,
  outbox_id bigint not null,
  status text not null check (status in ('done', 'skipped', 'failed', 'handed_over')),
  error text,
  created_at timestamptz not null default now(),
  primary key (tenant_id, automation_id, outbox_id)
);

create table notification (
  tenant_id uuid not null,
  id uuid not null,
  user_id uuid not null,
  title text not null,
  body text,
  entity text,
  record_id uuid,
  read_at timestamptz,
  created_at timestamptz not null default now(),
  -- Kinds of notification and each person's choice per kind (docs/plan/apps-ui.md §12.8; the
  -- kernel's notify.rs): in the bar only, by email at once, in a daily email digest, or not at all.
  -- The platform's kinds are mention, comment, assignment, watch and automation; apps declare
  -- theirs (`shell.notification`) as `{app}.{id}`.
  kind text not null default 'general',
  -- How it was delivered when sent: inapp, email or digest.
  delivery text not null default 'inapp' check (delivery in ('inapp', 'email', 'digest', 'push')),
  -- When it went out by email (at once, or in a digest).
  emailed_at timestamptz,
  primary key (tenant_id, id),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);
create index notification_digest on notification (tenant_id, user_id, created_at) where (delivery = 'digest') and (emailed_at is null) and (read_at is null);
create index notification_unread on notification (tenant_id, user_id, created_at desc) where read_at is null;

-- Requests counted per key (client, workspace...) and fixed window, shared by every replica so
-- rate limits hold however many replicas serve. Unlogged: a crash only forgets open windows.
create unlogged table rate_window (
  key text not null,
  window_start timestamptz not null,
  hits int not null,
  primary key (key, window_start)
);

create table notification_pref (
  tenant_id uuid not null,
  user_id uuid not null,
  kind text not null check (length(kind) between 1 and 80),
  channel text not null check (channel in ('inapp', 'email', 'digest', 'push', 'off')),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, user_id, kind),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);

-- One device's subscription: made in a browser session when the person turns notifications on
-- there, and kept with that session. Signing the device out ends it, and so do signing out
-- everywhere (`token_version` moves on) and a new server key (`vapid`: the public half it was
-- made for). An endpoint belongs to whoever registered it last. Keys are base64url, unpadded.
create table push_subscription (
  tenant_id uuid not null,
  session_id uuid not null,
  user_id uuid not null,
  endpoint text not null check (length(endpoint) between 10 and 2048),
  p256dh text not null check (length(p256dh) = 87),
  auth text not null check (length(auth) = 22),
  vapid text not null check (length(vapid) between 1 and 200),
  token_version int not null,
  created_at timestamptz not null default now(),
  primary key (tenant_id, session_id),
  foreign key (tenant_id, session_id) references user_session(tenant_id, id) on delete cascade,
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);
create unique index push_subscription_endpoint on push_subscription (endpoint);
create index push_subscription_user on push_subscription (tenant_id, user_id);

-- Durable background work (crates/kernel/src/jobs.rs): what takes longer than a request or must
-- survive a restart. A worker holds a job by a lease it renews, and `lease_token` fences one that
-- lost its lease from writing an outcome over the next worker's.
create table job (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  kind text not null check (length(kind) between 1 and 100),
  payload jsonb not null default '{}',
  priority smallint not null default 1,
  status text not null default 'queued' check (status in ('queued', 'running', 'done', 'failed', 'cancelled')),
  attempts int not null default 0,
  max_attempts int not null default 5 check (max_attempts between 1 and 25),
  run_at timestamptz not null default now(),
  lease_until timestamptz,
  lease_token uuid,
  cancel_requested boolean not null default false,
  progress jsonb not null default '{}',
  result jsonb,
  last_error text,
  dedupe_key text check (length(dedupe_key) <= 200),
  created_by uuid,
  correlation_id text default nullif(current_setting('ferrum.correlation', true), ''),
  created_at timestamptz not null default now(),
  started_at timestamptz,
  finished_at timestamptz,
  -- Workflows that cross systems (crates/kernel/src/saga.rs) are jobs that undo what they did when
  -- they cannot finish, or are asked to stop: asked, such a job is not dropped where it stands, nor
  -- marked cancelled while queued, but run again to undo its steps.
  undo_on_cancel boolean not null default false,
  depth smallint not null default coalesce(((nullif(current_setting('ferrum.depth', true), ''))::smallint)::integer, 0),
  build_date timestamptz,
  -- Zones, level 2 (docs/operations.md, "Zones"): the zone of the request that queued a job or an event, started a run
  -- or asked for an approval, when that zone narrows staff. Work done for it later is done in that zone, by name, against
  -- the worker's own zone settings; a name they do not hold fails that work. Empty for work queued from inside (no
  -- request).
  zone text default nullif(current_setting('ferrum.zone', true), ''),
  -- Step-up by rule (docs/operations.md, "Asking again before sensitive work"): whether the request that queued a job had
  -- proved who its person is: empty when it had (or nobody was asked), `no` when it had not, `machine` for a key or
  -- another client that cannot. The job does later only what that request could have done. Empty for jobs queued from
  -- inside (no request).
  step_up text default nullif(current_setting('ferrum.step_up', true), ''),
  primary key (tenant_id, id)
);
create index job_active on job (tenant_id, priority, run_at) where status in ('queued', 'running');
create unique index job_dedupe on job (tenant_id, kind, dedupe_key) where (dedupe_key is not null) and (status in ('queued', 'running'));
create index job_finished on job (finished_at) where finished_at is not null;

-- Apps' upkeep (server/src/packages.rs): a row for each workspace and each app with one of its packages installed there,
-- kept by a sync once a minute. A replica with a free slot claims the rows due, oldest first, by moving `due_at` to the
-- end of its lease, and records the outcome only while `due_at` still holds that lease. `timeouts` counts the runs in a
-- row that hit the time limit: each doubles the wait before the next.
create table upkeep (
  tenant_id uuid not null references tenant(id) on delete cascade,
  app text not null,
  due_at timestamptz not null default now(),
  ran_at timestamptz,
  failed_at timestamptz,
  error text,
  timeouts int not null default 0,
  primary key (tenant_id, app)
);
create index upkeep_due on upkeep (due_at);
alter table upkeep enable row level security;
alter table upkeep force row level security;
create policy tenant_isolation on upkeep using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

-- What a consumer has done about an event, in the transaction that did it. Delivery is at least once, so
-- this is what makes an effect happen once, and the count is what a repeat leaves behind.
create table event_receipt (
  tenant_id uuid not null,
  consumer text not null check (length(consumer) between 1 and 120),
  event_id bigint not null,
  duplicates int not null default 0,
  received_at timestamptz not null default now(),
  -- A consumer's own claim (outbox::first_time), which it may give back (outbox::give_back); false for the dispatcher's
  -- record that a consumer finished, which nothing gives back.
  claimed boolean not null default false,
  primary key (tenant_id, consumer, event_id)
);
create index event_receipt_age on event_receipt (received_at);

create table automation_occurrence (
  tenant_id uuid not null,
  automation_id uuid not null,
  record_id uuid not null,
  due_at timestamptz not null,
  created_at timestamptz not null default now(),
  primary key (tenant_id, automation_id, record_id, due_at),
  foreign key (tenant_id, automation_id) references automation(tenant_id, id) on delete cascade
);
create index automation_occurrence_due on automation_occurrence (due_at);

-- Approvals on a workflow transition (crates/kernel/src/approvals.rs, docs/builder/decisions.md BD-19). Taking a
-- transition whose policy has steps to decide makes a request; at most one is pending per record. `policy` is the
-- policy as it was when asked (a change to the workflow does not move a request under way), `steps` each step's
-- state, approvers and due moments, `next_due` the first reminder or escalation the minute loop owes.
create table approval_request (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  entity text not null,
  record_id uuid not null,
  action text not null,
  from_state text not null,
  requester uuid,
  params jsonb not null default '{}',
  policy jsonb not null,
  steps jsonb not null,
  status text not null default 'pending' check (status in ('pending', 'approved', 'rejected', 'recalled', 'cancelled', 'failed')),
  next_due timestamptz,
  created_at timestamptz not null default now(),
  ended_at timestamptz,
  -- Approval before a document is submitted or cancelled (`set_approval`, docs/builder/decisions.md BD-19).
  -- `document`: the request is on the document pipeline's submit or cancel (its `action`), not a workflow transition.
  -- `failed`: approved, but the submit or cancel, done as its requester when the policy was satisfied, was refused
  -- (their rights gone, a check of the app's); `failure` says why, and nothing was submitted or cancelled.
  document boolean not null default false,
  failure text,
  -- Approval before one of an entity's actions runs (`set_approval` with `on: {action: name}`, docs/builder/decisions.md
  -- BD-34). `on_action`: the request is on the action named in `action`, run once as its requester when the last step
  -- approves. `secret_params`: the names of its secret parameters, each kept in the vault and bound to the request;
  -- `params` holds them as `[redacted]`, so neither the request's API nor the audit trail can read them.
  on_action boolean not null default false,
  secret_params text[] not null default '{}',
  zone text default nullif(current_setting('ferrum.zone', true), ''),
  primary key (tenant_id, id)
);
create index approval_request_due on approval_request (next_due) where (status = 'pending') and (next_due is not null);
create unique index approval_request_pending on approval_request (tenant_id, entity, record_id) where status = 'pending';
create index approval_request_record on approval_request (tenant_id, entity, record_id, created_at);

-- Each decision, kept: who decided, for whom when standing in, under which step, and what. `void` when a change to
-- a field the policy names cancelled the decisions so far; nothing else changes a decision.
create table approval_decision (
  tenant_id uuid not null,
  id uuid not null,
  request_id uuid not null,
  step int not null check (step >= 0),
  decider uuid not null,
  on_behalf_of uuid,
  outcome text not null check (outcome in ('approve', 'reject')),
  comment text check (length(comment) <= 2000),
  at timestamptz not null default now(),
  void boolean not null default false,
  primary key (tenant_id, id),
  foreign key (tenant_id, request_id) references approval_request(tenant_id, id) on delete cascade
);
create index approval_decision_request on approval_decision (tenant_id, request_id);

-- Message templates and channels (crates/kernel/src/messages.rs, docs/builder/decisions.md BD-20).
-- A workspace's own templates, and its edited copies of packages' defaults: `base` is the digest of the default an
-- edit started from, so that a release changing that default is told, never written over the edit.
create table message_template (
  tenant_id uuid not null references tenant(id) on delete cascade,
  key text not null check (length(key) between 1 and 100),
  def jsonb not null,
  base text,
  updated_by uuid,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, key)
);

-- A text-message or chat-app gateway a workspace connects: its request described, never named in code; its secrets kept
-- in the vault (`has_secrets`, never read back). One enabled for each kind. `refused` counts inbound calls refused.
create table message_channel (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  kind text not null check (kind in ('text', 'chat')),
  name text not null check (length(name) between 1 and 100),
  description jsonb not null,
  has_secrets boolean not null default false,
  enabled boolean not null default true,
  refused int not null default 0,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
create unique index message_channel_kind on message_channel (tenant_id, kind) where enabled;

-- Every send: what, to whom, on which channel, and how it went. `recipient` is an address or a number in
-- international form (erasure reaches it); `pending` what goes out, held only until it has; `content` what the test
-- channel delivered (a code masked).
create table message_send (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  template text not null,
  channel text not null check (channel in ('email', 'text', 'chat', 'inapp')),
  channel_id uuid,
  recipient text,
  user_id uuid,
  entity text,
  record_id uuid,
  language text,
  state text not null check (state in ('queued', 'held', 'sent', 'delivered', 'failed', 'skipped')),
  reason text,
  provider_id text,
  error text,
  pending jsonb,
  content jsonb,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
create index message_send_provider on message_send (tenant_id, channel_id, provider_id);
create index message_send_recipient on message_send (tenant_id, recipient, created_at desc);
create index message_send_record on message_send (tenant_id, entity, record_id, created_at desc);

-- One-time codes: a salted digest, never the code.
create table message_code (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  purpose text not null,
  recipient text not null,
  digest text not null,
  attempts int not null default 0,
  expires_at timestamptz not null,
  used_at timestamptz,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
create index message_code_recipient on message_code (tenant_id, recipient, created_at desc);

-- A reply that recorded a number's choice on a channel (a stop or start word, crates/kernel/src/messages.rs), by the
-- SHA-256 of the body its provider sent: taken once, so that the same call sent again by someone who captured it
-- cannot undo a later choice. Kept while the channel is.
create table message_reply_seen (
  tenant_id uuid not null,
  channel_id uuid not null,
  digest text not null,
  primary key (tenant_id, channel_id, digest),
  foreign key (tenant_id, channel_id) references message_channel(tenant_id, id) on delete cascade
);

-- ---- Privacy ---------------------------------------------------------------------------------------------------------

-- What personal-data requests rest on (docs/registry/data-governance.md).
-- The addresses someone has had: a request made under any of them finds them, and a change of
-- address cannot lose what is filed under the old one. The trigger catches every path that changes
-- an address. Erasure clears these rows with the account.
create table person_identifier (
  tenant_id uuid not null,
  user_id uuid not null,
  email citext not null,
  replaced_at timestamptz not null default now(),
  primary key (tenant_id, user_id, email),
  foreign key (tenant_id, user_id) references app_user(tenant_id, id) on delete cascade
);
create index person_identifier_email on person_identifier (tenant_id, email);

-- What must not be erased or deleted while a matter is open: everything about a person, or one
-- record. Released holds stay as the record of what was held.
create table legal_hold (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  email citext,
  entity text,
  record_id uuid,
  reason text not null check (length(reason) between 1 and 500),
  created_by uuid not null,
  created_at timestamptz not null default now(),
  released_at timestamptz,
  released_by uuid,
  check ((email is not null) <> (record_id is not null)),
  constraint legal_hold_check1 check ((entity is null) = (record_id is null)),
  primary key (tenant_id, id)
);
create index legal_hold_active on legal_hold (tenant_id, entity, record_id) where released_at is null;
-- What the guard asks on every delete, kept small: the holds in force on records.
create index legal_hold_records on legal_hold (tenant_id, entity, record_id) where (released_at is null) and (record_id is not null);

-- A record under a legal hold is not deleted by any path (FERRUM-PRIVACY-004): the platform's own
-- deletes refuse it with their reason first, and this refuses the rest, an app's own SQL among
-- them, on every entity table the metadata sync makes (it names the entity as the argument). A
-- delete cascading from another (a workspace's deletion) is not stopped here.
create function ferrum_held_guard() returns trigger language plpgsql as $$
begin
  if pg_trigger_depth() = 1 and exists (
    select 1 from legal_hold h
    where h.tenant_id = old.tenant_id and h.released_at is null and h.entity = tg_argv[0] and h.record_id = old.id
  ) then
    raise exception 'ferrum: this % is under a legal hold; release it first', tg_argv[0] using errcode = 'P0001';
  end if;
  return old;
end $$;

-- Restriction of processing and retention rules (crates/kernel/src/retention.rs), beside the legal
-- holds and erasure that are already there: four separate operations, each with its own reason.
create table processing_restriction (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  email citext not null,
  reason text not null check (length(reason) between 1 and 500),
  created_by uuid,
  created_at timestamptz not null default now(),
  lifted_by uuid,
  lifted_at timestamptz,
  primary key (tenant_id, id)
);
create unique index processing_restriction_live on processing_restriction (tenant_id, email) where lifted_at is null;

create table retention_rule (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  entity text not null,
  after_days int not null check (after_days between 1 and 36500),
  date_field text not null default 'updated_at',
  action text not null check (action in ('delete', 'anonymize')),
  condition jsonb not null default '{}',
  reason text not null check (length(reason) between 1 and 500),
  is_active boolean not null default true,
  last_run_at timestamptz,
  last_result jsonb,
  created_by uuid,
  created_at timestamptz not null default now(),
  -- Where a retention rule's scan stands (FEO-005). Each daily run continues from here, so records it cannot
  -- remove (held, or failing) never keep it from the ones after them; a finished scan starts again from the
  -- beginning, revisiting those. Null: at the beginning. Changing a rule starts its scan again.
  scan_cursor text,
  primary key (tenant_id, id)
);

-- A held record's evidence survives a change, whoever's SQL makes it (FEO-007). The database already refuses
-- deleting a held record (the `ferrum_held_guard` trigger); a change to one is not refused, as work on it goes on, but the row
-- as it was before each change is kept while a legal hold covers the record, so an app's own SQL that blanks a
-- field leaves the held version readable. What is kept goes when the record's last hold is released.
create table held_version (
  tenant_id uuid not null references tenant(id) on delete cascade,
  entity text not null,
  record_id uuid not null,
  saved_at timestamptz not null default clock_timestamp(),
  "row" jsonb not null
);
create index held_version_record on held_version (tenant_id, entity, record_id, saved_at);

-- On every entity table the metadata sync makes (it names the entity as the argument), before each update.
create function ferrum_held_keep() returns trigger language plpgsql as $$
begin
  if exists (
    select 1 from legal_hold h
    where h.tenant_id = old.tenant_id and h.released_at is null and h.entity = tg_argv[0] and h.record_id = old.id
  ) then
    insert into held_version (tenant_id, entity, record_id, row) values (old.tenant_id, tg_argv[0], old.id, to_jsonb(old));
  end if;
  return new;
end $$;

-- What a person said about being contacted for a purpose (crates/kernel/src/retention.rs, FEO-015):
-- the latest word for each address. Marketing from any app asks it when a message goes out, so one
-- withdrawal stops every campaign, sequence, automation and connector sending marketing to them;
-- transactional notices do not ask it. A restriction of processing stops both.
create table contact_consent (
  tenant_id uuid not null references tenant(id) on delete cascade,
  email citext not null,
  purpose text not null check (purpose = 'marketing'),
  granted boolean not null,
  source text not null check (length(source) between 1 and 200),
  recorded_by uuid,
  recorded_at timestamptz not null default now(),
  -- Opt-outs per address or number and channel: the consent records there are, a channel more. What was recorded
  -- before is about e-mail.
  channel text not null default 'email' check (channel in ('email', 'text', 'chat')),
  primary key (tenant_id, email, purpose, channel)
);

-- Each erasure's journal entry, by ids and field names only, as it also goes to object storage
-- (crates/kernel/src/privacy.rs). A workspace taken back to a restored copy (extract_tenant) keeps this
-- table as it is now and erases again everything it names before anyone sees the copy, so an erasure
-- since the copy stays done without a separate task (FEO-078). A whole database restored from a backup
-- loses the rows written after it; the object-storage journal and `ferrum task reapply_erasures` cover that.
create table privacy_erasure (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id bigint generated always as identity,
  erasure jsonb not null,
  erased_at timestamptz not null default now(),
  primary key (tenant_id, id)
);

-- ---- Rules, processes and calendars ----------------------------------------------------------------------------------

-- Receipts of business capabilities (crates/kernel/src/capability.rs, docs/logic): one row per logical
-- effect, committed in the same transaction as what it did. A repeat of the key finds it and gets what was
-- done; the same key with other input is refused. A key belongs to an effect, never to an attempt.
create table logic_effect (
  tenant_id uuid not null references tenant(id) on delete cascade,
  environment text not null check (length(environment) between 1 and 40),
  capability text not null check (length(capability) between 3 and 200),
  effect_key text not null check (length(effect_key) between 1 and 400),
  input_digest text not null,
  version text not null,
  capability_digest text not null,
  status text not null default 'succeeded' check (status = 'succeeded'),
  result jsonb not null default '{}',
  record_entity text,
  record_id uuid,
  actor uuid,
  correlation_id text default nullif(current_setting('ferrum.correlation', true), ''),
  created_at timestamptz not null default now(),
  -- How long a repeat may find it, as its capability declares (`retention_days`); the hourly maintenance deletes it
  -- after that (UTC).
  retention_days int not null check (retention_days >= 0),
  kept_until timestamp generated always as (((created_at at time zone 'UTC') + make_interval(days => retention_days))) stored,
  -- The app whose call a receipt is (crates/kernel/src/capability.rs, Kernel::call_tx): null for a person's, a process's,
  -- an automation's or an agent's use of the gate.
  caller text,
  primary key (tenant_id, environment, capability, effect_key)
);
create index logic_effect_kept on logic_effect (kept_until);
-- A record's receipts, which its erasure finds (crate::privacy).
create index logic_effect_record on logic_effect (tenant_id, record_id) where record_id is not null;

-- Customer logic's releases, reviews and activations (crates/kernel/src/logic.rs, docs/logic/rules.md).
--
-- A release is what a solution's source compiled to, kept with the source: it never changes. A review is
-- of a release's artifact digest, by someone other than its author. An activation says which release of a
-- solution runs in a workspace, with a generation compared on every change. A refusal is evidence that a
-- rule refused a save, kept when the rule asks for it, written beside the save's rolled-back transaction.
create table logic_release (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null default gen_random_uuid(),
  solution text not null check (length(solution) between 2 and 101),
  version text not null check (length(version) between 5 and 32),
  source_digest text not null,
  artifact_digest text not null,
  compiler text not null,
  profile text not null check (profile in ('core', 'full_process')),
  source jsonb not null,
  release jsonb not null,
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, id),
  unique (tenant_id, solution, version)
);

create function logic_release_immutable() returns trigger language plpgsql as $$
begin
  raise exception 'a release does not change: compile a new version';
end $$;
create trigger logic_release_immutable before update on logic_release
  for each row
  execute function logic_release_immutable();

create table logic_review (
  tenant_id uuid not null,
  release_id uuid not null,
  -- What the reviewer was shown and decided on.
  artifact_digest text not null,
  decided_by uuid not null,
  decision text not null check (decision in ('approved', 'rejected')),
  reason text not null default '' check (length(reason) <= 2000),
  decided_at timestamptz not null default now(),
  primary key (tenant_id, release_id, decided_by),
  foreign key (tenant_id, release_id) references logic_release(tenant_id, id) on delete cascade
);

create table logic_activation (
  tenant_id uuid not null,
  solution text not null,
  release_id uuid not null,
  generation bigint not null default 1,
  activated_by uuid not null,
  activated_at timestamptz not null default now(),
  primary key (tenant_id, solution),
  foreign key (tenant_id, release_id) references logic_release(tenant_id, id)
);

create table logic_refusal (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id bigint generated always as identity,
  release_id uuid not null,
  rule text not null,
  entity text not null,
  record_id uuid,
  actor uuid,
  -- 'false': the check did not hold; 'unknown': it could not be decided, which refuses too.
  outcome text not null check (outcome in ('false', 'unknown')),
  correlation_id text default nullif(current_setting('ferrum.correlation', true), ''),
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
create index logic_refusal_created on logic_refusal (tenant_id, created_at);

create table event_chain (
  tenant_id uuid not null references tenant(id) on delete cascade,
  correlation_id text not null,
  events int not null default 0,
  first_at timestamptz not null default now(),
  primary key (tenant_id, correlation_id)
);

-- Durable processes' records (crates/kernel/src/process.rs, docs/logic/processes.md). The durable runtime keeps
-- the schedule; Ferrum keeps the business record: which runs were accepted on which release, what people were
-- asked and what they decided, what the runs wait for, the signals that came for them, and its calendars.
-- A run, accepted once per business key: its row is its start intent. The release is the one active when the
-- trigger was accepted, never "the latest" later.
create table logic_run (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null default gen_random_uuid(),
  release_id uuid not null,
  solution text not null,
  process text not null,
  business_key text not null check (length(business_key) between 1 and 400),
  trigger jsonb not null,
  trigger_event bigint,
  requester uuid,
  status text not null default 'starting' check (status in ('starting', 'running', 'needs_attention', 'cancelling', 'completed', 'refused', 'cancelled')),
  -- The state machine's run as the runtime last reported it: a read model, rebuilt from the runtime, never
  -- what schedules anything.
  state jsonb,
  reported int not null default 0,
  workflow_id text not null,
  output jsonb,
  correlation_id text default nullif(current_setting('ferrum.correlation', true), ''),
  accepted_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  finished_at timestamptz,
  -- The record a run is about, so its page can show the processes holding it.
  subject_entity text,
  subject_id uuid,
  -- Runs' changes in the order their transactions became visible. A reader keeps the cursor it was given, and a
  -- change made by a transaction still open when it read is in a later answer, never skipped (FLC-PROC-014).
  changed xid8 not null default pg_current_xact_id(),
  -- One writer per event (FLC-MIG-003). Each run records the generation of its solution's activation it was
  -- accepted under, and an automation's events, once the solution imported from it runs in its place, are claimed
  -- in the automation's own record of runs as handed over, by whichever of the two comes to the event first.
  generation bigint,
  zone text default nullif(current_setting('ferrum.zone', true), ''),
  primary key (tenant_id, id),
  unique (tenant_id, solution, process, business_key),
  foreign key (tenant_id, release_id) references logic_release(tenant_id, id)
);
create index logic_run_changes on logic_run (tenant_id, changed);
create index logic_run_open on logic_run (tenant_id, status) where finished_at is null;
create index logic_run_subject on logic_run (tenant_id, subject_id) where subject_id is not null;

create function logic_run_changed() returns trigger language plpgsql as $$
begin
  new.changed := pg_current_xact_id();
  return new;
end $$;
create trigger logic_run_changed before update on logic_run
  for each row
  execute function logic_run_changed();

-- What a run asks of people: a task to decide, a case and its tasks, a recovery, something to look at.
create table logic_task (
  tenant_id uuid not null,
  id uuid not null default gen_random_uuid(),
  run_id uuid not null,
  activation text not null check (length(activation) between 1 and 1000),
  kind text not null check (kind in ('task', 'case', 'recovery', 'attention')),
  label text not null,
  role text not null default '',
  material jsonb not null default '{}',
  digest text not null default '',
  form jsonb not null default '[]',
  approvals int not null default 1 check (approvals between 1 and 9),
  delegation int not null default 0 check (delegation between 0 and 3),
  exclude uuid[] not null default '{}',
  -- Who it was handed to, in order; the first is whoever handed it on first.
  delegated uuid[] not null default '{}',
  -- A case's tasks, or what a recovery may do.
  detail jsonb not null default '{}',
  status text not null default 'open' check (status in ('open', 'approved', 'rejected', 'timed_out', 'cancelled', 'done', 'resolved')),
  deadline timestamptz,
  -- What the deadline was worked out from: calendar, revision, zone, zone data, adjustment.
  deadline_basis jsonb,
  version int not null default 1,
  opened_at timestamptz not null default now(),
  closed_at timestamptz,
  primary key (tenant_id, id),
  unique (tenant_id, run_id, activation),
  foreign key (tenant_id, run_id) references logic_run(tenant_id, id) on delete cascade
);
create index logic_task_open on logic_task (tenant_id, status, role) where status = 'open';

-- Decisions never change: who decided what, on which material, when, with what they filled in.
create table logic_task_decision (
  tenant_id uuid not null,
  id uuid not null default gen_random_uuid(),
  task_id uuid not null,
  by_user uuid not null,
  decision text not null check (decision in ('approve', 'reject', 'delegate', 'done', 'close', 'resume', 'end', 'acknowledge')),
  -- For a case: which of its tasks.
  part text not null default '',
  to_user uuid,
  digest text not null default '',
  "values" jsonb not null default '{}',
  reason text not null default '' check (length(reason) <= 2000),
  decided_at timestamptz not null default now(),
  primary key (tenant_id, id),
  foreign key (tenant_id, task_id) references logic_task(tenant_id, id) on delete cascade
);
create unique index logic_task_decision_once on logic_task_decision (tenant_id, task_id, by_user, part) where decision in ('approve', 'reject', 'done');

create function logic_task_decision_immutable() returns trigger language plpgsql as $$
begin
  raise exception 'a decision does not change';
end $$;
create trigger logic_task_decision_immutable before update on logic_task_decision
  for each row
  execute function logic_task_decision_immutable();

-- What a run waits for, and the signals that came, before or after it began to wait.
create table logic_wait (
  tenant_id uuid not null,
  run_id uuid not null,
  activation text not null,
  event text not null,
  correlation text not null,
  status text not null default 'waiting' check (status in ('waiting', 'matched', 'timed_out', 'cancelled')),
  deadline timestamptz,
  signal_id bigint,
  created_at timestamptz not null default now(),
  primary key (tenant_id, run_id, activation),
  foreign key (tenant_id, run_id) references logic_run(tenant_id, id) on delete cascade
);
create index logic_wait_match on logic_wait (tenant_id, event, correlation) where status = 'waiting';

create table logic_signal (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id bigint generated always as identity,
  event_id bigint not null,
  event text not null,
  correlation text not null,
  payload jsonb not null,
  payload_digest text not null,
  emitter uuid,
  status text not null default 'pending' check (status in ('pending', 'delivered', 'expired', 'quarantined')),
  run_id uuid,
  activation text,
  reason text,
  arrived_at timestamptz not null default now(),
  expires_at timestamptz not null,
  primary key (tenant_id, id)
);
-- An event kept for a wait to come is kept once; one handed to a waiting run, once per run and wait.
create unique index logic_signal_early on logic_signal (tenant_id, event_id, event) where run_id is null;
create unique index logic_signal_matched on logic_signal (tenant_id, event_id, event, run_id, activation) where run_id is not null;
create index logic_signal_pending on logic_signal (tenant_id, event, correlation) where status = 'pending';

-- A workspace's business calendars. A new revision changes the deadlines resolved after it, never those before.
create table logic_calendar (
  tenant_id uuid not null references tenant(id) on delete cascade,
  name text not null check (name ~ '^[a-z][a-z0-9_]{0,63}$'),
  zone text not null,
  weekends text[] not null default '{sat,sun}',
  holidays date[] not null default '{}',
  revision bigint not null default 1,
  updated_by uuid,
  updated_at timestamptz not null default now(),
  -- Hours of the day on business calendars: per weekday, windows in the calendar's zone
  -- ({"mon": [["09:00", "13:00"], ["14:00", "18:00"]]}). Null: every hour of a business day is open.
  hours jsonb,
  -- Dated exceptions of a business calendar (ferrum_logic::calendar::Exception): `{"2026-12-24": {"open": false,
  -- "hours": [["12:00", "24:00"]]}}`, what a date is whatever its weekday and holidays say. None by default.
  exceptions jsonb not null default '{}',
  primary key (tenant_id, name)
);

-- What Studio's solution and run pages need (crates/studio/src/logic.rs, docs/logic/studio.md).
-- A solution being edited. Saved against the version its editor read: a stale save is refused with what is
-- there now, never merged silently (FLC-UI-006). A draft is not a release: saving one compiles and keeps an
-- immutable release beside it.
create table logic_draft (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null default gen_random_uuid(),
  solution text not null default '',
  label text not null default '',
  source text not null check (length(source) <= 1048576),
  version int not null default 1,
  created_by uuid,
  updated_by uuid,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id)
);

-- Each step's attempts, so an operator can tell how often a step was tried and how its last try went
-- (FLC-OPS-001): the receipt says what succeeded, this says what came before it.
create table logic_attempt (
  tenant_id uuid not null references tenant(id) on delete cascade,
  run_id uuid not null,
  activation text not null check (length(activation) between 1 and 1000),
  attempts int not null default 0,
  last_outcome text not null default '' check (length(last_outcome) <= 500),
  first_at timestamptz not null default now(),
  last_at timestamptz not null default now(),
  primary key (tenant_id, run_id, activation),
  foreign key (tenant_id, run_id) references logic_run(tenant_id, id) on delete cascade
);

-- What an erasure took from the copies runs keep of a record (FLC-OPS-006): the event that started a run about
-- it, the run's own record and the material its people were shown lose those fields, and this row stays so a
-- runtime reporting the run again, from a history taken before the erasure, cannot bring them back.
create table logic_erasure (
  tenant_id uuid not null references tenant(id) on delete cascade,
  subject_entity text not null check (length(subject_entity) between 1 and 200),
  subject_id uuid not null,
  fields text[] not null,
  erased_at timestamptz not null default now(),
  primary key (tenant_id, subject_entity, subject_id)
);

-- ---- The assistant ---------------------------------------------------------------------------------------------------

-- The assistant's conversations (docs/plan/apps-ui.md D2, §11.4; the server's assistant.rs): each
-- person's threads, and their messages exactly as the model's API takes them back. Messages are
-- only ever appended: the model's thinking stays valid only in the conversation that produced it.
create table assistant_thread (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  user_id uuid not null,
  title text not null default '',
  -- idle: nothing under way. running: a turn is (since running_since, on some replica; one that
  -- has not moved for a while is taken to have stopped). waiting: changes the assistant proposed
  -- wait for the person's decision (pending: the calls, and the results of those that ran).
  state text not null default 'idle' check (state in ('idle', 'running', 'waiting')),
  running_since timestamptz,
  stop_requested boolean not null default false,
  pending jsonb,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
create index assistant_thread_mine on assistant_thread (tenant_id, user_id, updated_at desc);

create table assistant_message (
  tenant_id uuid not null,
  thread_id uuid not null,
  seq int not null,
  role text not null check (role in ('user', 'assistant')),
  content jsonb not null,
  -- The model's report for an assistant message (tokens, the model that answered).
  usage jsonb,
  created_at timestamptz not null default now(),
  primary key (tenant_id, thread_id, seq),
  foreign key (tenant_id, thread_id) references assistant_thread(tenant_id, id) on delete cascade
);
-- A person's calls to the model in the last hour (their allowance): one assistant message each.
create index assistant_message_calls on assistant_message (tenant_id, created_at) where role = 'assistant';
-- Messages that hold what a tool gave the model and is not blanked yet: the hourly maintenance blanks it after a while,
-- an erasure when it names a record erased (crates/kernel/src/privacy.rs, HOLDS_RESULTS, the same text).
create index assistant_message_results on assistant_message (created_at) where (role = 'user') and jsonb_path_exists(content, '$[*]?(@."type" == "tool_result" && !(@."content"[0]."text" starts with "(removed"))'::jsonpath);

-- What the assistant was told, by version: each answer records the version of the instructions,
-- tools and model it answered under (`assistant_message.usage->>'prompt'`), so an evaluation, a
-- complaint or an audit can be traced to what the model saw.
create table assistant_prompt (
  tenant_id uuid not null references tenant(id) on delete cascade,
  digest text not null,
  model text not null,
  system text not null,
  tools jsonb not null,
  first_used timestamptz not null default now(),
  primary key (tenant_id, digest)
);

-- Tokens held back from a workspace's monthly allowance for a call to the model in flight
-- (server/src/assistant.rs, FEO-062): taken before the call, under one lock per workspace, and let go
-- when the call's use is recorded or it fails. One left by a server that stopped counts for 15 minutes.
create table assistant_reservation (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  user_id uuid not null,
  tokens bigint not null check (tokens > 0),
  created_at timestamptz not null default now(),
  primary key (tenant_id, id)
);

-- ---- Connections to outside systems ----------------------------------------------------------------------------------

-- Connections to outside systems (crates/kernel/src/connect.rs). `has_credentials`: credentials are kept in the vault,
-- bound to the connection; they are never read back through any API.
create table connection (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  provider text not null check (length(provider) between 1 and 100),
  name text not null check (length(name) between 1 and 100),
  config jsonb not null default '{}',
  mapping jsonb not null default '{}',
  has_credentials boolean not null default false,
  enabled boolean not null default false,
  cursors jsonb not null default '{}',
  last_success_at timestamptz,
  last_checked_at timestamptz,
  last_error text,
  created_by uuid,
  created_at timestamptz not null default now(),
  -- A provider that said to wait, or keeps failing, is left alone for a while: every job of the
  -- connection waits together. `failures` counts failures in a row.
  failures int not null default 0,
  waiting_until timestamptz,
  waiting_reason text,
  -- Connections that use one provider account (the same provider and credentials) are known by a keyed
  -- fingerprint of those credentials, which reveals nothing of them: when the provider tells one of them to
  -- wait, every connection on that account waits with it (crates/kernel/src/connect.rs, FEO-067).
  account_key text,
  primary key (tenant_id, id)
);
create index connection_account on connection (account_key) where account_key is not null;
create unique index connection_by_id on connection (id);

-- A record's identity at a provider, both ways, and what it last looked like.
create table connection_link (
  tenant_id uuid not null,
  connection_id uuid not null,
  stream text not null,
  external_id text not null,
  entity text not null,
  record_id uuid not null,
  external_version text,
  hash text,
  local_version int,
  synced_at timestamptz not null default now(),
  primary key (tenant_id, connection_id, stream, external_id),
  foreign key (tenant_id, connection_id) references connection(tenant_id, id) on delete cascade
);
create unique index connection_link_record on connection_link (tenant_id, connection_id, stream, record_id);

-- Webhook events seen, to tell a repeat from a new one.
create table connection_event (
  tenant_id uuid not null,
  connection_id uuid not null,
  event_id text not null,
  received_at timestamptz not null default now(),
  primary key (tenant_id, connection_id, event_id),
  foreign key (tenant_id, connection_id) references connection(tenant_id, id) on delete cascade
);
create index connection_event_received on connection_event (received_at);

-- What synchronization or reconciliation left for a person.
create table connection_drift (
  tenant_id uuid not null,
  id uuid not null,
  connection_id uuid not null,
  stream text not null,
  external_id text not null,
  kind text not null check (kind in ('conflict', 'refused', 'differs', 'missing_here', 'missing_there')),
  detail jsonb not null default '{}',
  created_at timestamptz not null default now(),
  resolved_at timestamptz,
  resolved_by uuid,
  primary key (tenant_id, id),
  foreign key (tenant_id, connection_id) references connection(tenant_id, id) on delete cascade
);
create unique index connection_drift_open on connection_drift (tenant_id, connection_id, stream, external_id, kind) where resolved_at is null;

-- Connections to outside services (crates/kernel/src/connections, docs/builder/decisions.md BD-29 to BD-33): a
-- described HTTP service a workspace calls, is called by, pulls from and signs in to. No provider is named in code.
-- A connection: its description (address, authentication, operations, inbound, pulls), its secrets kept in the vault
-- (`has_secrets`, never read back), off until an administrator turns it on. `source` says where the
-- description came from (`pack:<name>`, `package:<name>`), null for one made here. `refused` counts inbound calls
-- refused; `debug_until` turns on a short-lived, redacted capture of requests and answers in the call log.
create table conn (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  name text not null check (name ~ '^[a-z][a-z0-9_]{0,62}$'),
  label text not null check (length(label) between 1 and 100),
  description jsonb not null,
  has_secrets boolean not null default false,
  enabled boolean not null default false,
  source text,
  refused int not null default 0,
  debug_until timestamptz,
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  unique (tenant_id, name)
);

-- Tokens a person's sign-in at a provider gave (BD-32): the workspace's account (`user_id` nil) or a person's own.
-- The access and refresh tokens are kept in the vault; nothing reads them back out. `told` says the owner heard that it
-- needs reconnecting, so they hear it once.
create table conn_token (
  tenant_id uuid not null,
  conn_id uuid not null,
  user_id uuid not null,
  expires_at timestamptz,
  account text check ((account is null) or (length(account) <= 200)),
  status text not null default 'connected' check (status in ('connected', 'needs_reconnecting')),
  told boolean not null default false,
  connected_by uuid,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, conn_id, user_id),
  foreign key (tenant_id, conn_id) references conn(tenant_id, id) on delete cascade
);

-- A sign-in under way: the SHA-256 of its state (the state itself goes to the browser only), bound to the person and
-- their browser session; the PKCE verifier sealed. Used once, within ten minutes.
create table conn_signin (
  state_digest text primary key,
  tenant_id uuid not null,
  conn_id uuid not null,
  user_id uuid not null,
  owner_id uuid not null,
  session_id uuid not null,
  verifier text not null,
  expires_at timestamptz not null,
  foreign key (tenant_id, conn_id) references conn(tenant_id, id) on delete cascade
);

-- Every call: when, which operation, how it went and how long it took, the record it was about. No bodies, unless the
-- debug capture was on (redacted). A call that changes something with a key keeps its mapped result, so the same key
-- again gets it without calling twice.
create table conn_call (
  tenant_id uuid not null,
  id uuid not null,
  conn_id uuid not null,
  operation text not null,
  at timestamptz not null default now(),
  outcome text not null check (outcome in ('ok', 'failed', 'temporary', 'refused')),
  status int,
  duration_ms int not null default 0,
  entity text,
  record_id uuid,
  error text,
  key text,
  input_digest text,
  result jsonb,
  debug jsonb,
  -- Who made a connection's call (null for the system), so a person's data report and erasure reach the calls they made
  -- (docs/builder/decisions.md BD-35).
  user_id uuid,
  primary key (tenant_id, id),
  foreign key (tenant_id, conn_id) references conn(tenant_id, id) on delete cascade
);
create index conn_call_at on conn_call (tenant_id, conn_id, at desc);
create unique index conn_call_once on conn_call (tenant_id, conn_id, operation, key) where (key is not null) and (outcome = 'ok');
create index conn_call_user on conn_call (tenant_id, user_id) where user_id is not null;

-- Calls from outside, accepted: one row per delivery (the caller's id, else a digest of what it signed), processed
-- once by a job; the payload is dropped once it is.
create table conn_inbound (
  tenant_id uuid not null,
  conn_id uuid not null,
  delivery text not null,
  received_at timestamptz not null default now(),
  state text not null default 'accepted' check (state in ('accepted', 'done', 'failed')),
  payload jsonb,
  error text,
  -- A call signed without a timestamp (docs/builder/decisions.md BD-49): its replay guard is its delivery id, and the
  -- digest of the body it signed, each taken once while the row is kept. A body signed once is refused again whatever
  -- delivery id a header beside it claims, since the header is not signed.
  body_digest text,
  primary key (tenant_id, conn_id, delivery),
  foreign key (tenant_id, conn_id) references conn(tenant_id, id) on delete cascade
);
create unique index conn_inbound_body_once on conn_inbound (tenant_id, conn_id, body_digest) where body_digest is not null;

-- A pull's place between runs (BD-31).
create table conn_pull (
  tenant_id uuid not null,
  conn_id uuid not null,
  name text not null,
  cursor text,
  next_run timestamptz not null default now(),
  last_run timestamptz,
  last_error text,
  -- A pull's failure streak (docs/builder/decisions.md BD-36): `told` says the connection's administrators heard of the
  -- current streak of failed runs, so they hear once; a run that works ends the streak.
  told boolean not null default false,
  -- A pull on a connection each person signs in to runs as each person connected (docs/builder/decisions.md BD-50):
  -- one row, one cursor and one failure streak per person. `user_id` is nil for the workspace's own pull.
  user_id uuid not null default '00000000-0000-0000-0000-000000000000',
  primary key (tenant_id, conn_id, name, user_id),
  foreign key (tenant_id, conn_id) references conn(tenant_id, id) on delete cascade
);

-- Connections, part three (docs/builder/decisions.md BD-53 to BD-61).
--
-- One row per request a key made, or makes: the digest of its inputs, the clock its templates read (fixed at the first
-- attempt, so that a retry sends the same moment), whether a request went out whose answer is not known (`sending`:
-- it may have been delivered, so it is not sent again blindly), and a loop's or a wait's progress.
create table conn_attempt (
  tenant_id uuid not null,
  conn_id uuid not null,
  operation text not null,
  key text not null,
  input_digest text not null,
  clock timestamptz not null default now(),
  sending boolean not null default false,
  progress jsonb not null default '{}',
  at timestamptz not null default now(),
  primary key (tenant_id, conn_id, operation, key),
  foreign key (tenant_id, conn_id) references conn(tenant_id, id) on delete cascade
);

-- A description's `limit`: calls made in each window of `every` seconds, counted across workers.
create table conn_rate (
  tenant_id uuid not null,
  conn_id uuid not null,
  slot bigint not null,
  n int not null default 0,
  -- An operation's own `limit` (docs/builder/decisions.md BD-74): its calls counted in their window apart from the
  -- description's, under the operation's name ('' for the description's own).
  operation text not null default '',
  primary key (tenant_id, conn_id, operation, slot),
  foreign key (tenant_id, conn_id) references conn(tenant_id, id) on delete cascade
);

-- A short-lived public address of a stored file, for a service that pulls media by address (docs/builder/decisions.md
-- BD-64): which file, for which call (the connection, operation, key and the record it is about), for whom, until
-- when, and when it was ended early. The address is `/files/pull/{id}?s=<HMAC of the id under the workspace's key>`.
create table conn_file_address (
  tenant_id uuid not null,
  id uuid primary key,
  conn_id uuid not null,
  operation text not null,
  name text not null,
  call_key text,
  file_id uuid not null,
  entity text,
  record_id uuid,
  user_id uuid,
  expires_at timestamptz not null,
  ended_at timestamptz,
  at timestamptz not null default now(),
  foreign key (tenant_id, conn_id) references conn(tenant_id, id) on delete cascade
);
create index conn_file_address_call on conn_file_address (tenant_id, conn_id, operation, call_key);

-- ---- The installation: settings, identity and moves ------------------------------------------------------------------

-- What this database is, to notice when it has become something else: restored from a backup, or
-- restored over itself. One row. Nothing a tenant has anything to do with.
create table platform_incarnation (
  id boolean default true primary key check (id),
  system_identifier bigint not null,
  timeline int not null,
  database_oid oid not null,
  -- Set from the moment a restore is noticed until a person has looked at what came back.
  held_since timestamptz,
  held_because text,
  released_at timestamptz,
  released_by uuid,
  released_note text,
  -- The last authority this database held. A region that lost it does not take it back by itself.
  authority_epoch bigint not null default 0,
  -- A restore noticed on any service (docs/plan/stateless.md, D7; docs/operations.md, "After restoring a backup"): a
  -- counter that only moves forward, kept here and in object storage beside the journals, written here first. A database
  -- that comes up behind the store's counter has been taken back in time, whatever its identity says.
  -- NULL: a database from before the marker. 0: one born with it, which nothing can be behind yet.
  restore_marker bigint,
  -- When this database last moved it on (one replica moves it at a time, at most every 30 seconds).
  restore_marker_at timestamptz
);

-- A planned move of the whole installation (crates/kernel/src/moves.rs). `ferrum task plan_move` leaves one mark here:
-- first unfenced, while the work in hand finishes (nothing new that reaches outside starts), then fenced, and from then
-- every kernel transaction is read-only. A copy of this database (a dump restored, a physical copy promoted, a backup
-- recovered) finds the mark made on another database than itself, adopts its own identity without the restore hold, and
-- removes the mark. At most one row.
create table platform_move (
  id uuid primary key,
  one boolean not null default true unique check (one),
  made_at timestamptz not null default now(),
  -- A copy that starts later than this takes the restore hold.
  valid_until timestamptz not null,
  -- The database the move was planned on (as in platform_incarnation).
  system_identifier bigint not null,
  timeline int not null,
  database_oid oid not null,
  reason text not null,
  -- Null while the work in hand finishes; set when the database stopped taking changes.
  fenced_at timestamptz
);

-- Asked at the start of every kernel transaction, in the same round trip as its settings. PL/pgSQL, so that its query
-- reads after its lock is granted: `plan_move` fences under ACCESS EXCLUSIVE on the table, and a transaction that waited
-- for it sees the fence (an inlined SQL function would read with the snapshot taken before the wait).
create function ferrum_moving() returns boolean language plpgsql as $$
begin
  return exists (select 1 from platform_move where fenced_at is not null);
end
$$;

-- One workspace moving from one installation to another (crates/kernel/src/workspace_move.rs; docs/operations.md, "Moving
-- one workspace"). One row per workspace on each side, kept after the move as its record. At the source: `leaving`, held
-- (no new background work) and then, from `frozen_at`, frozen; then `moved`, for good. At the target: `arriving`, frozen,
-- while the copy is made and checked; then `arrived`, an ordinary workspace again. The column is `workspace`, not
-- `tenant_id`: this row is about the workspace, and does not travel with it.
create table workspace_move (
  workspace uuid primary key,
  -- The same on both sides.
  move_id uuid not null,
  side text not null check (side in ('source', 'target')),
  state text not null check (state in ('leaving', 'moved', 'arriving', 'arrived')),
  frozen_at timestamptz,
  started_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  -- The other side's database, as an operator names it (host and database, never a password).
  peer text not null,
  -- At the source once moved: the host the workspace answers at, at the target.
  moved_to text,
  -- At the source: the target keeps its files in this one's storage, so finishing the move deletes rows only.
  shared_files boolean not null default false,
  -- At the target: what the copy read at the source, table by table (rows and a digest), to check against before the end.
  copied jsonb,
  -- At the source: when `finish_move` deleted what was left here.
  finished_at timestamptz
);

-- Whether a workspace takes changes in a kernel transaction, asked by every tenant transaction in its first round trip
-- (db.rs). PL/pgSQL, so that its query reads after its lock is granted: freezing takes ACCESS EXCLUSIVE on the table, and
-- a transaction that waited for it sees the freeze (as `ferrum_moving()`).
create function ferrum_workspace_frozen(w text) returns boolean language plpgsql as $$
begin
  return w <> '' and exists (select 1 from workspace_move where workspace = w::uuid
    and (state in ('arriving', 'moved') or (state = 'leaving' and frozen_at is not null)));
end
$$;

-- Whether background work leaves a workspace alone: every claim across workspaces asks it.
create function ferrum_workspace_held(w uuid) returns boolean language sql stable as $$
  select exists (select 1 from workspace_move where workspace = w and state <> 'arrived' and finished_at is null)
$$;

create sequence setting_version;

create table setting (
  scope text not null check (length(scope) between 1 and 40),
  scope_name text not null default '' check (length(scope_name) <= 63),
  key text not null check (length(key) between 1 and 63),
  value jsonb,
  version bigint not null default nextval('public.setting_version'::regclass),
  updated_at timestamptz not null default now(),
  updated_by text not null check (length(updated_by) between 1 and 200),
  primary key (scope, scope_name, key)
);

-- Every change of a setting, with the value before and after: the operator's record of them, which no workspace sees.
create table setting_change (
  id bigint generated always as identity primary key,
  at timestamptz not null default now(),
  scope text not null,
  scope_name text not null,
  key text not null,
  old_value jsonb,
  new_value jsonb,
  version bigint not null,
  updated_by text not null
);
create trigger setting_change_append_only before delete or update on setting_change
  for each row
  execute function ferrum_forbid_change();
create trigger setting_change_no_truncate before truncate on setting_change
  for each statement
  execute function ferrum_forbid_change();

-- The replicas running now: each refreshes its row every 30 seconds, and a row not seen for two minutes is gone.
-- `settings_version` is the settings' version the replica started with: a setting that applies at start and changed
-- after it is one it still runs the older value of.
create table replica (
  id uuid primary key,
  pool text not null check (length(pool) between 1 and 63),
  build_date timestamptz not null,
  started_at timestamptz not null default now(),
  seen_at timestamptz not null default now(),
  settings_version bigint not null default 0
);
create index replica_pool on replica (pool, seen_at);

-- ---- The vault -------------------------------------------------------------------------------------------------------

-- The vault (crates/kernel/src/vault, docs/operations.md "The vault"): the installation key wrapped by each of its
-- slots, every key below it wrapped by the one above, and the secrets sealed by their owner's key. No key that opens
-- anything is stored unwrapped here: a dump of these tables opens nothing without a slot's key, which is never stored.
--
-- Only the vault module reads or writes these tables: row-level security is forced, and the one policy asks for the
-- transaction-local setting `ferrum.vault = 'on'`, which nothing else sets (`ferrum.system` does not open them).
-- Every statement is safe to run again: a test that drops and makes the public schema again runs this file again, and
-- finds the vault as it left it (nothing here depends on the public schema).

-- The installation key, wrapped once per slot: any slot opens it. One slot of each kind.
create table if not exists vault.root_slot (
  kind text primary key check (kind in ('file', 'kms', 'credential', 'recovery')),
  -- The installation key's version this slot wraps, and when that version was made: every slot wraps the current one.
  installation_version int not null check (installation_version > 0),
  installation_made_at timestamptz not null default now(),
  -- The installation key under this slot's key: nonce, ciphertext and tag (AES-256-GCM, bound to the kind and version).
  wrapped bytea not null,
  -- An HMAC of the slot's kind under the slot's key: tells a wrong key (a changed password) from a damaged row.
  check_value bytea not null,
  -- How the slot's key is made again: a salt (credential, recovery), Argon2id's costs (credential), how the
  -- key-management service is asked (kms: transit or command). Never a key.
  salt bytea,
  params jsonb not null default '{}',
  -- kms through Transit: the slot's key as the service's ciphertext.
  kms_wrapped text,
  created_at timestamptz not null default now()
);

-- Workspace keys (owner '') and owner keys, each version wrapped by the key above it.
create table if not exists vault.key (
  -- The workspace, or the nil id: the installation's own scope, for what belongs to no workspace.
  scope uuid not null,
  -- '' for the scope's workspace key; else the owner: platform, an app's id, or plugin:<name>.
  owner text not null check ((owner = '') or (owner ~ '^(platform|[a-z0-9][a-z0-9-]{0,62}|plugin:[a-z0-9][a-z0-9_.-]{0,99})$')),
  version int not null check (version > 0),
  -- The version of what wraps it: the installation key (a workspace key), or the scope's workspace key (an owner key).
  parent_version int not null check (parent_version > 0),
  -- An owner key's workspace key is the row of its scope with owner '': kept while an owner key names it.
  parent_owner text,
  wrapped bytea not null,
  -- Something sealed or derived with this owner key's version is kept outside the vault: it is not retired.
  outside boolean not null default false,
  created_at timestamptz not null default now(),
  check (((owner = '') and (parent_owner is null)) or ((owner <> '') and (parent_owner = ''))),
  primary key (scope, owner, version),
  foreign key (scope, parent_owner, parent_version) references vault.key(scope, owner, version)
);

-- Secrets, each version sealed by a version of its owner's key, which is kept while a secret names it.
create table if not exists vault.secret (
  scope uuid not null,
  owner text not null,
  name text not null check (name ~ '^[a-z0-9][a-z0-9_.-]{0,99}$'),
  version int not null check (version > 0),
  key_version int not null,
  kind text not null check (kind in ('value', 'ed25519')),
  sealed bytea not null,
  -- An Ed25519 key's public half, which is not secret.
  public_key bytea,
  -- What may be done with it (crate::vault::Rule): reveal, the hosts it may go to, a sandbox's treatment, the overlap
  -- of a rotation, the expiry.
  rule jsonb not null default '{}',
  -- A version a newer one replaced: it still verifies until then (its rule's overlap). Null: the current one.
  valid_until timestamptz,
  created_at timestamptz not null default now(),
  created_by uuid,
  -- The day it was last used, kept coarsely.
  last_used date,
  primary key (scope, owner, name, version),
  foreign key (scope, owner, key_version) references vault.key(scope, owner, version)
);
create index if not exists secret_by_key on vault.secret (scope, owner, key_version);

-- Uses of a secret in an hour, for the audit trail's cap (as sign-in failures are capped): the first ones are recorded
-- one by one, the rest counted and recorded as one summary once the hour is over.
create table if not exists vault.use_count (
  scope uuid not null,
  owner text not null,
  name text not null,
  verb text not null,
  hour timestamptz not null,
  n int not null,
  primary key (scope, owner, name, verb, hour)
);

-- The installation's own record of its vault (crates/kernel/src/vault): its slots, its installation key, and the
-- secrets of the installation's own scope, each change and each use (under the same hourly cap as a workspace's).
-- A workspace's are on that workspace's audit trail; these belong to no workspace, and no workspace sees them. It is
-- the vault's alone, as its other tables are (`ferrum.vault`), and append-only: nothing changes or removes a row. An
-- operator reads it with `ferrum task vault_events`. Every statement is safe to run again, as the vault's others.
create table if not exists vault.event (
  id bigint generated always as identity primary key,
  at timestamptz not null default now(),
  -- What happened: `made`, `slot_added`, `installation_key_rotated`, a secret's `set`, `reveal`, `reveal_summary`, ...
  action text not null check (length(action) between 1 and 100),
  -- Whose secret it was about, and which (empty for the root's own events); never a value.
  owner text not null default '',
  name text not null default '',
  details jsonb not null default '{}',
  -- Who asked, when a person did; the request or job it was part of.
  actor uuid,
  request_id text not null default ''
);

create or replace function vault.forbid_change() returns trigger language plpgsql as $$
begin
  raise exception 'ferrum: % on % is not allowed (append-only table)', tg_op, tg_table_name using errcode = 'P0001';
end $$;
drop trigger if exists event_append_only on vault.event;
create trigger event_append_only before delete or update on vault.event
  for each row
  execute function vault.forbid_change();
drop trigger if exists event_no_truncate on vault.event;
create trigger event_no_truncate before truncate on vault.event
  for each statement
  execute function vault.forbid_change();

-- A workspace's keys made ready to move to another installation (crates/kernel/src/vault/scope.rs, `ferrum task
-- vault_transfer`): each version of its workspace key, wrapped by a transfer key made for this one move of this one
-- workspace. The transfer key is shown once to the operator and never stored: without it these rows open nothing.
-- A new transfer of the workspace replaces the one before; finish_move and the workspace's deletion remove them with
-- the rest of its vault. Every statement is safe to run again, as the vault's others.
create table if not exists vault.transfer (
  scope uuid not null,
  version int not null check (version > 0),
  -- The transfer it belongs to: one id for all the versions it holds.
  id uuid not null,
  made_at timestamptz not null default now(),
  -- An HMAC under the transfer key: tells a wrong key from a damaged row.
  check_value bytea not null,
  -- The workspace key's version under the transfer key (AES-256-GCM, bound to the workspace, the transfer, the version).
  wrapped bytea not null,
  primary key (scope, version)
);

-- A secret its app manages itself (crates/kernel/src/vault/secrets.rs, `Vault::manage`): the routes people use to set,
-- rotate, remove, change the rule of or regenerate a secret refuse it, naming the app. A row of its own,
-- carried with the workspace's vault.
create table if not exists vault.managed (
  scope uuid not null,
  owner text not null,
  name text not null,
  managed_at timestamptz not null default now(),
  primary key (scope, owner, name)
);

-- ---- Row-level security -----------------------------------------------------------------------------------------

-- Tenant tables: a row is visible and writable inside its own tenant, and to the system.
do $$
declare t text;
begin
  foreach t in array array[
    'app_user', 'role', 'user_role', 'role_permission', 'user_scope', 'api_key', 'scim_group', 'scim_group_member',
    'support_grant', 'support_access', 'usage_day', 'role_conflict', 'person_away', 'user_recovery_code',
    'sso_provider', 'user_session', 'sign_in_code', 'passkey', 'sso_sid', 'sso_logout_seen', 'sign_in_failure',
    'known_device', 'ui_pref', 'tenant_ui_pin', 'public_frame', 'installed_package', 'meta_overlay',
    'naming_counter', 'plugin', 'plugin_kv', 'plugin_event_run', 'plugin_schedule', 'tenant_app', 'app_licence',
    'backfill_run', 'pack_install', 'custom_record', 'record_comment', 'record_assignment', 'record_tag',
    'record_follower', 'record_secret', 'coedit_doc', 'coedit_update', 'record_alias', 'shared_view', 'custom_key',
    'record_target', 'document_template', 'portal_signup', 'action_link', 'document_default',
    'document_template_version', 'published_key', 'published_view', 'published_vote', 'published_hit', 'file_object',
    'file_upload', 'audit_event', 'audit_anchor', 'outbox', 'idempotency_key', 'webhook', 'webhook_delivery',
    'automation', 'automation_run', 'notification', 'notification_pref', 'push_subscription', 'job', 'event_receipt',
    'automation_occurrence', 'approval_request', 'approval_decision', 'message_template', 'message_channel',
    'message_send', 'message_code', 'message_reply_seen', 'person_identifier', 'legal_hold',
    'processing_restriction', 'retention_rule', 'held_version', 'contact_consent', 'privacy_erasure', 'logic_effect',
    'logic_release', 'logic_review', 'logic_activation', 'logic_refusal', 'event_chain', 'logic_run', 'logic_task',
    'logic_task_decision', 'logic_wait', 'logic_signal', 'logic_calendar', 'logic_draft', 'logic_attempt',
    'logic_erasure', 'assistant_thread', 'assistant_message', 'assistant_prompt', 'assistant_reservation',
    'connection', 'connection_link', 'connection_event', 'connection_drift', 'conn', 'conn_token', 'conn_signin',
    'conn_call', 'conn_inbound', 'conn_pull', 'conn_attempt', 'conn_rate', 'conn_file_address'
  ] loop
    execute format('alter table %s enable row level security', t);
    execute format('alter table %s force row level security', t);
    execute format('create policy tenant_isolation on %s using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;

-- The audit trail's first months, with its policy (ferrum_ensure_month_partitions).
select ferrum_ensure_month_partitions('audit_event', now(), now() + interval '3 months');

-- Tables no workspace reaches, only the system: the installation's own (settings, replicas, the object store), and what
-- runs before anyone is signed in or any workspace is known (token endpoints, sign-in flows, keys).
do $$
declare t text;
begin
  foreach t in array array[
    'oauth_client', 'oauth_code', 'oauth_refresh', 'sso_flow', 'webauthn_challenge', 'email_code', 'host_code',
    'server_key', 'saml_sp_key', 'saml_enc_key', 'saml_logout', 'oauth_companion', 'oidc_code', 'oidc_subject',
    'oidc_sign_in', 'file_store', 'file_move', 'object_release', 'setting', 'setting_change', 'replica'
  ] loop
    execute format('alter table %s enable row level security', t);
    execute format('alter table %s force row level security', t);
    execute format('create policy system_only on %s using ((select ferrum_is_system())) with check ((select ferrum_is_system()))', t);
  end loop;
end $$;

-- The vault's tables: only a transaction that says it is the vault's (`ferrum.vault = on`).
do $$
declare t text;
begin
  foreach t in array array[
    'vault.root_slot', 'vault.key', 'vault.secret', 'vault.use_count', 'vault.event', 'vault.transfer',
    'vault.managed'
  ] loop
    execute format('alter table %s enable row level security', t);
    execute format('alter table %s force row level security', t);
    execute format('drop policy if exists vault_only on %s', t);
    execute format('create policy vault_only on %s using (current_setting(''ferrum.vault'', true) = ''on'') with check (current_setting(''ferrum.vault'', true) = ''on'')', t);
  end loop;
end $$;
