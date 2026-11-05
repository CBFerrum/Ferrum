-- A role may be given until a moment (FERRUM-IDENTITY-009): a contractor for the length of their
-- contract, a stand-in while someone is away. From then the person no longer holds it: their
-- context leaves it out (and a cached one lapses at that moment), and the hourly upkeep removes the
-- grant, so whatever reads who holds a role stops finding them.
alter table user_role add column expires_at timestamptz;
