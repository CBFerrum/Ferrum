//! Business-time arithmetic for service level agreements.
//!
//! A calendar is a weekly pattern of working windows read in a fixed UTC offset, minus holiday
//! dates. A deadline adds business time to the opening instant while skipping the intervals in
//! which the ticket's clock stood still (paused statuses, earlier resolutions). Everything here is
//! pure and counts milliseconds, so weekends, holidays, night shifts, time zones and pauses are
//! unit-tested without a database.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Utc};
use std::collections::BTreeSet;

pub type Ts = DateTime<Utc>;

/// How far ahead working time is searched for before a deadline counts as unreachable (~10 years).
const HORIZON_DAYS: i64 = 3660;

/// Share of a target (in %) that, once it is all that is left, puts a ticket "At Risk".
const AT_RISK_PERCENT: i64 = 20;

#[derive(Debug, Clone)]
pub struct Calendar {
    offset: Duration,
    /// Working minutes `[start, end)` per weekday (Monday = 0), sorted and merged.
    days: [Vec<(u32, u32)>; 7],
    holidays: BTreeSet<NaiveDate>,
}

impl Calendar {
    /// `windows`: `(weekday, start minute, end minute)` with Monday = 0 and `end ≤ 1440`; a shift
    /// past midnight is two windows. No windows at all means round the clock.
    pub fn new(offset_minutes: i64, windows: &[(usize, u32, u32)], holidays: BTreeSet<NaiveDate>) -> Calendar {
        let mut days: [Vec<(u32, u32)>; 7] = Default::default();
        if windows.is_empty() {
            days.iter_mut().for_each(|d| d.push((0, 1440)));
        }
        for &(day, start, end) in windows {
            if day < 7 && start < end && end <= 1440 {
                days[day].push((start, end));
            }
        }
        for d in days.iter_mut() {
            d.sort_unstable();
            let mut merged: Vec<(u32, u32)> = Vec::with_capacity(d.len());
            for &(s, e) in d.iter() {
                match merged.last_mut() {
                    Some(last) if s <= last.1 => last.1 = last.1.max(e),
                    _ => merged.push((s, e)),
                }
            }
            *d = merged;
        }
        Calendar { offset: Duration::minutes(offset_minutes), days, holidays }
    }

    /// Round the clock, every day.
    pub fn always() -> Calendar {
        Calendar::new(0, &[], BTreeSet::new())
    }

    /// The first working window that ends after `t`, starting no earlier than `t`.
    fn window_after(&self, t: Ts) -> Option<(Ts, Ts)> {
        let mut day = (t + self.offset).date_naive();
        for _ in 0..HORIZON_DAYS {
            if !self.holidays.contains(&day) {
                let midnight = day.and_time(NaiveTime::MIN).and_utc() - self.offset;
                for &(s, e) in &self.days[day.weekday().num_days_from_monday() as usize] {
                    let end = midnight + Duration::minutes(e.into());
                    if end > t {
                        return Some(((midnight + Duration::minutes(s.into())).max(t), end));
                    }
                }
            }
            day = day.succ_opt()?;
        }
        None
    }

    /// Like `window_after`, without the `stopped` intervals (sorted, non-overlapping).
    fn open_after(&self, mut t: Ts, stopped: &[(Ts, Ts)]) -> Option<(Ts, Ts)> {
        loop {
            let (a, b) = self.window_after(t)?;
            if let Some(&(_, e)) = stopped.iter().find(|(s, e)| *s <= a && a < *e) {
                t = e;
                continue;
            }
            let cut = stopped.iter().map(|(s, _)| *s).filter(|s| a < *s && *s < b).min();
            return Some((a, cut.unwrap_or(b)));
        }
    }

    /// The instant at which `ms` of business time has passed since `start`, not counting `stopped`.
    /// `None` when the calendar has no working time within the horizon.
    pub fn add(&self, start: Ts, ms: i64, stopped: &[(Ts, Ts)]) -> Option<Ts> {
        let limit = start + Duration::days(HORIZON_DAYS);
        let (mut t, mut left) = (start, ms);
        while left > 0 {
            let (a, b) = self.open_after(t, stopped)?;
            if a > limit {
                return None;
            }
            let len = (b - a).num_milliseconds();
            if len >= left {
                return Some(a + Duration::milliseconds(left));
            }
            left -= len;
            t = b;
        }
        Some(start)
    }

    /// Business time (ms) in `[from, to)`, not counting `stopped`.
    pub fn between(&self, from: Ts, to: Ts, stopped: &[(Ts, Ts)]) -> i64 {
        let (mut t, mut sum) = (from, 0);
        while t < to {
            let Some((a, b)) = self.open_after(t, stopped) else { break };
            if a >= to {
                break;
            }
            sum += (b.min(to) - a).num_milliseconds();
            t = b;
        }
        sum
    }
}

/// The intervals in which the clock stood still, from a status timeline sorted by time: each
/// status lasts until the next entry, the last one until `now`.
pub fn stopped_intervals(timeline: &[(String, Ts)], stops: impl Fn(&str) -> bool, now: Ts) -> Vec<(Ts, Ts)> {
    let mut out: Vec<(Ts, Ts)> = Vec::new();
    for (i, (status, at)) in timeline.iter().enumerate() {
        let end = timeline.get(i + 1).map_or(now, |(_, t)| *t);
        if !stops(status) || end <= *at {
            continue;
        }
        match out.last_mut() {
            Some(last) if *at <= last.1 => last.1 = last.1.max(end),
            _ => out.push((*at, end)),
        }
    }
    out
}

/// The intervals up to `until`: stops after a target was met (or after now) cannot move it.
pub fn clip(stopped: &[(Ts, Ts)], until: Ts) -> Vec<(Ts, Ts)> {
    stopped.iter().filter(|(s, _)| *s < until).map(|&(s, e)| (s, e.min(until))).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    OnTrack,
    AtRisk,
    Breached,
    Fulfilled,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::OnTrack => "On Track",
            State::AtRisk => "At Risk",
            State::Breached => "Breached",
            State::Fulfilled => "Fulfilled",
        }
    }
}

/// One ticket's clock: when it opened and when it stood still, as of `now`.
pub struct Clock<'a> {
    pub opened: Ts,
    pub stopped: &'a [(Ts, Ts)],
    pub now: Ts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub deadline: Option<Ts>,
    pub state: State,
}

/// Deadline of one target (`ms` of business time) and where the ticket stands against it; `done`
/// is when the target was met (first response, resolution), if it was. While a pause is still
/// running the deadline is the one that applies if the ticket resumes now.
pub fn verdict(cal: &Calendar, clock: &Clock, ms: i64, done: Option<Ts>) -> Verdict {
    let stopped = clip(clock.stopped, done.unwrap_or(clock.now));
    let Some(deadline) = cal.add(clock.opened, ms, &stopped) else { return Verdict { deadline: None, state: State::OnTrack } };
    let state = match done {
        Some(t) if t <= deadline => State::Fulfilled,
        Some(_) => State::Breached,
        None if clock.now > deadline => State::Breached,
        None if cal.between(clock.now, deadline, &[]) * 100 <= ms * AT_RISK_PERCENT => State::AtRisk,
        None => State::OnTrack,
    };
    Verdict { deadline: Some(deadline), state }
}

/// The ticket's SLA status: a breach of either target wins, then resolution, then the more urgent
/// of the pending targets.
pub fn overall(response: State, resolution: State) -> State {
    match (response, resolution) {
        (State::Breached, _) | (_, State::Breached) => State::Breached,
        (_, State::Fulfilled) => State::Fulfilled,
        (State::AtRisk, _) | (_, State::AtRisk) => State::AtRisk,
        _ => State::OnTrack,
    }
}

/// Business time from opening until `done`, without the time the clock stood still before it.
pub fn elapsed(cal: &Calendar, clock: &Clock, done: Ts) -> i64 {
    cal.between(clock.opened, done, &clip(clock.stopped, done))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Weekday;

    fn t(s: &str) -> Ts {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn h(n: i64) -> i64 {
        n * 3_600_000
    }

    fn days(list: &[&str]) -> BTreeSet<NaiveDate> {
        list.iter().map(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").unwrap()).collect()
    }

    /// Monday to Friday, 09:00-18:00 local time.
    fn office(offset: i64, holidays: &[&str]) -> Calendar {
        let w: Vec<(usize, u32, u32)> = (0..5).map(|d| (d, 9 * 60, 18 * 60)).collect();
        Calendar::new(offset, &w, days(holidays))
    }

    #[test]
    fn test_dates_are_the_weekdays_the_tests_assume() {
        assert_eq!(NaiveDate::from_ymd_opt(2026, 10, 26).unwrap().weekday(), Weekday::Mon);
        assert_eq!(NaiveDate::from_ymd_opt(2026, 10, 30).unwrap().weekday(), Weekday::Fri);
        assert_eq!(NaiveDate::from_ymd_opt(2026, 11, 2).unwrap().weekday(), Weekday::Mon);
    }

    #[test]
    fn round_the_clock() {
        let c = Calendar::always();
        assert_eq!(c.add(t("2026-10-31T22:00:00Z"), h(5), &[]), Some(t("2026-11-01T03:00:00Z")), "weekends and midnight count");
        assert_eq!(c.between(t("2026-10-30T12:00:00Z"), t("2026-11-02T12:00:00Z"), &[]), h(72));
        assert_eq!(c.add(t("2026-10-31T22:00:00Z"), 0, &[]), Some(t("2026-10-31T22:00:00Z")));
    }

    #[test]
    fn office_hours_within_a_day_and_overnight() {
        let c = office(0, &[]);
        assert_eq!(c.add(t("2026-10-26T10:00:00Z"), h(3), &[]), Some(t("2026-10-26T13:00:00Z")));
        assert_eq!(c.add(t("2026-10-26T09:00:00Z"), h(9), &[]), Some(t("2026-10-26T18:00:00Z")), "a deadline may fall on closing time");
        assert_eq!(c.add(t("2026-10-26T17:00:00Z"), h(2), &[]), Some(t("2026-10-27T10:00:00Z")), "the night does not count");
        assert_eq!(c.add(t("2026-10-26T07:00:00Z"), h(1), &[]), Some(t("2026-10-26T10:00:00Z")), "opened before opening time");
        assert_eq!(c.add(t("2026-10-26T20:00:00Z"), h(1), &[]), Some(t("2026-10-27T10:00:00Z")), "opened after closing time");
        assert_eq!(c.add(t("2026-10-26T09:00:00Z"), h(45), &[]), Some(t("2026-10-30T18:00:00Z")), "a full working week");
        assert_eq!(c.between(t("2026-10-26T17:00:00Z"), t("2026-10-27T10:00:00Z"), &[]), h(2));
    }

    #[test]
    fn weekends_and_holidays_are_skipped() {
        let c = office(0, &[]);
        assert_eq!(c.add(t("2026-10-30T17:00:00Z"), h(2), &[]), Some(t("2026-11-02T10:00:00Z")), "Friday evening to Monday morning");
        assert_eq!(c.add(t("2026-10-31T12:00:00Z"), h(1), &[]), Some(t("2026-11-02T10:00:00Z")), "opened on a Saturday");
        assert_eq!(c.between(t("2026-10-30T17:00:00Z"), t("2026-11-02T10:00:00Z"), &[]), h(2));
        let with_holiday = office(0, &["2026-11-02"]);
        assert_eq!(with_holiday.add(t("2026-10-30T17:00:00Z"), h(2), &[]), Some(t("2026-11-03T10:00:00Z")), "Monday is a holiday");
        let both = office(0, &["2026-11-02", "2026-11-03"]);
        assert_eq!(both.add(t("2026-10-30T17:00:00Z"), h(2), &[]), Some(t("2026-11-04T10:00:00Z")));
    }

    #[test]
    fn fixed_offset_time_zones() {
        // India (UTC+05:30): Friday 17:00 IST is 11:30 UTC; two hours later is Monday 10:00 IST.
        let ist = office(330, &[]);
        assert_eq!(ist.add(t("2026-10-30T11:30:00Z"), h(2), &[]), Some(t("2026-11-02T04:30:00Z")));
        // A holiday is a local date: Monday 2 November in India starts at 18:30 UTC on Sunday.
        let ist_holiday = office(330, &["2026-11-02"]);
        assert_eq!(ist_holiday.add(t("2026-10-30T11:30:00Z"), h(2), &[]), Some(t("2026-11-03T04:30:00Z")));
        // New York standard time (UTC-05:00): 02:00 UTC on Monday is still Sunday 21:00 locally.
        let ny = office(-300, &[]);
        assert_eq!(ny.add(t("2026-10-26T02:00:00Z"), h(1), &[]), Some(t("2026-10-26T15:00:00Z")));
        assert_eq!(ny.between(t("2026-10-26T13:00:00Z"), t("2026-10-26T23:30:00Z"), &[]), h(9), "09:00-18:00 local = 14:00-23:00 UTC");
    }

    #[test]
    fn night_shifts_and_split_shifts() {
        // Monday 22:00-24:00 and Tuesday 00:00-06:00: one shift across midnight.
        let night = Calendar::new(0, &[(0, 22 * 60, 1440), (1, 0, 6 * 60)], BTreeSet::new());
        assert_eq!(night.add(t("2026-10-26T23:00:00Z"), h(3), &[]), Some(t("2026-10-27T02:00:00Z")));
        assert_eq!(night.add(t("2026-10-26T05:00:00Z"), h(1), &[]), Some(t("2026-10-26T23:00:00Z")), "waits for the shift to start");
        assert_eq!(night.add(t("2026-10-27T05:00:00Z"), h(2), &[]), Some(t("2026-11-02T23:00:00Z")), "1h now, the rest next week");
        // The holiday removes Tuesday's half of the shift only.
        let night_holiday = Calendar::new(0, &[(0, 22 * 60, 1440), (1, 0, 6 * 60)], days(&["2026-10-27"]));
        assert_eq!(night_holiday.add(t("2026-10-26T23:00:00Z"), h(3), &[]), Some(t("2026-11-03T00:00:00Z")), "1h Monday, 2h the next Monday");
        // Lunch break, and overlapping rows merge.
        let split = Calendar::new(0, &[(0, 9 * 60, 13 * 60), (0, 14 * 60, 18 * 60)], BTreeSet::new());
        assert_eq!(split.add(t("2026-10-26T12:00:00Z"), h(2), &[]), Some(t("2026-10-26T15:00:00Z")));
        let overlapping = Calendar::new(0, &[(0, 9 * 60, 13 * 60), (0, 12 * 60, 15 * 60), (0, 14 * 60, 18 * 60)], BTreeSet::new());
        assert_eq!(overlapping.add(t("2026-10-26T10:00:00Z"), h(7), &[]), Some(t("2026-10-26T17:00:00Z")));
    }

    #[test]
    fn no_working_time_means_no_deadline() {
        let none = Calendar::new(0, &[(0, 600, 600)], BTreeSet::new());
        assert_eq!(none.add(t("2026-10-26T10:00:00Z"), h(1), &[]), None);
        let clock = Clock { opened: t("2026-10-26T10:00:00Z"), stopped: &[], now: t("2026-10-27T10:00:00Z") };
        assert_eq!(verdict(&none, &clock, h(1), None), Verdict { deadline: None, state: State::OnTrack });
    }

    #[test]
    fn stopped_time_does_not_count() {
        let c = office(0, &[]);
        let lunch = [(t("2026-10-26T11:00:00Z"), t("2026-10-26T13:00:00Z"))];
        assert_eq!(c.add(t("2026-10-26T10:00:00Z"), h(4), &lunch), Some(t("2026-10-26T16:00:00Z")));
        assert_eq!(c.between(t("2026-10-26T10:00:00Z"), t("2026-10-26T16:00:00Z"), &lunch), h(4));
        // A pause across closing time only removes working time.
        let overnight = [(t("2026-10-26T17:00:00Z"), t("2026-10-27T10:00:00Z"))];
        assert_eq!(c.add(t("2026-10-26T10:00:00Z"), h(8), &overnight), Some(t("2026-10-27T11:00:00Z")));
    }

    #[test]
    fn timeline_to_stopped_intervals() {
        let tl: Vec<(String, Ts)> = [
            ("Open", "2026-10-26T09:00:00Z"),
            ("On Hold", "2026-10-26T10:00:00Z"),
            ("Replied", "2026-10-26T11:00:00Z"),
            ("Open", "2026-10-26T12:00:00Z"),
            ("Replied", "2026-10-26T13:00:00Z"),
            ("Open", "2026-10-26T14:00:00Z"),
            ("Resolved", "2026-10-26T15:00:00Z"),
        ]
        .iter()
        .map(|(s, at)| (s.to_string(), t(at)))
        .collect();
        let now = t("2026-10-26T16:00:00Z");
        let all = stopped_intervals(&tl, |s| matches!(s, "On Hold" | "Replied" | "Resolved"), now);
        assert_eq!(
            all,
            vec![
                (t("2026-10-26T10:00:00Z"), t("2026-10-26T12:00:00Z")),
                (t("2026-10-26T13:00:00Z"), t("2026-10-26T14:00:00Z")),
                (t("2026-10-26T15:00:00Z"), now)
            ],
            "adjacent pauses merge; the last status runs until now"
        );
        let hold_only = stopped_intervals(&tl, |s| s == "On Hold", now);
        assert_eq!(hold_only, vec![(t("2026-10-26T10:00:00Z"), t("2026-10-26T11:00:00Z"))]);
        assert_eq!(clip(&all, t("2026-10-26T13:30:00Z")).last(), Some(&(t("2026-10-26T13:00:00Z"), t("2026-10-26T13:30:00Z"))));
    }

    fn timeline(entries: &[(&str, &str)]) -> Vec<(String, Ts)> {
        entries.iter().map(|(s, at)| (s.to_string(), t(at))).collect()
    }

    fn pauses(tl: &[(String, Ts)], now: Ts) -> Vec<(Ts, Ts)> {
        stopped_intervals(tl, |s| matches!(s, "On Hold" | "Replied" | "Resolved" | "Closed"), now)
    }

    #[test]
    fn verdicts_on_track_at_risk_breached_fulfilled() {
        let c = office(0, &[]);
        let opened = t("2026-10-26T10:00:00Z");
        let at = |now: &str| Clock { opened, stopped: &[], now: t(now) };
        let deadline = Some(t("2026-10-26T14:00:00Z"));
        assert_eq!(verdict(&c, &at("2026-10-26T10:30:00Z"), h(4), None), Verdict { deadline, state: State::OnTrack });
        assert_eq!(verdict(&c, &at("2026-10-26T13:12:00Z"), h(4), None).state, State::AtRisk, "48 minutes = 20% left");
        assert_eq!(verdict(&c, &at("2026-10-26T13:11:00Z"), h(4), None).state, State::OnTrack);
        assert_eq!(verdict(&c, &at("2026-10-26T14:00:00Z"), h(4), None).state, State::AtRisk, "due now is not late yet");
        assert_eq!(verdict(&c, &at("2026-10-26T14:01:00Z"), h(4), None).state, State::Breached);
        assert_eq!(verdict(&c, &at("2026-10-27T09:00:00Z"), h(4), Some(t("2026-10-26T13:00:00Z"))), Verdict { deadline, state: State::Fulfilled });
        assert_eq!(verdict(&c, &at("2026-10-27T09:00:00Z"), h(4), Some(t("2026-10-26T14:30:00Z"))).state, State::Breached);
        // At risk is measured in business time: on Friday at 17:30, 1.5 of 9 hours are left
        // although the deadline is on Monday.
        let friday = Clock { opened: t("2026-10-30T10:00:00Z"), stopped: &[], now: t("2026-10-30T17:30:00Z") };
        assert_eq!(verdict(&c, &friday, h(9), None), Verdict { deadline: Some(t("2026-11-02T10:00:00Z")), state: State::AtRisk });
    }

    #[test]
    fn pause_and_resume_moves_the_deadline_by_the_paused_business_time() {
        let c = office(0, &[]);
        let opened = t("2026-10-26T10:00:00Z");
        // On hold 11:00-13:00: a 4h target moves from 14:00 to 16:00.
        let tl = timeline(&[("Open", "2026-10-26T10:00:00Z"), ("On Hold", "2026-10-26T11:00:00Z"), ("Open", "2026-10-26T13:00:00Z")]);
        let now = t("2026-10-26T15:00:00Z");
        let stopped = pauses(&tl, now);
        let v = verdict(&c, &Clock { opened, stopped: &stopped, now }, h(4), None);
        assert_eq!(v, Verdict { deadline: Some(t("2026-10-26T16:00:00Z")), state: State::OnTrack }, "1h of 4h left is more than 20%");
        // Still on hold: the deadline is where it lands if the ticket resumes now, and the time
        // left stays what it was when the pause began.
        let held = timeline(&[("Open", "2026-10-26T10:00:00Z"), ("On Hold", "2026-10-26T11:00:00Z")]);
        for (now, due) in [("2026-10-26T12:00:00Z", "2026-10-26T15:00:00Z"), ("2026-10-26T16:30:00Z", "2026-10-27T10:30:00Z")] {
            let now = t(now);
            let stopped = pauses(&held, now);
            assert_eq!(verdict(&c, &Clock { opened, stopped: &stopped, now }, h(4), None), Verdict { deadline: Some(t(due)), state: State::OnTrack });
        }
        // A pause after the deadline passed does not undo the breach.
        let late = timeline(&[("Open", "2026-10-26T10:00:00Z"), ("On Hold", "2026-10-26T15:00:00Z")]);
        let now = t("2026-10-27T12:00:00Z");
        let stopped = pauses(&late, now);
        assert_eq!(
            verdict(&c, &Clock { opened, stopped: &stopped, now }, h(4), None),
            Verdict { deadline: Some(t("2026-10-26T14:00:00Z")), state: State::Breached }
        );
        // Pauses after a target was met do not move that target's deadline either.
        let replied = timeline(&[("Open", "2026-10-26T10:00:00Z"), ("Replied", "2026-10-26T11:00:00Z"), ("Open", "2026-10-26T16:00:00Z")]);
        let stopped = pauses(&replied, now);
        let clock = Clock { opened, stopped: &stopped, now };
        assert_eq!(verdict(&c, &clock, h(2), Some(t("2026-10-26T11:00:00Z"))), Verdict { deadline: Some(t("2026-10-26T12:00:00Z")), state: State::Fulfilled });
        assert_eq!(verdict(&c, &clock, h(8), None).deadline, Some(t("2026-10-27T14:00:00Z")), "resolution: 10-11 + 16-18 + 09-14 next day");
        assert_eq!(elapsed(&c, &clock, t("2026-10-26T11:00:00Z")), h(1));
    }

    #[test]
    fn priority_change_recomputes_from_the_opening() {
        let c = office(0, &[]);
        let clock = Clock { opened: t("2026-10-30T16:00:00Z"), stopped: &[], now: t("2026-10-30T16:30:00Z") };
        let low = verdict(&c, &clock, h(16), None);
        let urgent = verdict(&c, &clock, h(1), None);
        assert_eq!(low.deadline, Some(t("2026-11-03T14:00:00Z")), "2h Friday + 9h Monday + 5h Tuesday");
        assert_eq!(urgent, Verdict { deadline: Some(t("2026-10-30T17:00:00Z")), state: State::OnTrack });
        let later = Clock { now: t("2026-11-02T09:30:00Z"), ..clock };
        assert_eq!(verdict(&c, &later, h(1), None).state, State::Breached, "an upgrade can turn a ticket late at once");
    }

    #[test]
    fn reopened_ticket_does_not_count_the_time_it_sat_resolved() {
        let c = office(0, &[]);
        let opened = t("2026-10-26T10:00:00Z");
        let tl = timeline(&[("Open", "2026-10-26T10:00:00Z"), ("Resolved", "2026-10-26T12:00:00Z"), ("Open", "2026-10-27T10:00:00Z")]);
        let now = t("2026-10-27T11:00:00Z");
        let stopped = pauses(&tl, now);
        let clock = Clock { opened, stopped: &stopped, now };
        assert_eq!(verdict(&c, &clock, h(8), None), Verdict { deadline: Some(t("2026-10-27T16:00:00Z")), state: State::OnTrack });
        // Resolved again at 15:00: met, with 5 business hours spent.
        let tl = timeline(&[
            ("Open", "2026-10-26T10:00:00Z"),
            ("Resolved", "2026-10-26T12:00:00Z"),
            ("Open", "2026-10-27T10:00:00Z"),
            ("Resolved", "2026-10-27T13:00:00Z"),
        ]);
        let now = t("2026-10-28T09:00:00Z");
        let stopped = pauses(&tl, now);
        let clock = Clock { opened, stopped: &stopped, now };
        let resolved = t("2026-10-27T13:00:00Z");
        assert_eq!(verdict(&c, &clock, h(8), Some(resolved)).state, State::Fulfilled);
        assert_eq!(elapsed(&c, &clock, resolved), h(5));
    }

    #[test]
    fn overall_status() {
        use State::*;
        assert_eq!(overall(Fulfilled, OnTrack), OnTrack);
        assert_eq!(overall(OnTrack, OnTrack), OnTrack);
        assert_eq!(overall(AtRisk, OnTrack), AtRisk);
        assert_eq!(overall(Fulfilled, AtRisk), AtRisk);
        assert_eq!(overall(Breached, Fulfilled), Breached, "a late first response stays a breach");
        assert_eq!(overall(Fulfilled, Breached), Breached);
        assert_eq!(overall(Fulfilled, Fulfilled), Fulfilled);
        assert_eq!(Breached.label(), "Breached");
    }
}
