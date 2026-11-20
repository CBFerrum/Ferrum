-- A first second factor is confirmed by a code mailed to the person where the server mails (API-K2 part b). Those codes
-- are kept with the sign-in codes, apart from them by purpose, so that neither does the other's work.
alter table sign_in_code add column purpose text not null default 'sign_in';
alter table sign_in_code add constraint sign_in_code_purpose -- window: safe, an older build's rows take the default, which the check allows
  check (purpose in ('sign_in', 'second_factor')) not valid;
alter table sign_in_code validate constraint sign_in_code_purpose;
