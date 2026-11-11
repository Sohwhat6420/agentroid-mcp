"""Task 2.10b: the receipts the runner writes validate against schemas/receipt.json, with the
same jsonschema (draft 2020-12) check as test_schemas.py.

The samples are written by the Rust tests (run `cargo test --offline --workspace` first):
  generated/receipt-sample.json         runner::tests_b (engine + journal + receipt::build)
  generated/receipt-daemon-sample.json  tests/jobs.rs (job.start -> job.receipt over JSON-RPC)
"""
import json
import unittest
from pathlib import Path

from jsonschema import Draft202012Validator, FormatChecker

ROOT = Path(__file__).parent
SCHEMA = json.loads((ROOT / "schemas" / "receipt.json").read_text(encoding="utf-8"))
SAMPLES = ["receipt-sample.json", "receipt-daemon-sample.json"]


def card(doc):
    """The receipt.json card: the stored receipt is {"receipt": card, "detail": panel extras}."""
    if set(doc) == {"receipt", "detail"}:
        assert isinstance(doc["detail"], dict) and "outcome" in doc["detail"], doc["detail"]
        return doc["receipt"]
    return doc


class RunnerReceiptTests(unittest.TestCase):
    def test_samples_validate_against_receipt_json(self):
        Draft202012Validator.check_schema(SCHEMA)
        v = Draft202012Validator(SCHEMA, format_checker=FormatChecker())
        for name in SAMPLES:
            p = ROOT / "generated" / name
            self.assertTrue(p.exists(), "%s missing: run cargo test --offline --workspace first" % p)
            doc = card(json.loads(p.read_text(encoding="utf-8")))
            errors = [e.message for e in v.iter_errors(doc)]
            self.assertEqual(errors, [], name)

    def test_a_broken_receipt_is_caught(self):
        doc = card(json.loads((ROOT / "generated" / SAMPLES[0]).read_text(encoding="utf-8")))
        v =Draft202012Validator(SCHEMA)
        bad = dict(doc, outcome="done")  # the panel-only fields belong in `detail`, not the card
        self.assertTrue(list(v.iter_errors(bad)))
        bad = {k: x for k, x in doc.items() if k != "run_id"}
        self.assertTrue(list(v.iter_errors(bad)))


if __name__ == "__main__":
    unittest.main()
