#!/usr/bin/env python3
"""scripts/secret_paths.py finds who names the vault's tables and its ways in, and an app's own cryptography and unsafe
code, leaving out tests and comments, and refuses to let its baseline grow."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import secret_paths  # noqa: E402

APP = '''
pub async fn careless(k: &Kernel) -> Result<()> {
    // vault.secret and ferrum.vault in a comment are fine
    sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&k.db.pool).await?;
    let rows = sqlx::query("select sealed from \\
        vault.secret").fetch_all(&k.db.pool).await?;
    ferrum_kernel::vault::open(&k.db, opener, false).await?;
    k.vault_set(&ctx, "hr", "bank", value, rule).await?;
    let csp = "default-src 'none'; style-src 'unsafe-inline'";
    unsafe { do_it() };
    Ok(())
}

use ferrum_kernel::vault::{Opened, Opener};
enum Opener { Tcp(String) }

#[cfg(test)]
mod tests {
    fn t() { let _ = "select * from vault.key"; unsafe { std::env::set_var("A", "b") }; }
}
'''

with tempfile.TemporaryDirectory() as d:
    app = Path(d) / "app"
    (app / "src").mkdir(parents=True)
    (app / "src/lib.rs").write_text(APP)
    (app / "tests").mkdir()
    (app / "tests/it.rs").write_text('fn t() { let _ = "select * from vault.root_slot"; }')
    refusals, crypto, unsafe = secret_paths.scan([("ferrum-app-x", app, True, ["serde", "ring", "sha2"])])
    lines = sorted(int(r.split(":")[2]) for r in refusals)
    assert lines == [4, 6, 7, 8, 14], refusals  # the import, not an enum of its own named Opener
    assert crypto == {("ferrum-app-x", "ring")}, "a hash crate is not counted"
    assert unsafe == {"ferrum-app-x:src/lib.rs": 1}, "a policy's 'unsafe-inline' is not code, nor a test's block"

    # The platform's own code may call the ways in; only the vault's module names its tables.
    platform = Path(d) / "platform"
    (platform / "src").mkdir(parents=True)
    (platform / "src/lib.rs").write_text('fn f() { k.vault_set(&ctx, "crm", "x", v, r); let _ = "from vault.key"; }')
    refusals, _, unsafe = secret_paths.scan([("ferrum-server", platform, False, ["ring"])])
    assert len(refusals) == 1 and "tables" in refusals[0], refusals

    # The baseline only shrinks.
    secret_paths.BASELINE = Path(d) / "secret-paths.md"
    secret_paths.write_baseline({("ferrum-app-x", "ring")}, {"x:src/a.rs": 2})
    assert secret_paths.read_baseline() == ({("ferrum-app-x", "ring")}, {"x:src/a.rs": 2})

print("secret_paths: ok")
