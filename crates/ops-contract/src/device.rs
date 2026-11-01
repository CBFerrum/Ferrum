//! A device's lifecycle. A pump can be one asset with several sensors, and a gateway can stand for
//! many devices: the asset is the maintained business object, the device the authenticated
//! endpoint. Replacing a sensor must not erase the pump's history.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lifecycle {
    Imported,
    Claimed,
    Provisioned,
    Commissioned,
    Active,
    Maintenance,
    Quarantined,
    Retired,
}

impl Lifecycle {
    pub const ALL: [Lifecycle; 8] = [
        Lifecycle::Imported,
        Lifecycle::Claimed,
        Lifecycle::Provisioned,
        Lifecycle::Commissioned,
        Lifecycle::Active,
        Lifecycle::Maintenance,
        Lifecycle::Quarantined,
        Lifecycle::Retired,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Lifecycle::Imported => "Imported",
            Lifecycle::Claimed => "Claimed",
            Lifecycle::Provisioned => "Provisioned",
            Lifecycle::Commissioned => "Commissioned",
            Lifecycle::Active => "Active",
            Lifecycle::Maintenance => "Maintenance",
            Lifecycle::Quarantined => "Quarantined",
            Lifecycle::Retired => "Retired",
        }
    }

    pub fn parse(s: &str) -> Option<Lifecycle> {
        Lifecycle::ALL.into_iter().find(|v| v.as_str() == s)
    }

    /// May it move to `to`? Provisioned means a key was enrolled, so only that enrollment reaches it,
    /// and a quarantined device comes back only by enrolling a new key.
    pub fn can_move(self, to: Lifecycle) -> bool {
        use Lifecycle::*;
        match (self, to) {
            (Retired, _) => false,
            (_, Retired) => true,
            (Imported, Claimed) | (Claimed, Provisioned) | (Quarantined, Provisioned) => true,
            (Provisioned, Commissioned) | (Commissioned, Active) => true,
            (Active, Maintenance) | (Maintenance, Active) => true,
            (Provisioned | Commissioned | Active | Maintenance, Quarantined) => true,
            _ => false,
        }
    }

    /// May it authenticate at all? A quarantined device may still report, so that what it has is
    /// not lost, but it receives no work.
    pub fn may_connect(self) -> bool {
        matches!(self, Lifecycle::Provisioned | Lifecycle::Commissioned | Lifecycle::Active | Lifecycle::Maintenance | Lifecycle::Quarantined)
    }

    /// May work be sent to it?
    pub fn takes_commands(self) -> bool {
        self == Lifecycle::Active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_is_the_end() {
        for to in Lifecycle::ALL {
            assert!(!Lifecycle::Retired.can_move(to));
        }
    }

    #[test]
    fn nothing_skips_commissioning() {
        for from in Lifecycle::ALL {
            if from != Lifecycle::Commissioned && from != Lifecycle::Maintenance {
                assert!(!from.can_move(Lifecycle::Active), "{from:?}");
            }
        }
    }

    #[test]
    fn quarantine_ends_only_with_a_new_key() {
        for to in Lifecycle::ALL {
            let allowed = matches!(to, Lifecycle::Provisioned | Lifecycle::Retired);
            assert_eq!(Lifecycle::Quarantined.can_move(to), allowed, "{to:?}");
        }
    }

    #[test]
    fn only_an_active_device_takes_work() {
        for s in Lifecycle::ALL {
            assert_eq!(s.takes_commands(), s == Lifecycle::Active);
        }
        assert!(Lifecycle::Quarantined.may_connect() && !Lifecycle::Retired.may_connect() && !Lifecycle::Imported.may_connect());
    }
}
