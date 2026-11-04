//! An industrial solution in an app's bundle (FERRUM-IND-018): the equipment it works with and how
//! a site proves it before go-live. The manifest's `industrial` section names:
//!
//! * `connectors`: the schemes the site runtime must open for it (`modbus+tcp`, `opcua`,
//!   `listen+ocpp`, as Edge's contract names them), each with a version requirement of the site
//!   runtime's adapter (`*` when any);
//! * `device_profiles`: files under `industrial/profiles/`, one equipment type each (vendor,
//!   model, the scheme it speaks, its points and commands);
//! * `acceptance`: files under `industrial/acceptance/`, procedures a site runs at commissioning,
//!   ordered steps that read a point within limits, send a command, or ask a person to check.
//!
//! The bundle is refused when a file is missing or malformed, when a profile speaks a scheme the
//! section does not declare, or when a step names a profile, point or command the bundle does not
//! define. Which schemes a site can open is the site runtime's to say when it imports a solution:
//! this checks their names only.

use crate::AppError;
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const PROFILES_DIR: &str = "industrial/profiles/";
pub const ACCEPTANCE_DIR: &str = "industrial/acceptance/";

/// The manifest's `industrial` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndustrialSection {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connectors: Vec<ConnectorReq>,
    /// Paths of device profiles in the bundle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub device_profiles: Vec<String>,
    /// Paths of acceptance procedures in the bundle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorReq {
    pub scheme: String,
    #[serde(default = "any_version")]
    pub version: VersionReq,
}

fn any_version() -> VersionReq {
    VersionReq::STAR
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PointType {
    Bool,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Int64,
    Float32,
    Float64,
    String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    #[default]
    Read,
    ReadWrite,
}

/// A value a device reports, or takes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub name: String,
    /// Where the device holds it, as its scheme writes addresses (a register, a node id, an OBIS code).
    pub address: String,
    #[serde(rename = "type")]
    pub kind: PointType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Multiplies the raw value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(default)]
    pub access: Access,
}

/// Something a device is told to do: by writing a point, or as the scheme's own operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One type of equipment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceProfile {
    pub id: String,
    pub vendor: String,
    pub model: String,
    pub scheme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub points: Vec<Point>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepKind {
    /// Read a point and compare it with `expect`.
    Read,
    /// Send a command; `expect` on a point, when given, is checked after.
    Command,
    /// A person checks something and says so.
    Manual,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    pub kind: StepKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expect>,
    /// What a person is asked to do or look at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
}

/// A procedure a site runs at commissioning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptancePackage {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub steps: Vec<Step>,
}

/// What a bundle's industrial section holds, read and checked.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Solution {
    pub connectors: Vec<ConnectorReq>,
    pub profiles: Vec<DeviceProfile>,
    pub acceptance: Vec<AcceptancePackage>,
}

fn bad<T>(m: impl Into<String>) -> Result<T, AppError> {
    Err(AppError::Bundle(format!("industrial: {}", m.into())))
}

/// `modbus+tcp`, `opcua`, `listen+ocpp`: lower-case letters, digits and `+`.
fn is_scheme(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '+') && !s.starts_with('+') && !s.ends_with('+')
}

/// An id: `[a-z][a-z0-9_-]{0,63}`.
fn is_id(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase()) && s.len() <= 64 && c.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn read<T: for<'de> Deserialize<'de>>(files: &BTreeMap<String, Vec<u8>>, dir: &str, path: &str) -> Result<T, AppError> {
    if !path.starts_with(dir) || path[dir.len()..].contains('/') || !path.ends_with(".yaml") {
        return bad(format!("{path} must be a .yaml file directly under {dir}"));
    }
    let Some(data) = files.get(path) else { return bad(format!("app.yaml names {path}, which is not in the bundle")) };
    let text = std::str::from_utf8(data).map_err(|_| AppError::Bundle(format!("industrial: {path} is not UTF-8 text")))?;
    serde_yaml::from_str(text).map_err(|e| AppError::Bundle(format!("industrial: {path}: {e}")))
}

/// Reads and checks a bundle's industrial section.
pub fn check(section: &IndustrialSection, files: &BTreeMap<String, Vec<u8>>) -> Result<Solution, AppError> {
    let mut schemes = BTreeSet::new();
    for c in &section.connectors {
        if !is_scheme(&c.scheme) {
            return bad(format!("connector scheme {:?} is not a scheme's name (modbus+tcp, opcua, listen+ocpp)", c.scheme));
        }
        if !schemes.insert(c.scheme.as_str()) {
            return bad(format!("connector {} is named twice", c.scheme));
        }
    }
    for listed in [&section.device_profiles, &section.acceptance] {
        let unique: BTreeSet<&String> = listed.iter().collect();
        if unique.len() != listed.len() {
            return bad("a file is named twice");
        }
    }
    let shipped = |dir: &str| files.keys().filter(|p| p.starts_with(dir)).cloned().collect::<BTreeSet<_>>();
    for (dir, listed) in [(PROFILES_DIR, &section.device_profiles), (ACCEPTANCE_DIR, &section.acceptance)] {
        if let Some(stray) = shipped(dir).into_iter().find(|p| !listed.contains(p)) {
            return bad(format!("{stray} is in the bundle but not named in app.yaml (industrial)"));
        }
    }
    let mut profiles: Vec<DeviceProfile> = Vec::new();
    for path in &section.device_profiles {
        let p: DeviceProfile = read(files, PROFILES_DIR, path)?;
        let at = |m: String| bad::<()>(format!("{path}: {m}"));
        if !is_id(&p.id) {
            at(format!("profile id {:?} must be lower case letters, digits, - and _", p.id))?;
        }
        if profiles.iter().any(|q| q.id == p.id) {
            at(format!("profile {} is defined twice", p.id))?;
        }
        if !schemes.contains(p.scheme.as_str()) {
            at(format!("it speaks {}, which industrial.connectors does not name", p.scheme))?;
        }
        if p.points.is_empty() {
            at("a profile needs points".into())?;
        }
        let mut names = BTreeSet::new();
        for pt in &p.points {
            if !is_id(&pt.name) || pt.address.trim().is_empty() || !names.insert(pt.name.as_str()) {
                at(format!("point {:?} needs a unique lower-case name and an address", pt.name))?;
            }
            if pt.scale.is_some_and(|s| !s.is_finite() || s == 0.0) {
                at(format!("point {}: a scale is a number other than 0", pt.name))?;
            }
        }
        let mut commands = BTreeSet::new();
        for c in &p.commands {
            if !is_id(&c.name) || !commands.insert(c.name.as_str()) {
                at(format!("command {:?} needs a unique lower-case name", c.name))?;
            }
            if let Some(target) = &c.point {
                match p.points.iter().find(|pt| &pt.name == target) {
                    Some(pt) if pt.access == Access::ReadWrite => {}
                    Some(_) => at(format!("command {} writes {target}, which is read only", c.name))?,
                    None => at(format!("command {} writes {target}, which the profile does not have", c.name))?,
                }
            }
        }
        profiles.push(p);
    }
    let mut acceptance: Vec<AcceptancePackage> = Vec::new();
    for path in &section.acceptance {
        let a: AcceptancePackage = read(files, ACCEPTANCE_DIR, path)?;
        let at = |m: String| bad::<()>(format!("{path}: {m}"));
        if !is_id(&a.id) || acceptance.iter().any(|b| b.id == a.id) {
            at(format!("procedure id {:?} must be unique, lower case letters, digits, - and _", a.id))?;
        }
        if a.steps.is_empty() {
            at("a procedure needs steps".into())?;
        }
        let mut ids = BTreeSet::new();
        for s in &a.steps {
            let step = |m: &str| bad::<()>(format!("{path}: step {}: {m}", s.id));
            if !is_id(&s.id) || !ids.insert(s.id.as_str()) {
                at(format!("step id {:?} must be unique, lower case letters, digits, - and _", s.id))?;
            }
            if let Some(e) = &s.expect
                && let (Some(lo), Some(hi)) = (e.min, e.max)
                && lo > hi
            {
                step("its minimum is above its maximum")?;
            }
            if s.kind == StepKind::Manual {
                if s.instruction.as_deref().is_none_or(|i| i.trim().is_empty()) {
                    step("a manual step says what to check (instruction)")?;
                }
                continue;
            }
            let Some(profile) = s.profile.as_deref().and_then(|id| profiles.iter().find(|p| p.id == id)) else {
                step("names no profile of the bundle")?;
                continue;
            };
            let point = |name: &str| profile.points.iter().find(|p| p.name == name);
            match s.kind {
                StepKind::Read => {
                    if s.point.as_deref().and_then(point).is_none() {
                        step("reads no point of its profile")?;
                    }
                    if s.expect.is_none() {
                        step("a read says what it expects")?;
                    }
                }
                StepKind::Command => {
                    if !s.command.as_deref().is_some_and(|c| profile.commands.iter().any(|k| k.name == c)) {
                        step("sends no command of its profile")?;
                    }
                    if s.point.as_deref().is_some_and(|p| point(p).is_none()) {
                        step("checks a point its profile does not have")?;
                    }
                }
                StepKind::Manual => {}
            }
        }
        acceptance.push(a);
    }
    Ok(Solution { connectors: section.connectors.clone(), profiles, acceptance })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = "id: line-meter
vendor: Acme
model: PM-200
scheme: modbus+tcp
points:
  - {name: voltage, address: \"30001\", type: float32, unit: V}
  - {name: relay, address: \"00001\", type: bool, access: read_write}
commands:
  - {name: trip, point: relay}
";

    const PROCEDURE: &str = "id: meter-sat
title: Meter site acceptance
steps:
  - {id: voltage, kind: read, profile: line-meter, point: voltage, expect: {min: 207, max: 253}}
  - {id: trip, kind: command, profile: line-meter, command: trip, point: relay, expect: {equals: false}}
  - {id: seal, kind: manual, instruction: The meter's seal is intact}
";

    fn files(entries: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
        entries.iter().map(|(p, t)| (p.to_string(), t.as_bytes().to_vec())).collect()
    }

    fn section() -> IndustrialSection {
        IndustrialSection {
            connectors: vec![ConnectorReq { scheme: "modbus+tcp".into(), version: VersionReq::parse(">=0.3").unwrap() }],
            device_profiles: vec!["industrial/profiles/meter.yaml".into()],
            acceptance: vec!["industrial/acceptance/sat.yaml".into()],
        }
    }

    fn with(profile: &str, procedure: &str) -> Result<Solution, AppError> {
        check(&section(), &files(&[("industrial/profiles/meter.yaml", profile), ("industrial/acceptance/sat.yaml", procedure)]))
    }

    fn refused(r: Result<Solution, AppError>, needle: &str) {
        match r {
            Err(e) => assert!(e.to_string().contains(needle), "{e}"),
            Ok(_) => panic!("expected a refusal about {needle}"),
        }
    }

    #[test]
    fn a_solution_reads_whole() {
        let s = with(PROFILE, PROCEDURE).unwrap();
        assert_eq!((s.profiles[0].points.len(), s.profiles[0].commands[0].point.as_deref()), (2, Some("relay")));
        assert_eq!(s.acceptance[0].steps.iter().map(|s| s.kind).collect::<Vec<_>>(), vec![StepKind::Read, StepKind::Command, StepKind::Manual]);
    }

    #[test]
    fn what_cannot_run_at_a_site_is_refused() {
        refused(with(&PROFILE.replace("modbus+tcp", "opcua"), PROCEDURE), "does not name");
        refused(with(&PROFILE.replace("access: read_write", "access: read"), PROCEDURE), "read only");
        refused(with(&PROFILE.replace("point: relay}", "point: breaker}"), PROCEDURE), "does not have");
        refused(with(&PROFILE.replace("unit: V", "unit: V, colour: red"), PROCEDURE), "unknown field");
        refused(with(PROFILE, &PROCEDURE.replace("point: voltage", "point: current")), "reads no point");
        refused(with(PROFILE, &PROCEDURE.replace("profile: line-meter, point: voltage", "profile: pump, point: voltage")), "names no profile");
        refused(with(PROFILE, &PROCEDURE.replace("command: trip", "command: close")), "sends no command");
        refused(with(PROFILE, &PROCEDURE.replace("min: 207, max: 253", "min: 253, max: 207")), "above its maximum");
        refused(with(PROFILE, &PROCEDURE.replace("instruction: The meter's seal is intact", "instruction: \" \"")), "manual step");
        refused(with(PROFILE, &PROCEDURE.replace(", expect: {min: 207, max: 253}", "")), "expects");
        refused(with(PROFILE, &PROCEDURE.replace("id: trip,", "id: voltage,")), "unique");
        refused(check(&section(), &files(&[("industrial/profiles/meter.yaml", PROFILE)])), "not in the bundle");
        let stray =
            files(&[("industrial/profiles/meter.yaml", PROFILE), ("industrial/acceptance/sat.yaml", PROCEDURE), ("industrial/profiles/old.yaml", PROFILE)]);
        refused(check(&section(), &stray), "not named");
        let mut bad_scheme = section();
        bad_scheme.connectors[0].scheme = "Modbus TCP".into();
        refused(check(&bad_scheme, &files(&[])), "scheme's name");
    }

    #[test]
    fn a_bundle_carries_its_solution() {
        let mut f = crate::testutil::meta_app();
        let manifest = String::from_utf8(f["app.yaml"].clone()).unwrap()
            + "industrial:\n  connectors: [{scheme: modbus+tcp, version: \">=0.3\"}]\n  device_profiles: [industrial/profiles/meter.yaml]\n  acceptance: [industrial/acceptance/sat.yaml]\n";
        f.insert("app.yaml".into(), manifest.into_bytes());
        f.insert("industrial/profiles/meter.yaml".into(), PROFILE.as_bytes().to_vec());
        f.insert("industrial/acceptance/sat.yaml".into(), PROCEDURE.as_bytes().to_vec());
        let b = crate::Bundle::from_files(f.clone()).expect("a bundle with a solution");
        let s = b.industrial().unwrap().expect("its solution");
        assert_eq!((s.connectors[0].scheme.as_str(), s.profiles[0].id.as_str(), s.acceptance[0].id.as_str()), ("modbus+tcp", "line-meter", "meter-sat"));
        let mut without = crate::testutil::meta_app();
        without.insert("industrial/profiles/meter.yaml".into(), PROFILE.as_bytes().to_vec());
        let e = crate::Bundle::from_files(without).unwrap_err().to_string();
        assert!(e.contains("no industrial section"), "{e}");
        f.insert("industrial/profiles/nested/x.yaml".into(), PROFILE.as_bytes().to_vec());
        assert!(crate::Bundle::from_files(f).unwrap_err().to_string().contains("unexpected file"), "only one level");
    }
}
