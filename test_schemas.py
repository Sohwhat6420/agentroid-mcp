"""Checks every schema is a valid draft 2020-12 schema, every valid-*.json
fixture passes and every invalid-*.json fixture fails. Uses the jsonschema
package (importable here, v4.26)."""
import base64
import binascii
import hashlib
import json
import unittest
from pathlib import Path

from jsonschema import Draft202012Validator

ROOT = Path(__file__).parent / "schemas"
FIXTURES = ROOT / "fixtures"
NAMES = sorted(p.name for p in FIXTURES.iterdir() if p.is_dir())


def load(p):
    return json.loads(p.read_text(encoding="utf-8"))


class SchemaTests(unittest.TestCase):
    def test_every_schema_has_fixture_dir_and_vice_versa(self):
        schemas = sorted(p.stem for p in ROOT.glob("*.json"))
        self.assertEqual(schemas, NAMES)

    def test_expected_formats_present(self):
        for n in ["flow", "journal", "popup", "leases", "progress", "receipt",
                  "job-view", "segment", "profile", "lease-bundle", "link"]:
            self.assertIn(n, NAMES)


def popup_sdk_order(obj):
    """JSON Schema cannot compare two fields, so popup sdk_min <= sdk_max is checked here."""
    if "sdk_min" in obj and "sdk_max" in obj and obj["sdk_min"] > obj["sdk_max"]:
        return ["sdk_min is above sdk_max"]
    return []


def link_cross_checks(obj):
    """Rules of link.json that JSON Schema cannot state: nonces decode to 32..64 random bytes,
    the carried JSON-RPC id equals the frame id, refused req_epoch is on the right side of epoch,
    a segment frame carries a valid schemas/segment.json object, and human_touch has touch_ms <= sent_ms."""
    errs = []
    for k in ("nonce", "phone_nonce"):
        if isinstance(obj.get(k), str):
            try:
                n = len(base64.b64decode(obj[k], validate=True))
                if not 32 <= n <= 64:
                    errs.append(f"{k} decodes to {n} bytes, not 32..64")
            except (binascii.Error, ValueError):
                errs.append(f"{k} is not base64")
    rpc = obj.get("rpc")
    if obj.get("t") in ("request", "response") and isinstance(rpc, dict) and rpc.get("id") != obj.get("id"):
        errs.append("rpc.id differs from id")
    if obj.get("t") == "segment" and isinstance(obj.get("segment"), dict):
        # the carried segment is a whole schemas/segment.json object (format v1, task 2.14)
        seg_errs = sorted(e.message[:90] for e in Draft202012Validator(load(ROOT / "segment.json")).iter_errors(obj["segment"]))
        errs += ["segment: " + m for m in seg_errs]
    if obj.get("t") == "refused" and isinstance(obj.get("req_epoch"), int) and isinstance(obj.get("epoch"), int):
        if obj.get("reason") == "stale_epoch" and not obj["req_epoch"] < obj["epoch"]:
            errs.append("stale_epoch needs req_epoch < epoch")
        if obj.get("reason") == "epoch_ahead" and not obj["req_epoch"] > obj["epoch"]:
            errs.append("epoch_ahead needs req_epoch > epoch")
    if obj.get("t") == "human_touch" and isinstance(obj.get("touch_ms"), int) and isinstance(obj.get("sent_ms"), int):
        if obj["sent_ms"] < obj["touch_ms"]:
            errs.append("human_touch needs touch_ms <= sent_ms")
    return errs


EXTRA_CHECKS = {"popup": popup_sdk_order, "link": link_cross_checks}

LINK_FRAMES = Path(__file__).parent / "android" / "out" / "link-frames"


def _key_id(spki_b64):
    return hashlib.sha256(base64.b64decode(spki_b64)).hexdigest()


def _verify(spki_b64, sig_b64, text):
    """Independent of the Java code: rebuilds the key from SPKI and checks a DER ECDSA P-256/SHA-256 signature."""
    from cryptography.exceptions import InvalidSignature
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    key = serialization.load_der_public_key(base64.b64decode(spki_b64))
    assert isinstance(key, ec.EllipticCurvePublicKey) and key.curve.name == "secp256r1"
    try:
        key.verify(base64.b64decode(sig_b64), text.encode("utf-8"), ec.ECDSA(hashes.SHA256()))
        return True
    except InvalidSignature:
        return False


def link_invalid_reasons():
    """(fixture, reasons) for each invalid link fixture, validated against the $def chosen by its 't'
    ('v' and unknown 't' are judged by the root schema)."""
    schema = load(ROOT / "link.json")
    by_t = {d["properties"]["t"]["const"]: k for k, d in schema["$defs"].items()
            if isinstance(d, dict) and "t" in d.get("properties", {})}
    out = []
    for f in sorted((FIXTURES / "link").glob("invalid-*.json")):
        obj = load(f)
        if obj.get("t") in by_t and obj.get("v") == 1:
            sub = Draft202012Validator({"$defs": schema["$defs"], "$ref": "#/$defs/" + by_t[obj["t"]]})
            why = sorted({"/".join(map(str, e.absolute_path)) + ": " + e.message[:90] for e in sub.iter_errors(obj)})
        else:
            why = ["v/t: " + e.message[:60] for e in Draft202012Validator(schema).iter_errors(obj)]
        out.append((f, why + link_cross_checks(obj)))
    return out


class LinkHandshakeTests(unittest.TestCase):
    """The signed layouts written in link.json, checked against a real LinkClient/FakeHub transcript."""

    def transcripts(self):
        out = [FIXTURES / "link" / "transcript.json"]
        if (LINK_FRAMES / "transcript.json").exists():
            out.append(LINK_FRAMES / "transcript.json")
        return out

    def test_signatures_verify_with_the_documented_layout(self):
        try:
            import cryptography  # noqa: F401
        except ImportError:
            self.skipTest("cryptography not installed")
        for p in self.transcripts():
            with self.subTest(transcript=str(p)):
                t = load(p)
                hello, ch, resp = t["hello"], t["challenge"], t["challenge_response"]
                phone_id, hub_id = _key_id(t["phone_public_key"]), _key_id(t["hub_public_key"])
                self.assertEqual(hello["key_id"], phone_id)
                self.assertEqual(resp["key_id"], phone_id)
                self.assertEqual(ch["hub_key_id"], hub_id)
                self.assertEqual(ch["phone_nonce"], hello["phone_nonce"])
                self.assertEqual(resp["nonce"], ch["nonce"])
                hub_text = (f"agentroid-link/v1/hub-challenge\nnonce={ch['nonce']}\nphone_nonce={ch['phone_nonce']}"
                            f"\nepoch={ch['epoch']}\nserial={ch['serial']}\nphone_key_id={phone_id}")
                phone_text = (f"agentroid-link/v1/phone-response\nnonce={ch['nonce']}\nphone_nonce={ch['phone_nonce']}"
                              f"\nepoch={resp['epoch']}\nserial={t['serial']}\nhub_key_id={hub_id}")
                self.assertTrue(_verify(t["hub_public_key"], ch["hub_sig"], hub_text))
                self.assertTrue(_verify(t["phone_public_key"], resp["signature"], phone_text))
                # domain separation: neither signature verifies as the other message, or with a changed epoch
                self.assertFalse(_verify(t["phone_public_key"], resp["signature"],
                                         phone_text.replace("phone-response", "hub-challenge")))
                self.assertFalse(_verify(t["phone_public_key"], resp["signature"],
                                         phone_text.replace(f"epoch={resp['epoch']}", f"epoch={resp['epoch'] + 1}")))
                self.assertFalse(_verify(t["hub_public_key"], ch["hub_sig"], hub_text + "\n"))

    def test_frames_dumped_by_the_jvm_tests_match_the_schema(self):
        frames = sorted(LINK_FRAMES.glob("[0-9][0-9]-*.json"))
        if not frames:
            self.skipTest("run scaffold/android/run-tests.sh first to dump real frames")
        v = Draft202012Validator(load(ROOT / "link.json"))
        for f in frames:
            with self.subTest(frame=f.name):
                obj = load(f)
                errs = [e.message for e in v.iter_errors(obj)] + link_cross_checks(obj)
                self.assertEqual(errs, [])

    def test_each_invalid_fixture_fails_inside_its_own_message_type(self):
        """The top-level oneOf would also reject a frame for matching no branch at all; this pins every
        invalid fixture to a failure inside the $def its 't' selects (or a cross-check)."""
        for f, why in link_invalid_reasons():
            with self.subTest(fixture=f.name):
                self.assertTrue(why, "fails only because of the top-level oneOf")

    def test_a_segment_frame_must_carry_a_valid_segment_json_object(self):
        """The frame schema checks only the envelope; segment.json itself is applied by the cross-check."""
        frame = load(FIXTURES / "link" / "valid-segment.json")
        self.assertEqual(link_cross_checks(frame), [])
        for bad in sorted((FIXTURES / "segment").glob("invalid-*.json")):
            with self.subTest(segment=bad.name):
                f = dict(frame, segment=load(bad))
                self.assertTrue(link_cross_checks(f), bad.name + " passed inside a segment frame")
        for ok in sorted((FIXTURES / "segment").glob("valid-*.json")):
            with self.subTest(segment=ok.name):
                self.assertEqual(link_cross_checks(dict(frame, segment=load(ok))), [])

    def test_every_message_type_has_a_valid_fixture(self):
        schema = load(ROOT / "link.json")
        types = {d["properties"]["t"]["const"] for d in schema["$defs"].values()
                 if isinstance(d, dict) and "t" in d.get("properties", {})}
        seen = {load(f)["t"] for f in (FIXTURES / "link").glob("valid-*.json")}
        self.assertEqual(types, seen)


def make(name):
    schema = load(ROOT / (name + ".json"))
    valid = sorted((FIXTURES / name).glob("valid-*.json"))
    invalid = sorted((FIXTURES / name).glob("invalid-*.json"))
    extra_check = EXTRA_CHECKS.get(name, lambda obj: [])

    def test_schema_ok(self):
        Draft202012Validator.check_schema(schema)

    def test_counts(self):
        self.assertGreaterEqual(len(valid), 2)
        self.assertGreaterEqual(len(invalid), 2)

    def test_valid(self):
        v = Draft202012Validator(schema)
        for f in valid:
            with self.subTest(fixture=f.name):
                errs = list(v.iter_errors(load(f)))
                self.assertEqual(errs, [], [e.message for e in errs])
                extra = extra_check(load(f))
                self.assertEqual(extra, [], extra)

    def test_invalid(self):
        v = Draft202012Validator(schema)
        for f in invalid:
            with self.subTest(fixture=f.name):
                failed = list(v.iter_errors(load(f))) or extra_check(load(f))
                self.assertTrue(failed, "should have failed")

    return {"test_schema_ok": test_schema_ok, "test_counts": test_counts,
            "test_valid": test_valid, "test_invalid": test_invalid}


for _n in NAMES:
    _cls = "Test_" + _n.replace("-", "_")
    globals()[_cls] = type(_cls, (unittest.TestCase,), make(_n))

if __name__ == "__main__":
    unittest.main()
