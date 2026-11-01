//! Work a person does at the console: a pick, a move, a check. The journal is the equipment here, so
//! this works with no link at all, and a person's finding is the evidence.

use crate::adapter::{Adapter, Dispatched, Failure, Observation};
use crate::journal::Journal;
use async_trait::async_trait;
use ferrum_ops_contract::{Command, sync::SiteDevice};
use serde_json::json;
use std::sync::Arc;

pub struct OperatorTasks {
    journal: Arc<Journal>,
}

impl OperatorTasks {
    pub fn new(journal: Arc<Journal>) -> OperatorTasks {
        OperatorTasks { journal }
    }
}

fn gone(e: crate::journal::Error) -> Failure {
    Failure::NotSent(e.to_string())
}

#[async_trait]
impl Adapter for OperatorTasks {
    async fn dispatch(&self, c: &Command, _device: &SiteDevice) -> Result<Dispatched, Failure> {
        self.journal.open_task(c.id).map_err(gone)?;
        Ok(Dispatched::Accepted { external: None })
    }

    async fn observe(&self, c: &Command, _external: Option<&str>) -> Result<Observation, Failure> {
        let task = self.journal.task(c.id).map_err(gone)?;
        Ok(match task {
            None => Observation::NotFound,
            Some((state, evidence, by)) => {
                let evidence = json!({"by": by, "found": evidence});
                match state.as_str() {
                    "Waiting" => Observation::Waiting,
                    "Started" => Observation::Running,
                    "Done" => Observation::Completed(evidence),
                    "Aborted" => Observation::Aborted(evidence),
                    _ => Observation::Failed(evidence),
                }
            }
        })
    }

    async fn find(&self, c: &Command) -> Result<Option<Observation>, Failure> {
        // The record is in this journal, written before the answer: it can always say.
        self.observe(c, None).await.map(Some)
    }

    async fn cancel(&self, c: &Command, _external: Option<&str>) -> Result<(), Failure> {
        // Work someone has started is theirs to stop: it stays shown, marked, until they say.
        if let Some((state, ..)) = self.journal.task(c.id).map_err(gone)?
            && state == "Waiting"
        {
            self.journal.set_task(c.id, "Aborted", None, "cancelled before it started").map_err(gone)?;
        }
        Ok(())
    }
}
