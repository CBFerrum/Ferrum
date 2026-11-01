//! Measurements are not business records: no document lifecycle, and nothing here may decide a
//! stock movement. They keep what is needed to know how far to trust them: when the source took
//! them, when they were received, which boot of which device, in what order, and in what quality.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quality {
    Good,
    /// Usable, with a doubt: a repeated value, a value near its limits.
    Uncertain,
    /// Out of range, or the source said so. Kept, and never used to decide anything.
    Bad,
}

impl Quality {
    pub fn code(self) -> i16 {
        match self {
            Quality::Good => 0,
            Quality::Uncertain => 1,
            Quality::Bad => 2,
        }
    }
}

/// What may be given up first when space runs out, last to first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Class {
    /// A plain sample: the next one replaces it.
    Sample,
    /// A change of state, which a later sample does not imply.
    State,
    /// Someone has to know.
    Alarm,
}

impl Class {
    pub fn code(self) -> i16 {
        match self {
            Class::Sample => 0,
            Class::State => 1,
            Class::Alarm => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    pub device_id: Uuid,
    pub metric: String,
    pub value: f64,
    #[serde(default)]
    pub unit: Option<String>,
    /// The source's clock. It may be wrong: see [`Measurement::check`].
    pub source_time: DateTime<Utc>,
    /// A device that restarts counts from zero again. Boot and sequence together are unique, so an
    /// old replay cannot pass for what is happening now.
    pub boot: Uuid,
    pub seq: i64,
    pub quality: Quality,
    pub class: Class,
}

/// How far ahead of the receiver a source's clock may be before its time is not believed.
pub const MAX_FUTURE_SKEW_SECS: i64 = 300;

impl Measurement {
    /// Refuses what cannot be stored honestly. A wrong clock is not refused: the measurement is
    /// kept, marked bad, with the receipt time beside it.
    pub fn check(&mut self, received: DateTime<Utc>) -> Result<(), &'static str> {
        if !self.value.is_finite() {
            return Err("the value is not a number");
        }
        if self.metric.is_empty() || self.metric.len() > 120 || !self.metric.chars().all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c)) {
            return Err("the metric's name is not valid");
        }
        if self.seq < 0 {
            return Err("the sequence is negative");
        }
        if self.source_time > received + Duration::seconds(MAX_FUTURE_SKEW_SECS) {
            self.quality = Quality::Bad;
        }
        Ok(())
    }
}

/// What a metric may read, for a site to judge its sensors by. A sensor that keeps repeating one
/// value exactly is stuck, or at least not to be trusted: real signals have noise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Limits {
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// Identical readings in a row after which the value is doubted; 0 turns this off, for values
    /// that legitimately stand still.
    #[serde(default)]
    pub stuck_after: u32,
}

impl Limits {
    /// The quality of `value`, the `repeats`-th identical reading in a row.
    pub fn judge(&self, value: f64, repeats: u32) -> Quality {
        if !value.is_finite() || self.min.is_some_and(|m| value < m) || self.max.is_some_and(|m| value > m) {
            return Quality::Bad;
        }
        if self.stuck_after > 0 && repeats >= self.stuck_after {
            return Quality::Uncertain;
        }
        Quality::Good
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(value: f64, source_time: DateTime<Utc>) -> Measurement {
        Measurement {
            device_id: Uuid::now_v7(),
            metric: "vibration.rms".into(),
            value,
            unit: Some("mm/s".into()),
            source_time,
            boot: Uuid::now_v7(),
            seq: 1,
            quality: Quality::Good,
            class: Class::Sample,
        }
    }

    #[test]
    fn a_clock_in_the_future_marks_the_value_bad_and_keeps_it() {
        let now = Utc::now();
        let mut m = sample(1.0, now + Duration::hours(2));
        assert_eq!(m.check(now), Ok(()));
        assert_eq!(m.quality, Quality::Bad);
        let mut fine = sample(1.0, now + Duration::seconds(60));
        fine.check(now).unwrap();
        assert_eq!(fine.quality, Quality::Good, "ordinary drift");
    }

    #[test]
    fn a_value_that_is_not_a_number_is_refused() {
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(sample(v, Utc::now()).check(Utc::now()).is_err());
        }
    }

    #[test]
    fn out_of_range_is_bad_and_a_stuck_sensor_is_doubted() {
        let limits = Limits { min: Some(0.0), max: Some(100.0), stuck_after: 30 };
        assert_eq!(limits.judge(50.0, 1), Quality::Good);
        assert_eq!(limits.judge(-1.0, 1), Quality::Bad);
        assert_eq!(limits.judge(101.0, 1), Quality::Bad);
        assert_eq!(limits.judge(50.0, 29), Quality::Good);
        assert_eq!(limits.judge(50.0, 30), Quality::Uncertain);
        assert_eq!(limits.judge(f64::NAN, 1), Quality::Bad);
    }
}
