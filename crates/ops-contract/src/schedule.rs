//! What to run next. Deterministic and explainable: the same inputs give the same plan, and
//! everything left waiting says why. Only feasible work is ever assigned, so this is what a solver
//! falls back to, and what an AI's suggestion is checked against.
//!
//! The rest of the industrial stack does not know about this function: it takes tasks and resources
//! and answers with assignments. A robot's route, traffic and safety are its fleet manager's.
//
// ponytail: greedy by effective priority, O(tasks × resources). It does not plan ahead (a slightly
// worse pairing now for a better one later) or minimize travel across the set; a solver such as
// CP-SAT does that, behind these same inputs, when a site's numbers show it is worth running one.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    /// Larger first.
    pub priority: i32,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub due_at: Option<DateTime<Utc>>,
    /// What a resource must be able to do: `tote`, `pallet`, `operator:forklift`.
    pub needs: String,
    /// The zone it starts in, and the station it ends at, when that matters.
    #[serde(default)]
    pub zone: Option<String>,
    #[serde(default)]
    pub station: Option<String>,
    #[serde(default)]
    pub load_kg: f64,
    /// Energy the task uses, as a share of a full battery.
    #[serde(default)]
    pub energy: f64,
    /// Tasks that must have finished first.
    #[serde(default)]
    pub after: Vec<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    pub id: Uuid,
    pub can: Vec<String>,
    #[serde(default)]
    pub payload_kg: Option<f64>,
    /// Share of a full battery left. None for what does not run on one.
    #[serde(default)]
    pub battery: Option<f64>,
    /// What must be left to reach a charger and recover: not merely above zero.
    #[serde(default)]
    pub reserve: f64,
    #[serde(default)]
    pub zone: Option<String>,
    /// Blocked, broken, under maintenance or already working.
    #[serde(default)]
    pub available: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    /// Tasks a station may have on their way or in hand.
    #[serde(default)]
    pub station_capacity: BTreeMap<String, u32>,
    /// Already on their way to or at each station.
    #[serde(default)]
    pub station_load: BTreeMap<String, u32>,
    /// Zones nothing may be sent to now: a blocked aisle, an unavailable lift.
    #[serde(default)]
    pub closed_zones: BTreeSet<String>,
    /// Priority gained per hour waited, so low-priority work cannot starve.
    #[serde(default)]
    pub ageing_per_hour: f64,
    /// At most this many tasks in progress across the site; 0 for no limit.
    #[serde(default)]
    pub max_in_progress: u32,
    #[serde(default)]
    pub in_progress: u32,
    #[serde(default)]
    pub done: BTreeSet<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    pub task: Uuid,
    pub resource: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Held {
    /// A task that must finish first has not.
    Waiting(Uuid),
    ZoneClosed(String),
    StationFull(String),
    WorkInProgressLimit,
    /// Nothing can do this: the resources it would need, and why each cannot.
    NoResource(String),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub assign: Vec<Assignment>,
    pub held: Vec<(Uuid, Held)>,
}

fn effective(t: &Task, now: DateTime<Utc>, ageing_per_hour: f64) -> f64 {
    let hours = (now - t.created_at).num_seconds().max(0) as f64 / 3600.0;
    t.priority as f64 + hours * ageing_per_hour
}

/// Why `r` cannot take `t`, or nothing when it can.
fn unfit(t: &Task, r: &Resource) -> Option<&'static str> {
    if !r.available {
        return Some("not available");
    }
    if !r.can.iter().any(|c| c == &t.needs) {
        return Some("cannot do this");
    }
    if r.payload_kg.is_some_and(|p| t.load_kg > p) {
        return Some("cannot carry it");
    }
    if r.battery.is_some_and(|b| b - t.energy < r.reserve) {
        return Some("would end below its reserve");
    }
    None
}

/// The next assignments at `now`. Order of tasks is by effective priority, then due date, then age
/// and id, so that nothing depends on the order they were given in.
pub fn plan(tasks: &[Task], resources: &[Resource], limits: &Limits, now: DateTime<Utc>) -> Plan {
    let mut order: Vec<&Task> = tasks.iter().collect();
    order.sort_by(|a, b| {
        effective(b, now, limits.ageing_per_hour)
            .total_cmp(&effective(a, now, limits.ageing_per_hour))
            .then(a.due_at.is_none().cmp(&b.due_at.is_none()))
            .then(a.due_at.cmp(&b.due_at))
            .then(a.created_at.cmp(&b.created_at))
            .then(a.id.cmp(&b.id))
    });
    let mut free: Vec<&Resource> = resources.iter().collect();
    free.sort_by_key(|r| r.id);
    let mut load = limits.station_load.clone();
    let mut in_progress = limits.in_progress;
    let mut out = Plan::default();
    for t in order {
        if let Some(first) = t.after.iter().find(|a| !limits.done.contains(a)) {
            out.held.push((t.id, Held::Waiting(*first)));
            continue;
        }
        if let Some(z) = t.zone.as_ref().filter(|z| limits.closed_zones.contains(*z)) {
            out.held.push((t.id, Held::ZoneClosed(z.clone())));
            continue;
        }
        if limits.max_in_progress > 0 && in_progress >= limits.max_in_progress {
            out.held.push((t.id, Held::WorkInProgressLimit));
            continue;
        }
        if let Some(s) = &t.station
            && let Some(cap) = limits.station_capacity.get(s)
            && load.get(s).copied().unwrap_or(0) >= *cap
        {
            out.held.push((t.id, Held::StationFull(s.clone())));
            continue;
        }
        // Prefer one already in the task's zone, then the most charge.
        let best = free
            .iter()
            .enumerate()
            .filter(|(_, r)| unfit(t, r).is_none())
            .max_by(|(_, a), (_, b)| {
                let here = |r: &Resource| r.zone.is_some() && r.zone == t.zone;
                here(a).cmp(&here(b)).then(a.battery.unwrap_or(1.0).total_cmp(&b.battery.unwrap_or(1.0))).then(b.id.cmp(&a.id))
            })
            .map(|(i, _)| i);
        match best {
            Some(i) => {
                let r = free.remove(i);
                out.assign.push(Assignment { task: t.id, resource: r.id });
                in_progress += 1;
                if let Some(s) = &t.station {
                    *load.entry(s.clone()).or_default() += 1;
                }
            }
            None => {
                let why: BTreeSet<&str> = resources.iter().filter_map(|r| unfit(t, r)).collect();
                let why = match (why.is_empty(), resources.is_empty()) {
                    (_, true) => "no resources".to_string(),
                    (true, _) => "all taken by more urgent work".to_string(),
                    _ => why.into_iter().collect::<Vec<_>>().join(", "),
                };
                out.held.push((t.id, Held::NoResource(why)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use proptest::prelude::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn task(n: u128, priority: i32, now: DateTime<Utc>) -> Task {
        Task { id: id(n), priority, created_at: now, due_at: None, needs: "tote".into(), zone: None, station: None, load_kg: 10.0, energy: 0.1, after: vec![] }
    }

    fn robot(n: u128) -> Resource {
        Resource { id: id(n), can: vec!["tote".into()], payload_kg: Some(50.0), battery: Some(0.9), reserve: 0.2, zone: None, available: true }
    }

    #[test]
    fn the_most_urgent_goes_first_and_the_rest_say_why() {
        let now = Utc::now();
        let p = plan(&[task(1, 1, now), task(2, 9, now)], &[robot(100)], &Limits::default(), now);
        assert_eq!(p.assign, vec![Assignment { task: id(2), resource: id(100) }]);
        assert_eq!(p.held, vec![(id(1), Held::NoResource("all taken by more urgent work".into()))]);
    }

    #[test]
    fn a_battery_is_judged_by_what_is_left_afterwards() {
        let now = Utc::now();
        let mut low = robot(100);
        low.battery = Some(0.25);
        let mut t = task(1, 1, now);
        t.energy = 0.1;
        let p = plan(&[t], &[low], &Limits::default(), now);
        assert!(p.assign.is_empty());
        assert_eq!(p.held, vec![(id(1), Held::NoResource("would end below its reserve".into()))]);
    }

    #[test]
    fn work_waits_for_what_comes_before_it() {
        let now = Utc::now();
        let mut then = task(2, 9, now);
        then.after = vec![id(1)];
        let p = plan(&[task(1, 1, now), then.clone()], &[robot(100), robot(101)], &Limits::default(), now);
        assert_eq!(p.assign.len(), 1);
        assert_eq!(p.held, vec![(id(2), Held::Waiting(id(1)))]);
        let done = Limits { done: BTreeSet::from([id(1)]), ..Default::default() };
        assert_eq!(plan(&[then], &[robot(100)], &done, now).assign.len(), 1);
    }

    #[test]
    fn stations_zones_and_work_in_progress_are_limits_not_hints() {
        let now = Utc::now();
        let mut a = task(1, 2, now);
        a.station = Some("PACK-1".into());
        let mut b = task(2, 1, now);
        b.station = Some("PACK-1".into());
        let limits = Limits { station_capacity: BTreeMap::from([("PACK-1".into(), 1)]), ..Default::default() };
        let p = plan(&[a, b], &[robot(100), robot(101)], &limits, now);
        assert_eq!(p.assign.len(), 1);
        assert_eq!(p.held, vec![(id(2), Held::StationFull("PACK-1".into()))]);

        let mut c = task(3, 1, now);
        c.zone = Some("A".into());
        let closed = Limits { closed_zones: BTreeSet::from(["A".into()]), ..Default::default() };
        assert_eq!(plan(&[c], &[robot(100)], &closed, now).held, vec![(id(3), Held::ZoneClosed("A".into()))]);

        let wip = Limits { max_in_progress: 2, in_progress: 1, ..Default::default() };
        let p = plan(&[task(4, 2, now), task(5, 1, now)], &[robot(100), robot(101)], &wip, now);
        assert_eq!(p.assign.len(), 1);
        assert_eq!(p.held, vec![(id(5), Held::WorkInProgressLimit)]);
    }

    #[test]
    fn waiting_long_enough_beats_priority() {
        let now = Utc::now();
        let mut old = task(1, 1, now - Duration::hours(10));
        old.created_at = now - Duration::hours(10);
        let limits = Limits { ageing_per_hour: 1.0, ..Default::default() };
        let p = plan(&[task(2, 5, now), old], &[robot(100)], &limits, now);
        assert_eq!(p.assign[0].task, id(1), "11 against 5: it cannot starve");
    }

    #[test]
    fn a_robot_in_the_zone_is_preferred_then_the_fullest() {
        let now = Utc::now();
        let mut t = task(1, 1, now);
        t.zone = Some("B".into());
        let mut far = robot(100);
        far.battery = Some(0.99);
        far.zone = Some("A".into());
        let mut near = robot(101);
        near.battery = Some(0.5);
        near.zone = Some("B".into());
        assert_eq!(plan(&[t], &[far, near], &Limits::default(), now).assign[0].resource, id(101));
    }

    proptest! {
        // Nothing is assigned twice, nothing infeasible is assigned, and every task is accounted for.
        #[test]
        fn a_plan_is_feasible_and_complete(
            tasks in proptest::collection::vec((0..10i32, 0.0..0.5f64, 0.0..80.0f64), 0..12),
            robots in proptest::collection::vec((0.0..1.0f64, proptest::bool::ANY), 0..6),
        ) {
            let now = Utc::now();
            let ts: Vec<Task> = tasks.iter().enumerate().map(|(i, (p, e, l))| {
                let mut t = task(i as u128 + 1, *p, now);
                t.energy = *e;
                t.load_kg = *l;
                t
            }).collect();
            let rs: Vec<Resource> = robots.iter().enumerate().map(|(i, (b, a))| {
                let mut r = robot(i as u128 + 100);
                r.battery = Some(*b);
                r.available = *a;
                r
            }).collect();
            let p = plan(&ts, &rs, &Limits::default(), now);
            prop_assert_eq!(p.assign.len() + p.held.len(), ts.len());
            let mut used = BTreeSet::new();
            for a in &p.assign {
                prop_assert!(used.insert(a.resource), "a resource was given two tasks");
                let t = ts.iter().find(|t| t.id == a.task).unwrap();
                let r = rs.iter().find(|r| r.id == a.resource).unwrap();
                prop_assert!(unfit(t, r).is_none());
            }
            // The same inputs, in another order, give the same plan.
            let mut shuffled = ts.clone();
            shuffled.reverse();
            let mut rs2 = rs.clone();
            rs2.reverse();
            prop_assert_eq!(plan(&shuffled, &rs2, &Limits::default(), now), p);
        }
    }
}
