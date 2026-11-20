#!/usr/bin/env python3
"""scripts/app_ci.sh empties only the test databases of its own runs (`…_localci`): never the development database, the
platform's test database or a walk's, whatever an app's suites name."""
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "app_ci.sh"


def may_reset(name: str) -> bool:
    return subprocess.run(["bash", str(SCRIPT), "--may-reset", name], capture_output=True).returncode == 0


class MayReset(unittest.TestCase):
    def test_its_own_runs_databases_are_emptied(self):
        for name in ["ferrum_developers_test_localci", "ferrum_hr_test_localci", "ferrum_dev_tools_test_localci"]:
            self.assertTrue(may_reset(name), name)

    def test_the_development_test_and_walk_databases_never_are(self):
        for name in ["ferrum_dev", "ferrum_test", "ferrum_walk_localci", "ferrum_crm_walk", "ferrum_hr_test", "ferrum_developers_test", ""]:
            self.assertFalse(may_reset(name), name)


def databases(files: dict) -> list:
    with tempfile.TemporaryDirectory() as app:
        for name, text in files.items():
            (Path(app) / name).parent.mkdir(parents=True, exist_ok=True)
            (Path(app) / name).write_text(text)
        out = subprocess.run(["bash", str(SCRIPT), app, "--databases"], capture_output=True, text=True, check=True).stdout
        return sorted(line.rsplit("/", 1)[1] for line in out.split())


class Databases(unittest.TestCase):
    def test_a_suite_naming_its_database_through_a_helper_gets_one_of_its_own(self):
        found = databases({
            "tests/dump_it.rs": 'let a = url("FERRUM_ERP_DUMP_TEST_DATABASE_URL", "ferrum_erp_dump_test");\n'
            'let b = url(\n    "FERRUM_ERP_RESTORE_TEST_DATABASE_URL",\n    "ferrum_erp_restore_test",\n);\n',
            "tests/kit_it.rs": 'Kit::boot(&database("FERRUM_ERP_TEST_DATABASE_URL"), &[]).await;\n',
        })
        self.assertEqual(found, ["ferrum_erp_dump_test_localci", "ferrum_erp_restore_test_localci", "ferrum_erp_test_localci"])


LOG = """   Compiling ferrum-app-x v0.1.0
     Running unittests src/lib.rs (target/debug/deps/x-1)
test a::ok_one ... ok
test result: ok. 1 passed; 0 failed
     Running tests/dump_it.rs (target/debug/deps/dump_it-2)
test dumps_and_restores ... FAILED
failures:
---- dumps_and_restores stdout ----
thread 'dumps_and_restores' panicked at tests/dump_it.rs:40:5:
connect: database "ferrum_erp_dump_test" does not exist
test result: FAILED. 0 passed; 1 failed
error: test failed, to rerun pass `--test dump_it`
     Running tests/other_it.rs (target/debug/deps/other_it-3)
""" + "".join(f"test t{i} ... ok\n" for i in range(80)) + """test result: ok. 80 passed; 0 failed
error: 1 target failed:
    `--test dump_it`
"""


class Failures(unittest.TestCase):
    def test_a_failing_target_prints_its_own_failure(self):
        out = subprocess.run(["bash", str(SCRIPT), "--failures"], input=LOG, capture_output=True, text=True, check=True).stdout
        self.assertIn('database "ferrum_erp_dump_test" does not exist', out)
        self.assertIn("`--test dump_it`", out)
        self.assertNotIn("t79 ... ok", out)
        self.assertNotIn("ok_one", out)


if __name__ == "__main__":
    unittest.main()
