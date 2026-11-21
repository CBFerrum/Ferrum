//! What agents may do without a person (gapclose/design/A2-A3-agents.md 2.2, 2.3): each operation is of some classes,
//! and the workspace and each agent decide, per class, whether an agent's call of that class runs, waits for a person, or
//! is refused. A decision only a person makes (`decides`) and whatever an app has not classified never run without one.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What an operation does, as far as an agent is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Transactional,
    External,
    Money,
    Destructive,
    Decides,
    Unclassified,
}

impl Class {
    pub const ALL: [Class; 6] = [Class::Transactional, Class::External, Class::Money, Class::Destructive, Class::Decides, Class::Unclassified];

    pub fn as_str(self) -> &'static str {
        match self {
            Class::Transactional => "transactional",
            Class::External => "external",
            Class::Money => "money",
            Class::Destructive => "destructive",
            Class::Decides => "decides",
            Class::Unclassified => "unclassified",
        }
    }

    /// The least a decision of this class may be: a decision only a person makes, and what has not been classified,
    /// always wait for a person (or are refused).
    pub fn floor(self) -> Decision {
        match self {
            Class::Decides | Class::Unclassified => Decision::NeedsPerson,
            _ => Decision::Allowed,
        }
    }
}

/// What happens to an agent's call of a class: the stricter the greater.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allowed,
    NeedsPerson,
    Forbidden,
}

/// A table of decisions by class.
pub type Table = BTreeMap<Class, Decision>;

/// The workspace's table when its settings say nothing: plain changes run, everything else waits for a person.
pub fn default_table() -> Table {
    Class::ALL.into_iter().map(|c| (c, if c == Class::Transactional { Decision::Allowed } else { Decision::NeedsPerson })).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stricter_is_greater_and_floors_hold() {
        assert!(Decision::Allowed < Decision::NeedsPerson && Decision::NeedsPerson < Decision::Forbidden);
        assert_eq!(Class::Decides.floor(), Decision::NeedsPerson);
        assert_eq!(Class::Unclassified.floor(), Decision::NeedsPerson);
        assert_eq!(Class::Money.floor(), Decision::Allowed);
        assert_eq!(serde_json::to_value(default_table()).unwrap()["money"], "needs_person");
    }
}
