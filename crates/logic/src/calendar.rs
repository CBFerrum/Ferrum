//! Waiting (package MASTER_PLAN §12.3, FLC-HUMAN-005): an elapsed duration, or business days on a calendar.
//!
//! A business day is a date of the calendar's zone that is neither a weekend day nor one of its holidays, and
//! a deadline `n` business days on keeps the local time of day it started at. When that local time does not
//! exist on the day (clocks went forward), the deadline is the first instant after the gap; when it exists
//! twice (clocks went back), it is the earlier. A deadline records the calendar's revision, its zone and the
//! zone data it was worked out with, so a revised calendar changes later deadlines, never the ones resolved.

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// The most business days one wait may span (a year of five-day weeks).
pub const MAX_BUSINESS_DAYS: u32 = 260;
/// The longest a wait may be, in seconds (a year).
pub const MAX_WAIT: u64 = 31_536_000;

/// How long something waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wait {
    Elapsed(u64),
    BusinessDays { days: u32, calendar: String },
}

impl Wait {
    /// Seconds, an ISO 8601 duration (`P2D`, `PT4H30M`), or `{"business_days": n, "calendar": "name"}`.
    pub fn parse(v: &Value) -> Option<Wait> {
        if let Some(m) = v.as_object() {
            let days = m.get("business_days")?.as_u64()?;
            let calendar = m.get("calendar")?.as_str()?.to_string();
            if m.len() != 2 || days == 0 || days > u64::from(MAX_BUSINESS_DAYS) || calendar.is_empty() {
                return None;
            }
            return Some(Wait::BusinessDays { days: days as u32, calendar });
        }
        let secs = if let Some(n) = v.as_u64() { n } else { iso_duration(v.as_str()?)? };
        (secs > 0 && secs <= MAX_WAIT).then_some(Wait::Elapsed(secs))
    }

    pub fn calendar(&self) -> Option<&str> {
        match self {
            Wait::BusinessDays { calendar, .. } => Some(calendar),
            Wait::Elapsed(_) => None,
        }
    }
}

fn iso_duration(s: &str) -> Option<u64> {
    let rest = s.strip_prefix('P')?;
    let (days, time) = rest.split_once('T').unwrap_or((rest, ""));
    let take = |part: &str, units: &[(char, u64)]| -> Option<u64> {
        let mut total = 0u64;
        let mut num = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() {
                num.push(c);
            } else {
                let (_, mult) = units.iter().find(|(u, _)| *u == c)?;
                total = total.checked_add(num.parse::<u64>().ok()?.checked_mul(*mult)?)?;
                num.clear();
            }
        }
        num.is_empty().then_some(total)
    };
    take(days, &[('W', 604_800), ('D', 86_400)])?.checked_add(take(time, &[('H', 3600), ('M', 60), ('S', 1)])?)
}

/// A workspace's business calendar, at one revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Calendar {
    pub name: String,
    /// An IANA zone (`Asia/Kolkata`).
    pub zone: String,
    /// Days of the week that are not business days (`sat`, `sun`).
    pub weekends: Vec<String>,
    pub holidays: BTreeSet<NaiveDate>,
    pub revision: i64,
}

/// A wait resolved to an instant, with what it was worked out from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Deadline {
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    pub tzdb: String,
    /// What happened to the local time, when anything did: `skipped` (moved past a gap), `repeated` (the
    /// earlier of two).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adjusted: Option<String>,
}

fn weekday(name: &str) -> Option<Weekday> {
    Some(match name {
        "mon" => Weekday::Mon,
        "tue" => Weekday::Tue,
        "wed" => Weekday::Wed,
        "thu" => Weekday::Thu,
        "fri" => Weekday::Fri,
        "sat" => Weekday::Sat,
        "sun" => Weekday::Sun,
        _ => return None,
    })
}

impl Calendar {
    /// Whether it can be used: a known zone, known weekday names, and at least one business day a week.
    pub fn problem(&self) -> Option<String> {
        if self.zone.parse::<Tz>().is_err() {
            return Some(format!("{} is not a time zone", self.zone));
        }
        if let Some(bad) = self.weekends.iter().find(|w| weekday(w).is_none()) {
            return Some(format!("{bad} is not a day of the week (mon … sun)"));
        }
        let off: BTreeSet<&str> = self.weekends.iter().map(String::as_str).collect();
        (off.len() >= 7).then(|| "a calendar needs at least one business day a week".to_string())
    }

    fn business(&self, d: NaiveDate) -> bool {
        !self.weekends.iter().filter_map(|w| weekday(w)).any(|w| w == d.weekday()) && !self.holidays.contains(&d)
    }

    /// `days` business days after `from`, at the same local time.
    pub fn add_business_days(&self, from: DateTime<Utc>, days: u32) -> Result<Deadline, String> {
        if let Some(p) = self.problem() {
            return Err(p);
        }
        let tz: Tz = self.zone.parse().map_err(|_| format!("{} is not a time zone", self.zone))?;
        let local = from.with_timezone(&tz).naive_local();
        let (mut date, time) = (local.date(), local.time());
        let mut left = days;
        // A year and a half of days at most: a calendar with one business day a week still ends.
        for _ in 0..550 {
            if left == 0 {
                break;
            }
            date = date.succ_opt().ok_or("past the end of the calendar")?;
            if self.business(date) {
                left -= 1;
            }
        }
        if left > 0 {
            return Err(format!("{days} business days do not fit in the calendar's next 550 days"));
        }
        let (at, adjusted) = resolve(&tz, date.and_time(time))?;
        Ok(Deadline {
            at,
            calendar: Some(self.name.clone()),
            revision: Some(self.revision),
            zone: Some(self.zone.clone()),
            tzdb: chrono_tz::IANA_TZDB_VERSION.to_string(),
            adjusted,
        })
    }
}

/// A local time as an instant: the earlier of two, or the first instant after a gap.
fn resolve(tz: &Tz, local: NaiveDateTime) -> Result<(DateTime<Utc>, Option<String>), String> {
    match tz.from_local_datetime(&local) {
        LocalResult::Single(t) => Ok((t.with_timezone(&Utc), None)),
        LocalResult::Ambiguous(early, _) => Ok((early.with_timezone(&Utc), Some("repeated".into()))),
        LocalResult::None => {
            // Gaps are at most a few hours: walk forward a minute at a time to the first time that exists.
            let mut probe = local;
            for _ in 0..(6 * 60) {
                probe += Duration::minutes(1);
                // Zones move their clocks on whole minutes, so the first minute that exists is where the gap ends.
                if let LocalResult::Single(t) | LocalResult::Ambiguous(t, _) = tz.from_local_datetime(&probe) {
                    return Ok((t.with_timezone(&Utc), Some("skipped".into())));
                }
            }
            Err(format!("{local} does not exist in {tz} and no time follows it within six hours"))
        }
    }
}

/// When a wait started at `from` ends. A calendar is needed exactly when the wait counts business days.
pub fn deadline(wait: &Wait, from: DateTime<Utc>, calendar: Option<&Calendar>) -> Result<Deadline, String> {
    match wait {
        Wait::Elapsed(secs) => Ok(Deadline {
            at: from + Duration::seconds(*secs as i64),
            calendar: None,
            revision: None,
            zone: None,
            tzdb: chrono_tz::IANA_TZDB_VERSION.to_string(),
            adjusted: None,
        }),
        Wait::BusinessDays { days, calendar: name } => match calendar {
            Some(c) if c.name == *name => c.add_business_days(from, *days),
            _ => Err(format!("the calendar {name} is not in this workspace")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn india(holidays: &[&str], revision: i64) -> Calendar {
        Calendar {
            name: "india".into(),
            zone: "Asia/Kolkata".into(),
            weekends: vec!["sat".into(), "sun".into()],
            holidays: holidays.iter().map(|d| d.parse().unwrap()).collect(),
            revision,
        }
    }

    #[test]
    fn waits_are_read_strictly() {
        assert_eq!(Wait::parse(&json!("P2D")), Some(Wait::Elapsed(172_800)));
        assert_eq!(Wait::parse(&json!(60)), Some(Wait::Elapsed(60)));
        assert_eq!(Wait::parse(&json!({"business_days": 3, "calendar": "india"})), Some(Wait::BusinessDays { days: 3, calendar: "india".into() }));
        for bad in [
            json!(0),
            json!("P"),
            json!("2 days"),
            json!({"business_days": 0, "calendar": "x"}),
            json!({"business_days": 2}),
            json!({"business_days": 2, "calendar": "x", "extra": 1}),
            json!("P400D"),
        ] {
            assert_eq!(Wait::parse(&bad), None, "{bad}");
        }
    }

    #[test]
    fn business_days_skip_weekends_and_holidays_at_the_same_local_time() {
        // Friday 6 Nov 2026, 15:30 in Kolkata; Monday 9th is a holiday: two business days on is Wednesday 11th.
        let from = utc("2026-11-06T10:00:00Z");
        let d = india(&["2026-11-09"], 4).add_business_days(from, 2).unwrap();
        assert_eq!(d.at, utc("2026-11-11T10:00:00Z"));
        assert_eq!((d.calendar.as_deref(), d.revision, d.adjusted.as_deref()), (Some("india"), Some(4), None));
        // A revised calendar moves deadlines resolved after the revision, and says which revision did.
        let revised = india(&["2026-11-09", "2026-11-10"], 5).add_business_days(from, 2).unwrap();
        assert_eq!((revised.at, revised.revision), (utc("2026-11-12T10:00:00Z"), Some(5)));
    }

    #[test]
    fn a_skipped_local_time_moves_past_the_gap_and_a_repeated_one_takes_the_earlier() {
        let ny = Calendar { name: "ny".into(), zone: "America/New_York".into(), weekends: vec![], holidays: BTreeSet::new(), revision: 1 };
        // 02:30 on 8 March 2026 does not exist in New York (clocks go from 02:00 to 03:00).
        let from = Tz::America__New_York.with_ymd_and_hms(2026, 3, 7, 2, 30, 0).unwrap().with_timezone(&Utc);
        let d = ny.add_business_days(from, 1).unwrap();
        assert_eq!(d.adjusted.as_deref(), Some("skipped"));
        assert_eq!(d.at.with_timezone(&Tz::America__New_York).naive_local().to_string(), "2026-03-08 03:00:00");
        // 01:30 on 1 November 2026 happens twice; the earlier is the daylight-saving one (UTC-4).
        let from = Tz::America__New_York.with_ymd_and_hms(2026, 10, 31, 1, 30, 0).unwrap().with_timezone(&Utc);
        let d = ny.add_business_days(from, 1).unwrap();
        assert_eq!((d.adjusted.as_deref(), d.at), (Some("repeated"), utc("2026-11-01T05:30:00Z")));
    }

    #[test]
    fn a_wait_names_the_calendar_it_needs() {
        let wait = Wait::BusinessDays { days: 1, calendar: "india".into() };
        assert!(deadline(&wait, utc("2026-11-06T10:00:00Z"), None).unwrap_err().contains("not in this workspace"));
        assert!(Calendar { weekends: ["mon", "tue", "wed", "thu", "fri", "sat", "sun"].map(String::from).to_vec(), ..india(&[], 1) }.problem().is_some());
        assert!(Calendar { zone: "Mars/Olympus".into(), ..india(&[], 1) }.problem().is_some());
        assert_eq!(deadline(&Wait::Elapsed(60), utc("2026-11-06T10:00:00Z"), None).unwrap().at, utc("2026-11-06T10:01:00Z"));
    }
}
