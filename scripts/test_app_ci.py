#!/usr/bin/env python3
"""scripts/app_ci.sh empties only the test databases of its own runs (`…_localci`): never the development database, the
platform's test database or a walk's, whatever an app's suites name."""
import subprocess
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


if __name__ == "__main__":
    unittest.main()
