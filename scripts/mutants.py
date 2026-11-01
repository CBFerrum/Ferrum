#!/usr/bin/env python3
"""Deliberately breaks invariants and checks that the tests notice.

A green suite proves little if it stays green with the invariant gone. Each mutant below removes one
guarantee; its test must then fail. A mutant that survives means that test proves nothing, and this
script fails. Needs the test databases, as `cargo test` does. It restores every file it touches, even
when interrupted, and refuses to start on a dirty file.

    scripts/mutants.py            # all of them, 10-20 minutes
    scripts/mutants.py hook       # those whose name contains `hook`
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
# Generous enough to compile and run; passing it means the mutant hung the test.
TIMEOUT = 600

# name, file, text to replace, what breaks it, test
MUTANTS = [
    ("hook-may-call-out", "crates/plugins/src/host.rs", 'if matches!(self.scope, Scope::Hook(_)) => Err(Error::invalid(format!(', "if false => Err(Error::invalid(format!(", PLUGINS),
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
]


def dirty(path: str) -> bool:
    return bool(subprocess.run(["git", "status", "--porcelain", "--", path], cwd=ROOT, capture_output=True, text=True).stdout.strip())


def main() -> int:
    only = sys.argv[1] if len(sys.argv) > 1 else ""
    chosen = [m for m in MUTANTS if only in m[0]]
    for name, path, old, _, _ in chosen:
        if old not in (ROOT / path).read_text():
            print(f"{name}: its text is gone from {path}; update this script", file=sys.stderr)
            return 2
        if dirty(path):
            print(f"{path} has uncommitted changes; commit them first", file=sys.stderr)
            return 2
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
