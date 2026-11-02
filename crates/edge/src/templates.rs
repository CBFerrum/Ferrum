//! Definitions to start from. A template is a beginning, and none of them has been tried on the
//! equipment it names: an address, a register map or a topic will want changing, and whether it
//! works on a given machine is what `docs/compatibility.md` records.

use serde::Serialize;

macro_rules! templates {
    ($($file:literal),* $(,)?) => {
        const FILES: &[&str] = &[$(include_str!(concat!("../templates/", $file))),*];
        #[cfg(test)]
        const NAMES: &[&str] = &[$($file),*];
    };
}

templates![
    "vda5050-3.yaml",
    "rosbridge.yaml",
    "open-rmf.yaml",
    "opentcs.yaml",
    "mir-rest.yaml",
    "ur-dashboard.yaml",
    "opcua-machine.yaml",
    "modbus-tcp-plc.yaml",
    "modbus-rtu-meters.yaml",
    "sparkplug-node.yaml",
    "mtconnect.yaml",
    "mqtt-json.yaml",
    "http-poll.yaml",
    "tcp-line.yaml",
    "binary-frames.yaml",
    "called-in.yaml",
    "webhook-events.yaml",
    "chirpstack-lorawan.yaml",
    "zigbee2mqtt.yaml",
    "external-driver.yaml",
];

#[derive(Debug, Serialize)]
pub struct Template {
    pub name: String,
    pub about: String,
    pub group: String,
    pub body: &'static str,
}

fn line<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|l| l.strip_prefix("# ")?.strip_prefix(key)?.strip_prefix(": "))
}

pub fn all() -> Vec<Template> {
    FILES
        .iter()
        .map(|body| Template {
            name: body.lines().find_map(|l| l.strip_prefix("name: ")).unwrap_or_default().to_string(),
            about: line(body, "about").unwrap_or_default().to_string(),
            group: line(body, "group").unwrap_or("Generic").to_string(),
            body,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrum_ops::contract::driver::{Decoded, Ending, parse};
    use serde_json::json;

    fn template(name: &str) -> ferrum_ops::contract::driver::Compiled {
        let t = all().into_iter().find(|t| t.name == name).unwrap_or_else(|| panic!("no {name}"));
        parse(t.body).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn metric(got: &[Decoded], name: &str) -> Option<f64> {
        got.iter().find_map(|d| if let Decoded::Metric { metric, value, .. } = d { (metric == name).then_some(*value) } else { None })
    }

    fn attribute(got: &[Decoded], name: &str) -> Option<serde_json::Value> {
        got.iter().find_map(|d| if let Decoded::Attribute { key, value, .. } = d { (key == name).then(|| value.clone()) } else { None })
    }

    fn state(got: &[Decoded]) -> Option<String> {
        got.iter().find_map(|d| if let Decoded::State { state, .. } = d { Some(state.clone()) } else { None })
    }

    #[test]
    fn every_template_parses_says_what_it_is_and_none_is_left_out() {
        let on_disk = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/templates")).unwrap().count();
        assert_eq!(NAMES.len(), on_disk, "a template in the folder is missing from the list");
        for t in all() {
            parse(t.body).unwrap_or_else(|e| panic!("{}: {e}", t.name));
            assert!(!t.name.is_empty() && !t.about.is_empty(), "{t:?}");
        }
    }

    /// A state as the VDA 5050 3.0.0 schema has it, which renamed what 2.x called `agvPosition`.
    #[test]
    fn vda5050_follows_a_vehicle_and_knows_when_an_order_is_over() {
        let d = template("vda5050-3");
        let body = json!({
            "headerId": 7, "timestamp": "2026-11-02T10:00:00.000Z", "version": "3.0.0", "manufacturer": "acme", "serialNumber": "AGV-7",
            "orderId": "o1", "orderUpdateId": 0, "lastNodeId": "n1", "lastNodeSequenceId": 2,
            "nodeStates": [{"nodeId": "n2", "sequenceId": 4, "released": true}], "edgeStates": [], "driving": true,
            "actionStates": [], "instantActionStates": [], "operatingMode": "AUTOMATIC", "errors": [{"errorType": "pathBlocked", "errorLevel": "WARNING"}],
            "powerSupply": {"stateOfCharge": 72.5, "charging": false},
            "safetyState": {"activeEmergencyStop": "NONE", "fieldViolation": false},
            "mobileRobotPosition": {"x": 1.5, "y": 2.5, "theta": 0.5, "mapId": "hall", "localized": true},
        })
        .to_string();
        let got = d.decode("uagv/v3/acme/AGV-7/state", body.as_bytes());
        assert_eq!(metric(&got, "battery.percent"), Some(72.5));
        assert_eq!(attribute(&got, "position.x"), Some(json!(1.5)));
        assert_eq!(attribute(&got, "position.map"), Some(json!("hall")));
        assert_eq!(state(&got), Some("Running".to_string()));
        assert!(got.iter().any(|g| matches!(g, Decoded::Event { device: Some(c), name, .. } if c == "AGV-7" && name == "pathBlocked")), "{got:?}");

        let status = |body: serde_json::Value| d.status("transport.move", "AGV-7", "uagv/v3/acme/AGV-7/state", body.to_string().as_bytes(), &["o1"]);
        assert_eq!(status(json!({"orderId": "o1", "errors": [], "nodeStates": [{"nodeId": "n"}]})), Some(Ending::Waiting));
        assert_eq!(status(json!({"orderId": "o1", "errors": [], "nodeStates": []})), Some(Ending::Done));
        assert_eq!(status(json!({"orderId": "o1", "errors": [{"errorLevel": "FATAL"}], "nodeStates": [{"nodeId": "n"}]})), Some(Ending::Failed));
        assert_eq!(status(json!({"orderId": "other", "errors": [], "nodeStates": []})), None);
    }

    #[test]
    fn open_rmf_reports_each_robot_of_a_fleet() {
        let d = template("open-rmf");
        let body = json!({"name": "tinyRobot", "robots": {"tiny_1": {"status": "working", "task_id": "t1", "battery": 0.5, "location": {"map": "L1", "x": 3.0, "y": 4.0, "yaw": 0.0}}}});
        let got = d.decode("/fleets/tinyRobot/state", body.to_string().as_bytes());
        assert!(got.iter().all(|g| matches!(g, Decoded::Metric { device: Some(c), .. } | Decoded::State { device: Some(c), .. } | Decoded::Attribute { device: Some(c), .. } if c == "tiny_1")), "{got:?}");
        assert_eq!(metric(&got, "battery.percent"), Some(50.0));
        assert_eq!(state(&got), Some("Running".to_string()));
        assert_eq!(d.status("transport.task", "x", "/tasks/t1/state", br#"{"status":"completed"}"#, &["t1"]), Some(Ending::Done));
        assert_eq!(d.status("transport.task", "x", "/tasks/t1/state", br#"{"status":"killed"}"#, &["t1"]), Some(Ending::Aborted));
        assert_eq!(d.status("transport.task", "x", "/tasks/t1/state", br#"{"status":"underway"}"#, &["t1"]), Some(Ending::Running));
    }

    #[test]
    fn opentcs_lists_vehicles_and_can_be_asked_about_an_order() {
        let d = template("opentcs");
        let got = d.decode("/vehicles", br#"[{"name": "V1", "energyLevel": 80, "state": "EXECUTING", "currentPosition": "P-3", "transportOrder": "o1"}]"#);
        assert_eq!(metric(&got, "battery.percent"), Some(80.0));
        assert_eq!(state(&got), Some("Running".to_string()));
        assert_eq!(attribute(&got, "position.point"), Some(json!("P-3")));
        assert_eq!(d.found("transport.move", "V1", "/transportOrders/c1", br#"{"state": "BEING_PROCESSED"}"#, &["c1"]), Some(Ending::Running));
        assert_eq!(d.status("transport.move", "V1", "/transportOrders/c1", br#"{"state": "WITHDRAWN"}"#, &["c1"]), Some(Ending::Aborted));
    }

    #[test]
    fn rosbridge_tells_topics_apart() {
        let d = template("rosbridge");
        let got = d.decode("", br#"{"op": "publish", "topic": "/battery_state", "msg": {"percentage": 0.42, "voltage": 24.1}}"#);
        assert_eq!(metric(&got, "battery.percent"), Some(42.0));
        let got = d.decode("", br#"{"op": "publish", "topic": "/amcl_pose", "msg": {"pose": {"pose": {"position": {"x": 1.0, "y": 2.0}}}}}"#);
        assert_eq!(attribute(&got, "position.y"), Some(json!(2.0)));
        assert_eq!(d.status("transport.move", "R", "", br#"{"op": "action_result", "id": "c1", "result": true}"#, &["c1"]), Some(Ending::Done));
        assert_eq!(d.status("transport.move", "R", "", br#"{"op": "action_result", "id": "other", "result": true}"#, &["c1"]), None);
    }

    #[test]
    fn a_robot_arm_by_its_dashboard_server() {
        let d = template("ur-dashboard");
        assert_eq!(attribute(&d.decode("", b"Robotmode: RUNNING"), "mode"), Some(json!("RUNNING")));
        let got = d.decode("", b"PLAYING pallet.urp");
        assert_eq!(state(&got), Some("Running".to_string()));
        assert_eq!(attribute(&got, "program"), Some(json!("pallet.urp")));
        assert_eq!(d.status("program.start", "A", "", b"STOPPED pallet.urp", &["c"]), Some(Ending::Done));
    }

    #[test]
    fn a_modbus_plc_by_its_registers() {
        let d = template("modbus-tcp-plc");
        // 21.5 C, 2.5 bar in word-swapped order, 70000 cycles, Running.
        let pressure = 2.5f32.to_be_bytes();
        let body = [0, 215, pressure[2], pressure[3], pressure[0], pressure[1], 0, 1, 0x11, 0x70, 0, 1];
        let got = d.decode("holding/0", &body);
        assert_eq!(metric(&got, "temp.c"), Some(21.5));
        assert_eq!(metric(&got, "pressure.bar"), Some(2.5));
        assert_eq!(metric(&got, "cycles.count"), Some(70000.0));
        assert_eq!(state(&got), Some("Running".to_string()));
    }
}
