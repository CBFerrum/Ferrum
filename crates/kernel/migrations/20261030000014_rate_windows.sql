-- Requests counted per key (client, workspace...) and fixed window, shared by every replica so
-- rate limits hold however many replicas serve. Unlogged: a crash only forgets open windows.
create unlogged table rate_window (
  key text not null,
  window_start timestamptz not null,
  hits integer not null,
  primary key (key, window_start)
);
