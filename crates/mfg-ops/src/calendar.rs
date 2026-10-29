//! Working time of a workstation (weekly hours minus holidays) and forward scheduling against a
//! capacity of parallel jobs. Pure: every time here is local to the workspace.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;
use std::collections::BTreeSet;

/// How far ahead scheduling looks for working time before giving up.
const HORIZON_DAYS: i64 = 3 * 366;

/// Weekly working windows (minutes from midnight, `[start, end)`, Monday first) minus holidays.
/// No windows at all means the workstation is available around the clock.
#[derive(Debug, Clone, Default)]
pub struct Calendar {
    week: [Vec<(u32, u32)>; 7],
    holidays: BTreeSet<NaiveDate>,
}

/// `"08:30"` → 510 minutes; `"24:00"` (end of day) → 1440.
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (m < 60 && (h < 24 || (h == 24 && m == 0))).then_some(h * 60 + m)
}

pub fn weekday_index(name: &str) -> Option<usize> {
    ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"].iter().position(|d| *d == name)
}

impl Calendar {
    /// `hours`: `(weekday index, start minute, end minute)`.
    pub fn new(hours: &[(usize, u32, u32)], holidays: impl IntoIterator<Item = NaiveDate>) -> Calendar {
        let mut week: [Vec<(u32, u32)>; 7] = Default::default();
        for &(day, start, end) in hours {
            if day < 7 && start < end {
                week[day].push((start, end));
            }
        }
        for w in week.iter_mut() {
            w.sort_unstable();
        }
        Calendar { week, holidays: holidays.into_iter().collect() }
    }

    /// Working windows on `day`, in order.
    pub fn windows(&self, day: NaiveDate) -> Vec<(NaiveDateTime, NaiveDateTime)> {
        if self.holidays.contains(&day) {
            return vec![];
        }
        let midnight = day.and_hms_opt(0, 0, 0).unwrap_or_default();
        if self.week.iter().all(Vec::is_empty) {
            return vec![(midnight, midnight + Duration::days(1))];
        }
        self.week[day.weekday().num_days_from_monday() as usize]
            .iter()
            .map(|&(s, e)| (midnight + Duration::minutes(s as i64), midnight + Duration::minutes(e as i64)))
            .collect()
    }

    fn days_from(t: NaiveDateTime) -> impl Iterator<Item = NaiveDate> {
        let first = t.date();
        (0..HORIZON_DAYS).map(move |i| first + Duration::days(i))
    }

    /// The first working instant at or after `t`.
    pub fn next_working(&self, t: NaiveDateTime) -> Option<NaiveDateTime> {
        Self::days_from(t).flat_map(|d| self.windows(d)).find(|(_, end)| *end > t).map(|(start, _)| start.max(t))
    }

    /// When `secs` seconds of working time, started at `t`, are done.
    pub fn add_working(&self, t: NaiveDateTime, secs: i64) -> Option<NaiveDateTime> {
        if secs <= 0 {
            return self.next_working(t);
        }
        let mut left = secs;
        for (start, end) in Self::days_from(t).flat_map(|d| self.windows(d)) {
            if end <= t {
                continue;
            }
            let from = start.max(t);
            let avail = (end - from).num_seconds();
            if avail >= left {
                return Some(from + Duration::seconds(left));
            }
            left -= avail;
        }
        None
    }

    /// Working seconds inside `[from, to)`.
    pub fn working_secs(&self, from: NaiveDateTime, to: NaiveDateTime) -> i64 {
        if to <= from {
            return 0;
        }
        let days = (to.date() - from.date()).num_days();
        (0..=days).flat_map(|i| self.windows(from.date() + Duration::days(i))).map(|(s, e)| (e.min(to) - s.max(from)).num_seconds().max(0)).sum()
    }
}

/// The most intervals of `busy` that overlap at any one instant of `[s, e)` (half-open, so an
/// interval ending when another starts does not overlap it). A zero-length span counts the
/// intervals covering its instant.
pub fn peak_overlap<T: Ord + Copy>(busy: &[(T, T)], s: T, e: T) -> usize {
    if s >= e {
        return busy.iter().filter(|(a, b)| *a <= s && s < *b).count();
    }
    // Ends sort before starts at the same instant.
    let mut events: Vec<(T, i8)> = Vec::new();
    for &(a, b) in busy.iter().filter(|(a, b)| *a < e && *b > s && a < b) {
        events.push((a.max(s), 1));
        events.push((b.min(e), -1));
    }
    events.sort();
    let (mut now, mut peak) = (0i64, 0i64);
    for (_, delta) in events {
        now += delta as i64;
        peak = peak.max(now);
    }
    peak as usize
}

/// The earliest `[start, end)` at or after `earliest` holding `secs` of working time while fewer
/// than `capacity` of the `busy` intervals run at any moment of it. A job holds its slot from
/// start to end, idle hours included (it is not preempted overnight).
pub fn find_slot(
    cal: &Calendar,
    busy: &[(NaiveDateTime, NaiveDateTime)],
    capacity: usize,
    earliest: NaiveDateTime,
    secs: i64,
) -> Option<(NaiveDateTime, NaiveDateTime)> {
    // Load only drops when a busy interval ends, so those ends are the only other candidates.
    let mut candidates: Vec<NaiveDateTime> = std::iter::once(earliest).chain(busy.iter().map(|b| b.1).filter(|t| *t > earliest)).collect();
    candidates.sort_unstable();
    candidates.dedup();
    for c in candidates {
        let start = cal.next_working(c)?;
        let end = cal.add_working(start, secs)?;
        if peak_overlap(busy, start, end) < capacity.max(1) {
            return Some((start, end));
        }
    }
    None
}

/// Minutes an operation takes for `qty`: per unit or per started batch, plus the setup.
pub fn op_minutes(qty: Decimal, per_batch: bool, time: Decimal, batch: Decimal, setup: Decimal) -> Decimal {
    let runs = if per_batch {
        let batch = if batch > Decimal::ZERO { batch } else { Decimal::ONE };
        (qty / batch).ceil()
    } else {
        qty
    };
    runs * time + setup
}

/// Whole seconds in `minutes`, rounded up (a job never gets less time than planned).
pub fn secs_of(minutes: Decimal) -> i64 {
    use rust_decimal::prelude::ToPrimitive;
    (minutes * Decimal::from(60)).ceil().to_i64().unwrap_or(i64::MAX / 4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    /// Mon-Fri 08:00-12:00 and 13:00-17:00; 2026-11-04 (a Wednesday) is a holiday.
    fn shifts() -> Calendar {
        let mut h = vec![];
        for d in 0..5 {
            h.push((d, 480, 720));
            h.push((d, 780, 1020));
        }
        Calendar::new(&h, [NaiveDate::from_ymd_opt(2026, 11, 4).unwrap()])
    }

    #[test]
    fn times_parse() {
        assert_eq!(parse_hhmm("08:30"), Some(510));
        assert_eq!(parse_hhmm("24:00"), Some(1440));
        assert_eq!(parse_hhmm("24:01"), None);
        assert_eq!(parse_hhmm("7"), None);
        assert_eq!(weekday_index("Sunday"), Some(6));
    }

    #[test]
    fn working_time_skips_breaks_nights_weekends_and_holidays() {
        let c = shifts();
        // 2026-11-02 is a Monday.
        assert_eq!(c.next_working(at("2026-11-02 06:00")), Some(at("2026-11-02 08:00")));
        assert_eq!(c.next_working(at("2026-11-02 12:10")), Some(at("2026-11-02 13:00")));
        assert_eq!(c.next_working(at("2026-11-07 10:00")), Some(at("2026-11-09 08:00")), "Saturday rolls to Monday");
        // 10 hours from Monday 09:00: 3 + 4 on Monday, 3 on Tuesday morning.
        assert_eq!(c.add_working(at("2026-11-02 09:00"), 10 * 3600), Some(at("2026-11-03 11:00")));
        // 12 hours from Tuesday 13:00: 4 Tuesday, Wednesday is a holiday, 8 Thursday.
        assert_eq!(c.add_working(at("2026-11-03 13:00"), 12 * 3600), Some(at("2026-11-05 17:00")));
        assert_eq!(c.working_secs(at("2026-11-02 00:00"), at("2026-11-03 00:00")), 8 * 3600);
        assert_eq!(c.working_secs(at("2026-11-04 00:00"), at("2026-11-05 00:00")), 0);
        let always = Calendar::new(&[], []);
        assert_eq!(always.add_working(at("2026-11-07 22:00"), 3 * 3600), Some(at("2026-11-08 01:00")));
    }

    #[test]
    fn overlap_is_half_open() {
        let busy = [(at("2026-11-02 08:00"), at("2026-11-02 10:00")), (at("2026-11-02 09:00"), at("2026-11-02 11:00"))];
        assert_eq!(peak_overlap(&busy, at("2026-11-02 08:00"), at("2026-11-02 12:00")), 2);
        assert_eq!(peak_overlap(&busy, at("2026-11-02 10:00"), at("2026-11-02 12:00")), 1);
        assert_eq!(peak_overlap(&busy, at("2026-11-02 11:00"), at("2026-11-02 12:00")), 0);
        assert_eq!(peak_overlap(&busy, at("2026-11-02 07:00"), at("2026-11-02 08:00")), 0, "touching is not overlapping");
    }

    #[test]
    fn slots_respect_capacity() {
        let c = shifts();
        let busy = [(at("2026-11-02 08:00"), at("2026-11-02 11:00"))];
        let one = find_slot(&c, &busy, 1, at("2026-11-02 00:00"), 2 * 3600).unwrap();
        assert_eq!(one, (at("2026-11-02 11:00"), at("2026-11-02 14:00")), "after the busy job, across lunch");
        let two = find_slot(&c, &busy, 2, at("2026-11-02 00:00"), 2 * 3600).unwrap();
        assert_eq!(two, (at("2026-11-02 08:00"), at("2026-11-02 10:00")), "a second lane runs in parallel");
    }

    #[test]
    fn operation_time() {
        let n = |s: &str| Decimal::from_str(s).unwrap();
        assert_eq!(op_minutes(n("10"), false, n("6"), n("1"), n("30")), n("90"));
        assert_eq!(op_minutes(n("10"), true, n("20"), n("4"), n("0")), n("60"), "three started batches");
        assert_eq!(secs_of(n("0.01")), 1);
    }
}
