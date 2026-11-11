"""python -m pytest -q test_run_ablation.py (offline: the keep-or-drop walk and the report, no emulator)."""
import json

import run_ablation as ra


def walk(scores):
    lad = ra.Ladder()
    lad.record(lad.baseline(), scores[0], 20)
    for flag, sc in zip(ra.ORDER, scores[1:]):
        lad.record(lad.candidate(flag), sc, 20)
    return lad


def test_a_change_is_kept_only_over_its_threshold_and_later_ones_build_on_what_was_kept():
    lad = walk([8, 11, 12, 13, 13, 12])  # screens +3 kept; planner +1 dropped; verifier +2 over 11 kept; notes tie kept; stall -1 dropped
    assert [s.kept for s in lad.stages] == [True, True, False, True, True, False]
    assert lad.kept == ["screens", "verifier", "notes"]
    assert lad.best == 13
    assert lad.stages[3].flags == ("screens", "verifier"), "verifier tried without the dropped planner"
    assert lad.stages[5].flags == ("screens", "verifier", "notes", "stall")


def test_nothing_kept_leaves_the_baseline_as_best():
    lad = walk([10, 11, 11, 10, 9, 9])
    assert lad.kept == [] and lad.best == 10


def test_the_report_names_every_stage(tmp_path):
    class A:
        model, subset = "opus", "subset-20.json"
    ra.write_report(walk([8, 11, 12, 13, 13, 12]), tmp_path, A)
    j = json.loads((tmp_path / "ABLATION.json").read_text(encoding="utf-8"))
    assert [r["stage"] for r in j["stages"]] == ["A0", "+screens", "+planner", "+verifier", "+notes", "+stall"]
    assert "Kept: screens, verifier, notes. Best: 13." in (tmp_path / "ABLATION.md").read_text(encoding="utf-8")


def test_dry_run_needs_nothing():
    assert ra.main(["--dry-run", "--then-full"]) == 0


def test_resume_replays_scored_stages_and_runs_only_the_rest(tmp_path, monkeypatch):
    class A:
        model, subset = "opus", "subset-20.json"
    lad = ra.Ladder()
    for flag, sc in [(None, 15), ("screens", 16), ("planner", 14), ("verifier", 16)]:
        lad.record(lad.baseline() if flag is None else lad.candidate(flag), sc, 20)
    ra.write_report(lad, tmp_path, A)
    ran = []

    def fake_stage(s, a, out_root, extra=()):
        ran.append((s.label, s.flags, tuple(extra), out_root.name))
        return (17, 20) if s.label != "full-116" else (90, 116)

    monkeypatch.setattr(ra, "run_stage", fake_stage)
    monkeypatch.setattr(ra, "logged_in", lambda: True)
    monkeypatch.setattr(ra, "disk_free_gb", lambda: 200.0)
    monkeypatch.setattr(ra, "memory_pct", lambda: 50.0)
    monkeypatch.setattr(ra, "hidden", lambda *a, **k: type("R", (), {"returncode": 0, "stdout": "{}"})())
    assert ra.main(["--out", str(tmp_path), "--resume", "--then-full", "--bench-approve"]) == 0
    assert ran[0] == ("+notes", ("verifier", "notes"), (), tmp_path.name)
    assert ran[1][:2] == ("+stall", ("verifier", "notes", "stall"))
    assert ran[2][0] == "full-116" and ran[2][2] == ("--bench-approve",) and ran[2][3] == "v3-full-1"
    j = json.loads((tmp_path / "ABLATION.json").read_text(encoding="utf-8"))
    assert [r["stage"] for r in j["stages"]] == ["A0", "+screens", "+planner", "+verifier", "+notes", "+stall"]
