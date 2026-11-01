//! OEE, with its definition written down before any dashboard. For one piece of equipment, one
//! product and one period:
//!
//! - **Availability** = run time / planned production time
//! - **Performance** = (ideal cycle time × total count) / run time
//! - **Quality** = good count / total count
//!
//! A figure with nothing to divide by is unknown, not zero and not 100%. Missing telemetry is never
//! counted as running. OEE is an indicator, not a goal: maximizing one machine can worsen the line.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The contract's revision: stored with every figure, so old figures stay explainable.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Input {
    pub planned_secs: f64,
    pub run_secs: f64,
    /// Seconds with no information about the equipment, inside the planned time. Counted neither as
    /// running nor as stopped.
    #[serde(default)]
    pub unknown_secs: f64,
    /// Seconds per unit at full speed. Cavities and parallel tools are the caller's: `cavities`
    /// units per cycle make this `cycle / cavities`.
    pub ideal_cycle_secs: f64,
    pub total_count: f64,
    pub good_count: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Figures {
    pub availability: Option<f64>,
    pub performance: Option<f64>,
    pub quality: Option<f64>,
    pub oee: Option<f64>,
    /// How much of the planned time is known, 0 to 1. Figures from an incomplete period are figures
    /// about what was seen.
    pub coverage: Option<f64>,
    pub version: u32,
}

fn ratio(a: f64, b: f64) -> Option<f64> {
    (b > 0.0 && a.is_finite() && b.is_finite() && a >= 0.0).then(|| a / b)
}

/// Performance above 1 is kept: it says the ideal cycle time is wrong, which someone needs to see,
/// and capping it would hide that.
pub fn figures(i: &Input) -> Figures {
    let known = i.planned_secs - i.unknown_secs;
    let availability = ratio(i.run_secs, known);
    let performance = ratio(i.ideal_cycle_secs * i.total_count, i.run_secs);
    let quality = ratio(i.good_count, i.total_count);
    let oee = match (availability, performance, quality) {
        (Some(a), Some(p), Some(q)) => Some(a * p * q),
        _ => None,
    };
    Figures { availability, performance, quality, oee, coverage: ratio(known.max(0.0), i.planned_secs), version: VERSION }
}

/// A state equipment entered, at a time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateAt {
    pub at: DateTime<Utc>,
    pub state: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TimeInStates {
    pub run_secs: f64,
    /// Stopped for a planned reason: not planned production time at all.
    pub planned_stop_secs: f64,
    /// Nothing is known about it: before anything was reported, or marked unknown.
    pub unknown_secs: f64,
}

/// Seconds of `[from, to)` in each state. Events must be in time order; `before` is the state that
/// held when the period began, if anything had been reported by then. Late events are simply where
/// they belong in that order, because states are kept as events and not as intervals.
pub fn time_in_states(before: Option<&str>, events: &[StateAt], from: DateTime<Utc>, to: DateTime<Utc>) -> TimeInStates {
    let mut out = TimeInStates::default();
    let mut add = |state: Option<&str>, secs: f64| match state {
        Some("Running") => out.run_secs += secs,
        Some("Planned") => out.planned_stop_secs += secs,
        None | Some("Unknown") => out.unknown_secs += secs,
        Some(_) => {}
    };
    let (mut state, mut at) = (before, from);
    for e in events.iter().filter(|e| e.at >= from && e.at < to) {
        add(state, (e.at - at).num_milliseconds() as f64 / 1000.0);
        (state, at) = (Some(e.state.as_str()), e.at);
    }
    add(state, (to - at).num_milliseconds() as f64 / 1000.0);
    out
}

/// The count since `previous` on a counter that wraps at `max` or resets. A reading below the last
/// one is a wrap when `max` is known and the gap is plausible, otherwise a reset: what was counted
/// before the reset is lost, so only the new reading is sure.
pub fn counter_delta(previous: u64, now: u64, max: Option<u64>) -> u64 {
    if now >= previous {
        return now - previous;
    }
    match max {
        // Wrapped: it must have passed the top. Far from the top it was reset.
        Some(top) if previous > top / 2 => (top - previous) + now + 1,
        _ => now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    #[test]
    fn the_worked_example() {
        // An eight-hour shift less 47 minutes stopped; 1 minute each; 400 made, 380 good.
        let f = figures(&Input { planned_secs: 480.0 * 60.0, run_secs: 433.0 * 60.0, unknown_secs: 0.0, ideal_cycle_secs: 60.0, total_count: 400.0, good_count: 380.0 });
        assert!(close(f.availability, 433.0 / 480.0));
        assert!(close(f.performance, 400.0 / 433.0));
        assert!(close(f.quality, 0.95));
        assert!(close(f.oee, (433.0 / 480.0) * (400.0 / 433.0) * 0.95));
        assert!(close(f.coverage, 1.0));
    }

    #[test]
    fn nothing_to_divide_by_is_unknown_not_zero() {
        let f = figures(&Input::default());
        assert_eq!((f.availability, f.performance, f.quality, f.oee, f.coverage), (None, None, None, None, None));
        let idle = figures(&Input { planned_secs: 3600.0, ..Default::default() });
        assert!(close(idle.availability, 0.0), "planned and not running is a real zero");
        assert_eq!(idle.performance, None);
        assert_eq!(idle.quality, None);
        assert_eq!(idle.oee, None);
    }

    #[test]
    fn missing_telemetry_is_not_runtime() {
        let f = figures(&Input { planned_secs: 3600.0, run_secs: 1800.0, unknown_secs: 1800.0, ideal_cycle_secs: 1.0, total_count: 1800.0, good_count: 1800.0 });
        assert!(close(f.availability, 1.0), "of the half that was seen, all of it ran");
        assert!(close(f.coverage, 0.5), "and the figure says it covers half");
    }

    #[test]
    fn a_wrong_ideal_cycle_shows_rather_than_being_capped() {
        let f = figures(&Input { planned_secs: 100.0, run_secs: 100.0, unknown_secs: 0.0, ideal_cycle_secs: 2.0, total_count: 100.0, good_count: 100.0 });
        assert!(close(f.performance, 2.0));
    }

    #[test]
    fn time_is_counted_from_events_and_what_is_unseen_stays_unknown() {
        let at = |m: i64| DateTime::from_timestamp(m * 60, 0).unwrap();
        let e = |m: i64, s: &str| StateAt { at: at(m), state: s.into() };
        let events = [e(10, "Running"), e(30, "Stopped"), e(40, "Running"), e(50, "Planned")];
        let t = time_in_states(None, &events, at(0), at(60));
        assert_eq!(t, TimeInStates { run_secs: 30.0 * 60.0, planned_stop_secs: 10.0 * 60.0, unknown_secs: 10.0 * 60.0 });
        let t = time_in_states(Some("Running"), &[], at(0), at(60));
        assert_eq!(t.run_secs, 3600.0, "what held before the period holds until something changes");
        let t = time_in_states(None, &[e(90, "Running")], at(0), at(60));
        assert_eq!(t.unknown_secs, 3600.0, "nothing reported in the period is not the same as running");
    }

    #[test]
    fn counters_wrap_and_reset() {
        assert_eq!(counter_delta(10, 25, None), 15);
        assert_eq!(counter_delta(65_530, 4, Some(65_535)), 10, "wrapped");
        assert_eq!(counter_delta(120, 4, Some(65_535)), 4, "far from the top, so reset");
        assert_eq!(counter_delta(120, 4, None), 4, "with no top known, reset");
        assert_eq!(counter_delta(7, 7, None), 0);
    }
}
