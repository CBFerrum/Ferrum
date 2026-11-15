//! What the server's settings say (docs/plan/stateless.md, D2), read as the environment is read: `var("FERRUM_…")`
//! gives the environment's value, else the database's, else nothing (the reader's own default). The server resolves
//! them as it starts and again when a setting that applies at once changes; until it has, and for a name it does not
//! know, this is the environment itself.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Each known variable's value, `None` where nothing sets it.
static RESOLVED: RwLock<Option<Arc<HashMap<String, Option<String>>>>> = RwLock::new(None);

/// What `name` says, as `std::env::var(name).ok()` would say it.
pub fn var(name: &str) -> Option<String> {
    let resolved = RESOLVED.read().unwrap_or_else(|e| e.into_inner()).clone();
    match resolved.as_ref().and_then(|r| r.get(name)) {
        Some(value) => value.clone(),
        None => std::env::var(name).ok(),
    }
}

/// The server's resolved settings, by variable name.
pub fn set(resolved: HashMap<String, Option<String>>) {
    *RESOLVED.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(resolved));
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_known_name_says_what_was_resolved_and_an_unknown_one_the_environment() {
        super::set([("FERRUM_ENV_TEST_KNOWN".to_string(), Some("from the table".to_string())), ("FERRUM_ENV_TEST_UNSET".into(), None)].into());
        assert_eq!(super::var("FERRUM_ENV_TEST_KNOWN").as_deref(), Some("from the table"));
        assert_eq!(super::var("FERRUM_ENV_TEST_UNSET"), None, "nothing sets it: the reader's default");
        let dir = std::env::var("CARGO_MANIFEST_DIR").ok();
        assert!(dir.is_some() && super::var("CARGO_MANIFEST_DIR") == dir, "a name it does not know: the environment");
    }
}
