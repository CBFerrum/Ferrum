#!/usr/bin/env python3
"""scripts/mutation_paths.py finds an app's direct writes, by the function they are in, and leaves out
tests and comments."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mutation_paths  # noqa: E402

SOURCE = '''
pub async fn start(tx: &mut Tx) -> Result<()> {
    // update e_not_this: a comment
    sqlx::query("INSERT  INTO e_campaign_recipient (id) values ($1)").execute(tx).await?;
    let send = |r| async move {
        sqlx::query("update e_campaign_recipient set status = 'Queued'").execute(r).await
    };
    Ok(())
}

fn finish() {
    let _ = "delete from e_email_suppression where id = $1";
}

#[cfg(test)]
mod tests {
    fn writes() { let _ = "update e_test_only set x = 1"; }
}
'''

with tempfile.TemporaryDirectory() as d:
    root = Path(d)
    (root / "src").mkdir()
    (root / "src/campaign.rs").write_text(SOURCE)
    (root / "tests").mkdir()
    (root / "tests/it.rs").write_text('fn t() { let _ = "update e_in_tests set x = 1"; }')
    found = [(f, w, fn) for _, f, w, fn, _ in mutation_paths.statements("campaigns", root)]
    assert found == [
        ("src/campaign.rs", "insert into e_campaign_recipient", "start"),
        ("src/campaign.rs", "update e_campaign_recipient", "start"),
        ("src/campaign.rs", "delete from e_email_suppression", "finish"),
    ], found
print("ok")
