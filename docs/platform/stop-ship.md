# Stop-ship conditions

The specification's list (26.4), and what is known about each. A condition that is tested in software is not cleared for a profile
until that profile's real environment has been tested too.

| Condition | What is known |
|---|---|
| Tenant escape | Row-level security on every table, two workspaces in every scenario, and a walk of every route. No independent test |
| Untrusted build reaching platform credentials | A hostile build reached nothing, on Docker and under gVisor (`FERRUM_CLOUD_CONTAINER_RUNTIME=runsc`, CI's `gvisor` job). **Open for shared hosts** until a runtime class is qualified on a real cluster with egress enforced |
| Production and preview secret crossover | Tokens and capabilities are per environment, and both are tested |
| Signature policy bypass | Releases, snapshots and artifacts are verified where they are used, and mutants cover it |
| Unauthorized private routes | Every way in to a members-only site was tried on a real node |
| Duplicate physical intent | A dedupe key, one live command, and a command not sent again when the gateway that sent it dies, tested on real gateway processes. **No equipment involved** |
| Stale authority dispatch | Epochs and generations, tested. **No equipment involved** |
| False domain completion | A charger's acceptance, a command's ending and a session's evidence are kept apart, tested |
| Unrecoverable data loss in the promised fault model | **Open.** A database, a forge, artifacts and the key come back together somewhere new, and one workspace alone, through real tools on a laptop. No fault model has been promised, and no zone or provider has failed |
| Unqualified critical-provider assumptions | **Open.** Nothing has been run against a real provider |
