-- Hours of the day on business calendars: per weekday, windows in the calendar's zone
-- ({"mon": [["09:00", "13:00"], ["14:00", "18:00"]]}). Null: every hour of a business day is open.
alter table logic_calendar add column hours jsonb;
