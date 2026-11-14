"""Offline tests for the CI shard planner and merge (no emulator, no secret).

    python -B ci/test_ci.py      (from bench/androidworld)
"""
from __future__ import annotations

import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
sys.path.insert(0, str(HERE))
import merge_shards  # noqa: E402
import plan_shards  # noqa: E402


class Plan(unittest.TestCase):
    def test_all_covers_the_116_once_in_order(self):
        subset = plan_shards.load_subset()
        shards = plan_shards.plan("all", 13, "run2", subset)
        self.assertEqual(len(shards), 13)
        flat = [n for s in shards for n in s.split(",")]
        self.assertEqual(flat, subset)
        self.assertEqual(len(flat), 116)
        self.assertTrue(all(8 <= len(s.split(",")) <= 9 for s in shards))

    def test_smoke_is_one_shard_and_control_none(self):
        self.assertEqual(plan_shards.plan("smoke", 13, "run3", plan_shards.load_subset()), [",".join(plan_shards.SMOKE)])
        self.assertEqual(plan_shards.plan("all", 13, "control", plan_shards.load_subset()), [])

    def test_an_unknown_task_is_refused(self):
        with self.assertRaises(SystemExit):
            plan_shards.plan("NotATask", 13, "run2", plan_shards.load_subset())


class Merge(unittest.TestCase):
    def merge(self, shards: dict, tasks: str) -> tuple[dict, str]:
        with tempfile.TemporaryDirectory() as d:
            for name, body in shards.items():
                (Path(d) / name).mkdir()
                (Path(d) / name / "results.json").write_text(json.dumps(body), encoding="utf-8")
            buf = io.StringIO()
            with mock.patch.object(sys, "argv", ["merge_shards.py", d, "--config", "run3", "--tasks", tasks]), redirect_stdout(buf):
                merge_shards.main()
            return json.loads((Path(d) / "ci-merged.json").read_text(encoding="utf-8")), buf.getvalue()

    def test_the_headline_is_out_of_the_planned_tasks_and_names_what_did_not_run(self):
        s, md = self.merge({
            "results-0": {"results": [{"task": "AudioRecorderRecordAudio", "success": True, "steps": 3, "approvals": 1}],
                          "meta": {"stopped_before": "CameraTakePhoto"}},
        }, "smoke")
        self.assertEqual((s["planned"], s["passed"], s["passed_guarded"], s["approvals_total"]), (2, 1, 0, 1))
        self.assertEqual(s["not_run"], ["CameraTakePhoto"])
        self.assertIn("1 of 2", md)
        self.assertIn("guarded (no approval): **0**", md)
        self.assertIn("BENCH_STOP: results-0 (before CameraTakePhoto)", md)

    def test_a_shard_that_left_no_results_counts_against_the_score(self):
        s, md = self.merge({}, "smoke")
        self.assertEqual((s["planned"], s["passed"], len(s["not_run"])), (2, 0, 2))
        self.assertIn("0 of 2", md)


class Workflow(unittest.TestCase):
    WORKFLOW = HERE.parents[2] / ".github" / "workflows" / "androidworld.yml"

    def test_an_always_step_after_the_emulator_deletes_the_token_file(self):
        # if the emulator never boots, run_shard.sh never runs its own clean-up: this step must
        text = self.WORKFLOW.read_text(encoding="utf-8")
        written = text.index('> "${RUNNER_TEMP}/claude-sign-in"')
        emulator = text.index("bash bench/androidworld/ci/run_shard.sh")
        step = text.index("- name: Delete the sign-in token file")
        self.assertLess(written, emulator)
        self.assertLess(emulator, step)
        body = text[step:].split("\n      - ", 1)[0]
        self.assertIn("if: always()", body)
        self.assertIn('rm -f "${RUNNER_TEMP}/claude-sign-in"', body)


if __name__ == "__main__":
    unittest.main()
