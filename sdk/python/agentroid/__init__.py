"""Agentroid Python SDK: a small client for the bridged daemon's named pipe.

Standard library only. One JSON-RPC 2.0 document per line over ``\\\\.\\pipe\\agentroid``.

    from agentroid import Client
    with Client.connect(session="my-script") as c:
        dev = c.devices()[0]["device"]
        c.lease(dev)
        c.call(dev, "look")
        job = c.job_start(dev, flow)
        print(c.wait_job(job["run_id"]))

Every daemon refusal is raised as a typed :class:`DaemonError` subclass whose ``message`` (and
``str(err)``) is the daemon's text exactly as it was sent.
"""

from .client import (
    PIPE_NAME,
    Client,
    DaemonError,
    DeviceError,
    InternalError,
    InvalidParams,
    InvalidRequest,
    JobError,
    LeaseHeld,
    MethodNotFound,
    NotHolder,
    ParseError,
    Refused,
    StateError,
    TransportError,
    UnknownDevice,
    UnknownJob,
    error_for,
)

__all__ = [
    "PIPE_NAME",
    "Client",
    "DaemonError",
    "DeviceError",
    "InternalError",
    "InvalidParams",
    "InvalidRequest",
    "JobError",
    "LeaseHeld",
    "MethodNotFound",
    "NotHolder",
    "ParseError",
    "Refused",
    "StateError",
    "TransportError",
    "UnknownDevice",
    "UnknownJob",
    "error_for",
]
