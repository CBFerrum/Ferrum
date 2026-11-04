#!/usr/bin/env python3
"""Deliberately breaks invariants and checks that the tests notice.

A green suite proves little if it stays green with the invariant gone. Each mutant below removes one
guarantee; its test must then fail. A mutant that survives means that test proves nothing, and this
script fails. Needs the test databases, as `cargo test` does. It restores every file it touches, even
when interrupted, and refuses to start on a dirty file.

    scripts/mutants.py            # all of them, 10-20 minutes
    scripts/mutants.py hook       # those whose name contains `hook`
    scripts/mutants.py --check    # only that each still finds its text, which CI does on every push
"""

import os
import signal
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PLUGINS = ("-p", "ferrum-plugins", "--test", "after_commit_it")
EVENTS = ("-p", "ferrum-kernel", "--test", "events_it")
LIFECYCLE = ("-p", "ferrum-kernel", "--test", "lifecycle_it")
DEADLINE = ("-p", "ferrum-plugins", "--lib", "runtime::tests")
SCIM_UNIT = ("-p", "ferrum-kernel", "--lib", "scim::tests")
RULES = ("-p", "ferrum-kernel", "--test", "rules_it")
SECRETS = ("-p", "ferrum-kernel", "--test", "secret_it")
PRIVACY = ("-p", "ferrum-kernel", "--test", "privacy_it")
JOBS = ("-p", "ferrum-kernel", "--test", "jobs_it")
HANDOVER = ("-p", "ferrum-kernel", "--test", "handover_it")
KERNEL = ("-p", "ferrum-kernel", "--test", "kernel_it")
SCAN = ("-p", "ferrum-kernel", "--test", "scan_it")
SCAN_UNIT = ("-p", "ferrum-kernel", "--lib", "scan::tests")
CUSTOMIZE = ("-p", "ferrum-kernel", "--test", "customize_it")
HOOKS_UNIT = ("-p", "ferrum-kernel", "--lib", "hooks::tests")
CLOSURE = ("-p", "ferrum-kernel", "--test", "closure_it")
SSO = ("-p", "ferrum-server", "--test", "sso_it")
SSO_UNIT = ("-p", "ferrum-kernel", "--lib", "sso::tests")
# SAML needs xmlsec1 to build (docs/operations.md, Single sign-on (SAML 2.0)).
SAML_UNIT = ("-p", "ferrum-saml", "--features", "xmlsec", "--lib")
SAML = ("-p", "ferrum-kernel", "--features", "saml", "--test", "saml_it")
MERGE = ("-p", "ferrum-kernel", "--test", "merge_it")
RETENTION = ("-p", "ferrum-kernel", "--test", "retention_it")
DOCTOR = ("-p", "ferrum-kernel", "--test", "doctor_it")
CONNECT = ("-p", "ferrum-kernel", "--test", "connect_it")
COLLAB = ("-p", "ferrum-kernel", "--test", "collab_it")
HOST_UNIT = ("-p", "ferrum-plugins", "--lib", "host::tests")
SOCKETS_UNIT = ("-p", "ferrum-plugins", "--lib", "sockets::tests")
EGRESS = ("-p", "ferrum-server", "--test", "assistant_egress_it")
SHARES_UNIT = ("-p", "ferrum-server", "--lib", "state::tests")
EXTENSIONS = ("-p", "ferrum-server", "--test", "extensions_it")
SUPPORT = ("-p", "ferrum-kernel", "--test", "support_it")
CONTRACT = ("-p", "ferrum-ops-contract", "--lib")
OPS = ("-p", "ferrum-ops", "--test", "ops_it")
RECOVERY = ("-p", "ferrum-kernel", "--test", "recovery_it")
# Needs OpenBao at FERRUM_KMS_TEST_TRANSIT (crates/kernel/tests/kms_it.rs).
KMS = ("-p", "ferrum-kernel", "--test", "kms_it")
SAGA = ("-p", "ferrum-kernel", "--test", "saga_it")
SIGNUP = ("-p", "ferrum-kernel", "--test", "signup_it")
# Generous enough to compile and run; passing it means the mutant hung the test.
TIMEOUT = 600

# name, file, text to replace, what breaks it, test
MUTANTS = [
    # An app built in elsewhere.
    ("extension-hooks-for-everyone", "server/src/packages.rs", "                hooks.scoped(&packages, |h| (app.register)(h));", "                (app.register)(hooks);", EXTENSIONS),
    ("extension-tasks-left-out", "server/src/app.rs", "            (app.tasks)(tasks);", "            let _ = app;", EXTENSIONS),
    # A workspace's share of a replica.
    ("share-is-no-limit", "server/src/state.rs", "        if self.each == 0 || place.0.get().is_some() {\n            return Ok(());", "        if true {\n            return Ok(());", SHARES_UNIT),
    ("share-is-everyones", "server/src/state.rs", "            held.entry(tenant).or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(self.each))).clone()", "            held.entry(uuid::Uuid::nil()).or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(self.each))).clone()", SHARES_UNIT),
    ("share-hides-its-wait", "server/src/state.rs", "                let _ = place.0.set((turn, asked.elapsed()));", "                let _ = place.0.set((turn, Duration::ZERO));", SHARES_UNIT),
    ("extract-any-copy-will-do", "crates/kernel/src/recovery.rs", "        if !differ.is_empty() {\n", "        if false {\n", RECOVERY),
    ("extract-onto-itself", "crates/kernel/src/recovery.rs", "        if which(&source).await? == which(&self.db).await? {\n", "        if false {\n", RECOVERY),
    ("extract-without-looking", "crates/kernel/src/recovery.rs", "        if !apply {\n", "        if false {\n", RECOVERY),
    ("extract-references-unchecked", "crates/kernel/src/recovery.rs", "        if !broken.is_empty() {\n", "        if false {\n", RECOVERY),
    ("extract-old-password-returns", "crates/kernel/src/recovery.rs", "password_hash = p.password_hash, ", "", RECOVERY),
    ("extract-leaver-returns", "crates/kernel/src/recovery.rs", ", is_active = p.is_active", "", RECOVERY),
    ("extract-sessions-survive", "crates/kernel/src/recovery.rs", 'sqlx::query("update app_user set token_version = $2 where tenant_id = $1").bind(tenant).bind(version).execute(&mut *tx).await?;\n', "", RECOVERY),
    ("extract-keys-survive", "crates/kernel/src/recovery.rs", 'sqlx::query("update api_key set revoked_at = now() where tenant_id = $1 and revoked_at is null").bind(tenant).execute(&mut *tx).await?;\n', "", RECOVERY),
    ("extract-waiting-work-runs", "crates/kernel/src/recovery.rs", "        self.set_aside(&mut tx, Some(tenant)).await?;\n", "", RECOVERY),
    ("extract-sets-aside-everyone", "crates/kernel/src/recovery.rs", "        self.set_aside(&mut tx, Some(tenant)).await?;\n", "        self.set_aside(&mut tx, None).await?;\n", RECOVERY),
    ("extract-stays-suspended", "crates/kernel/src/recovery.rs", '        sqlx::query("update tenant set status = $2 where id = $1").bind(tenant).bind(&was).execute(&mut *tx).await?;\n', "", RECOVERY),
    ("closure-deletes-before-its-date", "crates/kernel/src/recovery.rs", "        if retain_until >= chrono::Utc::now().date_naive() {\n", "        if false {\n", CLOSURE),
    ("closure-deletes-what-is-held-elsewhere", "crates/kernel/src/recovery.rs", "        if !outside.is_empty() {\n", "        if false {\n", CLOSURE),
    ("closure-rows-go-without-their-files", "crates/kernel/src/recovery.rs", "        if !failed.is_empty() {\n", "        if false {\n", CLOSURE),
    ("closure-misses-other-references", "crates/kernel/src/recovery.rs", "        columns.extend(others);\n", "", CLOSURE),
    ("closure-closed-is-writable", "crates/kernel/src/auth.rs", "t.account_state in ('restricted', 'closed')", "t.account_state in ('restricted')", CLOSURE),
    ("ops-a-moved-device-starts-twice", "crates/ops/src/commands.rs", "and taken_by = $3 \\\n         and (state in ('Accepted', 'Executing') or (state = 'Queued' and sent_at is not null)) for update", "and taken_by = $3 \\\n         and (state in ('Accepted', 'Executing') or (state = 'Queued' and held_at is not null)) for update", OPS),
    ("ops-a-new-holder-starts-it-twice", "crates/ops/src/commands.rs", "and domain = $2 and (state in ('Accepted', 'Executing') or (state = 'Queued' and sent_at is not null)) for update", "and domain = $2 and (state in ('Accepted', 'Executing') or (state = 'Queued' and held_at is not null)) for update", OPS),
    ("ops-a-key-for-different-work", "crates/ops/src/commands.rs", "    if different {\n", "    if false {\n", OPS),
    ("ops-a-stale-proposal", "crates/ops/src/mcp.rs", "    if !(-5..=EVIDENCE_SECS).contains(&age) {\n", "    if false {\n", OPS),
    ("ops-a-silent-site-is-fine", "crates/ops/src/mcp.rs", "Some(secs) if secs <= EVIDENCE_SECS => Ok(()),", "Some(_) => Ok(()),", OPS),
    ("ops-a-newer-command-is-guessed-at", "crates/ops-contract/src/command.rs", "        if self.schema_version > COMMAND_SCHEMA {\n", "        if false {\n", CONTRACT),
    ("doctor-reserve-unwatched", "crates/kernel/src/doctor.rs", "        out.push(if (0..=max_connections - RESERVED_CONNECTIONS).contains(&limit) {\n", "        out.push(if true {\n", ("-p", "ferrum-kernel", "--test", "doctor_it")),
    ("ops-second-live-command", "crates/ops/packages/ops/sql/002_one_live_command.sql", "create unique index op_command_one_live", "create index op_command_one_live", OPS),
    ("ops-second-live-command-issued", "crates/ops/src/commands.rs", "    if let Some(found) = existing(tx, tenant, &cmd, exclusive).await? {\n        return Ok(found);\n    }\n", "", OPS),
    ("ops-engine-only-ignored", "crates/ops/src/commands.rs", "if allowed.engine_only && cmd.issuer != Issuer::Engine {", "if false {", OPS),
    ("ops-envelope-beats-a-second-person", "crates/ops/src/commands.rs", "Issuer::Automation | Issuer::Ai if allowed.needs_approval =>", "Issuer::Automation | Issuer::Ai if false =>", OPS),
    ("ops-envelope-covers-agents", "crates/ops/src/commands.rs", "if cmd.issuer == Issuer::Ai && agents != Some(true) {", "if false {", OPS),
    ("ops-envelope-any-device", "crates/ops/src/commands.rs", "if !listed.is_empty() && !listed.contains(&target.id) {", "if false {", OPS),
    ("ops-envelope-any-payload", "crates/ops/src/commands.rs", "if let Some(why) = limits.iter().find_map(|l| l.outside(&cmd.payload)) {", "if let Some(why) = limits.iter().find_map(|_| None::<String>) {", OPS),
    ("ops-envelope-any-state", "crates/ops/src/commands.rs", "if last.as_deref() != Some(wanted.as_str()) {", "if false {", OPS),
    ("ops-envelope-old-evidence", "crates/ops/src/commands.rs", "and source_time > now() - make_interval(secs => $4) limit 1", "and source_time > now() - make_interval(secs => $4 * 1000000) limit 1", OPS),
    ("ops-envelope-no-budget", "crates/ops/src/commands.rs", "if max_in_progress.is_some_and(|max| unfinished >= max) {", "if false {", OPS),
    ("ops-provisioned-by-saying-so", "crates/ops/src/devices.rs", "if to == Lifecycle::Provisioned && gateway.is_none() {", "if false {", OPS),
    ("ops-allowance-ignored", "crates/ops/src/sync.rs", "let kept_back = !req.measurements.is_empty() && telemetry::over_allowance(&mut tx, tenant, now).await?;", "let kept_back = false;", OPS),
    ("ops-devices-beyond-allowance", "crates/ops/src/devices.rs", "if over.unwrap_or(false) {", "if false {", OPS),
    ("ops-site-never-hears", "crates/ops/src/commands.rs", "State::parse(&s).filter(|s| s.is_final())", "State::parse(&s).filter(|_| false)", OPS),
    ("ops-report-goes-backwards", "crates/ops-contract/src/command.rs", "(a, b) if b.rank() > a.rank() => Step::Move,", "(a, b) if b.rank() != a.rank() => Step::Move,", CONTRACT),
    ("ops-unknown-needs-no-evidence", "crates/ops-contract/src/command.rs", "            if evidence {\n                Step::Move", "            if true {\n                Step::Move", CONTRACT),
    ("ops-old-authority-believed", "crates/ops/src/commands.rs", "    if r.epoch != epoch {", "    if false {", OPS),
    ("ops-transfer-keeps-running-work", "crates/ops/src/authority.rs", "    commands::mark_in_doubt(&mut tx, ctx.tenant, domain, epoch, \"authority changed while this may have been running\").await?;\n", "", OPS),
    ("ops-replay-accepted", "crates/ops/src/identity.rs", "    if fresh.rows_affected() == 0 {", "    if false {", OPS),
    ("ops-signature-unchecked", "crates/ops/src/identity.rs", "    key.verify(&signed, &Signature::from_bytes(&signature)).map_err(|_| Refused::Key(\"the signature does not match\"))?;\n", "    let _ = (&key, &signed, &signature);\n", OPS),
    ("ops-ai-queues-directly", "crates/ops/src/commands.rs", "            Err(why) => (State::Proposed, None, Some(why)),", "            Err(why) => (State::Queued, None, Some(why)),", OPS),
    ("ops-own-approval", "crates/ops/src/commands.rs", "    if row.issued_by == person(ctx) && person(ctx).is_some() {", "    if false {", OPS),
    ("ops-other-site-measurements", "crates/ops/src/telemetry.rs", "if !ours.contains(&m.device_id) || m.check(now).is_err() {", "if m.check(now).is_err() {", OPS),
    ("refused-token-kept", "crates/plugins/src/host.rs", "self.tokens.remove(&key);", "let _ = &key;", HOST_UNIT),
    ("secret-goes-anywhere-to-sign-in", "crates/plugins/src/host.rs", "if !host_allowed(&c.hosts, host) {", "if false && !host_allowed(&c.hosts, host) {", HOST_UNIT),
    ("rotated-secret-keeps-token", "crates/plugins/src/host.rs", 'let digest = hex::encode(Sha256::digest(format!("{user}:{secret}")));', 'let digest = user.to_string();', HOST_UNIT),
    ("anyone-changes-a-shared-view", "crates/kernel/src/views.rs", "Some(by) if by == ctx.user || ctx.perms.admin => Ok(()),", "Some(_) if true => Ok(()),", COLLAB),
    ("shared-view-on-a-closed-list", "crates/kernel/src/views.rs", "fn checked(ctx: &Ctx, entity: &str, name: &str, value: &Value) -> Result<String> {\n    ctx.require(entity, Action::Read)?;", "fn checked(ctx: &Ctx, entity: &str, name: &str, value: &Value) -> Result<String> {\n    let _ = (entity, Action::Read);", COLLAB),
    ("closed-list-named-in-links", "crates/kernel/src/describe.rs", "ctx.perms.access(&e.def.name, Action::Read) != Access::None)", "!e.def.name.is_empty())", MERGE),
    ("provider-never-left-alone", "crates/kernel/src/connect.rs", "if let Some((seconds, why)) = waiting {", "if let Some((seconds, why)) = waiting.filter(|_| false) {", CONNECT),
    ("wait-costs-an-attempt", "crates/kernel/src/jobs.rs", "\"update job set status = 'queued', attempts = attempts - 1, last_error = $4,", "\"update job set status = 'queued', last_error = $4,", CONNECT),
    ("breaker-never-opens", "crates/kernel/src/connect.rs", "(failures >= BREAK_AFTER).then(", "(failures >= BREAK_AFTER * 1000).then(", CONNECT),
    ("lease-taken-twice", "crates/kernel/src/outbox.rs", "where id = $1 and available_at = $2 and processed_at is null and not dead returning available_at", "where id = $1 and $2::timestamptz is not null and processed_at is null and not dead returning available_at", EVENTS),
    ("hook-may-call-out", "crates/plugins/src/host.rs", '"http.fetch" | "sockets.send" | "sockets.close" | "resource.query" | "resource.execute" if matches!(self.scope, Scope::Hook(_)) => {', '"http.fetch" | "sockets.send" | "sockets.close" | "resource.query" | "resource.execute" if false => {', PLUGINS),
    ("hook-may-send-on-a-socket", "crates/plugins/src/host.rs", '"http.fetch" | "sockets.send" | "sockets.close" | "resource.query" | "resource.execute" if matches!(self.scope, Scope::Hook(_)) => {', '"http.fetch" | "sockets.close" | "resource.query" | "resource.execute" if matches!(self.scope, Scope::Hook(_)) => {', PLUGINS),
    ("hook-may-close-a-socket", "crates/plugins/src/host.rs", '"http.fetch" | "sockets.send" | "sockets.close" | "resource.query" | "resource.execute" if matches!(self.scope, Scope::Hook(_)) => {', '"http.fetch" | "sockets.send" | "resource.query" | "resource.execute" if matches!(self.scope, Scope::Hook(_)) => {', PLUGINS),
    ("socket-budget-ignored", "crates/plugins/src/sockets.rs", "if behind && self.queued.load(Ordering::Relaxed) + bytes > self.budget {", "if false {", SOCKETS_UNIT),
    ("socket-budget-punishes-the-quick", "crates/plugins/src/sockets.rs", "if behind && self.queued.load(Ordering::Relaxed) + bytes > self.budget {", "if self.queued.load(Ordering::Relaxed) + bytes > self.budget {", SOCKETS_UNIT),
    ("socket-budget-never-given-back", "crates/plugins/src/sockets.rs", "        let msg = self.rx.try_recv()?;\n        self.queued.fetch_sub(weight(&msg), Ordering::Relaxed);\n", "        let msg = self.rx.try_recv()?;\n", SOCKETS_UNIT),
    ("socket-closing-keeps-its-bytes", "crates/plugins/src/sockets.rs", "        self.rx.close();\n        while self.try_recv().is_ok() {}\n", "        self.rx.close();\n", SOCKETS_UNIT),
    ("hook-event-outlives-rollback", "crates/plugins/src/host.rs", "Scope::Hook(hc) => ferrum_kernel::outbox::enqueue(hc.tx, tenant, &topic, None, None, payload).await?,", "Scope::Hook(hc) => {\n                        let mut tx = hc.kernel.db.tenant_tx(tenant).await?;\n                        ferrum_kernel::outbox::enqueue(&mut tx, tenant, &topic, None, None, payload).await?;\n                        tx.commit().await?;\n                    }", PLUGINS),
    ("host-call-outlives-deadline", "crates/plugins/src/runtime.rs", "match tokio::time::timeout_at(end, host.call(&r.op, r.args)).await {", "match Ok::<_, tokio::time::error::Elapsed>(host.call(&r.op, r.args).await) {", DEADLINE),
    ("no-epoch-deadline", "crates/plugins/src/runtime.rs", "store.set_epoch_deadline((deadline.as_millis() / EPOCH_TICK.as_millis()) as u64 + 1);", "store.set_epoch_deadline(u64::MAX / 2);", DEADLINE),
    ("no-tenant-share", "crates/kernel/src/outbox.rs", ".bind(TENANT_SHARE)", ".bind(BATCH)", EVENTS),
    ("no-priority", "crates/kernel/src/outbox.rs", "order by o.priority, o.id limit $2", "order by o.id limit $2", EVENTS),
    ("event-loses-its-cause", "crates/kernel/src/outbox.rs", "causation: Some(ev.id),", "causation: None,", EVENTS),
    # Row-level security holds even without the statement's filter, so both have to go.
    ("replay-crosses-workspaces", "crates/kernel/src/outbox.rs", 'let mut tx = self.db.tenant_tx(ctx.tenant).await?;\n        let topic: Option<String> = sqlx::query_scalar(\n            "update outbox set dead = false, attempts = 0, last_error = null, available_at = now() where tenant_id = $1 and id = $2 and dead returning topic",', 'let mut tx = self.db.system_tx().await?;\n        let topic: Option<String> = sqlx::query_scalar(\n            "update outbox set dead = false, attempts = 0, last_error = null, available_at = now() where ($1::uuid is not null) and id = $2 and dead returning topic",', EVENTS),
    ("dead-list-shows-payloads", "crates/kernel/src/outbox.rs", "'last_error', last_error, 'created_at', created_at, 'correlation_id', correlation_id)", "'last_error', last_error, 'created_at', created_at, 'correlation_id', correlation_id, 'payload', payload)", EVENTS),
    ("webhook-holds-transaction", "crates/kernel/src/outbox.rs", "        .bind(WEBHOOK_LEASE_SECS)\n        .fetch_all(&mut *tx)\n        .await?;\n        tx.commit().await?;\n        let outcomes", "        .bind(WEBHOOK_LEASE_SECS)\n        .fetch_all(&mut *tx)\n        .await?;\n        let outcomes", EVENTS),
    ("keys-survive-deprovisioning", "crates/kernel/src/lifecycle.rs", '"update api_key set revoked_at = now() where tenant_id = $1 and user_id = $2 and revoked_at is null"', '"update api_key set revoked_at = revoked_at where tenant_id = $1 and user_id = $2 and revoked_at is null"', LIFECYCLE),
    ("last-admin-can-go", "crates/kernel/src/lifecycle.rs", 'if is_admin && !people.iter().any(|p| p.0 != user && p.1 && p.2 && p.3 == "member") {', "if false {", LIFECYCLE),
    ("work-left-behind", "crates/kernel/src/lifecycle.rs", 'update record_assignment a set user_id = $3 where a.tenant_id', 'update record_assignment a set user_id = $2 where $3::uuid is not null and a.tenant_id', LIFECYCLE),
    ("replicas-keep-the-session", "crates/kernel/src/lifecycle.rs", "notify_users(&mut tx, ctx.tenant, Some(user)).await?;\n        tx.commit().await?;", "tx.commit().await?;", LIFECYCLE),
    ("groups-take-hand-given-roles", "crates/kernel/src/scim.rs", 'delete from user_role where tenant_id = $1 and user_id = $2 and via_scim and role not in', "delete from user_role where tenant_id = $1 and user_id = $2 and role not in", LIFECYCLE),
    ("plugin-roles-through-groups", "crates/kernel/src/scim.rs", "and name = $3 and name not like 'plugin:%'), '{}'))", "and name = $3), '{}'))", LIFECYCLE),
    ("range-reads-open-in-sql", "crates/kernel/src/ctx.rs", 'for (end, op) in [(&r.low, ">="), (&r.high, "<=")] {', 'for (end, op) in [(&r.low, ">=")] {', RULES),
    ("range-reads-open-in-memory", "crates/kernel/src/ctx.rs", "&& g.ranges.iter().all(|r| r.holds(doc.get(&r.field).filter(|v| !v.is_null())))", "", RULES),
    ("empty-value-inside-a-range", "crates/kernel/src/ctx.rs", 'let Some(value) = number(&text) else { return false };', 'let Some(value) = number(&text) else { return true };', RULES),
    ("restricted-title-shown", "crates/kernel/src/doc.rs", "Reference::Restricted => match row_scope(ctx, tent, Action::Read) {", "Reference::Restricted => match Ok::<_, Error>(crate::ctx::RowScope::default()) {", RULES),
    ("restricted-title-ignores-rows", "crates/kernel/src/doc.rs", "            if let Some(scope) = &scope {\n                scope.push_sql(&mut qb, \"a\");\n            }\n            let rows: Vec<(Uuid, String)>", "            let _ = &scope;\n            let rows: Vec<(Uuid, String)>", RULES),
    ("explanation-ignores-the-record", "crates/kernel/src/access.rs", "let reached = scope.matches(&doc);", "let reached = true;", RULES),
    ("retired-key-forgotten", "crates/kernel/src/secret.rs", "self.settings.secret_key.iter().chain(&self.settings.retired_secret_keys)", "self.settings.secret_key.iter().chain(&[])", SECRETS),
    ("lost-key-blanks-values", "crates/kernel/src/secret.rs", "                *unopened += 1;\n                Ok(None)", "                *unopened += 1;\n                current.seal(aad, \"\").map(Some)", SECRETS),
    ("webhook-secret-stays-plain", "crates/kernel/src/secret.rs", 'None => Some(format!("{SEALED_WEBHOOK}{}", current.seal(&label, &stored)?)),', "None => None,", SECRETS),
    ("privacy-hold-on-a-person-ignored", "crates/kernel/src/privacy.rs", 'if person_held(&mut tx, ctx.tenant, &emails).await? {\n            return Err(Error::Conflict', 'if false {\n            return Err(Error::Conflict', PRIVACY),
    ("privacy-held-record-deletable", "crates/kernel/src/doc.rs", "if crate::privacy::record_held(tx, ctx.tenant, &ent.def.name, id).await? {", "if false {", PRIVACY),
    ("privacy-retained-records-erased", "crates/kernel/src/privacy.rs", "if let Some(why) = &def.retain {", "if let Some(why) = None::<&String> {", PRIVACY),
    ("privacy-erased-files-stay", "crates/kernel/src/privacy.rs", "        outbox::release_objects(tx, ctx.tenant, &keys).await?;\n", "        let _ = &keys;\n", PRIVACY),
    ("privacy-old-address-lost", "crates/kernel/src/privacy.rs", "           union select user_id from person_identifier where tenant_id = $1 and email = $2::citext) \\\n", "           ) \\\n", PRIVACY),
    ("privacy-account-stays-after-restore", "crates/kernel/src/privacy.rs", 'if present.is_some_and(|e| !e.ends_with("@erased.invalid")) {', "if false {", PRIVACY),
    ("privacy-journal-records-values", "crates/kernel/src/privacy.rs", '"records": journal, "reason": reason}', '"records": journal, "reason": reason, "email": email}', PRIVACY),
    ("unknown-filter-ignored", "crates/kernel/src/scim.rs", 'return Err(bad_filter(format!("cannot filter on {attr}")));', "return Ok(vec![]);", SCIM_UNIT),
    ("job-outcome-unfenced", "crates/kernel/src/jobs.rs", """let fence = "tenant_id = $1 and id = $2 and lease_token = $3 and status = 'running'";""", """let fence = "tenant_id = $1 and id = $2 and $3::uuid is not null";""", JOBS),
    ("job-retries-the-impossible", "crates/kernel/src/jobs.rs", "let last = gives_up(e) || job.attempts >= job.max_attempts;", "let last = job.attempts >= job.max_attempts;", JOBS),
    ("job-retries-for-ever", "crates/kernel/src/jobs.rs", "let last = gives_up(e) || job.attempts >= job.max_attempts;", "let last = gives_up(e);", JOBS),
    ("job-crash-free-of-charge", "crates/kernel/src/jobs.rs", "update job j set status = 'running', attempts = j.attempts + 1,", "update job j set status = 'running', attempts = j.attempts,", JOBS),
    ("job-stop-costs-an-attempt", "crates/kernel/src/jobs.rs", "update job set status = 'queued', attempts = attempts - 1, run_at = now()", "update job set status = 'queued', run_at = now()", JOBS),
    ("job-unfair-to-workspaces", "crates/kernel/src/jobs.rs", "order by j.priority, j.run_at limit 1 for update of j skip locked", "order by j.priority, j.run_at limit 50 for update of j skip locked", JOBS),
    ("job-no-running-cap", "crates/kernel/src/jobs.rs", ".bind(TENANT_RUNNING)", ".bind(1000i64)", JOBS),
    ("job-priority-ignored", "crates/kernel/src/jobs.rs", "order by j.priority, j.run_at limit 1", "order by j.run_at limit 1", JOBS),
    ("job-cancel-unheard", "crates/kernel/src/jobs.rs", "Ok(Some(true)) => return Ended::Cancelled,", "Ok(Some(true)) => {}", JOBS),
    ("job-internal-error-shown", "crates/kernel/src/jobs.rs", 'Error::Db(_) | Error::Internal(_) => "An internal error occurred".to_string(),', 'Error::Db(_) | Error::Internal(_) => e.to_string(),', JOBS),
    ("job-seen-by-anyone", "crates/kernel/src/jobs.rs", 'format!("{VIEW} and id = $2 and ($3 or created_by = $4)")', 'format!("{VIEW} and id = $2 and ($3 or created_by = $4 or true)")', JOBS),
    ("job-runs-without-asking-rights", "crates/kernel/src/jobs.rs", 'k.load_ctx(self.tenant_id, user, None, format!("job-{}", self.id)).await.map_err(|e| match e {', 'Ok(Ctx::system(self.tenant_id, k.registry.schema(self.tenant_id).await?)).map_err(|e: Error| match e {', JOBS),
    ("bulk-pages-shift-under-it", "crates/kernel/src/export.rs", "ids.retain(|id| done.after.is_none_or(|after| *id > after));", "ids.truncate(MAX_BULK);", JOBS),
    ("bulk-restarts-from-nothing", "crates/kernel/src/export.rs", "ids.retain(|id| done.after.is_none_or(|after| *id > after));", "", JOBS),
    ("bulk-events-jump-the-queue", "crates/kernel/src/bulk.rs", "Trace::at_priority(PRIORITY_BULK)\n            .scope(async {", "Trace::default()\n            .scope(async {", JOBS),
    ("list-mail-jumps-the-queue", "crates/kernel/src/outbox.rs", "case when $6 and coalesce(nullif(current_setting('ferrum.priority', true), '')::smallint, 1) < 2 then 0", "case when $6 then 0", JOBS),
    ("export-for-anyone", "crates/kernel/src/export.rs", """if !ctx.is_admin {\n            return Err(Error::Forbidden("exports are for administrators".into()));\n        }""", "", JOBS),
    ("export-on-the-file-route", "crates/kernel/src/export.rs", "self.stored.push(self.k.keep_file(self.ctx, Some((EXPORT_ENTITY, self.job)),", "self.stored.push(self.k.keep_file(self.ctx, None,", JOBS),
    ("export-holds-hashes", "crates/kernel/src/export.rs", "'created_at', created_at, 'last_login_at', last_login_at) from app_user", "'created_at', created_at, 'password_hash', password_hash) from app_user", JOBS),
    ("export-forgets-child-rows", "crates/kernel/src/export.rs", "k.whole_records(&all, name, !ent.children.is_empty(), cursor)", "k.whole_records(&all, name, false, cursor)", JOBS),
    ("export-forgets-engines", "crates/kernel/src/export.rs", "ledgers.chain(all.schema.export_tables.iter().cloned()).collect();", "ledgers.collect();", JOBS),
    ("export-stays-for-ever", "crates/kernel/src/lib.rs", " + self.purge_exports().await?", "", JOBS),
    ("cursor-rounds-to-milliseconds", "crates/kernel/src/doc.rs", "let next_cursor = rows.get(limit as usize - 1).filter(|_| has_more && keyset_ok).and_then(|r| {\n            let ts = r.get(&sort_field)?.as_str()?;", "let next_cursor = rows.get(limit as usize - 1).filter(|_| has_more && keyset_ok).and_then(|r| {\n            let ts = crate::value::parse_datetime(r.get(&sort_field)?.as_str()?).ok()?.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);\n            let ts = ts.as_str();", KERNEL),
    ("handover-ignores-state", "crates/kernel/src/lifecycle.rs", "condition: def.condition.clone(), ..Default::default()", "..Default::default()", HANDOVER),
    ("scan-passes-the-unknown", "crates/kernel/src/scan.rs", 'None => Err(Error::Unavailable(format!("the malware scanner answered: {answer}"))),', "None => Ok(Verdict::Clean),", SCAN_UNIT),
    ("scan-fails-open", "crates/kernel/src/scan.rs", ".map_err(|_| Error::Unavailable(\"the malware scanner did not answer in time\".into()))??;", ".unwrap_or(Ok(Verdict::Clean)).unwrap_or(Verdict::Clean);", SCAN),
    ("scan-keeps-what-it-found", "crates/kernel/src/scan.rs", 'Err(Error::Invalid(format!("{file_name} was not stored: the scanner found {signature} in it")))', 'Ok("clean")', SCAN),
    ("scan-skipped", "crates/kernel/src/files.rs", 'let scanned = self.scan_stream(ctx, file_name, data.len() as u64, futures::stream::iter([Ok::<_, Error>(data)])).await?;', 'let scanned = "clean";', SCAN),
    ("conflict-unnoticed", "crates/kernel/src/customize.rs", "if now.as_ref() != Some(&was) {", "if false {", CUSTOMIZE),
    ("requirement-silently-undone", "crates/kernel/src/customize.rs", "packaged.entity(entity).and_then(|e| e.field(field)).is_some_and(|f| f.required)", "packaged.entity(entity).and_then(|e| e.field(field)).is_some_and(|_| false)", CUSTOMIZE),
    ("hooks-cycle-accepted", "crates/kernel/src/hooks.rs", 'let Some(next) = (0..hooks.len()).find(|&i| !placed[i] && waits[i].iter().all(|&w| placed[w])) else {', 'let Some(next) = (0..hooks.len()).find(|&i| !placed[i]) else {', HOOKS_UNIT),
    ("hooks-conflict-accepted", "crates/kernel/src/hooks.rs", "if !shared.is_empty() && !reaches(&waits, i, j) && !reaches(&waits, j, i) {", "if false {", HOOKS_UNIT),
    ("logout-token-replayed", "crates/kernel/src/sso.rs", "        if !fresh {\n            return Ok(tx.commit().await?);\n        }\n", "", SSO),
    ("logout-event-unchecked", "crates/kernel/src/sso.rs", 'if !self.events.get(LOGOUT_EVENT).is_some_and(Value::is_object) {', 'if false {', SSO_UNIT),
    ("logout-accepts-id-tokens", "crates/kernel/src/sso.rs", "if self.nonce.is_some() {", "if false {", SSO_UNIT),
    ("logout-accepts-old-tokens", "crates/kernel/src/sso.rs", "if self.iat > now + CLOCK_SKEW_SECS || self.iat < now - LOGOUT_TOKEN_SECS {", "if self.iat > now + CLOCK_SKEW_SECS {", SSO_UNIT),
    ("logout-leaves-sessions", "crates/kernel/src/sso.rs", "crate::auth::end_sessions(&mut tx, tenant, user).await?;", "", SSO),
    # Lists read in an order an index gives.
    ("lists-without-their-index", "crates/kernel/src/ddl.rs", "        ddl.sort_index(&t, ent).await?;\n", "", KERNEL),
    ("lists-say-nulls-last-always", "crates/kernel/src/query.rs", 'if NEVER_EMPTY.contains(&field) { dir.to_string() } else { format!("{dir} nulls last") }', 'format!("{dir} nulls last")', KERNEL),
    # SAML: what samael leaves to its caller, and the sign-in around it.
    ("saml-any-audience", "crates/saml/src/checked.rs", ".is_some_and(|all| !all.is_empty() && all.iter().all(|r| r.audience.iter().any(|a| a.trim() == sp.entity_id)));", ".is_none_or(|_| true);", SAML_UNIT),
    ("saml-any-algorithm", "crates/saml/src/checked.rs", "allowed_signature_algorithms: Some(ALGORITHMS.to_vec()),", "allowed_signature_algorithms: None,", SAML_UNIT),
    ("saml-unsigned-metadata", "crates/saml/src/checked.rs", 'Ok(_) => return Err("the metadata holds no signing certificate, so nothing the provider sends could be checked".into()),', "Ok(_) => {}", SAML_UNIT),
    ("saml-stale-metadata", "crates/saml/src/checked.rs", "&& until < chrono::Utc::now()", "&& until < chrono::DateTime::<chrono::Utc>::MIN_UTC", SAML_UNIT),
    ("saml-unasked-answers", "crates/saml/src/checked.rs", "allow_idp_initiated: false,", "allow_idp_initiated: true,", SAML_UNIT),
    ("saml-returns-anywhere", "crates/kernel/src/sso.rs", "if !self.own_return(tenant, &back).await? {", "if false {", SAML),
    ("saml-refusal-keeps-the-sign-in", "crates/kernel/src/sso.rs", '                sqlx::query("delete from sso_flow where state_hash = $1").bind(hash(relay_state)).execute(&mut *tx).await?;\n', "", SAML),
    ("saml-any-code", "crates/kernel/src/sso.rs", "code_hash.as_deref() == Some(hash(code).as_str())", "code_hash.is_some()", SAML),
    ("saml-unsigned-requests", "crates/saml/src/checked.rs", "Some(key) => request.signed_redirect(relay_state, &private_key(key)?),", "Some(_) => request.redirect(relay_state),", SAML_UNIT),
    ("saml-encrypted-read-anyway", "crates/saml/src/checked.rs", 'if xml.contains("EncryptedAssertion") {', "if false {", SAML_UNIT),
    ("saml-any-domain", "crates/kernel/src/sso.rs", "                if domains.is_empty() {\n                    return Err(Error::field(\"domains\"", "                if false {\n                    return Err(Error::field(\"domains\"", SAML),
    # The secret key from a key-management service.
    ("kms-ciphertext-taken-for-a-key", "crates/kernel/src/kms.rs", "    if !is_wrapped(value) {\n        return SecretKey::from_base64(value);", "    if true {\n        return SecretKey::from_base64(value);", KMS),
    ("kms-any-service-will-do", "crates/kernel/src/kms.rs", 'let v = self.call(&client, &format!("{}/decrypt/{}", self.mount, self.key)', 'let v = self.call(&client, &format!("{}/decrypt/{}", self.mount, "ferrum")', KMS),
    # Workflows that cross systems.
    ("saga-forgets-what-it-did", "crates/kernel/src/saga.rs", "                        p.done.push(Done { step: step.name().to_string(), output });\n                        Saga::save(k, job, &p).await?;", "                        p.done.push(Done { step: step.name().to_string(), output });", SAGA),
    ("saga-undoes-oldest-first", "crates/kernel/src/saga.rs", "for i in (0..p.done.len()).rev() {", "for i in 0..p.done.len() {", SAGA),
    ("saga-never-turns-around", "crates/kernel/src/saga.rs", "Err(e) if final_error(&e) || job.attempts >= FORWARD_ATTEMPTS => {", "Err(e) if final_error(&e) => {", SAGA),
    ("saga-deaf-to-a-cancel", "crates/kernel/src/saga.rs", "if job.cancel_requested(k).await? {", "if false {", SAGA),
    ("saga-new-key-each-try", "crates/kernel/src/saga.rs", 'format!("ferrum-saga-{workflow}-{step}")', 'format!("ferrum-saga-{workflow}-{step}-{}", Uuid::now_v7())', SAGA),
    ("saga-cancel-drops-the-queued", "crates/kernel/src/jobs.rs", "status = case when status = 'queued' and not undo_on_cancel then 'cancelled'", "status = case when status = 'queued' then 'cancelled'", SAGA),
    ("merged-id-still-listed", "crates/kernel/src/query.rs", "    if ent.def.mergeable {\n        qb.push(\" and not exists", "    if false {\n        qb.push(\" and not exists", MERGE),
    ("merge-misses-child-rows", "crates/kernel/src/merge.rs", 'out.push((info.table.clone(), f.name.clone(), c.storage_of(&f.name) == Some(Storage::Ext), false));', '', MERGE),
    ("merge-misses-custom-fields", "crates/kernel/src/merge.rs", 'format!("update {t} set ext = jsonb_set(ext, array[$4], to_jsonb($1::text)) where tenant_id = $2 and ext->>$4 = $3")', 'format!("update {t} set ext = ext where tenant_id = $2 and ext->>$4 = $3 and $1 = $1")', MERGE),
    ("merge-past-a-legal-hold", "crates/kernel/src/merge.rs", 'if record_held(&mut tx, ctx.tenant, entity, id).await? {', 'if false {', MERGE),
    ("merge-history-unfindable", "crates/kernel/src/merge.rs", "ids.insert(0, stays);\n    Ok(ids)", "Ok(vec![stays])", MERGE),
    ("merge-leaves-chains", "crates/kernel/src/merge.rs", 'sqlx::query("update record_alias set canonical_id = $1 where tenant_id = $2 and entity = $3 and canonical_id = $4")\n            .bind(keep)\n            .bind(ctx.tenant)\n            .bind(entity)\n            .bind(duplicate)\n            .execute(&mut *tx)\n            .await?;\n', "", MERGE),
    ("merged-record-still-editable", "crates/kernel/src/doc.rs", "if lock && ent.def.mergeable && crate::merge::canonical(tx, ctx.tenant, &ent.def.name, id).await?.is_some() {", "if false {", MERGE),
    ("stale-link-saved", "crates/kernel/src/doc.rs", "crate::merge::resolve_links(ctx, tx, ent, doc).await?;", "", MERGE),
    ("mask-shows-it-all", "crates/kernel/src/doc.rs", "doc.insert(f.name.clone(), Value::String(mask.apply(&v)));", "doc.insert(f.name.clone(), Value::String(v));", RETENTION),
    ("restriction-ignored", "crates/kernel/src/outbox.rs", "if !self.may_contact(tx, tenant, to).await? {", "if false {", RETENTION),
    ("restriction-misses-old-addresses", "crates/kernel/src/retention.rs", "let emails = addresses(tx, tenant, &email).await?;\n        let restricted", "let emails = vec![email.clone()];\n        let restricted", RETENTION),
    ("retention-past-a-legal-hold", "crates/kernel/src/retention.rs", "if on_hold {", "if false {", RETENTION),
    ("retention-deletes-what-is-kept", "crates/kernel/src/retention.rs", "if let Some(why) = declared.iter().find_map(|d| d.retain.as_deref()) {", "if let Some(why) = None::<&str> {", RETENTION),
    ("retention-more-than-once-a-day", "crates/kernel/src/retention.rs", "if ran {", "if false {", RETENTION),
    ("doctor-misses-unforced-security", "crates/kernel/src/doctor.rs", "not (c.relrowsecurity and c.relforcerowsecurity)", "not c.relrowsecurity", DOCTOR),
    ("connection-credentials-shown", "crates/kernel/src/connect.rs", "'has_credentials', c.sealed is not null,", "'has_credentials', c.sealed is not null, 'sealed', c.sealed,", CONNECT),
    ("sync-restarts-from-the-top", "crates/kernel/src/connect.rs", "self.stored_cursor(job.tenant_id, connection, stream).await?", "None", CONNECT),
    ("sync-doubles-on-a-repeat", "crates/kernel/src/connect.rs", "(Some(l), false) if l.hash.as_deref() == Some(hash.as_str()) => Applied::Unchanged,", "", CONNECT),
    ("conflict-overwrites-ours", "crates/kernel/src/connect.rs", 'if changed_here && map.conflict != "source" {', "if false {", CONNECT),
    ("bad-record-stops-the-run", "crates/kernel/src/connect.rs", "Err(e) if !e.is_retryable() && !matches!(e, Error::Unavailable(_)) => {", "Err(e) if false => {", CONNECT),
    ("webhook-repeat-applied", "crates/kernel/src/connect.rs", "            if !fresh {\n                continue;\n            }\n            let record", "            let record", CONNECT),
    ("change-echoes-back", "crates/kernel/src/connect.rs", "if synced.flatten().zip(ev.source_version).is_some_and(|(synced, version)| synced >= version) {", "if false {", CONNECT),
    ("delivery-key-changes", "crates/kernel/src/connect.rs", "let change = Outgoing { key: key.to_string(),", "let change = Outgoing { key: Uuid::now_v7().to_string(),", CONNECT),
    ("untested-connection-runs", "crates/kernel/src/connect.rs", """        if !row.enabled {\n            return Err(Error::invalid("test this connection first"));\n        }\n""", "", CONNECT),
    ("disconnect-keeps-credentials", "crates/kernel/src/connect.rs", "update connection set enabled = false, sealed = null where", "update connection set enabled = false where", CONNECT),
    ("assistant-reads-everything", "server/src/assistant.rs", "let ctx = &ctx.for_provider(policy.egress.clone());", "let ctx = ctx;", EGRESS),
    ("internal-entity-goes-out", "crates/kernel/src/ctx.rs", "if ctx.egress.as_ref().is_some_and(|e| !e.allows_entity(&ent.def)) {", "if false {", EGRESS),
    ("field-group-goes-out", "crates/kernel/src/doc.rs", "                if !egress.allows_field(f) {\n                    doc.remove(&f.name);\n                } else", "                if false {\n                    doc.remove(&f.name);\n                } else", EGRESS),
    ("salary-by-filter", "crates/kernel/src/query.rs", "ctx.perms.can_read_field(&ent.def.name, f) && ctx.egress.as_ref().is_none_or(|e| e.allows_field(f))", "ctx.perms.can_read_field(&ent.def.name, f)", EGRESS),
    ("approval-outlives-its-record", "server/src/assistant.rs", 'if json!(version_of(k, ctx, &call["input"]).await) != call["version"] => {', 'if false => {', EGRESS),
    ("approval-never-expires", "server/src/assistant.rs", 'if chrono::Utc::now().timestamp() - p["asked_at"].as_i64().unwrap_or(0) > APPROVAL_SECS => {', 'if false => {', EGRESS),
    ("budget-ignored", "server/src/assistant.rs", "&& tokens_this_month(k, ctx.tenant).await? >= budget", "&& tokens_this_month(k, ctx.tenant).await? >= i64::MAX - budget", EGRESS),
    ("any-model-allowed", "server/src/assistant.rs", ".filter(|m| *m == config.model || config.models.contains(m))", "", EGRESS),
    ("support-key-outlives-revocation", "crates/kernel/src/support.rs", "update api_key set revoked_at = now() where tenant_id = $1 and user_id = $2 and revoked_at is null", "update api_key set revoked_at = revoked_at where tenant_id = $1 and user_id = $2 and revoked_at is null", SUPPORT),
    ("support-lasts-for-ever", "crates/kernel/src/support.rs", "if !(1..=longest).contains(&input.hours) {", "if false {", SUPPORT),
    ("support-writes-when-told-not", "crates/kernel/src/support.rs", ".bind(e)\n                .bind(input.can_write)\n", ".bind(e)\n                .bind(true)\n", SUPPORT),
    ("support-unseen-by-the-customer", "crates/kernel/src/support.rs", "if first {", "if false {", SUPPORT),
    ("restricted-account-still-writes", "crates/kernel/src/ctx.rs", "if self.read_only && !matches!(action, Action::Read | Action::Export) {", "if false {", SUPPORT),
    ("restricted-account-loses-its-export", "crates/kernel/src/ctx.rs", "if self.read_only && !matches!(action, Action::Read | Action::Export) {", "if self.read_only {", SUPPORT),
    ("quota-ignored", "crates/kernel/src/files.rs", "self.check_storage_quota(ctx.tenant, data.len() as u64).await?;", "", SUPPORT),
    # Sign-up and plans (FERRUM-CLOUD-002).
    ("signup-on-a-closed-server", "crates/kernel/src/signup.rs", "        if !policy.open {", "        if false {", SIGNUP),
    ("signup-from-any-domain", "crates/kernel/src/signup.rs", "        if !policy.domains.is_empty() && !policy.domains.iter().any(|d| d.eq_ignore_ascii_case(domain)) {", "        if false {", SIGNUP),
    ("signup-without-limit", "crates/kernel/src/signup.rs", "        if made >= self.settings.signup.per_address {", "        if false {", SIGNUP),
    ("signup-without-the-code", "crates/kernel/src/signup.rs", "        proof?;", "        let _ = proof;", SIGNUP),
    ("signup-spends-the-code-on-a-bad-form", "crates/kernel/src/signup.rs", "        new.check()?;", "", SIGNUP),
    ("plan-quotas-not-written", "crates/kernel/src/plans.rs", "            .bind(&quotas)", "            .bind(json!({}))", SIGNUP),
    ("people-quota-ignored", "crates/kernel/migrations/20261104000002_people_quota.sql", "  if used >= cap then", "  if false then", SIGNUP),
    ("people-quota-only-on-insert", "crates/kernel/migrations/20261104000002_people_quota.sql", "before insert or update of is_active, kind on app_user", "before insert on app_user", SIGNUP),
]


def dirty(path: str) -> bool:
    return bool(subprocess.run(["git", "status", "--porcelain", "--", path], cwd=ROOT, capture_output=True, text=True).stdout.strip())


def main() -> int:
    if any(a.startswith("--") and a != "--check" for a in sys.argv[1:]):
        print(__doc__, file=sys.stderr)
        return 2
    check = "--check" in sys.argv
    only = next((a for a in sys.argv[1:] if not a.startswith("--")), "")
    chosen = [m for m in MUTANTS if only in m[0]]
    for name, path, old, _, _ in chosen:
        if old not in (ROOT / path).read_text():
            print(f"{name}: its text is gone from {path}; update this script", file=sys.stderr)
            return 2
        if not check and dirty(path):
            print(f"{path} has uncommitted changes; commit them first", file=sys.stderr)
            return 2
    if check:
        print(f"{len(chosen)} mutants find their text")
        return 0
    env = {**os.environ, "CARGO_INCREMENTAL": "0"}
    survived = []
    for name, path, old, new, test in chosen:
        file = ROOT / path
        original = file.read_text()
        try:
            file.write_text(original.replace(old, new, 1))
            # Its own process group: a hung test binary must go with cargo, not spin on as an orphan.
            job = subprocess.Popen(["cargo", "test", "--locked", *test], cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
            try:
                output, _ = job.communicate(timeout=TIMEOUT)
            except subprocess.TimeoutExpired:
                # A guest left spinning keeps the test's runtime from shutting down: that is noticed.
                os.killpg(job.pid, signal.SIGKILL)
                job.wait()
                print(f"caught   {name} (it hung)", flush=True)
                continue
            if "error[E" in output or "could not compile" in output:
                print(f"BROKEN   {name}: the mutant does not compile\n{output[-1500:]}")
                survived.append(name)
            elif job.returncode == 0:
                print(f"SURVIVED {name}: the tests stayed green")
                survived.append(name)
            else:
                print(f"caught   {name}", flush=True)
        finally:
            file.write_text(original)
    print(f"{len(chosen) - len(survived)} of {len(chosen)} caught")
    return 1 if survived else 0


if __name__ == "__main__":
    sys.exit(main())
