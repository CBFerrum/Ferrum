-- Dated exceptions of a business calendar (ferrum_logic::calendar::Exception): `{"2026-12-24": {"open": false,
-- "hours": [["12:00", "24:00"]]}}`, what a date is whatever its weekday and holidays say. None by default, so a
-- calendar behaves as before.
alter table logic_calendar add column exceptions jsonb not null default '{}';
