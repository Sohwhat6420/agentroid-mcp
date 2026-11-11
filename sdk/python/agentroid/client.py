"""The pipe client. Standard library only; no child processes (so no window can ever open)."""

import json
import threading
import time

PIPE_NAME = r"\\.\pipe\agentroid"

#: A response line longer than this is a protocol error (the daemon caps its own lines too).
MAX_LINE = 16 * 1024 * 1024

_ERROR_PIPE_BUSY = 231


class TransportError(Exception):
    """The pipe could not be opened, closed mid-call, or sent something that is not JSON-RPC."""


class DaemonError(Exception):
    """A JSON-RPC error from the daemon.

    ``message`` is the daemon's text exactly as sent (``str(err)`` returns it unchanged, with no
    prefix); ``code`` is the JSON-RPC code; ``data`` is the optional ``data`` member (for a lease
    refusal, the current lease).
    """

    code = None

    def __init__(self, message, code=None, data=None):
        super().__init__(message)
        self.message = message
        if code is not None:
            self.code = code
        self.data = data

    def __str__(self):
        return self.message


class ParseError(DaemonError):
    code = -32700


class InvalidRequest(DaemonError):
    code = -32600


class MethodNotFound(DaemonError):
    code = -32601


class InvalidParams(DaemonError):
    code = -32602


class InternalError(DaemonError):
    code = -32603


class LeaseHeld(DaemonError):
    """Someone else holds the device's lease; ``data`` is that lease."""

    code = -32001


class NotHolder(DaemonError):
    """The call needs the lease and this connection does not hold it."""

    code = -32002


class Refused(DaemonError):
    """The device-mode gate refused (read_only, the protected phone, the allowlist) or a job is R2+."""

    code = -32003


class DeviceError(DaemonError):
    code = -32004


class UnknownDevice(DaemonError):
    code = -32005


class StateError(DaemonError):
    code = -32006


class JobError(DaemonError):
    """The job's state does not allow this (already ended, another job on the device...)."""

    code = -32007


class UnknownJob(DaemonError):
    code = -32008


_BY_CODE = {
    cls.code: cls
    for cls in (
        ParseError,
        InvalidRequest,
        MethodNotFound,
        InvalidParams,
        InternalError,
        LeaseHeld,
        NotHolder,
        Refused,
        DeviceError,
        UnknownDevice,
        StateError,
        JobError,
        UnknownJob,
    )
}


def error_for(err):
    """The typed exception for a JSON-RPC ``error`` object. The message is never altered."""
    if not isinstance(err, dict):
        return TransportError("malformed JSON-RPC error: %r" % (err,))
    code = err.get("code")
    message = err.get("message")
    if not isinstance(message, str):
        message = "" if message is None else json.dumps(message)
    cls = _BY_CODE.get(code, DaemonError)
    return cls(message, code=code, data=err.get("data"))


class _PipeStream:
    """A byte-mode named pipe opened as a plain unbuffered file."""

    def __init__(self, name, timeout):
        deadline = time.monotonic() + timeout
        while True:
            try:
                self._f = open(name, "r+b", buffering=0)
                break
            except FileNotFoundError:
                raise TransportError("no daemon is listening on %s (is bridged running?)" % name) from None
            except OSError as e:
                if getattr(e, "winerror", None) == _ERROR_PIPE_BUSY and time.monotonic() < deadline:
                    time.sleep(0.05)
                    continue
                raise TransportError("cannot open %s: %s" % (name, e)) from e

    def write(self, data):
        view = memoryview(data)
        while view:
            n = self._f.write(view)
            if not n:
                raise TransportError("the daemon closed the pipe")
            view = view[n:]

    def read(self, n):
        return self._f.read(n)

    def close(self):
        self._f.close()


class Client:
    """One connection to the daemon. The lease belongs to this connection: closing it releases
    every lease it holds (the daemon's rule)."""

    def __init__(self, stream):
        self._s = stream
        self._buf = b""
        self._next = 1
        self._lock = threading.Lock()
        self.hello_result = None

    @classmethod
    def connect(cls, pipe=PIPE_NAME, session=None, timeout=5.0):
        """Open ``pipe`` and say hello. ``session`` becomes the lease holder name."""
        c = cls(_PipeStream(pipe, timeout))
        try:
            c.hello_result = c.hello(session)
        except BaseException:
            c.close()
            raise
        return c

    def close(self):
        self._s.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

    # ------------------------------------------------------------------ the wire

    def _read_line(self):
        while b"\n" not in self._buf:
            chunk = self._s.read(65536)
            if not chunk:
                raise TransportError("the daemon closed the pipe")
            self._buf += chunk
            if len(self._buf) > MAX_LINE:
                raise TransportError("response line longer than %d bytes" % MAX_LINE)
        line, self._buf = self._buf.split(b"\n", 1)
        return line

    def request(self, method, params=None):
        """Send one request and return its ``result``; raise the typed error on an ``error``."""
        with self._lock:
            rid = self._next
            self._next += 1
            msg = {"jsonrpc": "2.0", "id": rid, "method": method, "params": {} if params is None else params}
            self._s.write(json.dumps(msg, ensure_ascii=False).encode("utf-8") + b"\n")
            while True:
                raw = self._read_line()
                try:
                    resp = json.loads(raw.decode("utf-8"))
                except (UnicodeDecodeError, ValueError) as e:
                    raise TransportError("the daemon sent a line that is not JSON: %s" % e) from None
                if not isinstance(resp, dict):
                    raise TransportError("the daemon sent a non-object response")
                if "id" not in resp:
                    continue  # a notification; nothing in v0 sends one
                if resp.get("id") != rid:
                    raise TransportError("response id %r does not match request id %r" % (resp.get("id"), rid))
                if resp.get("error") is not None:
                    raise error_for(resp["error"])
                if "result" not in resp:
                    raise TransportError("response has neither result nor error")
                return resp["result"]

    # ------------------------------------------------------------------ session, devices, lease

    def hello(self, session=None):
        return self.request("hello", {} if session is None else {"session": session})

    def devices(self):
        """The connected devices (rows of ``devices.list``)."""
        return self.request("devices.list", {})["devices"]

    def lease(self, device):
        """Acquire the device's lease for this connection; returns the lease."""
        return self.request("lease.acquire", {"device": device})

    def release(self, device):
        return self.request("lease.release", {"device": device})["released"]

    def lease_status(self, device=None):
        return self.request("lease.status", {} if device is None else {"device": device})["leases"]

    def call(self, device, tool, **args):
        """One core tool call (``device.call``): ``call(dev, "tap", x=10, y=20, space="device")``."""
        c = {"tool": tool}
        c.update(args)
        return self.request("device.call", {"device": device, "call": c})

    # ------------------------------------------------------------------ jobs

    def job_start(self, device, flow, vars=None, oem=None, sdk=None, auto_dismiss=None):
        """Start a flow (a ``schemas/flow.json`` dict, or YAML text). R2+ flows are refused."""
        p = {"device": device, "flow": flow}
        for k, v in (("vars", vars), ("oem", oem), ("sdk", sdk), ("auto_dismiss", auto_dismiss)):
            if v is not None:
                p[k] = v
        return self.request("job.start", p)

    def job_status(self, run_id):
        return self.request("job.status", {"run_id": run_id})

    def job_pause(self, run_id):
        return self.request("job.pause", {"run_id": run_id})

    def job_resume(self, run_id, human_confirmed=False):
        return self.request("job.resume", {"run_id": run_id, "human_confirmed": bool(human_confirmed)})

    def job_stop(self, run_id):
        return self.request("job.stop", {"run_id": run_id})

    def job_receipt(self, run_id):
        return self.request("job.receipt", {"run_id": run_id})

    def job_dry_run(self, device, flow, trees):
        """``trees``: ``[(name, xml), ...]`` or ``[{"name": ..., "xml": ...}, ...]``."""
        rows = [t if isinstance(t, dict) else {"name": t[0], "xml": t[1]} for t in trees]
        return self.request("job.dry_run", {"device": device, "flow": flow, "trees": rows})

    def job_run_script(self, device, script):
        """JavaScript in the daemon's sandbox; every refusal is in the result and the journal."""
        return self.request("job.run_script", {"device": device, "script": script})

    run_script = job_run_script

    def wait_job(self, run_id, timeout=600.0, poll=0.25):
        """Poll ``job.status`` until the job is not running; returns the last status."""
        deadline = time.monotonic() + timeout
        while True:
            s = self.job_status(run_id)
            if s.get("status") != "running":
                return s
            if time.monotonic() >= deadline:
                raise TimeoutError("job %s still running after %.0f s" % (run_id, timeout))
            time.sleep(poll)
