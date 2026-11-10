//! Service targets (docs/builder/decisions.md BD-04): "this must happen within so much working time", kept per
//! record as one round at a time. This is the one decision: the round as it was, what the target's expressions
//! say on the record as it will be stored, the clock's now and the calendar in; the round as it is now, the due
//! moment and the state word out. The kernel only loads, calls and stores.
//!
//! A round keeps no list of intervals: at a pause `worked_secs` grows by the open time since `resumed_at`; at a
//! resume `resumed_at` is now and the due moment is the remaining time added from now. A breach is set by the
//! kernel's sweep, never here; a round met after its due moment, or after a breach, is `met_late`.

use crate::calendar::{Calendar, MAX_WAIT, Wait};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

/// One record's round of one target. Not started: `started_at` is none (`round` counts the rounds begun).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Round {
    pub round: i32,
    pub started_at: Option<DateTime<Utc>>,
    pub resumed_at: Option<DateTime<Utc>>,
    pub paused_at: Option<DateTime<Utc>>,
    pub worked_secs: i64,
    pub within_secs: i64,
    pub due_at: Option<DateTime<Utc>>,
    pub met_at: Option<DateTime<Utc>>,
    pub breached_at: Option<DateTime<Utc>>,
}

/// What a target's expressions said on the record.
#[derive(Debug, Clone, Copy)]
pub struct Said {
    /// `start_when` holds (or there is none).
    pub start: bool,
    /// `within`, in seconds: none when it gave nothing.
    pub within: Option<u64>,
    pub pause: bool,
    pub met: bool,
}

/// `within`'s value as seconds: a whole number of seconds or an ISO 8601 duration (`PT4H`, `P1D`), more than
/// none and at most a year; null or empty is none. Anything else is an error naming it.
pub fn within(v: &Value) -> Result<Option<u64>, String> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) if s.trim().is_empty() => Ok(None),
        _ => match Wait::parse(v) {
            Some(Wait::Elapsed(secs)) => Ok(Some(secs)),
            _ => Err(format!("within gave {v}: it gives seconds or an ISO 8601 duration such as PT4H, more than none and at most {MAX_WAIT} seconds")),
        },
    }
}

fn add(cal: Option<&Calendar>, from: DateTime<Utc>, secs: i64) -> Result<DateTime<Utc>, String> {
    match cal {
        Some(c) => c.add_working(from, secs.max(0) as u64, &[]),
        None => Ok(from + Duration::seconds(secs.max(0))),
    }
}

fn between(cal: Option<&Calendar>, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<i64, String> {
    match cal {
        Some(c) => Ok(c.working_between(from, to, &[])?.num_seconds()),
        None => Ok((to - from).num_seconds().max(0)),
    }
}

impl Round {
    /// The same count of rounds, none running.
    fn idle(&self) -> Round {
        Round { round: self.round, ..Round::default() }
    }

    /// Open time worked in this round up to `now`.
    fn worked(&self, cal: Option<&Calendar>, now: DateTime<Utc>) -> Result<i64, String> {
        Ok(self.worked_secs + self.resumed_at.map(|r| between(cal, r, now)).transpose()?.unwrap_or(0))
    }

    /// The due moment from `now`, with what is left of `within_secs`; due now when none is left.
    fn due_from(&self, cal: Option<&Calendar>, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
        let left = self.within_secs - self.worked(cal, now)?;
        if left > 0 { add(cal, now, left) } else { Ok(now) }
    }

    /// `running`, `paused`, `met`, `met_late` or `breached`; none when not started.
    pub fn word(&self) -> Option<&'static str> {
        self.started_at?;
        Some(match (self.met_at, self.breached_at, self.paused_at) {
            (Some(m), b, _) if b.is_some() || self.due_at.is_some_and(|d| d < m) => "met_late",
            (Some(_), ..) => "met",
            (None, Some(_), _) => "breached",
            (None, None, Some(_)) => "paused",
            _ => "running",
        })
    }
}

/// The next round, its due moment and its state word after a save at `now` (each step as BD-04's note says):
/// start; pause or resume; a new `within` while running; met; a met round re-opened starts the next; `within`
/// giving nothing drops a running round.
pub fn step(prev: &Round, said: Said, now: DateTime<Utc>, cal: Option<&Calendar>) -> Result<(Round, Option<DateTime<Utc>>, Option<&'static str>), String> {
    let mut r = prev.clone();
    let Some(within) = said.within else {
        if r.met_at.is_none() {
            r = r.idle();
        }
        return Ok((r, None, None));
    };
    let within = within.min(i64::MAX as u64) as i64;
    if r.met_at.is_some() && !said.met {
        r = r.idle();
    }
    if r.started_at.is_none() {
        if !said.start || said.met {
            return Ok((r, None, None));
        }
        r = Round { round: r.round + 1, started_at: Some(now), resumed_at: Some(now), within_secs: within, ..Round::default() };
        r.due_at = Some(r.due_from(cal, now)?);
    }
    if r.met_at.is_none() {
        let running = r.paused_at.is_none();
        if running && said.pause {
            r.worked_secs = r.worked(cal, now)?;
            (r.resumed_at, r.paused_at, r.due_at) = (None, Some(now), None);
        } else if !running && !said.pause {
            (r.resumed_at, r.paused_at, r.within_secs) = (Some(now), None, within);
            r.due_at = Some(r.due_from(cal, now)?);
        }
        if r.within_secs != within {
            r.within_secs = within;
            if r.paused_at.is_none() {
                r.due_at = Some(r.due_from(cal, now)?);
            }
        }
        if said.met {
            r.worked_secs = r.worked(cal, now)?;
            (r.resumed_at, r.met_at) = (None, Some(now));
        }
    }
    let (due, word) = (r.due_at, r.word());
    Ok((r, due, word))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    const H: u64 = 3600;

    fn said(start: bool, within: Option<u64>, pause: bool, met: bool) -> Said {
        Said { start, within, pause, met }
    }

    /// Kolkata, Monday to Friday 09:00–13:00 and 14:00–18:00; Tuesday 10 November 2026 a holiday.
    fn office() -> Calendar {
        let day = json!([["09:00", "13:00"], ["14:00", "18:00"]]);
        serde_json::from_value(json!({"name": "office", "zone": "Asia/Kolkata", "weekends": ["sat", "sun"], "holidays": ["2026-11-10"],
            "revision": 1, "hours": {"mon": day, "tue": day, "wed": day, "thu": day, "fri": day}}))
        .unwrap()
    }

    /// Steps through saves, each `(now, said, due, word)`, from a round never started.
    fn walk(cal: Option<&Calendar>, saves: &[(&str, Said, Option<&str>, Option<&str>)]) -> Round {
        let mut r = Round::default();
        for (i, (now, s, due, word)) in saves.iter().enumerate() {
            let (next, d, w) = step(&r, *s, utc(now), cal).unwrap();
            assert_eq!((d, w), (due.map(utc), *word), "save {} at {now}: {next:?}", i + 1);
            assert_eq!(d, next.due_at, "the due field is the round's");
            r = next;
        }
        r
    }

    #[test]
    fn rounds_as_a_table() {
        let none = None;
        let t = "2026-11-02T10:00:00Z";
        let run = said(true, Some(4 * H), false, false);
        // Plain elapsed time.
        let r = walk(
            none,
            &[
                (t, run, Some("2026-11-02T14:00:00Z"), Some("running")),
                // A save with nothing new keeps the due moment.
                ("2026-11-02T11:00:00Z", run, Some("2026-11-02T14:00:00Z"), Some("running")),
                // Paused an hour in: three hours left, the due moment unknown while stopped.
                ("2026-11-02T11:00:00Z", said(true, Some(4 * H), true, false), None, Some("paused")),
                // A new within while paused counts at the resume.
                ("2026-11-02T12:00:00Z", said(true, Some(2 * H), true, false), None, Some("paused")),
                // Resumed two hours later: one hour of the two left.
                ("2026-11-02T13:00:00Z", said(true, Some(2 * H), false, false), Some("2026-11-02T14:00:00Z"), Some("running")),
                // Within raised while running: from now, what is left of eight hours (six and a half).
                ("2026-11-02T13:30:00Z", said(true, Some(8 * H), false, false), Some("2026-11-02T20:00:00Z"), Some("running")),
                // Lowered below what was used: due now.
                ("2026-11-02T14:00:00Z", said(true, Some(H), false, false), Some("2026-11-02T14:00:00Z"), Some("running")),
                // Met after the due moment (no sweep yet): late.
                ("2026-11-02T15:00:00Z", said(true, Some(H), false, true), Some("2026-11-02T14:00:00Z"), Some("met_late")),
                // Still met: nothing moves.
                ("2026-11-02T16:00:00Z", said(true, Some(H), false, true), Some("2026-11-02T14:00:00Z"), Some("met_late")),
                // Re-opened: round two from now.
                ("2026-11-02T17:00:00Z", said(true, Some(H), false, false), Some("2026-11-02T18:00:00Z"), Some("running")),
                // Met in time.
                ("2026-11-02T17:30:00Z", said(true, Some(H), false, true), Some("2026-11-02T18:00:00Z"), Some("met")),
            ],
        );
        assert_eq!((r.round, r.worked_secs), (2, 1800));

        // start_when later: nothing until it holds; met before it ever started: nothing.
        walk(
            none,
            &[
                (t, said(false, Some(H), false, false), None, None),
                (t, said(true, Some(H), false, true), None, None),
                ("2026-11-02T12:00:00Z", said(true, Some(H), false, false), Some("2026-11-02T13:00:00Z"), Some("running")),
            ],
        );
        // Started already paused; within nothing drops the round and a later start is round two.
        let r = walk(
            none,
            &[
                (t, said(true, Some(H), true, false), None, Some("paused")),
                ("2026-11-02T11:00:00Z", said(true, None, false, false), None, None),
                ("2026-11-02T12:00:00Z", said(true, Some(H), false, false), Some("2026-11-02T13:00:00Z"), Some("running")),
            ],
        );
        assert_eq!(r.round, 2);
        // A breach (the sweep's) shows until the round is met, then the round is late.
        let mut r = walk(none, &[(t, run, Some("2026-11-02T14:00:00Z"), Some("running"))]);
        r.breached_at = Some(utc("2026-11-02T14:00:30Z"));
        let (r, _, w) = step(&r, run, utc("2026-11-02T15:00:00Z"), None).unwrap();
        assert_eq!(w, Some("breached"));
        // Raising within past now leaves the breach on record.
        let (r, d, w) = step(&r, said(true, Some(10 * H), false, false), utc("2026-11-02T15:00:00Z"), None).unwrap();
        assert_eq!((d, w), (Some(utc("2026-11-02T20:00:00Z")), Some("breached")));
        assert_eq!(step(&r, said(true, Some(10 * H), false, true), utc("2026-11-02T16:00:00Z"), None).unwrap().2, Some("met_late"));
    }

    #[test]
    fn rounds_on_a_calendar_with_hours() {
        let c = office();
        // Friday 6 November 2026, 17:00 in Kolkata: thirteen hours are one on Friday, eight on Monday, none on
        // the holiday Tuesday and four on Wednesday, to 13:00 there.
        let friday = "2026-11-06T11:30:00Z";
        walk(
            Some(&c),
            &[
                (friday, said(true, Some(13 * H), false, false), Some("2026-11-11T07:30:00Z"), Some("running")),
                // Paused on Monday at 11:00 there (three working hours in), resumed on Wednesday at 09:00:
                // ten hours left, to 11:00 on Thursday.
                ("2026-11-09T05:30:00Z", said(true, Some(13 * H), true, false), None, Some("paused")),
                ("2026-11-11T03:30:00Z", said(true, Some(13 * H), false, false), Some("2026-11-12T05:30:00Z"), Some("running")),
            ],
        );
        // Started out of hours (Saturday): the count starts at Monday's opening.
        walk(Some(&c), &[("2026-11-07T06:00:00Z", said(true, Some(2 * H), false, false), Some("2026-11-09T05:30:00Z"), Some("running"))]);
    }

    #[test]
    fn within_reads_seconds_or_a_duration() {
        assert_eq!(within(&json!(3600)), Ok(Some(3600)));
        assert_eq!(within(&json!("PT8H")), Ok(Some(8 * H)));
        assert_eq!(within(&json!(null)), Ok(None));
        assert_eq!(within(&json!(" ")), Ok(None));
        for bad in [json!("soon"), json!(-1), json!(0), json!(true), json!("P2Y")] {
            assert!(within(&bad).is_err(), "{bad}");
        }
    }
}
