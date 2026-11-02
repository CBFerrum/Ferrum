-- What has been established about one exact artifact. A label is of a digest, so a new build of the
-- same app starts with none, however much it resembles the last one.
create table app_label (
  app_id text not null,
  digest text not null,
  dimension text not null check (dimension in ('community', 'security_reviewed', 'operationally_verified', 'provider_qualified', 'production_supported')),
  profile text not null,
  evidence text not null check (length(trim(evidence)) > 0),
  reviewer text not null check (length(trim(reviewer)) > 0),
  granted_at timestamptz not null default now(),
  expires_at timestamptz,
  primary key (app_id, digest, dimension, profile)
);
