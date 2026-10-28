//! Pure project maths: time spans, overlaps, dependency cycles, progress, billing status and the
//! grouping of time logs into invoice lines. No I/O, so every rule here is unit-tested.

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use uuid::Uuid;

/// Task statuses that mean work has started (blocked while a dependency is not completed).
pub const STARTED: &[&str] = &["Working", "Pending Review", "Completed"];
/// Task statuses that close a task.
pub const CLOSED: &[&str] = &["Completed", "Cancelled"];
/// Project statuses that freeze its tasks and refuse new time.
pub const CLOSED_PROJECT: &[&str] = &["Completed", "Cancelled"];

pub type Span = (DateTime<Utc>, DateTime<Utc>);

/// Hours between two instants, at the 9 decimals quantities are stored with.
pub fn hours_between(from: DateTime<Utc>, to: DateTime<Utc>) -> Decimal {
    (Decimal::from((to - from).num_milliseconds()) / Decimal::from(3_600_000)).round_dp(9)
}

/// End of a span starting at `from` that lasts `hours` (to the millisecond); `None` if out of range.
pub fn add_hours(from: DateTime<Utc>, hours: Decimal) -> Option<DateTime<Utc>> {
    let ms = (hours * Decimal::from(3_600_000)).round().to_i64()?;
    from.checked_add_signed(TimeDelta::try_milliseconds(ms)?)
}

/// Half-open spans overlap when each starts before the other ends (back-to-back is fine).
pub fn overlaps(a: Span, b: Span) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// The first pair of overlapping spans (indexes, lower first), in one sweep over the spans sorted
/// by start: a span overlaps an earlier one exactly when it starts before the latest end so far.
pub fn first_overlap(spans: &[Span]) -> Option<(usize, usize)> {
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|&i| spans[i]);
    let mut latest: Option<usize> = None;
    for i in order {
        match latest {
            Some(l) if overlaps(spans[l], spans[i]) => return Some((l.min(i), l.max(i))),
            Some(l) if spans[l].1 >= spans[i].1 => {}
            _ => latest = Some(i),
        }
    }
    None
}

/// The cycle `task → … → task` that making `task` depend on `deps` would close, given the other
/// tasks' dependency edges (task → the tasks it depends on). Breadth-first, so the shortest cycle
/// is reported; `task`'s own stored edges are never followed (they are being replaced).
pub fn dependency_cycle(task: Uuid, deps: &[Uuid], edges: &HashMap<Uuid, Vec<Uuid>>) -> Option<Vec<Uuid>> {
    let mut came_from: HashMap<Uuid, Uuid> = HashMap::new();
    let mut queue = VecDeque::new();
    for &d in deps {
        if d == task {
            return Some(vec![task, task]);
        }
        if came_from.insert(d, task).is_none() {
            queue.push_back(d);
        }
    }
    while let Some(n) = queue.pop_front() {
        for &m in edges.get(&n).into_iter().flatten() {
            if m == task {
                let mut chain = vec![n];
                let mut cur = n;
                while let Some(&p) = came_from.get(&cur) {
                    if p == task {
                        break;
                    }
                    chain.push(p);
                    cur = p;
                }
                chain.reverse();
                let mut path = vec![task];
                path.extend(chain);
                path.push(task);
                return Some(path);
            }
            if let Entry::Vacant(e) = came_from.entry(m) {
                e.insert(n);
                queue.push_back(m);
            }
        }
    }
    None
}

/// Project % complete from its tasks' `(status, progress, expected hours)`: cancelled tasks are
/// left out, completed ones count as done, and each task weighs its expected hours (all weigh
/// the same when none has any).
pub fn weighted_progress(tasks: &[(String, Decimal, Decimal)]) -> Decimal {
    let hundred = Decimal::ONE_HUNDRED;
    let live: Vec<(Decimal, Decimal)> = tasks
        .iter()
        .filter(|(status, _, _)| status != "Cancelled")
        .map(|(status, p, h)| (if status == "Completed" { hundred } else { (*p).clamp(Decimal::ZERO, hundred) }, (*h).max(Decimal::ZERO)))
        .collect();
    if live.is_empty() {
        return Decimal::ZERO;
    }
    let hours: Decimal = live.iter().map(|t| t.1).sum();
    let pct = if hours.is_zero() {
        live.iter().map(|t| t.0).sum::<Decimal>() / Decimal::from(live.len())
    } else {
        live.iter().map(|(p, h)| p * h).sum::<Decimal>() / hours
    };
    pct.round_dp(2).min(hundred)
}

pub fn percent(part: Decimal, whole: Decimal) -> Decimal {
    if whole.is_zero() { Decimal::ZERO } else { (part / whole * Decimal::ONE_HUNDRED).round_dp(2) }
}

/// Timesheet status from its document status and billing.
pub fn timesheet_status(docstatus: i16, billable: Decimal, billed: Decimal) -> &'static str {
    match docstatus {
        0 => "Draft",
        2 => "Cancelled",
        _ if billed > Decimal::ZERO && billed >= billable => "Billed",
        _ if billed > Decimal::ZERO => "Partly Billed",
        _ => "Submitted",
    }
}

/// `(open, overdue, completed)` for a task on `today`. Overdue tasks are open ones past their
/// expected end, whether or not the periodic job has marked them yet.
pub fn task_bucket(status: &str, exp_end: Option<NaiveDate>, today: NaiveDate) -> (bool, bool, bool) {
    let open = !CLOSED.contains(&status);
    let overdue = open && (status == "Overdue" || exp_end.is_some_and(|e| e < today));
    (open, overdue, status == "Completed")
}

#[derive(Debug, Clone)]
pub struct BillableLog {
    pub id: Uuid,
    pub activity_type: Uuid,
    pub hours: Decimal,
    pub rate: Decimal,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimeLine {
    pub activity_type: Uuid,
    pub rate: Decimal,
    pub hours: Decimal,
    pub logs: Vec<Uuid>,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

/// One invoice line per activity type and billing rate, in order of first appearance: time of
/// one activity billed at different (per-user) rates cannot share a line price.
pub fn group_time(logs: &[BillableLog]) -> Vec<TimeLine> {
    let mut out: Vec<TimeLine> = Vec::new();
    for l in logs {
        match out.iter_mut().find(|x| x.activity_type == l.activity_type && x.rate == l.rate) {
            Some(x) => {
                x.hours += l.hours;
                x.logs.push(l.id);
                x.from = x.from.min(l.from);
                x.to = x.to.max(l.to);
            }
            None => out.push(TimeLine { activity_type: l.activity_type, rate: l.rate, hours: l.hours, logs: vec![l.id], from: l.from, to: l.to }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }
    fn n(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn hours_follow_the_span() {
        assert_eq!(hours_between(t("2026-10-26T09:00:00Z"), t("2026-10-26T12:30:00Z")), n("3.5"));
        assert_eq!(hours_between(t("2026-10-26T09:00:00Z"), t("2026-10-26T09:20:00Z")), n("0.333333333"));
        assert_eq!(add_hours(t("2026-10-26T09:00:00Z"), n("1.25")), Some(t("2026-10-26T10:15:00Z")));
        assert_eq!(add_hours(t("2026-10-26T09:00:00Z"), n("1e18")), None, "absurd hours are refused, not panicking");
    }

    #[test]
    fn overlap_detection() {
        let a = (t("2026-10-26T09:00:00Z"), t("2026-10-26T11:00:00Z"));
        let b = (t("2026-10-26T10:30:00Z"), t("2026-10-26T12:00:00Z"));
        let c = (t("2026-10-26T11:00:00Z"), t("2026-10-26T13:00:00Z"));
        assert!(overlaps(a, b) && overlaps(b, a));
        assert!(!overlaps(a, c), "back-to-back spans do not overlap");
        assert_eq!(first_overlap(&[c, a]), None);
        assert_eq!(first_overlap(&[c, b, a]), Some((1, 2)));
        // A long span hides behind a short one that starts later: the sweep still finds it.
        let long = (t("2026-10-26T08:00:00Z"), t("2026-10-26T18:00:00Z"));
        let short = (t("2026-10-26T09:00:00Z"), t("2026-10-26T09:30:00Z"));
        let late = (t("2026-10-26T17:00:00Z"), t("2026-10-26T19:00:00Z"));
        assert_eq!(first_overlap(&[late, short, long]), Some((1, 2)));
        assert_eq!(first_overlap(&[late, long]), Some((0, 1)));
        assert_eq!(first_overlap(&[]), None);
    }

    #[test]
    fn dependency_cycles() {
        let [a, b, c, d] = [Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
        assert_eq!(dependency_cycle(a, &[a], &HashMap::new()), Some(vec![a, a]), "self dependency");
        // b depends on a: a depending on b closes a → b → a.
        let edges: HashMap<Uuid, Vec<Uuid>> = [(b, vec![a])].into_iter().collect();
        assert_eq!(dependency_cycle(a, &[b], &edges), Some(vec![a, b, a]));
        // c → b → a: a depending on c closes a → c → b → a (indirect).
        let edges: HashMap<Uuid, Vec<Uuid>> = [(b, vec![a]), (c, vec![b])].into_iter().collect();
        assert_eq!(dependency_cycle(a, &[d, c], &edges), Some(vec![a, c, b, a]));
        // A diamond is not a cycle.
        let edges: HashMap<Uuid, Vec<Uuid>> = [(b, vec![d]), (c, vec![d])].into_iter().collect();
        assert_eq!(dependency_cycle(a, &[b, c], &edges), None);
        // An existing cycle elsewhere neither loops forever nor counts.
        let edges: HashMap<Uuid, Vec<Uuid>> = [(b, vec![c]), (c, vec![b])].into_iter().collect();
        assert_eq!(dependency_cycle(a, &[b], &edges), None);
    }

    #[test]
    fn progress_is_weighted_by_expected_hours() {
        let task = |s: &str, p: &str, h: &str| (s.to_string(), n(p), n(h));
        assert_eq!(weighted_progress(&[]), Decimal::ZERO);
        assert_eq!(weighted_progress(&[task("Open", "50", "10"), task("Working", "0", "30")]), n("12.5"));
        assert_eq!(weighted_progress(&[task("Completed", "0", "10"), task("Open", "0", "30")]), n("25"), "completed counts as 100 %");
        assert_eq!(weighted_progress(&[task("Open", "40", "0"), task("Open", "80", "0")]), n("60"), "no hours: plain average");
        assert_eq!(weighted_progress(&[task("Cancelled", "0", "100"), task("Completed", "0", "1")]), n("100"), "cancelled tasks do not count");
        assert_eq!(weighted_progress(&[task("Open", "33.333", "1"), task("Open", "0", "2")]), n("11.11"));
    }

    #[test]
    fn statuses() {
        assert_eq!(timesheet_status(0, n("10"), n("0")), "Draft");
        assert_eq!(timesheet_status(1, n("10"), n("0")), "Submitted");
        assert_eq!(timesheet_status(1, n("0"), n("0")), "Submitted");
        assert_eq!(timesheet_status(1, n("10"), n("4")), "Partly Billed");
        assert_eq!(timesheet_status(1, n("10"), n("10")), "Billed");
        assert_eq!(timesheet_status(2, n("10"), n("10")), "Cancelled");
        let day = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let today = day("2026-10-28");
        assert_eq!(task_bucket("Open", Some(day("2026-10-27")), today), (true, true, false));
        assert_eq!(task_bucket("Working", Some(day("2026-10-28")), today), (true, false, false));
        assert_eq!(task_bucket("Overdue", None, today), (true, true, false));
        assert_eq!(task_bucket("Completed", Some(day("2026-01-01")), today), (false, false, true));
        assert_eq!(percent(n("1"), n("3")), n("33.33"));
        assert_eq!(percent(n("1"), Decimal::ZERO), Decimal::ZERO);
    }

    #[test]
    fn time_is_grouped_by_activity_and_rate() {
        let (dev, consult) = (Uuid::now_v7(), Uuid::now_v7());
        let log = |a: Uuid, h: &str, r: &str, from: &str, to: &str| BillableLog {
            id: Uuid::now_v7(),
            activity_type: a,
            hours: n(h),
            rate: n(r),
            from: t(from),
            to: t(to),
        };
        let logs = [
            log(dev, "3", "120", "2026-10-26T09:00:00Z", "2026-10-26T12:00:00Z"),
            log(consult, "2.5", "150", "2026-10-26T13:00:00Z", "2026-10-26T15:30:00Z"),
            log(dev, "2", "120.00", "2026-10-25T09:00:00Z", "2026-10-25T11:00:00Z"),
            log(dev, "1", "100", "2026-10-27T09:00:00Z", "2026-10-27T10:00:00Z"),
        ];
        let lines = group_time(&logs);
        assert_eq!(lines.len(), 3, "same activity at another rate gets its own line");
        assert_eq!((lines[0].activity_type, lines[0].hours, lines[0].logs.len()), (dev, n("5"), 2));
        assert_eq!((lines[0].from, lines[0].to), (t("2026-10-25T09:00:00Z"), t("2026-10-26T12:00:00Z")));
        assert_eq!((lines[1].activity_type, lines[1].rate), (consult, n("150")));
        assert_eq!((lines[2].rate, lines[2].hours), (n("100"), n("1")));
    }
}
