-- Which package wrote a packaged rule row (docs/builder/decisions.md BD-40, corrected): a packaged row follows the
-- releases of that package only. Two packages declaring a rule for the same role and entity no longer overwrite one
-- another: the first row stays, as before BD-40. Rows written packaged since BD-40 without a package are of unknown
-- origin and no package claims them.
alter table role_permission add column package text;
