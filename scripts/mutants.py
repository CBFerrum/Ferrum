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
META_EXPR = ("-p", "ferrum-meta", "--lib", "expr")
JOBS = ("-p", "ferrum-kernel", "--test", "jobs_it")
HANDOVER = ("-p", "ferrum-kernel", "--test", "handover_it")
KERNEL = ("-p", "ferrum-kernel", "--test", "kernel_it")
SCAN = ("-p", "ferrum-kernel", "--test", "scan_it")
SCAN_UNIT = ("-p", "ferrum-kernel", "--lib", "scan::tests")
CUSTOMIZE = ("-p", "ferrum-kernel", "--test", "customize_it")
CAPABILITY = ("-p", "ferrum-kernel", "--test", "capability_it")
AUTOMATION_MAIL = ("-p", "ferrum-kernel", "--test", "automation_mail_it")
LOGIC_UNIT = ("-p", "ferrum-logic", "--lib")
LOGIC_RULES = ("-p", "ferrum-kernel", "--test", "logic_rules_it")
EVENT_CHAIN = ("-p", "ferrum-kernel", "--test", "event_chain_it")
PROCESS_RECORDS = ("-p", "ferrum-kernel", "--test", "process_it")
# Needs a Temporal server at TEMPORAL_ADDRESS (docs/logic/processes.md); set, it fails rather than skips.
RUNTIME = ("-p", "ferrum-process", "--test", "conformance_it")
STUDIO_SOLUTIONS = ("-p", "ferrum-studio", "--test", "solutions_it")
PROCESS_BUDGET = ("-p", "ferrum-process", "--lib", "budget::tests")
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
PORTAL = ("-p", "ferrum-kernel", "--test", "portal_it")
CUSTOM_LINES = ("-p", "ferrum-kernel", "--test", "custom_lines_it")
PACKAGED_LINES = ("-p", "ferrum-kernel", "--test", "packaged_lines_it")
STUDIO_LINES = ("-p", "ferrum-studio", "--test", "custom_lines_it")
NESTED_LINES = ("-p", "ferrum-kernel", "--test", "nested_lines_it")
AUTHORING = ("-p", "ferrum-kernel", "--test", "authoring_it")
ASSISTANT = ("-p", "ferrum-server", "--test", "assistant_it")
BOUNDARIES = ("-p", "ferrum-server", "--test", "assistant_boundaries_it")
ASSISTANT_UNIT = ("-p", "ferrum-server", "--lib", "assistant::tests")
RECEIPTS = ("-p", "ferrum-kernel", "--test", "receipts_it")
CONSENT = ("-p", "ferrum-kernel", "--test", "consent_it")
SCHEDULE = ("-p", "ferrum-kernel", "--test", "schedule_it")
PACKS = ("-p", "ferrum-studio", "--test", "pack_it")
ACTIONS = ("-p", "ferrum-kernel", "--test", "actions_it")
INVOKE = ("-p", "ferrum-kernel", "--test", "invoke_it")
TARGETS = ("-p", "ferrum-kernel", "--test", "targets_it")
META_TARGETS = ("-p", "ferrum-meta", "--lib", "service_targets")
AGGREGATES = ("-p", "ferrum-kernel", "--test", "logic_aggregates_it")
SHARES_UNIT = ("-p", "ferrum-server", "--lib", "state::tests")
EXPORT_UNIT = ("-p", "ferrum-server", "--lib", "controllers::docs::tests")
EXTENSIONS = ("-p", "ferrum-server", "--test", "extensions_it")
SUPPORT = ("-p", "ferrum-kernel", "--test", "support_it")
PUBLISHED = ("-p", "ferrum-server", "--test", "published_lists_it")
META_PUBLISHED = ("-p", "ferrum-meta", "--lib", "published")
CONTRACT = ("-p", "ferrum-ops-contract", "--lib")
OPS = ("-p", "ferrum-ops", "--test", "ops_it")
RECOVERY = ("-p", "ferrum-kernel", "--test", "recovery_it")
# Needs OpenBao at FERRUM_KMS_TEST_TRANSIT (crates/kernel/tests/kms_it.rs).
KMS = ("-p", "ferrum-kernel", "--test", "kms_it")
SAGA = ("-p", "ferrum-kernel", "--test", "saga_it")
SIGNUP = ("-p", "ferrum-kernel", "--test", "signup_it")
ACCEPTANCE = ("-p", "ferrum-ops", "--test", "acceptance_it")
INDUSTRIAL = ("-p", "ferrum-apps", "--lib", "industrial::tests")
MANIFEST_UNIT = ("-p", "ferrum-apps", "--lib", "manifest::tests")
SERVICES = ("-p", "ferrum-server", "--test", "service_apps_it")
SHELL = ("-p", "ferrum-kernel", "--test", "shell_it")
APPS_API = ("-p", "ferrum-server", "--test", "apps_api")
LINKS = ("-p", "ferrum-kernel", "--test", "links_it")
META_LINKS = ("-p", "ferrum-meta", "--lib", "linked_records")
STREAMED = ("-p", "ferrum-server", "--test", "streamed_files_it")
# Generous enough to compile and run; passing it means the mutant hung the test.
TIMEOUT = 600

# name, file, text to replace, what breaks it, test
MUTANTS = [
    # A provider's limit is its account's (FEO-067).
    ("account-limit-per-connection", "crates/kernel/src/connect.rs", "where (tenant_id = $1 and id = $2) or account_key = (select account_key from connection where tenant_id = $1 and id = $2)", "where (tenant_id = $1 and id = $2)", CONNECT),
    ("account-by-provider-alone", "crates/kernel/src/connect.rs", 'key.fingerprint("connection account", &format!("{}:{}", input.provider, String::from_utf8_lossy(&crate::value::canonical_json(c))))', 'key.fingerprint("connection account", &format!("{}:{}", input.provider, c.is_null()))', CONNECT),
    # An operation found by the id a person quotes (FEO-079).
    ("operation-finds-the-wrong-events", "crates/kernel/src/outbox.rs", "from outbox where tenant_id = $1 and correlation_id = $2 order by id limit 200", "from outbox where tenant_id = $1 and correlation_id is distinct from $2 order by id limit 200", EVENTS),
    # Who stands in for someone away (FEO-034).
    ("substitute-ignored", "crates/kernel/src/process.rs", "if !standing_for.contains(to) {", "if !standing_for.contains(to) || standing_for.len() < 1000 {", PROCESS_RECORDS),
    ("away-forever", "crates/kernel/src/process.rs", "and current_date between starts_on and ends_on", "and current_date >= starts_on", PROCESS_RECORDS),
    # An export holds every row or says it cannot (FEO-066).
    ("workbook-cut-short", "server/src/controllers/docs.rs", "Some(_) if rows.len() >= cap => {", "Some(_) if rows.len() >= cap && cap == 0 => {", EXPORT_UNIT),
    ("csv-stops-at-a-page", "server/src/controllers/docs.rs", "(None, next.map(Some), fetch, columns)", "(None, None, fetch, columns)", EXPORT_UNIT),
    # A person's choices follow them to their record's new address (FEO-009).
    ("address-change-forgets-consent", "crates/kernel/src/retention.rs", "where tenant_id = $1 and email = $2 on conflict (tenant_id, email, purpose) do nothing", "where tenant_id = $1 and email = $3 on conflict (tenant_id, email, purpose) do nothing", CONSENT),
    ("address-change-overrides-own-word", "crates/kernel/src/retention.rs", "where tenant_id = $1 and email = $2 on conflict (tenant_id, email, purpose) do nothing", "where tenant_id = $1 and email = $2 on conflict (tenant_id, email, purpose) do update set granted = excluded.granted", CONSENT),
    ("address-change-forgets-restriction", "crates/kernel/src/retention.rs", "where tenant_id = $1 and email = $2 and lifted_at is null", "where tenant_id = $1 and email = $3 and lifted_at is null", CONSENT),
    # Duties kept apart across documents (FEO-013).
    ("approves-what-they-raised", "crates/kernel/src/doc.rs", "if raised_linked(tx, ctx, &ent, &doc, path, ctx.user).await? {", "if raised_linked(tx, ctx, &ent, &doc, path, ctx.user).await? && path.is_empty() {", COLLAB),
    ("approvals-list-what-they-raised", "crates/kernel/src/collab.rs", "if !raised && holds {", "if holds {", COLLAB),
    # The Logic Platform's capability gate (docs/logic, FLC-CAP-001 to 003, FLC-PROC-002).
    ("capability-same-key-other-input", "crates/kernel/src/capability.rs", "if was != digest {", "if false {", CAPABILITY),
    ("capability-no-receipt", "crates/kernel/src/capability.rs", "        if !claimed {\n", "        if false {\n", CAPABILITY),
    ("capability-rights-ignored", "crates/kernel/src/capability.rs", "    cap.permissions.iter().all(|p| match p.split_once(':') {", "    cap.permissions.iter().all(|p| true || match p.split_once(':') {", CAPABILITY),
    ("capability-ledger-offered", "crates/kernel/src/capability.rs", "matches!(ent.def.kind, EntityKind::Ledger | EntityKind::Child)", "matches!(ent.def.kind, EntityKind::Child)", CAPABILITY),
    ("capability-list-unmasked", "crates/kernel/src/capability.rs", "Value::Object(self.mask(ctx, &ent, d))", "Value::Object({ let _ = &ent; d })", CAPABILITY),
    ("capability-version-adopted", "crates/kernel/src/capability.rs", "            && v != &cap.version\n", "            && v.is_empty()\n", CAPABILITY),
    ("automation-mails-before-commit", "crates/kernel/src/automation.rs", "k.queue_mail_for(tx, ctx.tenant, &to, &subject, &body, &[], purpose).await?;", "{ let _ = purpose; k.mailer.as_ref().ok_or_else(|| Error::Unavailable(String::new()))?.send(&to, &subject, &body).await?; }", AUTOMATION_MAIL),
    # Customer rules (docs/logic/rules.md): every path that saves, reviewed releases, evidence kept apart.
    ("rules-skip-system-saves", "crates/kernel/src/logic.rs", "doc: &Doc) -> Result<()> {\n        let active = active(hc).await?;", "doc: &Doc) -> Result<()> {\n        if hc.ctx.is_system {\n            return Ok(());\n        }\n        let active = active(hc).await?;", LOGIC_RULES),
    ("refusal-evidence-not-kept", "crates/kernel/src/logic.rs", "                if r.audit {\n", "                if false && r.audit {\n", LOGIC_RULES),
    ("release-runs-unapproved", "crates/kernel/src/logic.rs", "approval::satisfied(&r.plan(), &r.digest, &r.approvals(), POLICY, Utc::now().timestamp_millis()).map_err(approval_error)?;", "let _ = approval::satisfied(&r.plan(), &r.digest, &r.approvals(), POLICY, Utc::now().timestamp_millis());", LOGIC_RULES),
    ("release-approved-unseen", "crates/kernel/src/logic.rs", "approval::may_decide(&r.plan(), shown,", "approval::may_decide(&r.plan(), &r.digest,", LOGIC_RULES),
    ("release-beside-a-rival", "crates/kernel/src/logic.rs", "        if !problems.is_empty() {\n            return Err(Error::Conflict(format!(\"{} {} cannot run beside", "        if false {\n            return Err(Error::Conflict(format!(\"{} {} cannot run beside", LOGIC_RULES),
    ("rules-read-restricted", "crates/logic/src/rules.rs", "shape.fields.iter().filter(|(_, f)| !f.restricted).map(", "shape.fields.iter().filter(|(_, f)| f.restricted || !f.restricted).map(", LOGIC_UNIT),
    ("rules-unknown-passes", "crates/logic/src/rules.rs", "Some(Err(_)) => return Err(r.refused(\"unknown\", format!(\"{} (it could not be checked)\", r.message))),", "Some(Err(_)) => {}", LOGIC_UNIT),
    ("rules-derive-in-written-order", "crates/logic/src/rules.rs", ".find(|&i| derives[i].reads.iter().all(|r| !filled_later(i).contains(r.as_str())))", ".find(|&i| i == 0 || filled_later(i).is_empty())", LOGIC_UNIT),
    # Chains of events stop, whoever answers whom (FLC-HOOK-008).
    ("chain-depth-not-carried", "crates/kernel/src/outbox.rs", "depth: ev.depth.saturating_add(1),", "depth: 0,", EVENT_CHAIN),
    ("chain-budget-ignored", "crates/kernel/src/outbox.rs", "} else if events > budget {", "} else if events < 0 {", EVENT_CHAIN),
    ("run-started-twice", "crates/kernel/src/process.rs", '.bind(&s.process)\n    .bind(&key)', '.bind(&s.process)\n    .bind(format!("{key}/{id}"))', PROCESS_RECORDS),
    ("requester-decides", "crates/kernel/src/process.rs", '        if exclude.contains(&ctx.user) {', '        if false && exclude.contains(&ctx.user) {', PROCESS_RECORDS),
    ("decided-on-other-material", "crates/kernel/src/process.rs", '        if shown != digest {', '        if false && shown != digest {', PROCESS_RECORDS),
    ("deadline-overrides-decision", "crates/kernel/src/process.rs", '"open" | "timed_out" => {', '"open" | "timed_out" | "approved" => {', PROCESS_RECORDS),
    ("repeated-deadline-says-nothing", "crates/kernel/src/process.rs", '"open" | "timed_out" => {', '"open" => {', PROCESS_RECORDS),
    # A task handed to someone who leaves is not stranded (FEO-012).
    ("departure-strands-handed-task", "crates/kernel/src/lifecycle.rs", "let tasks = hand_on_tasks(&mut tx, ctx, user, &email, successor).await?;", "let tasks = 0u64;", PROCESS_RECORDS),
    ("successor-takes-task-without-role", "crates/kernel/src/lifecycle.rs", "(role.is_empty() || heir_roles.contains(&role))", "(role.is_empty() || !heir_roles.contains(&role) || true)", PROCESS_RECORDS),
    ("task-back-to-someone-gone", "crates/kernel/src/lifecycle.rs", "|p| *p == user || !active.contains(p)", "|p| *p == user", PROCESS_RECORDS),
    ("delegation-unbounded", "crates/kernel/src/process.rs", 'if delegated.len().saturating_sub(1) as i32 >= bound {', 'if false && delegated.len().saturating_sub(1) as i32 >= bound {', PROCESS_RECORDS),
    ("delegation-counts-people", "crates/kernel/src/process.rs", 'if delegated.len().saturating_sub(1) as i32 >= bound {', 'if delegated.len() as i32 >= bound {', PROCESS_RECORDS),
    ("administrator-decides", "crates/kernel/src/process.rs", 'if !role.is_empty() && !ctx.roles.iter().any(|r| r == role) {', 'if !role.is_empty() && !ctx.roles.iter().any(|r| r == role) && !ctx.is_admin {', PROCESS_RECORDS),
    ("requester-closes-case", "crates/kernel/src/process.rs", 'if !ctx.roles.contains(&owner) || exclude.contains(&ctx.user) {', 'if !ctx.roles.contains(&owner) || exclude.is_empty() && exclude.contains(&ctx.user) {', PROCESS_RECORDS),
    ("delegation-goes-back", "crates/kernel/src/process.rs", 'if to == ctx.user || delegated.contains(&to) || exclude.contains(&to) {', 'if to == ctx.user || exclude.contains(&to) {', PROCESS_RECORDS),
    # Batch 2 of closing the registry's open rows (docs/logic/evidence/closing-gaps.md).
    ("older-cel-reads-the-clock", "crates/meta/src/expr.rs", "bind(&mut ctx, key, scope.user.get(key).unwrap_or(&Value::Null));", 'bind(&mut ctx, key, scope.user.get(key).unwrap_or(&Value::String(chrono::Utc::now().to_rfc3339())));', META_EXPR),
    ("clock-end-takes-earlier", "crates/logic/src/calendar.rs", "Some((at(s, false)?, at(e, true)?))", "Some((at(s, false)?, at(e, false)?))", LOGIC_UNIT),
    ("clock-repeat-counted-twice", "crates/logic/src/calendar.rs", ".filter(|(s2, _)| *s2 < e)", ".filter(|_| false)", LOGIC_UNIT),
    ("clock-stopped-ignored", "crates/logic/src/calendar.rs", ".filter(|&(a, b)| a < b && b > from)", ".filter(|_| false)", LOGIC_UNIT),
    ("clock-holiday-open", "crates/logic/src/calendar.rs", "        if !self.business(d) {\n            return vec![];", "        if false {\n            return vec![];", LOGIC_UNIT),
    ("rule-writes-the-apps-field", "crates/logic/src/compile.rs", "if !f.customer {", "if false && !f.customer {", LOGIC_UNIT),
    ("deprecation-unsaid", "crates/logic/src/compile.rs", "if let Some(why) = &c.deprecated {", "if let Some(why) = c.deprecated.as_ref().filter(|_| false) {", LOGIC_UNIT),
    ("erasure-misses-run-copies", "crates/kernel/src/privacy.rs", "let runs = crate::process::erase_run_copies(tx, ctx.tenant, entity, id, fields).await?;", "let runs = 0u64;", PROCESS_RECORDS),
    ("report-brings-erased-back", "crates/kernel/src/process.rs", "if let Some((subject, fields)) = &erased {", "if let Some((subject, fields)) = erased.as_ref().filter(|_| false) {", PROCESS_RECORDS),
    ("evidence-crosses-tasks", "crates/kernel/src/process.rs", "and entity = $3 and record_id = $4", "and entity = $3 and $4::uuid is not null", PROCESS_RECORDS),
    ("evidence-for-anyone", "crates/kernel/src/process.rs", "let t = self.logic_task(ctx, task).await?;", "let t = self.logic_task(&Ctx::system(ctx.tenant, ctx.schema.clone()), task).await?;", PROCESS_RECORDS),
    ("support-report-shows-payloads", "crates/kernel/src/support.rs", "'status', status, 'since', updated_at)", "'status', status, 'since', updated_at, 'trigger', trigger)", PROCESS_RECORDS),
    ("dependents-overlooked", "crates/kernel/src/logic.rs", '.filter(|l| packages.iter().any(|p| l.id.starts_with(&format!("{p}."))))', '.filter(|l| packages.iter().any(|p| l.id.starts_with(&format!("{p}.x"))))', PROCESS_RECORDS),
    ("removal-forgets-what-waits", "crates/kernel/src/apps.rs", 'json!({"packages": names, "waiting": waiting})', 'json!({"packages": names, "waiting": waiting.iter().take(0).collect::<Vec<_>>()})', PROCESS_RECORDS),
    ("references-of-any-kind", "crates/logic/src/expr.rs", "(Ty::Ref(_), Ty::Text) | (Ty::Text, Ty::Ref(_)) => true,", "(Ty::Ref(_), _) | (_, Ty::Ref(_)) => true,", PROCESS_RECORDS),
    # Company-owned runs act as their solution's own account, with the reviewed closure only (FLC-CAP-005).
    ("company-run-acts-as-its-activator", "crates/kernel/src/process.rs", '"select id, is_active from app_user where tenant_id = $1 and email = $2"', """"select a.activated_by, true from logic_activation a where a.tenant_id = $1 and $2 like 'solution.' || a.solution || '@%'\"""", PROCESS_RECORDS),
    ("solution-account-holds-more", "crates/kernel/src/logic.rs", '"write" => d.write = true,', '"write" => { d.write = true; d.read = true; d.create = true }', PROCESS_RECORDS),
    ("admin-capability-for-anyone", "crates/kernel/src/capability.rs", 'None => p == "admin" && (ctx.is_admin || granted),', 'None => p == "admin",', CAPABILITY),
    # The console's lookup reads the receipt under the step's own key (FLC-UI-008).
    ("look-up-misses-the-receipt", "crates/kernel/src/process.rs", "from logic_effect where tenant_id = $1 and effect_key = $2 order by created_at limit 1", "from logic_effect where tenant_id = $1 and effect_key = $2 || '?' order by created_at limit 1", PROCESS_RECORDS),
    # A replaced capability version is waited out, never taken up (FLC-CAP-004).
    ("replaced-version-taken-up", "crates/kernel/src/process.rs", "Ok(c) if c.version == version => self.unpaused(tenant, run, activation).await?,", "Ok(_) => self.unpaused(tenant, run, activation).await?,", PROCESS_RECORDS),
    # Destinations are held to at the connection (FLC-SEC-005).
    ("egress-follows-redirects", "crates/kernel/src/outbound.rs", ".redirect(reqwest::redirect::Policy::none())", ".redirect(reqwest::redirect::Policy::limited(5))", CAPABILITY),
    ("egress-reaches-private-hosts", "crates/kernel/src/outbound.rs", "if !allow_private && addrs.iter().any(|a| crate::outbox::is_private(a.ip())) {", "if false && addrs.iter().any(|a| crate::outbox::is_private(a.ip())) {", CAPABILITY),
    # Core refuses a durable process (FLC-BASE-005).
    ("core-runs-a-process", "crates/logic/src/compile.rs", "    if profile == RuntimeProfile::Core {\n        p.fault(\n            Code::UnsupportedProfile,", "    if false {\n        p.fault(\n            Code::UnsupportedProfile,", LOGIC_RULES),
    # A replica takes up an activation made on another with the very next event (FLC-MIG-003).
    ("replica-misses-an-activation", "crates/kernel/src/process.rs", 'let version: i64 = sqlx::query_scalar("select meta_version from tenant where id = $1").bind(tenant).fetch_one(&mut *tx).await?;', "let version: i64 = k.registry.schema(tenant).await?.version;", PROCESS_RECORDS),
    ("effect-on-stale-approval", "crates/kernel/src/process.rs", '&& let Err(e) = self.approval_holds(tenant, run, a).await', '&& let Err(e) = self.approval_holds(tenant, run, a).await.or(Ok::<(), Error>(()))', PROCESS_RECORDS),
    ("approval-of-deleted-record", "crates/kernel/src/process.rs", 'Err(Error::NotFound(_)) => {\n                    tx.commit().await?;\n                    return Err(refusal(Code::StaleApproval', 'Err(Error::NotFound(_)) if false => {\n                    tx.commit().await?;\n                    return Err(refusal(Code::StaleApproval', PROCESS_RECORDS),
    ("effect-borrows-rights", "crates/kernel/src/process.rs", 'if let Some(user) = requester {', 'if let Some(user) = requester.filter(|_| false) {', PROCESS_RECORDS),
    ("early-event-dropped", "crates/kernel/src/process.rs", '            if waiting.is_empty() {', '            if false && waiting.is_empty() {', PROCESS_RECORDS),
    ("event-crosses-records", "crates/kernel/src/process.rs", "and event = $2 and correlation = $3 and status = 'waiting' order by", "and event = $2 and ($3::text is not null) and status = 'waiting' order by", PROCESS_RECORDS),
    ("case-order-ignored", "crates/kernel/src/process.rs", 'find(|r| !done.iter().any(|d| d == r))', 'find(|r| r.is_empty() && !done.iter().any(|d| d == r))', PROCESS_RECORDS),
    ("recovery-forces-success", "crates/kernel/src/process.rs", 'if resume && detail.get("resumable") != Some(&Value::Bool(true)) {', 'if false && resume && detail.get("resumable") != Some(&Value::Bool(true)) {', PROCESS_RECORDS),
    ("late-effect-erased", "crates/logic/src/process.rs", '                Invoked::Done { output } => out.push(Command::Attention {\n                    activation: act.to_string(),\n                    why: "an effect settled after it was no longer wanted: decide what to do with it".into(),\n                    evidence: output.clone(),\n                }),', '                Invoked::Done { .. } => {}', LOGIC_UNIT),
    ("compensation-oldest-first", "crates/logic/src/process.rs", '(0..run.effects.len()).rev().filter(', '(0..run.effects.len()).filter(', LOGIC_UNIT),
    ("rework-unbounded", "crates/logic/src/process.rs", 'if again && *round < *max {', 'if again && *round < *max + 1 {', LOGIC_UNIT),
    ("item-keys-collide", "crates/logic/src/process.rs", 'if keys.contains(&k) {', 'if false && keys.contains(&k) {', LOGIC_UNIT),
    ("step-budget-ignored", "crates/logic/src/process.rs", 'if run.steps > max {', 'if run.steps > max.saturating_mul(1000) {', LOGIC_UNIT),
    ("cancel-drops-work-in-hand", "crates/logic/src/process.rs", 'Some(Waiting::Invoke { .. }) => out.push(Command::Cancel { activation: a }),', 'Some(Waiting::Invoke { .. }) => {\n                    run.waiting.remove(&a);\n                }', LOGIC_UNIT),
    ("subprocess-recurses", "crates/logic/src/compile.rs", 'if s.recursive.contains(process) {', 'if s.recursive.contains(process) && false {', LOGIC_UNIT),
    ("resume-reads-unrun", "crates/logic/src/compile.rs", 'find(|&&d| d != ti && !dom[si].contains(&d))', 'find(|&&d| d != ti && !dom[si].contains(&d) && d == usize::MAX)', LOGIC_UNIT),
    ("approval-may-come-later", "crates/logic/src/compile.rs", '&& (!s.tasks.contains(task) || !env.vars.contains_key(task))', '&& !s.tasks.contains(task)', LOGIC_UNIT),
    ("history-not-sealed", "crates/process/src/lib.rs", '    options.data_converter = converter(s.keys.clone());', '    let _ = converter(s.keys.clone());', RUNTIME),
    ("run-started-again", "crates/process/src/lib.rs", 'options.id_reuse_policy = WorkflowIdReusePolicy::RejectDuplicate;', 'options.id_reuse_policy = WorkflowIdReusePolicy::AllowDuplicate;', RUNTIME),
    ("late-delivery-retried", "crates/process/src/lib.rs", 'Err(e) if matches!(k.process_load(', 'Err(e) if false && matches!(k.process_load(', RUNTIME),
    ("draft-save-overwrites", "crates/kernel/src/authoring.rs", 'where tenant_id = $1 and id = $2 and version = $3 returning version', 'where tenant_id = $1 and id = $2 and ($3::int is not null) returning version', STUDIO_SOLUTIONS),
    ("approval-without-consent", "crates/kernel/src/logic.rs", '            if named != growth {', '            if false && named != growth {', STUDIO_SOLUTIONS),
    ("cursor-passes-open-transaction", "crates/kernel/src/process.rs", 'select pg_snapshot_xmin(s)::text, pg_snapshot_xmax(s)::text', 'select pg_snapshot_xmax(s)::text, pg_snapshot_xmax(s)::text', STUDIO_SOLUTIONS),
    ("restored-cursor-not-resynced", "crates/kernel/src/process.rs", 'let resync = cursor.is_some() && parsed.is_none_or(', 'let resync = false && cursor.is_some() && parsed.is_none_or(', STUDIO_SOLUTIONS),
    ("author-reviews", "crates/kernel/src/logic.rs", 'privileges: &[String], approve: bool, reason: &str) -> Result<()> {\n        require_admin(ctx)?;', 'privileges: &[String], approve: bool, reason: &str) -> Result<()> {\n        crate::authoring::require_author(ctx)?;', STUDIO_SOLUTIONS),
    ("related-runs-unchecked", "crates/kernel/src/process.rs", '        self.get(ctx, entity, record, false).await?;', '        let _ = self.get(ctx, entity, record, false).await;', STUDIO_SOLUTIONS),
    ("diff-shows-old-as-new", "crates/logic/src/diff.rs", "[shown(ao.get(k)), shown(bo.get(k))]", "[shown(ao.get(k)), shown(ao.get(k))]", LOGIC_UNIT),
    ("effect-ignores-restore-hold", "crates/kernel/src/process.rs", "if changes && (self.db.fenced() || self.holding().await?) {", "if false && changes && (self.db.fenced() || self.holding().await?) {", PROCESS_RECORDS),
    ("automation-imported-twice", "crates/kernel/src/authoring.rs", "if existing.contains(&solution) {", "if false && existing.contains(&solution) {", STUDIO_SOLUTIONS),
    # W8: one writer at a cutover, rollback truth, shadows that run nothing, steps that wait for a capability out of
    # service, packs checked on arrival, moves approved by a second person and refused over a decision.
    ("cutover-runs-both", "crates/kernel/src/process.rs", "                && !claim_handover(&mut tx, tenant, automation, ev.id).await?\n", "                && automation.is_nil()\n", PROCESS_RECORDS),
    ("cutover-leaves-automation-on", "crates/kernel/src/logic.rs", '"update automation set is_active = false, updated_at = now() where tenant_id = $1 and id = $2"', '"update automation set updated_at = now() where tenant_id = $1 and id = $2"', PROCESS_RECORDS),
    ("run-forgets-its-generation", "crates/kernel/src/process.rs", "    .bind(generation)\n    .fetch_optional(&mut **tx)", "    .bind(None::<i64>.or(Some(generation)).filter(|_| false))\n    .fetch_optional(&mut **tx)", PROCESS_RECORDS),
    ("shadow-forgets-what-happened", "crates/kernel/src/authoring.rs", "s.results.insert(activation.clone(), answer_met(output));", "let _ = (activation, output);", PROCESS_RECORDS),
    ("shadow-replays-refusal-as-success", "crates/kernel/src/authoring.rs", '        (Some(e), _) => Invoked::Refused {', '        (Some(e), _) if e.is_null() => Invoked::Refused {', PROCESS_RECORDS),
    ("revoked-capability-fails-the-step", "crates/kernel/src/process.rs", "Err(e) if e.code() == Code::CapabilityUnavailable.as_str() => return self.paused(", "Err(e) if false && e.code() == Code::CapabilityUnavailable.as_str() => return self.paused(", PROCESS_RECORDS),
    ("pack-taken-in-changed", "crates/kernel/src/authoring.rs", 'if pack.get("source_digest").and_then(Value::as_str) != Some(digest.as_str()) {', 'if digest.is_empty() {', STUDIO_SOLUTIONS),
    ("move-approved-by-its-planner", "crates/kernel/src/process.rs", "if planner == Some(ctx.user) {", "if false && planner == Some(ctx.user) {", PROCESS_RECORDS),
    ("move-over-a-decision", "crates/kernel/src/process.rs", '.filter(|(_, st)| st != "open")', '.filter(|(_, st)| st == "never")', PROCESS_RECORDS),
    ("move-renames-a-holder", "crates/logic/src/process.rs", "if !leaf && target != *node {", "if false && !leaf && target != *node {", LOGIC_UNIT),
    ("shadow-assumes-over-history", "crates/kernel/src/authoring.rs", "if !s.results.contains_key(key) {", "if !key.is_empty() {", PROCESS_RECORDS),
    ("restored-effect-unnoticed", "crates/kernel/src/process.rs", "            if restored.is_some() {\n", "            if restored.is_none() {\n", PROCESS_RECORDS),
    ("workspace-takes-every-slot", "crates/process/src/budget.rs", "|| mine >= self.limits.per_workspace", "|| mine >= usize::MAX", PROCESS_BUDGET),
    ("effects-take-bookkeeping-slots", "crates/process/src/budget.rs", "let effects = slots - (slots / 4).max(1);", "let effects = slots;", PROCESS_BUDGET),
    ("signal-and-wait-unlocked", "crates/kernel/src/process.rs", "        signal_lock(&mut tx, tenant, event, &key).await?;\n", "", PROCESS_RECORDS),
    ("restore-not-rechecked", "crates/kernel/src/recovery.rs", "        self.recheck_logic_runs().await?;\n", "", PROCESS_RECORDS),
    ("tries-not-counted", "crates/kernel/src/process.rs", "set attempts = logic_attempt.attempts + 1,", "set attempts = logic_attempt.attempts,", PROCESS_RECORDS),
    ("case-close-waits-for-deliveries", "crates/logic/src/process.rs", 'for (task, output) in recorded.as_object().into_iter().flatten() {', 'for (task, output) in recorded.as_object().into_iter().flatten().take(0) {', LOGIC_UNIT),
    ("capability-input-says-who-asks", "crates/logic/src/capability.rs", "map.keys().find(|k| CONTEXT_FIELDS.contains(&k.as_str()) && !self.declares(k))", "map.keys().find(|k| k.is_empty() && !self.declares(k))", CAPABILITY),
    # The directory of people is the members'.
    ("directory-read-by-anyone", "crates/kernel/src/collab.rs", "if !ctx.is_system && self.user_kind(ctx.tenant, ctx.user).await? != \"member\" {", "if false {", SHELL),
    ("directory-lists-everyone", "crates/kernel/src/collab.rs", "where tenant_id = $1 and is_active and kind = 'member' \\", "where tenant_id = $1 and is_active \\", SHELL),
    # Large files: an app's whole read has a ceiling.
    ("files-read-whole-past-the-cap", "crates/kernel/src/files.rs", "        if f.size > MAX_READ_WHOLE {\n", "        if false {\n", COLLAB),
    # Services on their publisher's servers.
    ("service-hears-the-record", "crates/kernel/src/outbox.rs", "case when w.app is null then $4 else $4 - 'doc' - 'was' end", "$4", SERVICES),
    ("service-hears-what-it-cannot-read", "crates/apps/src/manifest.rs", "            if !reads(entity) {\n", "            if false {\n", MANIFEST_UNIT),
    ("service-off-still-hears", "crates/kernel/src/service.rs", "update webhook set is_active = $3 where tenant_id = $1 and app = $2", "update webhook set is_active = $3 or true where tenant_id = $1 and app = $2", SERVICES),
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
    ("extract-unerases", "crates/kernel/src/recovery.rs", "erased_again += self.erase_again(&mut tx, &system, erasure).await?.0;", "erased_again += usize::from(system.is_system && erasure.is_null());", RECOVERY),
    ("extract-files-unchecked", "crates/kernel/src/recovery.rs", "            if !store.exists(key).await? {", "            if store.exists(key).await? && false {", RECOVERY),
    ("extract-what-went-out-unsaid", "crates/kernel/src/recovery.rs", "processed_at > coalesce($2, '-infinity'::timestamptz)", "processed_at <= coalesce($2, '-infinity'::timestamptz)", RECOVERY),
    ("extract-forgets-erasures", "crates/kernel/src/recovery.rs", '"contact_consent", "privacy_erasure"]', '"contact_consent"]', RECOVERY),
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
    # A connection is one gateway's at a time, by generation (FLC-EDGE-009): the old one closing ends
    # nothing of the new, and an open older than what is known does not take it back.
    ("ops-old-close-ends-the-new", "crates/ops/src/connections.rs", "where tenant_id = $1 and device_id = $2 and gateway_id = $3 and closed_at is null", "where tenant_id = $1 and device_id = $2 and (gateway_id = $3 or true) and closed_at is null", OPS),
    ("ops-late-open-takes-it-back", "crates/ops/src/connections.rs", "        Some((_, _, since, _)) if at < since => Ok(None),\n", "        Some((_, _, since, _)) if at < since && false => Ok(None),\n", OPS),
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
    ("replay-crosses-workspaces", "crates/kernel/src/outbox.rs", 'let mut tx = self.db.tenant_tx(ctx.tenant).await?;\n        // Replayed by a person\'s decision, it starts a chain of its own: its depth and its chain\'s count\n        // start again, so it is not stopped at once for what it was stopped for before.\n        let topic: Option<String> = sqlx::query_scalar(\n            "with chain as (delete from event_chain c using outbox o where o.tenant_id = $1 and o.id = $2 and o.dead \\\n               and c.tenant_id = o.tenant_id and c.correlation_id = coalesce(o.correlation_id, \'event-\' || o.id)) \\\n             update outbox set dead = false, attempts = 0, last_error = null, depth = 0, available_at = now() \\\n             where tenant_id = $1 and id = $2 and dead returning topic",', 'let mut tx = self.db.system_tx().await?;\n        // Replayed by a person\'s decision, it starts a chain of its own: its depth and its chain\'s count\n        // start again, so it is not stopped at once for what it was stopped for before.\n        let topic: Option<String> = sqlx::query_scalar(\n            "with chain as (delete from event_chain c using outbox o where o.tenant_id = $1 and o.id = $2 and o.dead \\\n               and c.tenant_id = o.tenant_id and c.correlation_id = coalesce(o.correlation_id, \'event-\' || o.id)) \\\n             update outbox set dead = false, attempts = 0, last_error = null, depth = 0, available_at = now() \\\n             where ($1::uuid is not null) and id = $2 and dead returning topic",', EVENTS),
    ("dead-list-shows-payloads", "crates/kernel/src/outbox.rs", "'last_error', last_error, 'created_at', created_at, 'correlation_id', correlation_id)", "'last_error', last_error, 'created_at', created_at, 'correlation_id', correlation_id, 'payload', payload)", EVENTS),
    ("webhook-holds-transaction", "crates/kernel/src/outbox.rs", "        .bind(WEBHOOK_LEASE_SECS)\n        .fetch_all(&mut *tx)\n        .await?;\n        tx.commit().await?;\n        let outcomes", "        .bind(WEBHOOK_LEASE_SECS)\n        .fetch_all(&mut *tx)\n        .await?;\n        let outcomes", EVENTS),
    # Changes of access outlive a restore from before them (FEO-014).
    ("departure-unjournalled", "crates/kernel/src/lifecycle.rs", 'outbox::enqueue(&mut tx, ctx.tenant, ACCESS_TOPIC, Some("User"), Some(user), json!({"user": user, "active": false})).await?;', "", LIFECYCLE),
    ("return-unjournalled", "crates/kernel/src/auth.rs", "crate::lifecycle::ACCESS_TOPIC,", "\"ferrum.user.unjournalled\",", LIFECYCLE),
    ("replay-leaves-leavers-in", "crates/kernel/src/lifecycle.rs", "Some(true) if !active => {", "Some(true) if !active && user.is_nil() => {", LIFECYCLE),
    ("replay-steps-through", "crates/kernel/src/lifecycle.rs", "last.insert((tenant, user), active);", "if !active { last.insert((tenant, user), active); }", LIFECYCLE),
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
    ("explanation-ignores-the-record", "crates/kernel/src/access.rs", "let reached = by.is_some();", "let reached = true;", RULES),
    ("retired-key-forgotten", "crates/kernel/src/secret.rs", "self.settings.secret_key.iter().chain(&self.settings.retired_secret_keys)", "self.settings.secret_key.iter().chain(&[])", SECRETS),
    ("lost-key-blanks-values", "crates/kernel/src/secret.rs", "                *unopened += 1;\n                Ok(None)", "                *unopened += 1;\n                current.seal(aad, \"\").map(Some)", SECRETS),
    ("webhook-secret-stays-plain", "crates/kernel/src/secret.rs", 'None => Some(format!("{SEALED_WEBHOOK}{}", current.seal(&label, &stored)?)),', "None => None,", SECRETS),
    ("privacy-hold-on-a-person-ignored", "crates/kernel/src/privacy.rs", 'if person_held(&mut tx, ctx.tenant, &emails).await? {\n            return Err(Error::Conflict', 'if false {\n            return Err(Error::Conflict', PRIVACY),
    # What apps keep of people in tables of their own (FEO-016).
    ("erasure-forgets-app-tables", "crates/kernel/src/privacy.rs", "let n = app.erase(&mut tx, ctx.tenant, account, &emails, reason).await?;", "let n = u64::from(app.app().is_empty());", PRIVACY),
    ("report-omits-app-tables", "crates/kernel/src/privacy.rs", "for held in app.report(&mut tx, ctx.tenant, account_id, &emails).await? {", "for held in Vec::<Value>::new().into_iter().filter(|_| app.app().is_empty()) {", PRIVACY),
    ("name-in-a-list-missed", "crates/kernel/src/privacy.rs", "[,;]", "[|]", PRIVACY),
    ("restore-revives-app-tables", "crates/kernel/src/privacy.rs", "done += app.erase(tx, tenant, account, &addresses, reason).await? as usize;", "done += usize::from(app.app().is_empty());", PRIVACY),
    ("privacy-held-record-deletable", "crates/kernel/src/doc.rs", "if crate::privacy::held(ctx, tx, &ent.def.name, id).await? {", "if false {", PRIVACY),
    ("privacy-retained-records-erased", "crates/kernel/src/privacy.rs", "if let Some(why) = &def.retain {", "if let Some(why) = None::<&String> {", PRIVACY),
    ("privacy-erased-files-stay", "crates/kernel/src/privacy.rs", "        outbox::release_objects(tx, ctx.tenant, &keys).await?;\n", "        let _ = &keys;\n", PRIVACY),
    ("privacy-old-address-lost", "crates/kernel/src/privacy.rs", "           union select user_id from person_identifier where tenant_id = $1 and email = $2::citext) \\\n", "           ) \\\n", PRIVACY),
    ("privacy-account-stays-after-restore", "crates/kernel/src/privacy.rs", 'if let Some(email) = present.filter(|e| !e.ends_with("@erased.invalid")) {', "if let Some(email) = present.filter(|_| false) {", PRIVACY),
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
    # FEO-056: an upgrade's plan says what it parts and which records it changes things under, before anyone agrees.
    ("workflow-customization-uncompared", "crates/meta/src/effective.rs", "OverlayOp::SetWorkflow { .. } => Some(e.workflow", "OverlayOp::SetWorkflow { .. } if false => Some(e.workflow", CUSTOMIZE),
    ("old-conflicts-blamed-on-the-plan", "crates/kernel/src/customize.rs", "today != (!c.gone).then(|| c.now.clone())", "today.is_some() || today.is_none()", CUSTOMIZE),
    ("open-work-unseen", "crates/kernel/src/customize.rs", "if then.is_some() && then == state_of(&w.def, &state.name) {", "if then.is_some() || then.is_none() {", CUSTOMIZE),
    ("legacy-customizations-never-compared", "crates/kernel/src/customize.rs", "let Some(base) = packaged.entity(&entity).and_then(|e| effective::replaced_value(e, &op)) else { continue };", "let Some(base) = None::<Value> else { continue };", CUSTOMIZE),
    ("consent-ignores-what-it-parts", "server/src/appmgr.rs", "    for c in parted {\n", "    for c in &parted[..0] {\n", APPS_API),
    ("other-workspaces-changed-silently", "crates/kernel/src/apps.rs", "for tenant in tenants.into_iter().filter(|t| Some(*t) != rel.shown_to) {", "for tenant in tenants.into_iter().filter(|_| false) {", APPS_API),
    ("installer-told-twice", "crates/kernel/src/apps.rs", "for tenant in tenants.into_iter().filter(|t| Some(*t) != rel.shown_to) {", "for tenant in tenants.into_iter().filter(|t| Some(*t) != rel.shown_to || true) {", APPS_API),
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
    ("solution-speaks-undeclared-schemes", "crates/apps/src/industrial.rs", "        if !schemes.contains(p.scheme.as_str()) {", "        if false && !schemes.contains(p.scheme.as_str()) {", INDUSTRIAL),
    ("solution-writes-read-only-points", "crates/apps/src/industrial.rs", "                    Some(_) => at(format!(\"command {} writes {target}, which is read only\", c.name))?,", "                    Some(_) => {}", INDUSTRIAL),
    ("solution-reads-unknown-points", "crates/apps/src/industrial.rs", "                    if s.point.as_deref().and_then(point).is_none() {", "                    if false {", INDUSTRIAL),
    ("solution-keeps-strays", "crates/apps/src/industrial.rs", "        if let Some(stray) = shipped(dir).into_iter().find(|p| !listed.contains(p)) {", "        if let Some(stray) = shipped(dir).into_iter().find(|_| false) {", INDUSTRIAL),
    ("solution-manual-says-nothing", "crates/apps/src/industrial.rs", "                if s.instruction.as_deref().is_none_or(|i| i.trim().is_empty()) {", "                if s.instruction.is_none() {", INDUSTRIAL),
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
    ("restriction-ignored", "crates/kernel/src/retention.rs", "Ok(!(restricted || (purpose == Purpose::Marketing && withdrawn)))", "Ok(!(purpose == Purpose::Marketing && withdrawn))", RETENTION),
    ("restriction-misses-old-addresses", "crates/kernel/src/retention.rs", "let emails = addresses(tx, tenant, &email).await?;\n        let (restricted, withdrawn)", "let emails = vec![email.clone()];\n        let (restricted, withdrawn)", RETENTION),
    ("retention-past-a-legal-hold", "crates/kernel/src/retention.rs", "if on_hold {", "if false {", RETENTION),
    ("retention-deletes-what-is-kept", "crates/kernel/src/retention.rs", "if let Some(why) = declared.iter().find_map(|d| d.retain.as_deref()) {", "if let Some(why) = None::<&str> {", RETENTION),
    ("retention-more-than-once-a-day", "crates/kernel/src/retention.rs", "if ran {", "if false {", RETENTION),
    # A scan goes on from where the last run stopped, so held or failing records never starve the rest (FEO-005).
    # A write sent again is done once, for its own person and request (FEO-003).
    ("receipt-for-anyone", "crates/kernel/src/receipt.rs", "    if by != actor {", "    if false {", RECEIPTS),
    ("receipt-without-its-request", "crates/kernel/src/receipt.rs", "    if hash != digest {", "    if false {", RECEIPTS),
    ("receipt-not-kept", "crates/kernel/src/receipt.rs", "        .bind(status)\n        .bind(answer)", "        .bind(status)\n        .bind(None::<Value>)", RECEIPTS),
    ("receipts-gone-in-a-day", "crates/kernel/src/outbox.rs", ".bind(crate::receipt::RECEIPT_DAYS)", ".bind(1)", RECEIPTS),
    # A held record's evidence survives changes and a workspace's deletion (FEO-007).
    ("held-record-changed-without-a-trace", "crates/kernel/src/ddl.rs", "ddl.exec(format!(\"create or replace trigger ferrum_held_keep before update on {} for each row execute function ferrum_held_keep('{named}')\", q(&t)))\n            .await?;", "let _ = &named;", PRIVACY),
    ("held-versions-outlive-the-hold", "crates/kernel/src/privacy.rs", "and o.released_at is null and o.entity = h.entity and o.record_id = h.record_id)", "and o.entity = h.entity and o.record_id = h.record_id)", PRIVACY),
    ("workspace-deleted-under-a-hold", "crates/kernel/src/recovery.rs", "if holds > 0 {", "if false {", CLOSURE),
    # The values chosen to stay are written in the merge's own transaction (FEO-006).
    ("merge-forgets-the-chosen-values", "crates/kernel/src/merge.rs", "if let Some((chosen, version)) = values.filter(|(v, _)| !v.is_empty()) {", "if let Some((chosen, version)) = values.filter(|_| false) {", MERGE),
    # Marketing stops where consent ends, asked as each message goes (FEO-015).
    ("marketing-ignores-withdrawal", "crates/kernel/src/retention.rs", "Ok(!(restricted || (purpose == Purpose::Marketing && withdrawn)))", "Ok(!restricted)", CONSENT),
    ("mail-not-asked-as-it-goes", "crates/kernel/src/outbox.rs", "            if !may {\n                tracing::info!(tenant = %ev.tenant_id, ?purpose, \"a queued mail was not sent: its recipient may no longer be contacted for it\");\n                return Ok(());\n            }\n", "", CONSENT),
    ("automation-mail-loses-its-purpose", "crates/kernel/src/automation.rs", "k.queue_mail_for(tx, ctx.tenant, &to, &subject, &body, &[], purpose).await?;", "k.queue_mail_for(tx, ctx.tenant, &to, &subject, &body, &[], crate::retention::Purpose::Transactional).await?;", CONSENT),
    ("marketing-capability-as-a-notice", "crates/kernel/src/capability.rs", 'let purpose = if cap.id == "ferrum.mail.marketing" {', 'let purpose = if cap.id == "never" {', CONSENT),
    ("marketing-stream-asks-no-one", "crates/kernel/src/connect.rs", "        if let Some(field) = &map.consent {", "        if let Some(field) = map.consent.as_ref().filter(|_| false) {", CONNECT),
    ("going-back-undoes-what-people-asked", "crates/kernel/src/recovery.rs", '&["audit_event", "support_grant", "support_access", "legal_hold", "held_version", "processing_restriction", "contact_consent", "privacy_erasure"];', '&["audit_event", "support_grant", "support_access", "privacy_erasure"];', RECOVERY),
    ("retention-starts-over-every-day", "crates/kernel/src/retention.rs", "match self.due_records(sys, rule, rule.scan_cursor.clone(), limit).await {", "match self.due_records(sys, rule, None, limit).await {", RETENTION),
    # A person under a hold keeps their records from retention, deletion and an app's removal (FEO-007).
    ("retention-ignores-held-people", "crates/kernel/src/retention.rs", "let on_hold = people.contains(id) || record_held(", "let on_hold = record_held(", RETENTION),
    ("held-person-record-deleted", "crates/kernel/src/privacy.rs", "Ok(record_held(tx, ctx.tenant, entity, record).await? || held_people_records(ctx, tx).await?.iter().any(|(e, ids)| e == entity && ids.contains(&record)))", "Ok(record_held(tx, ctx.tenant, entity, record).await?)", PRIVACY),
    ("app-removed-over-a-hold", "crates/kernel/src/apps.rs", "if placed > 0 || people > 0 {", "if placed < 0 && people > 0 {", PRIVACY),
    # A record's identities at providers follow it into the merge (FEO-006).
    ("merge-leaves-provider-links", "crates/kernel/src/merge.rs", "set record_id = $1, local_version = -1 where d.tenant_id", "set record_id = d.record_id where d.tenant_id", MERGE),
    # A workspace's settings changing is heard at once, wherever they changed (FEO-004).
    ("policy-change-unheard", "server/src/assistant.rs", "            && *read_at == revision\n", "", ASSISTANT),
    ("plan-change-unheard", "crates/kernel/src/plans.rs", "        crate::registry::Registry::bump_version(&mut tx, tenant).await?;\n        tx.commit().await?;\n        self.registry.invalidate(tenant);\n", "        tx.commit().await?;\n", SIGNUP),
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
    ("assistant-reads-everything", "server/src/assistant.rs", "    let now = now.for_provider(policy.egress.clone());", "    let now = now;", EGRESS),
    ("internal-entity-goes-out", "crates/kernel/src/ctx.rs", "if ctx.egress.as_ref().is_some_and(|e| !e.allows_entity(&ent.def)) {", "if false {", EGRESS),
    ("field-group-goes-out", "crates/kernel/src/doc.rs", "                if !egress.allows_field(f) {\n                    doc.remove(&f.name);\n                } else", "                if false {\n                    doc.remove(&f.name);\n                } else", EGRESS),
    ("salary-by-filter", "crates/kernel/src/query.rs", "ctx.perms.can_read_field(&ent.def.name, f) && ctx.egress.as_ref().is_none_or(|e| e.allows_field(f))", "ctx.perms.can_read_field(&ent.def.name, f)", EGRESS),
    ("approval-outlives-its-record", "server/src/assistant.rs", 'if json!(version_of(k, ctx, &call["input"]).await) != call["version"] => {', 'if false => {', EGRESS),
    ("approval-never-expires", "server/src/assistant.rs", 'if chrono::Utc::now().timestamp() - p["asked_at"].as_i64().unwrap_or(0) > APPROVAL_SECS => {', 'if false => {', EGRESS),
    ("budget-ignored", "server/src/assistant.rs", "    if room < MIN_OUTPUT {", "    if room < i64::MIN + MIN_OUTPUT {", EGRESS),
    # A policy that cannot be read never means fewer restrictions (FEO-004).
    # What the assistant may still do once things change under it (FEO-059 to FEO-062).
    ("turn-trusts-its-start", "server/src/assistant.rs", "                None => standing(env, first, signed_in, Some(&begun)).await,", "                None => Ok((ctx.clone(), Policy::default())),", BOUNDARIES),
    ("retry-without-asking", "server/src/assistant.rs", "                    if let Err(e) = standing(env, first, signed_in, Some(&begun)).await {\n                        release(k, ctx.tenant, held).await;\n                        return Err(e);\n                    }\n", "", BOUNDARIES),
    ("earlier-answers-always-sent", "server/src/assistant.rs", "        let sendable = still_sendable(k, ctx, turn, here).await;", "        let sendable = true;", BOUNDARIES),
    ("earlier-results-sent-again", "server/src/assistant.rs", '.filter(|b| b["type"] == "text" && (role == "assistant" || !context_block(b)))', ".filter(|_| true)", BOUNDARIES),
    ("tool-says-it-reads", "server/src/assistant.rs", "        _ => false,\n    }\n}\n\n/// The person's tools", "        _ => true,\n    }\n}\n\n/// The person's tools", BOUNDARIES),
    ("null-for-any-type", "server/src/assistant.rs", "    if !types.is_empty() && !types.iter().any(|t| is(t)) {", "    if !types.is_empty() && !v.is_null() && !types.iter().any(|t| is(t)) {", ASSISTANT_UNIT),
    ("nested-inputs-unchecked", "server/src/assistant.rs", "                    Some(p) => conforms(p, value, &path)?,", "                    Some(_) => {}", ASSISTANT_UNIT),
    ("allowance-not-held", "server/src/assistant.rs", "+ (select coalesce(sum(tokens), 0) from assistant_reservation where tenant_id = $1 and created_at > now() - interval '15 minutes')", "+ 0", BOUNDARIES),
    ("keys-streamed-as-written", "server/src/assistant.rs", 'let out = if kind == "thinking" { thinking.push(piece) } else { text.push(piece) };', "let out = piece.to_string();", BOUNDARIES),
    ("answers-kept-unredacted", "server/src/assistant.rs", 'b["text"] = json!(redact(t, true));', 'b["text"] = json!(t);', BOUNDARIES),
    ("unread-policy-is-the-default", "server/src/assistant.rs", '_ => (Err(Error::Unavailable("the assistant\'s settings cannot be read now: try again shortly".into())), None),', "_ => (Ok(Policy::of(&Value::Null, &Value::Null)), None),", ASSISTANT),
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
    ("signup-opens-an-existing-account", "crates/kernel/src/signup.rs", "let account = if has_account { None }", "let account = if false { None }", SIGNUP),
    ("closing-without-the-name", "crates/kernel/src/account.rs", "        if confirm.trim() != slug {", "        if false {", SIGNUP),
    ("closing-by-anyone", "crates/kernel/src/account.rs", "    pub async fn close_own_workspace(&self, ctx: &Ctx, confirm: &str) -> Result<NaiveDate> {\n        if !ctx.is_admin {", "    pub async fn close_own_workspace(&self, ctx: &Ctx, confirm: &str) -> Result<NaiveDate> {\n        if false {", SIGNUP),
    ("plan-quotas-not-written", "crates/kernel/src/plans.rs", "            .bind(&quotas)", "            .bind(json!({}))", SIGNUP),
    ("people-quota-ignored", "crates/kernel/migrations/20261104000002_people_quota.sql", "  if used >= cap then", "  if false then", SIGNUP),
    ("people-quota-only-on-insert", "crates/kernel/migrations/20261104000002_people_quota.sql", "before insert or update of is_active, kind on app_user", "before insert on app_user", SIGNUP),
    # An industrial solution's acceptance on site (FERRUM-IND-018).
    ("commissioned-without-acceptance", "crates/ops/src/devices.rs", "        if !waiting.is_empty() {", "        if false {", ACCEPTANCE),
    ("a-running-run-is-a-pass", "crates/ops/src/acceptance.rs", "r.procedure = p.id and r.status = 'Passed') \\", "r.procedure = p.id and r.status <> 'Failed') \\", ACCEPTANCE),
    ("any-procedure-for-any-device", "crates/ops/src/acceptance.rs", "    if !applies {", "    if false {", ACCEPTANCE),
    ("a-step-recorded-twice", "crates/ops/src/acceptance.rs", "    if outcome != \"Pending\" {", "    if false {", ACCEPTANCE),
    ("a-failed-step-fails-nothing", "crates/ops/src/acceptance.rs", "    let finished = if failed > 0 {", "    let finished = if false {", ACCEPTANCE),
    ("readings-above-the-maximum-pass", "crates/ops/src/acceptance.rs", ".is_none_or(|m| n <= m)", ".is_none_or(|_| true)", ACCEPTANCE),
    ("import-with-the-callers-schema", "crates/ops/src/acceptance.rs", "    let mut system = Ctx::system(ctx.tenant, k.registry.schema(ctx.tenant).await?);", "    let mut system = Ctx::system(ctx.tenant, ctx.schema.clone());", ACCEPTANCE),
    ("an-upgrade-adds-procedures", "crates/ops/src/acceptance.rs", "        match existing(&mut tx, ctx.tenant, \"e_acceptance_procedure\", &key).await? {", "        match None::<Uuid> {", ACCEPTANCE),
    # Portal roles (docs/builder/decisions.md BD-02).
    ("portal-path-not-looked-up", "crates/kernel/src/ctx.rs", "        Ok(self.reached_by(tx, tenant, doc).await?.is_some())", "        Ok(false)", PORTAL),
    ("portal-path-is-whoever-made-it", "crates/kernel/src/ctx.rs", "        match (&g.via, &self.owner_email) {", "        match (&None::<ViaScope>, &self.owner_email) {", PORTAL),
    ("portal-path-ignores-the-rules-condition", "crates/kernel/src/ctx.rs", "            if !Self::holds(g, doc) {", "            if false {", PORTAL),
    ("portal-path-matches-case", "crates/kernel/src/ctx.rs", "{c}.parent_id = {alias}.id{} and lower(trim({expr})) = \",", "{c}.parent_id = {alias}.id{} and {expr} = \",", PORTAL),
    ("portal-people-hold-member-roles", "crates/kernel/src/auth.rs", "and ur.user_id = $2 and r.portal and (ur.expires_at", "and ur.user_id = $2 and (ur.expires_at", PORTAL),
    ("portal-role-holds-anything", "crates/meta/src/types.rs", "    pub fn portal_problem(&self, personal: bool) -> Option<String> {\n", "    pub fn portal_problem(&self, personal: bool) -> Option<String> {\n        return None;\n", PORTAL),
    ("portal-reads-personal-data-whole", "crates/meta/src/types.rs", "            if personal || !reads_only {", "            if !reads_only {", PORTAL),
    ("named-action-runs-on-any-record", "crates/kernel/src/ctx.rs", "            scope_of(ctx, ent, Some(grants))", "            scope_of(ctx, ent, None)", PORTAL),
    ("rule-names-an-action-that-is-not-there", "crates/kernel/src/auth.rs", "        if let Some(why) = p.unknown_action(ent.as_ref().map(|e| &e.def)) {", "        if let Some(why) = None::<String> {", PORTAL),
    ("portal-people-read-a-records-surroundings", "crates/kernel/src/collab.rs", "return Err(Error::Forbidden(\"a record's comments, files and people are for the workspace's members\".into()));", "return self.get(ctx, entity, id, false).await;", PORTAL),
    ("portal-people-read-a-records-history", "crates/kernel/src/audit.rs", "return Err(Error::Forbidden(\"a record's history is for the workspace's members\".into()));", "let _ = ();", PORTAL),
    # Who takes a workflow transition: the run path and the form's `may_take` ask one gate.
    ("transition-without-roles-for-readers", "crates/kernel/src/ctx.rs", "        return row_scope(ctx, ent, Action::Write);\n    }\n    if !t.roles", "        return row_scope(ctx, ent, Action::Read);\n    }\n    if !t.roles", PORTAL),
    ("transition-without-roles-for-outsiders", "crates/kernel/src/ctx.rs", "        if ctx.outside {\n            return Err(Error::Forbidden(format!(\"'{}' is for the workspace's members\"", "        if false {\n            return Err(Error::Forbidden(format!(\"'{}' is for the workspace's members\"", PORTAL),
    ("transition-gate-not-asked", "crates/kernel/src/doc.rs", "        if !crate::ctx::transition_gate(ctx, &ent, t)?.admits(tx, ctx.tenant, &doc).await? {", "        if false {", PORTAL),
    ("transition-offered-to-anyone", "crates/kernel/src/describe.rs", "json!(crate::ctx::transition_gate(ctx, ent, def).is_ok())", "json!(true)", PORTAL),
    # Tying a file to a record needs the right to change it.
    ("attach-on-read", "crates/kernel/src/doc.rs", "        if !scope.admits(&mut tx, ctx.tenant, &doc).await? {\n            return Err(Error::Forbidden(format!(\"only someone who may change", "        if !scope.admits(&mut tx, ctx.tenant, &doc).await? && false {\n            return Err(Error::Forbidden(format!(\"only someone who may change", STREAMED),
    ("upload-attaches-on-read", "server/src/objects.rs", "Some((entity, record)) => k.may_attach(ctx, entity, *record).await?,", "Some((entity, record)) => k.get(ctx, entity, *record, false).await.map(drop)?,", STREAMED),
    # A file tied to no record is its uploader's until a record they save names it.
    ("loose-file-served-to-anyone", "crates/kernel/src/files.rs", "            _ if owns_loose(ctx, by) => Ok(()),", "            _ if true => Ok(()),", PORTAL),
    ("save-ties-no-file", "crates/kernel/src/files.rs", "update file_object set entity = $3, record_id = $4 where tenant_id = $1 and id = $2\"", "update file_object set entity = $3, record_id = $4 where tenant_id = $1 and id = $2 and false\"", PORTAL),
    ("upgrade-ties-no-file", "crates/kernel/src/files.rs", "where o.tenant_id = $1 and o.id = pick.file and o.record_id is null returning o.id", "where o.tenant_id = $1 and o.id = pick.file and false returning o.id", PORTAL),
    ("upgrade-skips-lines", "crates/kernel/src/files.rs", "                for f in c.value_fields().filter(|f| f.ftype == FieldType::Attach) {", "                for f in c.value_fields().filter(|f| f.ftype == FieldType::Attach && false) {", PORTAL),
    ("upgrade-ties-to-the-last-made", "crates/kernel/src/files.rs", "order by file, at, record)", "order by file, at desc, record)", PORTAL),
    ("save-names-anyones-file", "crates/kernel/src/files.rs", "                _ => return Err(refused()),", "                _ => {}", PORTAL),
    ("outsiders-upload-without-limit", "crates/kernel/src/files.rs", "        if !ctx.outside {\n            return Ok(());", "        if !ctx.outside || true {\n            return Ok(());", STREAMED),
    # A workspace's menu marked for the portal lists only what the person's portal roles reach (docs/builder/portal.md).
    ("portal-menu-lists-what-the-person-does-not-reach", "crates/kernel/src/access.rs", "for name in m.items.iter().filter(|i| reached(i)) {", "for name in m.items.iter().filter(|_| true) {", PORTAL),
    ("portal-menu-offers-create-the-role-lacks", "crates/kernel/src/access.rs", "m.create.contains(name) && ctx.perms.access(name, Action::Create) != Access::None", "m.create.contains(name)", PORTAL),
    ("portal-roles-given-to-anyone", "crates/kernel/src/auth.rs", "                if let Some(other) = roles.iter().find(|r| !portal.contains(r)) {", "                if let Some(other) = None::<&String> {", PORTAL),
    ("custom-lines-load-mixes-tables", "crates/kernel/src/store.rs", "                info.predicate(&ci)\n", "                String::new()\n", CUSTOM_LINES),
    ("custom-lines-filter-mixes-tables", "crates/kernel/src/query.rs", "                    info.predicate(\"c\")\n", "                    String::new()\n", CUSTOM_LINES),
    ("custom-lines-portal-path-mixes-tables", "crates/kernel/src/ctx.rs", "                    info.predicate(&c)\n", "                    String::new()\n", CUSTOM_LINES),
    ("custom-lines-studio-grain-mixes-tables", "crates/studio/src/query.rs", "a.id{rows}\", q(&g.sql_table)", "a.id\", q(&g.sql_table)", STUDIO_LINES),
    ("custom-lines-studio-filter-mixes-tables", "crates/studio/src/query.rs", "{alias}.parent_id = a.id{}\", entity_predicate(child, alias)", "{alias}.parent_id = a.id{}\", String::new()", STUDIO_LINES),
    ("custom-keys-not-kept", "crates/kernel/src/store.rs", "    if !ent.custom || keys.is_empty() {", "    if true {", CUSTOM_LINES),
    ("custom-keys-held-by-cancelled", "crates/kernel/src/store.rs", "a.id = $2 and a.docstatus <> 2 and v.value", "a.id = $2 and v.value", CUSTOM_LINES),
    ("custom-key-over-duplicates", "crates/kernel/src/customize.rs", "            if !shared.is_empty() {", "            if false {", CUSTOM_LINES),
    ("rollup-not-computed", "crates/kernel/src/doc.rs", "    roll_up(ent, doc)\n}", "    Ok(())\n}", CUSTOM_LINES),
    # Lines within lines (docs/builder/decisions.md BD-06, 11 November 2026).
    ("nested-lines-load-one-level", "crates/kernel/src/store.rs", "                tables_sql(cent, schema, &c, depth + 1),", "                String::new(),", NESTED_LINES),
    ("nested-lines-save-one-level", "crates/kernel/src/store.rs", "            Box::pin(write_tables(tx, schema, cent, tenant, &next)).await?;", "            let _ = next;", NESTED_LINES),
    ("nested-lines-unchanged-rewritten", "crates/kernel/src/store.rs", ".is_none_or(|o| !rows_equal(cent, o, r)))", ".is_none_or(|o| !rows_equal(cent, o, r) || true))", NESTED_LINES),
    ("nested-lines-rows-move", "crates/kernel/src/doc.rs", "Some(i) if !at.is_empty() && placed.contains(&i) => {", "Some(i) if false && placed.contains(&i) => {", NESTED_LINES),
    ("nested-lines-not-validated", "crates/kernel/src/doc.rs", "                    Box::pin(self.validate_rows(ctx, tx, &cent, rm, &rview, on, errors, &path)).await?;", "                    let _ = (&rview, &path);", NESTED_LINES),
    ("nested-lines-no-line-totals", "crates/kernel/src/doc.rs", "                compute_rows(cent, m, user, schema)?;\n", "\n", NESTED_LINES),
    ("nested-lines-error-without-path", "crates/kernel/src/error.rs", "self.path = (path.len() > 1).then(", "self.path = (path.len() > 9).then(", NESTED_LINES),
    ("nested-lines-third-level", "crates/meta/src/effective.rs", "if b.def.kind == EntityKind::Child && building.get(of)", "if false && building.get(of)", NESTED_LINES),
    ("nested-lines-move-between-tables", "crates/kernel/src/customize.rs", "                if let Some(of) = elsewhere {", "                if let Some(of) = elsewhere.filter(|_| false) {", NESTED_LINES),
    ("nested-lines-pack-one-level", "crates/kernel/src/pack.rs", "rows.push(Value::Object(Box::pin(self.portable(ctx, tx, &child, r, keys)).await?));", "rows.push(Value::Object(self.portable_values(ctx, tx, &child, r, keys).await?));", NESTED_LINES),
    # Lines a workspace adds to a package's entity (docs/builder/decisions.md BD-06, 10 November 2026).
    ("uninstall-deletes-custom-record-wholesale", "crates/kernel/src/apps.rs", ".filter(|c| c.table != ferrum_meta::effective::CUSTOM_RECORD_TABLE)", ".filter(|_| true)", PACKAGED_LINES),
    ("uninstall-keeps-packaged-lines", "crates/kernel/src/apps.rs", "         and exists (select 1 from {} p where p.tenant_id = c.tenant_id and p.id = c.parent_id)\",", "         and false and exists (select 1 from {} p where p.tenant_id = c.tenant_id and p.id = c.parent_id)\",", PACKAGED_LINES),
    ("packaged-record-delete-leaves-lines", "crates/kernel/src/store.rs", "    if !ent.custom {\n        sqlx::query(\"delete from custom_record where tenant_id = $1 and parent_id = $2\")", "    if false {\n        sqlx::query(\"delete from custom_record where tenant_id = $1 and parent_id = $2\")", PACKAGED_LINES),
    ("packaged-lines-own-table", "crates/meta/src/effective.rs", "let rows = if f.of.as_deref().is_some_and(|c| custom.contains(c)) {", "let rows = if b.custom {", PACKAGED_LINES),
    ("packaged-lines-two-levels", "crates/meta/src/effective.rs", "        if !b.custom && building.get(of).is_some_and(", "        if false && building.get(of).is_some_and(", PACKAGED_LINES),
    ("packaged-lines-taken-over", "crates/kernel/src/customize.rs", "and not exists (select 1 from {under} r.tenant_id = c.tenant_id and r.id = c.parent_id)", "and false", PACKAGED_LINES),
    ("custom-lines-cascade-gone", "crates/kernel/migrations/20261110000010_packaged_lines.sql", "    delete from custom_record c using gone g where c.tenant_id = g.tenant_id and c.parent_id = g.id;", "    perform 1;", CUSTOM_LINES),
    ("nested-lines-delete-guard-blind","crates/kernel/src/doc.rs", "                        checks.push((sub.table.clone(), f.name.clone(), \"id\", own));", "                        let _ = own;", NESTED_LINES),
    # Configured actions, update_linked, the day in aggregate filters, and the two faults found with them
    # (docs/api.md, Configured actions; docs/builder/decisions.md BD-11).
    ("configured-effects-run-as-the-runner", "crates/kernel/src/actions.rs", "        is_system: true,\n", "        is_system: false,\n", ACTIONS),
    ("configured-roles-ignored", "crates/kernel/src/ctx.rs", "if !def.roles.is_empty() && !def.roles.iter().any(|r| ctx.has_role(r)) {", "if false {", ACTIONS),
    # Changing, removing and describing them (docs/api.md, Configured actions).
    ("package-action-replaced", "crates/meta/src/effective.rs", "                    if packaged.get(&ov.entity).is_some_and(|(d, _)| d.actions.iter().any(|a| &a.name == name)) =>", "                    if false =>", ACTIONS),
    ("action-effects-described-to-anyone", "crates/kernel/src/describe.rs", "                if !ctx.is_admin {\n                    m.remove(\"effects\");", "                if false {\n                    m.remove(\"effects\");", ACTIONS),
    ("may-run-always", "crates/kernel/src/describe.rs", "json!(crate::ctx::action_gate(ctx, ent, a).is_ok())", "json!(true)", ACTIONS),
    ("mail-template-without-params", "crates/kernel/src/automation.rs", "let mut vars: BTreeMap<&str, &Value> = extra.iter().copied().collect();", "let mut vars: BTreeMap<&str, &Value> = BTreeMap::new();", ACTIONS),
    ("configured-refusals-ignored", "crates/kernel/src/actions.rs", "        if holds == Value::Bool(true) {", "        if false && holds == Value::Bool(true) {", ACTIONS),
    ("configured-effects-write-anything", "crates/kernel/src/actions.rs", "    if !own && (f.read_only || f.read_only_if.is_some() || f.perm_group.is_some()) {", "    if false {", ACTIONS),
    ("configured-effects-unchecked", "crates/kernel/src/customize.rs", "        configured_actions(self, ctx.tenant, was, &now)?;\n", "\n", ACTIONS),
    ("update-linked-unlocked-and-unchecked", "crates/kernel/src/automation.rs", "            let d = k.update_tx(ctx, tx, target, id, patch, None).await?;", "            k.set_values_tx(ctx, tx, target, id, patch.clone()).await?;\n            let d = patch;", ACTIONS),
    ("read-only-if-ignored", "crates/kernel/src/doc.rs", "        (Some(expr), Some(doc)) => expr.eval_bool(&Scope { doc, old: None, parent, user }).unwrap_or(true),", "        (Some(_), Some(_)) => false,", ACTIONS),
    ("whole-numbers-unsigned", "crates/meta/src/expr.rs", "            (Some(i), _) => C::Int(i),", "            (Some(i), _) if i < 0 => C::Int(i),", META_EXPR),
    ("today-filter-in-utc", "crates/kernel/src/logic.rs", "a.filter_for(hc.ctx.today(), doc)", "a.filter_for(chrono::Utc::now().date_naive(), doc)", AGGREGATES),
    # Automations that fire on time (docs/api.md, Automations).
    ("scheduled-moment-not-rechecked", "crates/kernel/src/automation.rs", "        if !crate::schedule::still_due(k, ev.tenant_id, &schema, &ent, &plan, record, due).await? {", "        if false {", SCHEDULE),
    ("scheduled-ignores-the-zone", "crates/kernel/src/schedule.rs", "    qb.push_bind(tz.to_string()).push(\"::text as tz, \").push_bind(at)", "    qb.push_bind(\"UTC\".to_string()).push(\"::text as tz, \").push_bind(at)", SCHEDULE),
    ("scheduled-catches-up-forever", "crates/kernel/src/schedule.rs", "let floor = now - chrono::Duration::days(CATCH_UP_DAYS);", "let floor = now - chrono::Duration::days(3650);", SCHEDULE),
    ("scheduled-page-repeats-its-cursor", "crates/kernel/src/schedule.rs", "qb.push(\" and a.id > \").push_bind(c);", "qb.push(\" and a.id >= \").push_bind(c);", SCHEDULE),
    ("scheduled-occurrences-never-pruned", "crates/kernel/src/schedule.rs", "\"delete from automation_occurrence where due_at < now() - make_interval(days => $1)\"", "\"delete from automation_occurrence where false and $1 = 0\"", SCHEDULE),
    ("schedule-takes-any-field", "crates/kernel/src/schedule.rs", "Some(_) => Err(bad(format!(\"{at}: {name} is not a date or datetime field\"))),", "Some(_) => Ok(false),", SCHEDULE),
    ("batch-checked-only-as-a-whole", "crates/kernel/src/customize.rs", "let now = if stepwise { Some(", "let now = if false { Some(", AUTHORING),
    ("dry-run-kept", "crates/kernel/src/customize.rs", "        if dry_run || ids.is_empty() {", "        if ids.is_empty() {", AUTHORING),
    ("redefinition-retypes-values", "crates/kernel/src/customize.rs", "                if holds(tx, ctx.tenant, name, Some(&f.name)).await? {", "                if false {", AUTHORING),
    ("redefinition-changes-kind-under-records", "crates/kernel/src/customize.rs", "if old.def.kind != e.def.kind && holds(", "if false && holds(", AUTHORING),
    # Packs (docs/api.md, Packs).
    ("packs-digest-not-checked", "crates/kernel/src/pack.rs", "        if given != digest {", "        if given.is_empty() {", PACKS),
    ("packs-plan-kept", "crates/kernel/src/pack.rs", "Self::overlay_rows(&mut tx, ctx.tenant).await?);\n        tx.rollback().await?;", "Self::overlay_rows(&mut tx, ctx.tenant).await?);\n        tx.commit().await?;", PACKS),
    ("packs-automations-duplicated", "crates/kernel/src/pack.rs", "from automation where tenant_id = $1 and name = $2 order by", "from automation where tenant_id = $1 and name = $2 and false order by", PACKS),
    ("packs-carry-people", "crates/kernel/src/pack.rs", "    if let Some(why) = not_reference(ctx, &ent) {", "    if let Some(why) = None::<&str> {", PACKS),
    ("packs-role-seeded-by-one-package", "crates/kernel/src/pack.rs", ".filter_map(|(p, _)| catalog.package(p))", ".filter(|(p, _)| *p == pkg).filter_map(|(p, _)| catalog.package(p))", PACKS),
    # Another app's steps from configuration (docs/api.md, Calling another app's steps).
    ("invoke-default-key-ignores-the-version", "crates/kernel/src/actions.rs", "def.name, doc.get(\"version\").unwrap_or(&Value::Null));", "def.name, \"0\");", INVOKE),
    ("invoke-results-not-shown", "crates/kernel/src/actions.rs", "let extra = [(\"params\", &params), (\"results\", &done), (\"links\", &links)];", "let extra = [(\"params\", &params), (\"results\", &Value::Null), (\"links\", &links)];", INVOKE),
    ("invoke-input-names-unchecked", "crates/kernel/src/automation.rs", "if let Some(n) = input.keys().find(", "if let Some(n) = input.keys().filter(|_| false).find(", INVOKE),
    ("invoke-not-rechecked-as-it-runs", "crates/kernel/src/automation.rs", "            check_invoke(k, ctx, &capability, &input)?;\n", "", INVOKE),
    # Service targets (docs/api.md, Service targets).
    ("target-pause-ignored", "crates/logic/src/target.rs", "        if running && said.pause {", "        if false {", LOGIC_UNIT),
    ("target-late-met-in-time", "crates/logic/src/target.rs", "(Some(m), b, _) if b.is_some() ||", "(Some(m), b, _) if false && b.is_some() ||", LOGIC_UNIT),
    ("target-reopen-stays-met", "crates/logic/src/target.rs", "    if r.met_at.is_some() && !said.met {", "    if false {", LOGIC_UNIT),
    ("target-shares-a-field", "crates/meta/src/effective.rs", "            if !kept.insert(name.as_str()) {", "            if !kept.insert(name.as_str()) && false {", META_TARGETS),
    ("target-keeps-a-packages-field", "crates/meta/src/effective.rs", "OverlayOp::AddTarget { target: t } if !b.custom &&", "OverlayOp::AddTarget { target: t } if false &&", META_TARGETS),
    ("target-breached-twice", "crates/kernel/src/targets.rs", "paused_at is null and breached_at is null and due_at <= $1", "paused_at is null and due_at <= $1", TARGETS),
    ("target-fields-take-input", "crates/kernel/src/doc.rs", "                || ent.target_keeping(k).is_some()\n", "\n", TARGETS),
    ("target-calendar-unchecked", "crates/kernel/src/customize.rs", "        crate::targets::check_calendars(tx, ctx.tenant, was, &now).await?;\n", "\n", TARGETS),
    ("target-calendar-deleted-under-it", "crates/kernel/src/process.rs", "if let Some(by) = crate::targets::naming(&ctx.schema, name) {", "if let Some(by) = None::<String> {", TARGETS),
    ("target-rounds-outlive-record", "crates/kernel/src/doc.rs", "\"record_secret\", \"record_target\"]", "\"record_secret\"]", TARGETS),
    ("target-skips-transitions", "crates/kernel/src/doc.rs", "        crate::targets::keep(self, ctx, tx, &ent, id, &mut next).await?;\n", "\n", TARGETS),
    ("target-breach-through-the-rules", "crates/kernel/src/targets.rs", "match self.keep_targets_tx(&system, &mut tx, entity, record).await {", "match self.update(&system, entity, record, Doc::new(), None).await.map(|_| true) {", TARGETS),
    ("app-webhook-hears-what-changed", "crates/kernel/src/outbox.rs", "else $4 - 'doc' - 'was' end", "else $4 - 'doc' end", SERVICES),
    ("automation-old-unseen", "crates/kernel/src/automation.rs", "(&cel_doc, cel_old.as_ref())", "(&cel_doc, None)", TARGETS),
    ("links-read-as-the-saver", "crates/kernel/src/links.rs", "let workspace = Ctx::system(ctx.tenant, ctx.schema.clone());", "let workspace = ctx.clone();", LINKS),
    ("links-keep-secrets", "crates/kernel/src/links.rs", "                    m.remove(&f.name);\n", "\n", LINKS),
    ("links-inbox-guard-evaluated", "crates/kernel/src/collab.rs", "!g.links().is_empty() || g.eval_bool(", "false || g.eval_bool(", LINKS),
    ("links-names-unchecked", "crates/meta/src/catalog.rs", "if let Some(n) = e.links().iter().find(|n| !on.fields", "if let Some(n) = e.links().iter().find(|n| false && !on.fields", META_LINKS),
    ("links-in-browser-expressions", "crates/meta/src/catalog.rs", "    if !reads {\n        return Err(format!(", "    if false {\n        return Err(format!(", META_LINKS),
    ("line-patch-forgets-lines", "crates/kernel/src/automation.rs", "rows.iter().map(|r| serde_json::json!({\"id\": r.get(\"id\")}))", "rows.iter().map(|_| serde_json::json!({}))", LINKS),
    ("set-line-takes-the-first", "crates/kernel/src/automation.rs", "                [i] => (i, Some(", "                [i, ..] => (i, Some(", LINKS),
    ("aggregate-own-field-ignored", "crates/logic/src/solution.rs", "*v = doc.get(f).cloned().unwrap_or(Value::Null);", "*v = Value::Null;", LINKS),
    # Published lists (docs/api.md, Published lists; BD-15).
    ("published-unlisted-field-sent", "crates/kernel/src/published.rs", "let doc = row_to_doc(ent, &schema, row);", "let doc = row_to_doc(ent, &schema, row); let fields: Vec<(&str, Option<&FieldDef>)> = doc.keys().map(|k| (k.as_str(), ent.field(k))).collect();", PUBLISHED),
    ("published-draft-shown", "crates/kernel/src/published.rs", "qb.push(\" and a.docstatus = 1\");", "qb.push(\"\");", PUBLISHED),
    ("published-csv-guard-skipped", "crates/kernel/src/published.rs", "if raw.starts_with(['=', '+', '-', '@', '\\t', '\\r']) && raw.parse::<f64>().is_err()", "if false", PUBLISHED),
    ("published-404s-differ", "crates/kernel/src/published.rs", "let Some(l) = schema.published_list(name) else { return Ok(None) };", "let Some(l) = schema.published_list(name) else { return Err(Error::NotFound(format!(\"list {name}\"))) };", PUBLISHED),
    ("published-inactive-served", "crates/kernel/src/published.rs", ".filter(|(_, status, _)| status == \"active\")", "", PUBLISHED),
    ("published-email-allowed", "crates/meta/src/published.rs", "if matches!(f.ftype, FieldType::Email | FieldType::Phone | FieldType::Secret | FieldType::Attach) {", "if false {", META_PUBLISHED),

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
