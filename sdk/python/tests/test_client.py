"""SDK tests against a fake pipe server (a real Windows named pipe with a test-only name).

The real daemon pipe, \\\\.\\pipe\\agentroid, is never opened: every test uses a unique name and
the module's ``open`` is wrapped so that opening the real pipe fails the test.
"""

import builtins
import json
import os
import sys
import threading
import unittest
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import agentroid  # noqa: E402
from agentroid import client as client_mod  # noqa: E402

if sys.platform == "win32":
    import _winapi
else:  # pragma: no cover
    _winapi = None

REAL_PIPE = agentroid.PIPE_NAME
# winbase.h; _winapi exports only the message-mode names
PIPE_TYPE_BYTE = 0x0
PIPE_READMODE_BYTE = 0x0
POCO = "DEVICE0000EXAMPLE"

# Real refusal texts from bridged (and some hostile ones): quotes, backslashes, non-ASCII,
# astral-plane characters, control characters, a trailing space, JSON-looking text.
REFUSALS = [
    'device DEVICE0000EXAMPLE is leased by "claude-a" (epoch 3); ask them to release it or wait',
    "tap refused: this device is read_only, so it only answers reads",
    "job refused before start: step wipe is R2; Phase 2 refuses R2 and above with no exception",
    "refused: path C:\\Users\\alex\\x \u2014 caf\u00e9 \u00fcber \u65e5\u672c \U0001F4F1",
    "line one\nline two\ttabbed\r\n\u0007 bell, trailing space ",
    '{"not": "json to parse"} \\u00e9 stays six characters',
    "",
]


def _guarded_open(name, *a, **kw):
    if isinstance(name, str) and name.lower().rstrip("\\") == REAL_PIPE.lower():
        raise AssertionError("a test tried to open the real daemon pipe")
    return builtins.open(name, *a, **kw)


class FakeDaemon:
    """A named-pipe server speaking line JSON-RPC. ``handlers[method](params, conn)`` returns
    ``("result", value)``, ``("error", obj)`` or ``("raw", bytes)`` (sent as-is)."""

    def __init__(self, handlers):
        self.name = r"\\.\pipe\agentroid-sdk-test-%d-%s" % (os.getpid(), uuid.uuid4().hex)
        assert self.name.lower() != REAL_PIPE.lower()
        self.handlers = handlers
        self.requests = []
        self.raw_lines = []
        self._stop = False
        self._handle = self._instance()
        self._t = threading.Thread(target=self._serve, daemon=True)
        self._t.start()

    def _instance(self):
        return _winapi.CreateNamedPipe(
            self.name,
            _winapi.PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | _winapi.PIPE_WAIT,
            1,
            65536,
            65536,
            0,
            _winapi.NULL,
        )

    def _serve(self):
        conn = 0
        while not self._stop:
            h = self._handle
            try:
                _winapi.ConnectNamedPipe(h, False)
            except OSError as e:
                if getattr(e, "winerror", None) != 535:  # ERROR_PIPE_CONNECTED
                    _winapi.CloseHandle(h)
                    return
            conn += 1
            if self._stop:
                _winapi.CloseHandle(h)
                return
            try:
                self._conversation(h, conn)
            finally:
                _winapi.CloseHandle(h)  # disconnects the client too
            if not self._stop:
                self._handle = self._instance()

    def _conversation(self, h, conn):
        buf = b""
        while True:
            try:
                data, _err = _winapi.ReadFile(h, 65536, False)
            except OSError:
                return
            if not data:
                return
            buf += data
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                self.raw_lines.append(line)
                req = json.loads(line.decode("utf-8"))
                self.requests.append(req)
                kind, val = self.handlers.get(req["method"], lambda p, c: ("error", {"code": -32601, "message": "method not found: " + req["method"]}))(req.get("params"), conn)
                if kind == "raw":
                    out = val
                elif kind == "result":
                    out = json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": val}).encode("utf-8") + b"\n"
                else:
                    out = json.dumps({"jsonrpc": "2.0", "id": req["id"], "error": val}).encode("utf-8") + b"\n"
                try:
                    _winapi.WriteFile(h, out, False)
                except OSError:
                    return

    def close(self):
        if self._stop:
            return
        self._stop = True
        try:  # unblock ConnectNamedPipe
            builtins.open(self.name, "r+b", buffering=0).close()
        except OSError:
            pass
        self._t.join(5)


def hello(params, conn):
    return ("result", {"name": "bridged", "version": "test", "holder": (params or {}).get("session") or "conn-%d" % conn, "capabilities": []})


@unittest.skipUnless(sys.platform == "win32", "named pipes are Windows-only")
class ClientTests(unittest.TestCase):
    def setUp(self):
        client_mod.open = _guarded_open
        self.addCleanup(delattr, client_mod, "open")

    def serve(self, **handlers):
        handlers.setdefault("hello", hello)
        s = FakeDaemon(handlers)
        self.addCleanup(s.close)  # cleanups run last-in first-out: clients close before servers
        return s

    def connect(self, server, session="sdk-test"):
        c = agentroid.Client.connect(server.name, session=session)
        self.addCleanup(c.close)
        return c

    # ------------------------------------------------------------------ the brief's test

    def test_refusal_text_passes_through_byte_for_byte(self):
        """Every refusal message reaches the caller unchanged, whichever way the JSON escapes it."""
        for ensure_ascii in (True, False):
            for code in (-32003, -32002, -32001, -32007, -32099):
                for text in REFUSALS:
                    wire = json.dumps({"code": code, "message": text, "data": {"device": POCO}}, ensure_ascii=ensure_ascii)
                    s = self.serve(**{"device.call": lambda p, c, wire=wire: ("raw", ('{"jsonrpc":"2.0","id":2,"error":%s}\n' % wire).encode("utf-8"))})
                    c = self.connect(s)
                    with self.assertRaises(agentroid.DaemonError) as cm:
                        c.call(POCO, "tap", x=1, y=2, space="device")
                    e = cm.exception
                    self.assertEqual(e.message, text)
                    self.assertEqual(str(e), text, "str() adds nothing")
                    self.assertEqual(e.message.encode("utf-8"), text.encode("utf-8"))
                    self.assertEqual(e.args, (text,))
                    self.assertEqual(e.code, code)
                    self.assertEqual(e.data, {"device": POCO})
                    c.close()
                    s.close()

    def test_errors_are_typed_by_code(self):
        expect = {
            -32700: agentroid.ParseError,
            -32600: agentroid.InvalidRequest,
            -32601: agentroid.MethodNotFound,
            -32602: agentroid.InvalidParams,
            -32603: agentroid.InternalError,
            -32001: agentroid.LeaseHeld,
            -32002: agentroid.NotHolder,
            -32003: agentroid.Refused,
            -32004: agentroid.DeviceError,
            -32005: agentroid.UnknownDevice,
            -32006: agentroid.StateError,
            -32007: agentroid.JobError,
            -32008: agentroid.UnknownJob,
        }
        for code, cls in expect.items():
            e = agentroid.error_for({"code": code, "message": "m %d" % code})
            self.assertIs(type(e), cls)
            self.assertIsInstance(e, agentroid.DaemonError)
            self.assertEqual((e.code, str(e)), (code, "m %d" % code))
        e = agentroid.error_for({"code": -32050, "message": "other"})
        self.assertIs(type(e), agentroid.DaemonError)
        self.assertEqual(e.code, -32050)

    # ------------------------------------------------------------------ the methods on the wire

    def test_every_method_sends_the_daemon_shape(self):
        flow = {"flow": "f", "version": 1, "app": {"package": "p", "version_range": "*"}, "steps": []}
        results = {
            "devices.list": {"devices": [{"device": POCO, "mode": "full"}]},
            "lease.acquire": {"device": POCO, "holder": "sdk-test", "epoch": 4},
            "lease.release": {"released": True},
            "lease.status": {"leases": []},
            "device.call": {"tool": "look", "text": "ok"},
            "job.start": {"run_id": "job-1-0", "status": "running", "journal": "j", "progress": "p"},
            "job.status": {"run_id": "job-1-0", "status": "done"},
            "job.pause": {"run_id": "job-1-0", "status": "paused"},
            "job.resume": {"run_id": "job-1-0", "status": "running"},
            "job.stop": {"run_id": "job-1-0", "status": "stopped", "journal_seq": 3},
            "job.receipt": {"run_id": "job-1-0", "receipt": {}, "detail": {}},
            "job.dry_run": {"flow": "f", "matched": 0, "rows": [], "overlays": [], "text": ""},
            "job.run_script": {"run_id": "script-1-0", "outcome": "ok", "refusals": []},
        }
        s = self.serve(**{m: (lambda p, c, r=r: ("result", r)) for m, r in results.items()})
        c = self.connect(s)
        self.assertEqual(c.hello_result["holder"], "sdk-test")
        self.assertEqual(c.devices(), results["devices.list"]["devices"])
        self.assertEqual(c.lease(POCO)["epoch"], 4)
        self.assertEqual(c.lease_status(), [])
        self.assertEqual(c.call(POCO, "tap", x=1, y=2, space="device")["text"], "ok")
        self.assertEqual(c.job_start(POCO, flow, vars={"a": 1}, oem="xiaomi")["run_id"], "job-1-0")
        self.assertEqual(c.job_status("job-1-0")["status"], "done")
        c.job_pause("job-1-0")
        c.job_resume("job-1-0", human_confirmed=True)
        self.assertEqual(c.job_stop("job-1-0")["journal_seq"], 3)
        c.job_receipt("job-1-0")
        c.job_dry_run(POCO, flow, [("settings", "<hierarchy/>")])
        self.assertEqual(c.run_script(POCO, "1 + 1")["outcome"], "ok")
        self.assertTrue(c.release(POCO))
        self.assertEqual(c.wait_job("job-1-0")["status"], "done")
        sent = [(r["method"], r["params"]) for r in s.requests]
        self.assertEqual(sent[0], ("hello", {"session": "sdk-test"}))
        self.assertIn(("device.call", {"device": POCO, "call": {"tool": "tap", "x": 1, "y": 2, "space": "device"}}), sent)
        self.assertIn(("job.start", {"device": POCO, "flow": flow, "vars": {"a": 1}, "oem": "xiaomi"}), sent)
        self.assertIn(("job.resume", {"run_id": "job-1-0", "human_confirmed": True}), sent)
        self.assertIn(("job.dry_run", {"device": POCO, "flow": flow, "trees": [{"name": "settings", "xml": "<hierarchy/>"}]}), sent)
        self.assertIn(("job.run_script", {"device": POCO, "script": "1 + 1"}), sent)
        for r in s.requests:
            self.assertEqual(r["jsonrpc"], "2.0")
        ids = [r["id"] for r in s.requests]
        self.assertEqual(ids, sorted(set(ids)), "ids are unique and increasing")
        for line in s.raw_lines:
            self.assertNotIn(b"\n", line)

    def test_a_job_refusal_from_the_lease_check_is_not_holder_with_the_text(self):
        text = 'device DEVICE0000EXAMPLE is leased by "claude-a" (epoch 3); this connection is not the holder'
        s = self.serve(**{"job.status": lambda p, c: ("error", {"code": -32002, "message": text})})
        c = self.connect(s)
        with self.assertRaises(agentroid.NotHolder) as cm:
            c.job_status("job-1-0")
        self.assertEqual(str(cm.exception), text)

    # ------------------------------------------------------------------ framing and failures

    def test_split_and_coalesced_frames_and_notifications(self):
        def chatty(p, c):
            # a notification first, then the response; one write
            return ("raw", b'{"jsonrpc":"2.0","method":"note","params":{}}\n{"jsonrpc":"2.0","id":2,"result":{"devices":["\xc3\xa9"]}}\n')

        s = self.serve(**{"devices.list": chatty})
        c = self.connect(s)
        self.assertEqual(c.devices(), ["\u00e9"])

    def test_a_mismatched_id_or_garbage_is_a_transport_error(self):
        s = self.serve(**{"devices.list": lambda p, c: ("raw", b'{"jsonrpc":"2.0","id":99,"result":{}}\n')})
        c = self.connect(s)
        with self.assertRaises(agentroid.TransportError):
            c.devices()
        s2 = self.serve(**{"devices.list": lambda p, c: ("raw", b"not json\n")})
        c2 = self.connect(s2)
        with self.assertRaises(agentroid.TransportError):
            c2.devices()

    def test_a_missing_pipe_is_a_transport_error(self):
        name = r"\\.\pipe\agentroid-sdk-test-absent-%s" % uuid.uuid4().hex
        with self.assertRaises(agentroid.TransportError) as cm:
            agentroid.Client.connect(name)
        self.assertIn("no daemon", str(cm.exception))

    def test_the_guard_refuses_the_real_pipe(self):
        with self.assertRaises(AssertionError):
            agentroid.Client.connect(REAL_PIPE)

    def test_the_sdk_is_standard_library_only(self):
        import ast

        stdlib = set(sys.stdlib_module_names)
        pkg = os.path.join(os.path.dirname(HERE), "agentroid")
        for fn in os.listdir(pkg):
            if not fn.endswith(".py"):
                continue
            with open(os.path.join(pkg, fn), encoding="utf-8") as f:
                tree = ast.parse(f.read())
            for node in ast.walk(tree):
                if isinstance(node, ast.Import):
                    names = [a.name for a in node.names]
                elif isinstance(node, ast.ImportFrom) and node.level == 0:
                    names = [node.module]
                else:
                    continue
                for n in names:
                    self.assertIn(n.split(".")[0], stdlib, "%s imports %s" % (fn, n))
                self.assertNotIn("subprocess", names, "the SDK starts no processes")


if __name__ == "__main__":
    unittest.main()
