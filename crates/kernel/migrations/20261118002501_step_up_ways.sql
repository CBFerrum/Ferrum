-- Ways to confirm it is you (docs/operations.md, "Asking again before sensitive work"): besides the password, a passkey
-- and signing in again at the identity provider. Each proof is bound to the browser session and the person it is for,
-- and is used once.

-- A passkey's challenge for a step-up names the session it is for.
alter table webauthn_challenge drop constraint webauthn_challenge_purpose_check;
alter table webauthn_challenge add constraint webauthn_challenge_purpose_check check (purpose in ('register', 'sign_in', 'step_up'))
  -- window: safe, the check only widens: an older build writes and takes 'register' and 'sign_in' alone, as before
  ;
alter table webauthn_challenge add column session_id uuid;

-- Signing in again at the identity provider: the session and person it is for, and the page on the host they work on
-- that hands the proof to their dialog. Such a flow never signs anyone in (crates/kernel/src/sso.rs), and a sign-in's
-- flow (these columns empty) never confirms anyone.
alter table sso_flow add column step_up_session uuid;
alter table sso_flow add column step_up_user uuid;
alter table sso_flow add column step_up_back text;
