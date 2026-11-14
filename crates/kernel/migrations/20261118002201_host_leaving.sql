-- A released host's leaving period (docs/operations.md, "Changing or removing an address"): until this moment it
-- still serves the people already signed in there, so that what their browsers keep for it can be sent; then it
-- redirects as a released host does. Empty: released with no leaving period (as before this column).
alter table tenant_host add column leaving_until timestamptz;
