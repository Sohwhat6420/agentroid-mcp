# Agentroid JSON Schemas

One JSON Schema (draft 2020-12) per file format, in `schemas/<name>.json`. Fixtures are in `schemas/fixtures/<name>/`: `valid-*.json` must pass and `invalid-*.json` must fail, and each invalid fixture is named for the rule it breaks.

| Schema | What it describes |
| --- | --- |
| `flow.json` | A replayable flow (the YAML a `job_start` runs), checked as the equivalent JSON object |
| `journal.json` | One line of a run journal, written before each acting step is sent |
| `popup.json` | A known pop-up and how a job handles it, scoped by phone maker and Android version |
| `leases.json` | Which session holds which device |
| `registry-lease.json` | One device lease: `{device, holder, epoch}` |
| `lease-bundle.json` | The human and agent settings profiles applied and restored around a lease |
| `progress.json` | One NDJSON progress event from a running job |
| `receipt.json` | The receipt attached to every tool result (calls, wall time, retries, errors) |
| `job-view.json` | The state the job view page renders |
| `segment.json` | A signed segment of steps, with a risk class per step |
| `profile.json` | A device profile recorded by `agentroid-setup` |
| `safe-classes.json` | The step kinds and packages allowed per risk class (R0 and R1 only) |
| `link.json` | Messages on the phone link between the helper app and the laptop |

YAML formats (flows, pop-ups) are validated as the JSON object a YAML loader produces.

## Test

From the repository root:

```
pip install jsonschema cryptography
python -B test_schemas.py
```

The tests check that every schema is a valid 2020-12 schema, that every schema has a fixture folder and the reverse, that each has at least two valid and two invalid fixtures, and that every valid fixture passes and every invalid one fails. They do not check that an invalid fixture fails for the rule in its name.
