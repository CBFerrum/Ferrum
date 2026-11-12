//! Waiting (package MASTER_PLAN §12.3, FLC-HUMAN-005): an elapsed duration, or business days on a calendar.
//!
//! A business day is a date of the calendar's zone that is neither a weekend day nor one of its holidays, and
//! a deadline `n` business days on keeps the local time of day it started at. When that local time does not
//! exist on the day (clocks went forward), the deadline is the first instant after the gap; when it exists
//! twice (clocks went back), it is the earlier. A deadline records the calendar's revision, its zone and the
//! zone data it was worked out with, so a revised calendar changes later deadlines, never the ones resolved.
//!
//! A calendar may also have hours of the day: windows per weekday (`"mon": [["09:00", "13:00"]]`) in its zone.
//! Without them every hour of a business day is open; with them a weekday without windows is closed, and a
//! weekend day or a holiday is closed whatever its windows say. Working time (`add_working`,
//! `working_between`, `next_open`, `is_open`) is real elapsed time inside open windows. A window starts at the
//! first instant its local start time names and ends at the last instant its local end time names: a start
//! or end that does not exist (clocks went forward) is the first instant after the gap, one that exists twice
//! (clocks went back) is the earlier for a start and the later for an end. So a window across a gap is
//! shorter by the gap, one across a repeated hour is longer by it, a window inside a gap is empty, and none
//! has negative length; windows that the repeated hour makes overlap count their shared time once.
//!
//! Dated exceptions say what one local date is, whatever its weekday or holidays say: `open`, with its own
//! windows (all day without them), which opens a holiday or a weekend day and replaces a weekday's hours; or
//! closed, all day, or with windows only those hours of the day's own. A date open by an exception is a business
//! day; one closed all day is not.

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The most business days one wait may span (a year of five-day weeks).
pub const MAX_BUSINESS_DAYS: u32 = 260;
/// How far the working-time functions look for open time, in days: five years, so that a year of open time
/// at forty hours a week fits.
pub const MAX_SEARCH_DAYS: u32 = 1830;
/// Dated exceptions one calendar holds.
pub const MAX_EXCEPTIONS: usize = 1000;
/// The longest a wait may be, in seconds (a year).
pub const MAX_WAIT: u64 = 31_536_000;

/// How long something waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wait {
    Elapsed(u64),
    BusinessDays {
        days: u32,
        calendar: String,
    },
    /// Seconds of the calendar's open time.
    Working {
        seconds: u64,
        calendar: String,
    },
}

impl Wait {
    /// Seconds, an ISO 8601 duration (`P2D`, `PT4H30M`), `{"business_days": n, "calendar": "name"}`, or
    /// `{"working": "PT8H", "calendar": "name"}` (seconds or a duration of open time).
    pub fn parse(v: &Value) -> Option<Wait> {
        if let Some(m) = v.as_object() {
            let calendar = m.get("calendar")?.as_str()?.to_string();
            if m.len() != 2 || calendar.is_empty() {
                return None;
            }
            if let Some(working) = m.get("working") {
                return Some(Wait::Working { seconds: seconds(working)?, calendar });
            }
            let days = m.get("business_days")?.as_u64()?;
            if days == 0 || days > u64::from(MAX_BUSINESS_DAYS) {
                return None;
            }
            return Some(Wait::BusinessDays { days: days as u32, calendar });
        }
        Some(Wait::Elapsed(seconds(v)?))
    }

    pub fn calendar(&self) -> Option<&str> {
        match self {
            Wait::BusinessDays { calendar, .. } | Wait::Working { calendar, .. } => Some(calendar),
            Wait::Elapsed(_) => None,
        }
    }
}

/// Seconds or an ISO 8601 duration, more than none and at most [`MAX_WAIT`].
fn seconds(v: &Value) -> Option<u64> {
    let secs = if let Some(n) = v.as_u64() { n } else { iso_duration(v.as_str()?)? };
    (secs > 0 && secs <= MAX_WAIT).then_some(secs)
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
    /// Hours of the day each weekday is open, in the zone: `{"mon": [["09:00", "13:00"], ["14:00", "18:00"]]}`,
    /// end at most `24:00`. Left out, every hour of a business day is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<BTreeMap<String, Vec<(String, String)>>>,
    /// What given local dates are, whatever the rest says: `{"2026-12-24": {"open": false, "hours": [["12:00", "24:00"]]}}`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exceptions: BTreeMap<NaiveDate, Exception>,
}

/// One date's exception: open (in `hours`, all day without them), or closed (only in `hours` when given).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exception {
    pub open: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hours: Vec<(String, String)>,
}

/// Windows as minutes of the day, sorted, or what is wrong with them (`what` names them).
fn spans_of(what: &str, windows: &[(String, String)]) -> Result<Vec<(u32, u32)>, String> {
    let mut spans = vec![];
    for (s, e) in windows {
        match (minute(s), minute(e)) {
            (Some(a), Some(b)) if a < b => spans.push((a, b)),
            _ => return Err(format!("{what} {s}–{e} is not a window (HH:MM to a later HH:MM, at most 24:00)")),
        }
    }
    spans.sort();
    if spans.windows(2).any(|w| w[1].0 < w[0].1) {
        return Err(format!("the windows of {what} overlap"));
    }
    Ok(spans)
}

/// An open stretch of time, `[start, end)`.
pub type Span = (DateTime<Utc>, DateTime<Utc>);

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

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
        if off.len() >= 7 {
            return Some("a calendar needs at least one business day a week".to_string());
        }
        if self.exceptions.len() > MAX_EXCEPTIONS {
            return Some(format!("exceptions: at most {MAX_EXCEPTIONS} dates"));
        }
        if let Some(p) = self.exceptions.iter().find_map(|(d, x)| spans_of(&d.to_string(), &x.hours).err()) {
            return Some(format!("exceptions: {p}"));
        }
        let hours = self.hours.as_ref()?;
        for (day, windows) in hours {
            if weekday(day).is_none() {
                return Some(format!("hours: {day} is not a day of the week (mon … sun)"));
            }
            if !windows.is_empty() && off.contains(day.as_str()) {
                return Some(format!("hours: {day} is a weekend day, so it has no hours"));
            }
            if let Err(p) = spans_of(day, windows) {
                return Some(format!("hours: {p}"));
            }
        }
        (!hours.values().any(|w| !w.is_empty())).then(|| "hours: a calendar with hours needs at least one window a week".to_string())
    }

    /// A business day by its weekday and holidays alone.
    fn regular(&self, d: NaiveDate) -> bool {
        !self.weekends.iter().filter_map(|w| weekday(w)).any(|w| w == d.weekday()) && !self.holidays.contains(&d)
    }

    /// A business day, its exception first: open makes it one, closed all day takes it away.
    fn business(&self, d: NaiveDate) -> bool {
        match self.exceptions.get(&d) {
            Some(x) if x.open => true,
            Some(x) if x.hours.is_empty() => false,
            _ => self.regular(d),
        }
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
        let (at, adjusted) = resolve(&tz, date.and_time(time), false)?;
        Ok(self.deadline(at, adjusted))
    }

    fn deadline(&self, at: DateTime<Utc>, adjusted: Option<String>) -> Deadline {
        Deadline {
            at,
            calendar: Some(self.name.clone()),
            revision: Some(self.revision),
            zone: Some(self.zone.clone()),
            tzdb: chrono_tz::IANA_TZDB_VERSION.to_string(),
            adjusted,
        }
    }

    /// The open windows of a local date, as instants (see the module's header for skipped and repeated times).
    fn windows(&self, tz: &Tz, d: NaiveDate) -> Vec<Span> {
        let own = || -> Vec<(u32, u32)> {
            if !self.regular(d) {
                return vec![];
            }
            match &self.hours {
                None => vec![(0, 1440)],
                Some(h) => {
                    h.get(DAYS[d.weekday().num_days_from_monday() as usize]).into_iter().flatten().filter_map(|(s, e)| Some((minute(s)?, minute(e)?))).collect()
                }
            }
        };
        let given = |x: &Exception| spans_of("", &x.hours).unwrap_or_default();
        let mut spans: Vec<(u32, u32)> = match self.exceptions.get(&d) {
            None => own(),
            Some(x) if x.open && x.hours.is_empty() => vec![(0, 1440)],
            Some(x) if x.open => given(x),
            Some(x) if x.hours.is_empty() => vec![],
            // Closed in some hours: the day's own windows without them.
            Some(x) => {
                let shut = given(x);
                own()
                    .into_iter()
                    .flat_map(|(s, e)| {
                        let (mut cur, mut out) = (s, vec![]);
                        for &(a, b) in &shut {
                            if b <= cur || a >= e {
                                continue;
                            }
                            if a > cur {
                                out.push((cur, a));
                            }
                            cur = cur.max(b);
                        }
                        if cur < e {
                            out.push((cur, e));
                        }
                        out
                    })
                    .collect()
            }
        };
        spans.sort();
        // A time no later than six hours after a gap always exists, except on a day the zone skipped whole
        // (Pacific/Apia, 30 December 2011): such a window is not there.
        let at = |m: u32, later| resolve(tz, d.and_time(NaiveTime::MIN) + Duration::minutes(m.into()), later).ok().map(|(t, _)| t);
        spans.into_iter().filter_map(|(s, e)| Some((at(s, false)?, at(e, true)?))).filter(|(a, b)| a < b).collect()
    }

    /// The open stretches that end after `t`, in order, within [`MAX_SEARCH_DAYS`] of it. Windows that a
    /// repeated hour makes overlap are one stretch.
    fn open_after(&self, t: DateTime<Utc>) -> Result<impl Iterator<Item = Span> + '_, String> {
        if let Some(p) = self.problem() {
            return Err(p);
        }
        let tz: Tz = self.zone.parse().map_err(|_| format!("{} is not a time zone", self.zone))?;
        let first = t.with_timezone(&tz).date_naive().pred_opt().ok_or("before the start of the calendar")?;
        let mut raw = first.iter_days().take(MAX_SEARCH_DAYS as usize + 2).flat_map(move |d| self.windows(&tz, d)).peekable();
        let merged = std::iter::from_fn(move || {
            let (s, mut e) = raw.next()?;
            while let Some(&(_, e2)) = raw.peek().filter(|(s2, _)| *s2 < e) {
                e = e.max(e2);
                raw.next();
            }
            Some((s, e))
        });
        Ok(merged.filter(move |&(_, e)| e > t))
    }

    /// Open time from `from` on, without the `stopped` intervals, as successive pieces.
    fn counted(&self, from: DateTime<Utc>, stopped: &[Span]) -> Result<impl Iterator<Item = Span> + '_, String> {
        let mut stop: Vec<Span> = stopped.iter().copied().filter(|&(a, b)| a < b && b > from).collect();
        stop.sort();
        Ok(self.open_after(from)?.flat_map(move |(a, b)| {
            let (mut cur, mut out) = (a.max(from), vec![]);
            // ponytail: scans the stopped list for every window; fine for the dozens a record has.
            for &(s, e) in &stop {
                if s >= b || cur >= b {
                    break;
                }
                if e > cur {
                    if s > cur {
                        out.push((cur, s));
                    }
                    cur = e;
                }
            }
            if cur < b {
                out.push((cur, b));
            }
            out
        }))
    }

    /// The instant at which `seconds` of open time have passed since `from`, not counting time inside the
    /// `stopped` intervals (in any order, possibly overlapping). Outside open time the count starts at the next
    /// opening; ending exactly at a window's end gives that end. No seconds give `from`.
    pub fn add_working(&self, from: DateTime<Utc>, seconds: u64, stopped: &[Span]) -> Result<DateTime<Utc>, String> {
        let pieces = self.counted(from, stopped)?;
        let horizon = format!("{seconds} seconds of open time do not fit in the calendar's next {MAX_SEARCH_DAYS} days");
        if seconds == 0 {
            return Ok(from);
        }
        if seconds > u64::from(MAX_SEARCH_DAYS) * 86_400 {
            return Err(horizon);
        }
        let mut left = Duration::seconds(seconds as i64);
        for (a, b) in pieces {
            if b - a >= left {
                return Ok(a + left);
            }
            left -= b - a;
        }
        Err(horizon)
    }

    /// Open time in `[from, to)`, not counting the `stopped` intervals; none when `to` is not after `from`.
    pub fn working_between(&self, from: DateTime<Utc>, to: DateTime<Utc>, stopped: &[Span]) -> Result<Duration, String> {
        let pieces = self.counted(from, stopped)?;
        if to - from > Duration::days(MAX_SEARCH_DAYS.into()) {
            return Err(format!("working time is counted over at most {MAX_SEARCH_DAYS} days"));
        }
        Ok(pieces.take_while(|&(a, _)| a < to).fold(Duration::zero(), |sum, (a, b)| sum + (b.min(to) - a)))
    }

    /// `t` when the calendar is open then, otherwise the next opening.
    pub fn next_open(&self, t: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
        self.counted(t, &[])?.next().map(|(a, _)| a).ok_or_else(|| format!("the calendar does not open in the next {MAX_SEARCH_DAYS} days"))
    }

    /// Whether the calendar is open at `t` (a window's end is not in it); never for a calendar with a problem.
    pub fn is_open(&self, t: DateTime<Utc>) -> bool {
        self.next_open(t) == Ok(t)
    }
}

/// `HH:MM` as minutes since midnight; `24:00` is the end of the day.
fn minute(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    if h.len() != 2 || m.len() != 2 || !h.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (m < 60 && (h < 24 || (h, m) == (24, 0))).then_some(h * 60 + m)
}

/// A local time as an instant: the earlier of two (the later when `later`), or the first instant after a gap.
fn resolve(tz: &Tz, local: NaiveDateTime, later: bool) -> Result<(DateTime<Utc>, Option<String>), String> {
    match tz.from_local_datetime(&local) {
        LocalResult::Single(t) => Ok((t.with_timezone(&Utc), None)),
        LocalResult::Ambiguous(early, late) => Ok((if later { late } else { early }.with_timezone(&Utc), Some("repeated".into()))),
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
        Wait::Working { seconds, calendar: name } => match calendar {
            Some(c) if c.name == *name => Ok(c.deadline(c.add_working(from, *seconds, &[])?, None)),
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
            hours: None,
            exceptions: Default::default(),
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
        let ny = Calendar {
            name: "ny".into(),
            zone: "America/New_York".into(),
            weekends: vec![],
            holidays: BTreeSet::new(),
            revision: 1,
            hours: None,
            exceptions: Default::default(),
        };
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

    /// A local time in a zone, `2026-11-09 09:00`, as an instant (the earlier when it happens twice).
    fn local(tz: Tz, s: &str) -> DateTime<Utc> {
        tz.from_local_datetime(&NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()).earliest().unwrap().with_timezone(&Utc)
    }

    fn calendar(zone: &str, weekends: &[&str], holidays: &[&str], hours: Value) -> Calendar {
        Calendar {
            name: "office".into(),
            zone: zone.into(),
            weekends: weekends.iter().map(|w| w.to_string()).collect(),
            holidays: holidays.iter().map(|d| d.parse().unwrap()).collect(),
            revision: 3,
            hours: serde_json::from_value(hours).unwrap(),
            exceptions: Default::default(),
        }
    }

    /// Kolkata, Monday to Friday, two windows a day; Wednesday 11 November 2026 is a holiday.
    fn office() -> Calendar {
        let day = json!([["09:00", "13:00"], ["14:00", "18:00"]]);
        calendar("Asia/Kolkata", &["sat", "sun"], &["2026-11-11"], json!({"mon": day, "tue": day, "wed": day, "thu": day, "fri": day}))
    }

    fn ist(s: &str) -> DateTime<Utc> {
        local(Tz::Asia__Kolkata, s)
    }

    fn berlin(s: &str) -> DateTime<Utc> {
        local(Tz::Europe__Berlin, s)
    }

    const H: u64 = 3600;

    #[test]
    fn working_waits_are_read_strictly() {
        assert_eq!(Wait::parse(&json!({"working": "PT8H", "calendar": "office"})), Some(Wait::Working { seconds: 28_800, calendar: "office".into() }));
        assert_eq!(Wait::parse(&json!({"calendar": "office", "working": 60})), Some(Wait::Working { seconds: 60, calendar: "office".into() }));
        for bad in [
            json!({"working": "PT8H"}),
            json!({"working": 0, "calendar": "office"}),
            json!({"working": "P400D", "calendar": "office"}),
            json!({"working": "8 hours", "calendar": "office"}),
            json!({"working": "PT8H", "calendar": ""}),
            json!({"working": "PT8H", "business_days": 1}),
            json!({"working": "PT8H", "calendar": "office", "business_days": 1}),
        ] {
            assert_eq!(Wait::parse(&bad), None, "{bad}");
        }
        let d = deadline(&Wait::Working { seconds: 2 * H, calendar: "office".into() }, ist("2026-11-09 17:00"), Some(&office())).unwrap();
        assert_eq!((d.at, d.calendar.as_deref(), d.revision, d.zone.as_deref()), (ist("2026-11-10 10:00"), Some("office"), Some(3), Some("Asia/Kolkata")));
        assert!(
            deadline(&Wait::Working { seconds: H, calendar: "office".into() }, ist("2026-11-09 17:00"), None).unwrap_err().contains("not in this workspace")
        );
    }

    #[test]
    fn hours_are_checked() {
        let bad = |hours: Value| calendar("Asia/Kolkata", &["sat", "sun"], &[], hours).problem().unwrap();
        assert!(bad(json!({"sat": [["09:00", "12:00"]]})).contains("weekend"));
        assert!(bad(json!({"monday": [["09:00", "12:00"]]})).contains("not a day"));
        assert!(bad(json!({"mon": [["12:00", "09:00"]]})).contains("not a window"));
        assert!(bad(json!({"mon": [["09:00", "24:30"]]})).contains("not a window"));
        assert!(bad(json!({"mon": [["9:00", "12:00"]]})).contains("not a window"));
        assert!(bad(json!({"mon": [["24:00", "24:00"]]})).contains("not a window"));
        assert!(bad(json!({"mon": [["09:00", "12:00"], ["11:00", "13:00"]]})).contains("overlap"));
        assert!(bad(json!({"mon": []})).contains("at least one window"));
        assert_eq!(calendar("Asia/Kolkata", &["sat", "sun"], &[], json!({"mon": [["09:00", "12:00"], ["12:00", "24:00"]], "sat": []})).problem(), None);
        assert!(!calendar("Asia/Kolkata", &["sat", "sun"], &[], json!({"mon": []})).is_open(ist("2026-11-09 10:00")));
        // Read and written as the API takes it.
        let c = office();
        assert_eq!(serde_json::from_value::<Calendar>(serde_json::to_value(&c).unwrap()).unwrap(), c);
    }

    #[test]
    fn several_windows_a_day_a_holiday_and_a_weekend() {
        let c = office();
        let add = |from: &str, secs: u64| c.add_working(ist(from), secs, &[]).unwrap();
        // Before, inside, at the end of and after a window (Monday 9 November 2026).
        assert_eq!(add("2026-11-09 08:00", H), ist("2026-11-09 10:00"));
        assert_eq!(add("2026-11-09 12:30", H), ist("2026-11-09 14:30"));
        assert_eq!(add("2026-11-09 09:00", 4 * H), ist("2026-11-09 13:00"), "ending at a window's end gives that end");
        assert_eq!(add("2026-11-09 13:00", 0), ist("2026-11-09 13:00"));
        assert_eq!(add("2026-11-09 13:00", 1), ist("2026-11-09 14:00") + Duration::seconds(1));
        assert_eq!(add("2026-11-09 13:30", H), ist("2026-11-09 15:00"));
        assert_eq!(add("2026-11-09 19:00", H), ist("2026-11-10 10:00"));
        // Wednesday is a holiday; Saturday and Sunday are the weekend.
        assert_eq!(add("2026-11-10 17:00", 2 * H), ist("2026-11-12 10:00"));
        assert_eq!(add("2026-11-13 17:00", 2 * H), ist("2026-11-16 10:00"));
        let between = |a: &str, b: &str| c.working_between(ist(a), ist(b), &[]).unwrap();
        assert_eq!(between("2026-11-09 08:00", "2026-11-10 10:00"), Duration::hours(9));
        assert_eq!(between("2026-11-10 12:00", "2026-11-12 12:00"), Duration::hours(1 + 4 + 3));
        assert_eq!(between("2026-11-10 12:00", "2026-11-10 12:00"), Duration::zero());
        assert_eq!(between("2026-11-10 12:00", "2026-11-09 12:00"), Duration::zero());
        assert_eq!(c.next_open(ist("2026-11-09 13:00")).unwrap(), ist("2026-11-09 14:00"));
        assert_eq!(c.next_open(ist("2026-11-09 10:00")).unwrap(), ist("2026-11-09 10:00"));
        assert_eq!(c.next_open(ist("2026-11-14 10:00")).unwrap(), ist("2026-11-16 09:00"));
        assert!(c.is_open(ist("2026-11-09 12:59")) && !c.is_open(ist("2026-11-09 13:00")) && c.is_open(ist("2026-11-09 09:00")));
        assert!(!c.is_open(ist("2026-11-11 10:00")) && !c.is_open(ist("2026-11-14 10:00")));
        assert!(!Calendar { zone: "Mars/Olympus".into(), ..office() }.is_open(ist("2026-11-09 10:00")));
        assert!(Calendar { zone: "Mars/Olympus".into(), ..office() }.add_working(ist("2026-11-09 10:00"), H, &[]).is_err());
    }

    #[test]
    fn a_shift_over_midnight_is_one_stretch() {
        let c = calendar("Asia/Kolkata", &[], &[], json!({"mon": [["22:00", "24:00"]], "tue": [["00:00", "06:00"]]}));
        assert_eq!(c.add_working(ist("2026-11-09 23:00"), 2 * H, &[]).unwrap(), ist("2026-11-10 01:00"));
        assert_eq!(c.add_working(ist("2026-11-09 12:00"), 8 * H, &[]).unwrap(), ist("2026-11-10 06:00"));
        assert_eq!(c.add_working(ist("2026-11-09 12:00"), 8 * H + 1, &[]).unwrap(), ist("2026-11-16 22:00") + Duration::seconds(1));
        assert_eq!(c.working_between(ist("2026-11-09 21:00"), ist("2026-11-10 07:00"), &[]).unwrap(), Duration::hours(8));
        assert!(c.is_open(ist("2026-11-10 00:00")) && c.is_open(ist("2026-11-09 23:59")) && !c.is_open(ist("2026-11-10 06:00")));
    }

    #[test]
    fn stopped_time_does_not_count() {
        let c = office();
        let from = ist("2026-11-09 09:00");
        // Starting outside open time.
        let early = [(ist("2026-11-09 08:00"), ist("2026-11-09 10:00"))];
        assert_eq!(c.add_working(from, 4 * H, &early).unwrap(), ist("2026-11-09 15:00"));
        // Overlapping each other, out of order.
        let overlap = [(ist("2026-11-09 10:30"), ist("2026-11-09 11:30")), (ist("2026-11-09 10:00"), ist("2026-11-09 11:00"))];
        assert_eq!(c.add_working(from, 4 * H, &overlap).unwrap(), ist("2026-11-09 15:30"));
        assert_eq!(c.working_between(from, ist("2026-11-09 18:00"), &overlap).unwrap(), Duration::minutes(8 * 60 - 90));
        // A whole day, and one that ends before the count starts or is empty.
        let day = [(ist("2026-11-09 00:00"), ist("2026-11-10 00:00")), (ist("2026-11-01 00:00"), ist("2026-11-02 00:00")), (from, from)];
        assert_eq!(c.add_working(from, 4 * H, &day).unwrap(), ist("2026-11-10 13:00"));
        assert_eq!(c.working_between(from, ist("2026-11-10 10:00"), &day).unwrap(), Duration::hours(1));
        // Stopped for good: never.
        let ever = [(ist("2026-11-01 00:00"), ist("2040-01-01 00:00"))];
        assert!(c.add_working(from, 1, &ever).unwrap_err().contains("do not fit"));
        assert!(c.add_working(from, u64::from(MAX_SEARCH_DAYS) * 86_400 + 1, &[]).unwrap_err().contains("do not fit"));
        assert!(c.working_between(from, from + Duration::days(i64::from(MAX_SEARCH_DAYS) + 1), &[]).is_err());
    }

    #[test]
    fn without_hours_a_business_day_is_open_all_day() {
        let c = india(&[], 1);
        // Friday 6 November 2026, 15:30 in Kolkata: a business day of open time on is Monday, 15:30, as
        // `add_business_days` has it.
        let from = utc("2026-11-06T10:00:00Z");
        assert_eq!(c.add_working(from, 24 * H, &[]).unwrap(), c.add_business_days(from, 1).unwrap().at);
        assert_eq!(c.working_between(ist("2026-11-06 00:00"), ist("2026-11-13 00:00"), &[]).unwrap(), Duration::days(5));
        assert!(c.is_open(ist("2026-11-06 23:59")) && !c.is_open(ist("2026-11-07 00:00")));
        assert_eq!(c.next_open(ist("2026-11-07 12:00")).unwrap(), ist("2026-11-09 00:00"));
    }

    #[test]
    fn daylight_saving_in_berlin() {
        // Sunday 29 March 2026 clocks go from 02:00 to 03:00; Sunday 25 October 2026 from 03:00 back to 02:00.
        let all = |hours: Value| calendar("Europe/Berlin", &[], &[], hours);
        let day = |c: &Calendar, d: &str| c.working_between(berlin(&format!("{d} 00:00")), berlin(&format!("{d} 23:59")) + Duration::minutes(1), &[]).unwrap();
        // Whole days: 23 hours, then 25.
        let open = calendar("Europe/Berlin", &[], &[], Value::Null);
        assert_eq!(
            (day(&open, "2026-03-29"), day(&open, "2026-10-25"), day(&open, "2026-10-26")),
            (Duration::hours(23), Duration::hours(25), Duration::hours(24))
        );
        // A window across the gap is shorter by it; across the repeated hour, longer by it.
        let night = all(json!({"sun": [["01:00", "04:00"]]}));
        assert_eq!(
            (day(&night, "2026-03-29"), day(&night, "2026-10-25"), day(&night, "2026-10-18")),
            (Duration::hours(2), Duration::hours(4), Duration::hours(3))
        );
        // An end in the gap is the first instant after it.
        let ends = all(json!({"sun": [["01:00", "02:30"]]}));
        assert_eq!(ends.add_working(berlin("2026-03-29 00:00"), 2 * H, &[]).unwrap(), utc("2026-04-05T00:00:00Z"), "only an hour that Sunday");
        assert_eq!(ends.next_open(berlin("2026-03-29 01:30")).unwrap(), berlin("2026-03-29 01:30"));
        assert!(!ends.is_open(berlin("2026-03-29 03:00")));
        // A window entirely inside the gap is not there; it is there on other Sundays.
        let gap = all(json!({"sun": [["02:15", "02:45"]], "mon": [["09:00", "10:00"]]}));
        assert_eq!(day(&gap, "2026-03-29"), Duration::zero());
        assert_eq!(day(&gap, "2026-03-22"), Duration::minutes(30));
        assert_eq!(gap.next_open(berlin("2026-03-29 00:00")).unwrap(), berlin("2026-03-30 09:00"));
        // Inside the repeated hour a window runs from the first time its start shows to the last time its end
        // shows; two windows the repeat makes overlap count their shared time once.
        let repeat = all(json!({"sun": [["02:15", "02:45"]]}));
        assert_eq!(day(&repeat, "2026-10-25"), Duration::minutes(90));
        assert_eq!(repeat.next_open(berlin("2026-10-25 00:00")).unwrap(), utc("2026-10-25T00:15:00Z"));
        let halves = all(json!({"sun": [["02:00", "02:30"], ["02:30", "03:00"]]}));
        assert_eq!(day(&halves, "2026-10-25"), Duration::hours(2));
        assert_eq!(halves.add_working(utc("2026-10-25T00:00:00Z"), 2 * H, &[]).unwrap(), utc("2026-10-25T02:00:00Z"));
    }

    #[test]
    fn dated_exceptions_open_and_close_days_and_hours() {
        let with = |x: Value| Calendar { exceptions: serde_json::from_value(x).unwrap(), ..office() };
        let c = with(json!({
            // The holiday (Wednesday) opens in the morning; Saturday opens all day; Friday closes from 12:00;
            // Thursday closes all day; Tuesday has other hours.
            "2026-11-11": {"open": true, "hours": [["10:00", "12:00"]]},
            "2026-11-14": {"open": true},
            "2026-11-13": {"open": false, "hours": [["12:00", "24:00"]]},
            "2026-11-12": {"open": false},
            "2026-11-10": {"open": true, "hours": [["07:00", "08:00"]]}
        }));
        let between = |a: &str, b: &str| c.working_between(ist(a), ist(b), &[]).unwrap();
        assert_eq!(between("2026-11-11 00:00", "2026-11-12 00:00"), Duration::hours(2), "the holiday, open in the morning");
        assert_eq!(between("2026-11-14 00:00", "2026-11-15 00:00"), Duration::hours(24), "a weekend day open all day");
        assert_eq!(between("2026-11-13 00:00", "2026-11-14 00:00"), Duration::hours(3), "closed from 12:00: 09:00-12:00 is left");
        assert_eq!(between("2026-11-12 00:00", "2026-11-13 00:00"), Duration::zero(), "closed all day");
        assert_eq!(between("2026-11-10 00:00", "2026-11-11 00:00"), Duration::hours(1), "its own hours replace the day's");
        assert_eq!(between("2026-11-09 00:00", "2026-11-10 00:00"), Duration::hours(8), "other days as before");
        assert!(c.is_open(ist("2026-11-11 10:30")) && !c.is_open(ist("2026-11-11 12:30")));
        assert!(c.is_open(ist("2026-11-13 11:59")) && !c.is_open(ist("2026-11-13 14:30")));
        assert!(!c.is_open(ist("2026-11-10 09:30")) && c.is_open(ist("2026-11-10 07:30")));
        assert_eq!(c.next_open(ist("2026-11-11 12:00")).unwrap(), ist("2026-11-13 09:00"), "past the day closed all day");
        assert_eq!(c.add_working(ist("2026-11-13 11:00"), 2 * H, &[]).unwrap(), ist("2026-11-14 01:00"), "on into the opened Saturday");
        // Business days count what the exceptions make of a date; one closed only some hours still counts.
        let days = |from: &str, n: u32| c.add_business_days(ist(from), n).unwrap().at;
        assert_eq!(days("2026-11-10 10:00", 1), ist("2026-11-11 10:00"), "the opened holiday is one");
        assert_eq!(days("2026-11-11 10:00", 1), ist("2026-11-13 10:00"), "the day closed all day is not");
        assert_eq!(days("2026-11-13 10:00", 1), ist("2026-11-14 10:00"), "nor is it lost to one closed from noon");
        // Checked like hours, and read and written as the API takes it.
        assert!(with(json!({"2026-11-13": {"open": false, "hours": [["13:00", "12:00"]]}})).problem().unwrap().starts_with("exceptions:"));
        assert!(with(json!({"2026-11-13": {"open": true, "hours": [["09:00", "12:00"], ["11:00", "13:00"]]}})).problem().unwrap().contains("overlap"));
        assert!(serde_json::from_value::<Calendar>(json!({"name": "x", "zone": "UTC", "weekends": [], "holidays": [], "revision": 1,
            "exceptions": {"2026-11-13": {"closed": true}}})).is_err());
        assert_eq!(serde_json::from_value::<Calendar>(serde_json::to_value(&c).unwrap()).unwrap(), c);
        assert!(!serde_json::to_value(office()).unwrap().as_object().unwrap().contains_key("exceptions"), "none: as before");
    }

    #[test]
    fn exceptions_on_a_daylight_saving_day() {
        // Sunday 29 March 2026 in Berlin has 23 hours; 25 October 25.
        let c = Calendar {
            exceptions: serde_json::from_value(json!({
                "2026-03-29": {"open": true},
                "2026-10-25": {"open": true, "hours": [["01:00", "04:00"]]},
                "2026-10-26": {"open": false, "hours": [["02:00", "03:00"]]}
            }))
            .unwrap(),
            ..calendar("Europe/Berlin", &["sun"], &[], json!({"mon": [["01:00", "04:00"]]}))
        };
        let day = |d: &str| c.working_between(berlin(&format!("{d} 00:00")), berlin(&format!("{d} 23:59")) + Duration::minutes(1), &[]).unwrap();
        assert_eq!(day("2026-03-29"), Duration::hours(23), "a weekend day opened whole is as long as the day");
        assert_eq!(day("2026-10-25"), Duration::hours(4), "its window across the repeated hour is longer by it");
        assert_eq!(day("2026-10-26"), Duration::hours(2), "closed an hour of the Monday's window");
        assert_eq!(day("2026-03-22"), Duration::zero(), "other Sundays stay closed");
    }

    #[test]
    fn a_zone_without_daylight_saving_keeps_its_hours_all_year() {
        let c = calendar("Asia/Kolkata", &[], &[], json!({"sun": [["02:00", "03:00"]]}));
        for d in ["2026-03-29", "2026-10-25", "2026-11-01"] {
            assert_eq!(c.working_between(ist(&format!("{d} 00:00")), ist(&format!("{d} 23:00")), &[]).unwrap(), Duration::hours(1), "{d}");
        }
    }

    /// A small deterministic generator, so that the properties below see the same cases every run.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, below: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % below
        }
    }

    #[test]
    fn working_time_adds_up() {
        let berlin_shifts = calendar(
            "Europe/Berlin",
            &["sat"],
            &["2026-03-30"],
            json!({
                "sun": [["01:00", "04:00"]],
                "mon": [["00:00", "02:30"], ["02:30", "03:00"], ["22:00", "24:00"]],
                "tue": [["00:00", "06:00"]],
                "wed": [["09:00", "17:00"]],
                "thu": [["02:15", "02:45"], ["09:00", "12:00"]],
                "fri": [["08:00", "20:00"]],
            }),
        );
        let ny = calendar("America/New_York", &["sat"], &[], json!({"sun": [["01:30", "02:30"]], "mon": [["09:00", "17:00"]]}));
        let mut g = Lcg(7);
        for c in [office(), berlin_shifts, ny, india(&["2026-11-09"], 1)] {
            for _ in 0..150 {
                // Around the daylight-saving changes half the time.
                let base = [utc("2026-03-27T00:00:00Z"), utc("2026-10-23T00:00:00Z"), utc("2026-01-01T00:00:00Z")][g.next(3) as usize];
                let from = base + Duration::minutes(g.next(if base.month() == 1 { 525_600 } else { 7200 }) as i64);
                let stopped: Vec<Span> = (0..g.next(4))
                    .map(|_| {
                        let a = from + Duration::minutes(g.next(10_000) as i64 - 2000);
                        (a, a + Duration::minutes(g.next(3000) as i64))
                    })
                    .collect();
                let (n, m) = (g.next(150 * H), g.next(40 * H));
                let add = |at: DateTime<Utc>, secs: u64| c.add_working(at, secs, &stopped).unwrap();
                let end = add(from, n);
                assert_eq!(c.working_between(from, end, &stopped).unwrap(), Duration::seconds(n as i64), "{} {from} {n}", c.zone);
                assert!(add(from, n + m) >= end, "monotonic");
                assert_eq!(add(end, m), add(from, n + m), "{} {from} {n} {m}", c.zone);
            }
            let ever = [(utc("2025-01-01T00:00:00Z"), utc("2040-01-01T00:00:00Z"))];
            assert!(c.add_working(utc("2026-05-05T00:00:00Z"), 1, &ever).is_err());
        }
    }
}
