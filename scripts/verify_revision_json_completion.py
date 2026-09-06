# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
"""Real CLI regression: LORE_TEST_BINARY=/absolute/path/to/lore python scripts/verify_revision_json_completion.py.

Uses an offline repository and isolated credentials; no server is required.
"""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class RevisionJsonCompletionTests(unittest.TestCase):
    def setUp(self):
        self.binary = str(Path(os.environ["LORE_TEST_BINARY"]).resolve(strict=True))
        self.temporary = tempfile.TemporaryDirectory(prefix="lore-json-completion-")
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.repository = root / "repository"
        self.repository.mkdir()
        self.environment = os.environ.copy()
        self.environment.pop("LORE_USE_SERVICE", None)
        self.environment.pop("LORE_REMOTE_URL", None)
        self.environment.update(
            LORE_GLOBAL_PATH=str(root / "global"),
            LORE_AUTH_PATH=str(root / "auth"),
        )
        self.invoke("repository", "create", "json-regression")
        (self.repository / "asset.txt").write_text("asset revision\n")
        self.invoke("stage", "--scan", ".")

    def invoke(self, *arguments, machine=True, succeeds=True):
        result = subprocess.run(
            [self.binary, "--repository", str(self.repository), "--offline",
             "--identity", "regression-user", "--no-pager",
             *(["--json"] if machine else []), *arguments],
            cwd=self.repository,
            env=self.environment,
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(result.returncode == 0, succeeds, result.stdout + result.stderr)
        if machine:
            events = [json.loads(line) for line in result.stdout.splitlines() if line.strip()]
            completions = [event["data"]["status"] for event in events
                           if event.get("tagName") == "complete"]
            self.assertEqual(len(completions), 1, result.stdout)
            self.assertEqual(completions[0] == 0, succeeds, result.stdout)
        return result.stdout

    def test_commit_has_one_completion(self):
        self.invoke("revision", "commit", "first revision")

    def test_amend_has_one_completion(self):
        self.invoke("revision", "commit", "first revision", machine=False)
        self.invoke("revision", "amend", "amended revision")

    def test_history_has_one_completion(self):
        self.invoke("revision", "commit", "first revision", machine=False)
        self.invoke("revision", "history")

    def test_info_has_one_completion(self):
        self.invoke("revision", "commit", "first revision", machine=False)
        self.invoke("revision", "info")

    def test_operation_failure_is_not_hidden(self):
        self.invoke("revision", "info", "missing-revision", succeeds=False)

    def test_commit_failure_keeps_json_output(self):
        self.invoke("revision", "commit", "first revision", machine=False)
        self.invoke("revision", "commit", "nothing staged", succeeds=False)

    def test_human_commit_still_describes_revision(self):
        output = self.invoke("revision", "commit", "human revision", machine=False)
        self.assertIn("human revision", output)
        self.assertIn("Commit succeeded", output)


if __name__ == "__main__":
    unittest.main()
