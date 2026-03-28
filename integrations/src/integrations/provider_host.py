"""
Provider host process — the bridge between the Rust core and Python providers.

Reads a single JSON command from stdin, dispatches to the appropriate
ComputeProvider method, writes a JSON response to stdout, then exits.

Usage:
    echo '{"method": "launch", "provider": "local", ...}' | python -m integrations.provider_host
"""

import asyncio
import json
import sys
from dataclasses import asdict

from .providers.interface import JobHandle, JobStatus, ResourceRequest, SprintConfig
from .providers.local import LocalProvider

PROVIDERS = {
    "local": LocalProvider(),
}

OK = {"type": "ok", "data": None}


def _serialize_response(obj) -> dict:
    """Convert provider response objects to JSON-serializable dicts."""
    if isinstance(obj, JobHandle):
        return {"type": "handle", "data": asdict(obj)}
    if isinstance(obj, JobStatus):
        return {"type": "status", "data": {"status": obj.value}}
    if hasattr(obj, "status") and hasattr(obj, "gpu_seconds"):
        d = asdict(obj)
        d["status"] = obj.status.value
        return {"type": "result", "data": d}
    return OK


def _parse_handle(cmd: dict) -> JobHandle:
    return JobHandle(**cmd["handle"])


async def handle_command(cmd: dict) -> dict:
    method = cmd["method"]
    provider = PROVIDERS.get(cmd["provider"])
    if provider is None:
        return {"error": f"unknown provider: {cmd['provider']}"}

    if method == "preflight":
        await provider.preflight()
        return OK

    if method == "capabilities":
        return {"type": "capabilities", "data": asdict(provider.capabilities())}

    if method == "launch":
        req_data = cmd["request"]
        request = ResourceRequest(**{
            k: v for k, v in req_data.items()
            if k in ResourceRequest.__dataclass_fields__
        })
        config = SprintConfig(
            command=(cmd.get("config") or {}).get("command", []),
            env=(cmd.get("config") or {}).get("env", {}),
            working_dir=(cmd.get("config") or {}).get("working_dir"),
        )
        handle = await provider.launch(request, config, cmd.get("workspace_dir", "/workspace"))
        return _serialize_response(handle)

    if method == "poll":
        return _serialize_response(await provider.poll(_parse_handle(cmd)))

    # Simple handle-based methods that return OK
    handle = _parse_handle(cmd)
    if method == "cancel":
        await provider.cancel(handle)
    elif method == "cleanup":
        await provider.cleanup(handle)
    elif method == "copy_in":
        await provider.copy_in(handle, cmd["local_path"], cmd["remote_path"])
    elif method == "copy_out":
        await provider.copy_out(handle, cmd["remote_path"], cmd["local_path"])
    else:
        return {"error": f"unknown method: {method}"}
    return OK


def main():
    raw = sys.stdin.read()
    try:
        cmd = json.loads(raw)
    except json.JSONDecodeError as e:
        response = {"error": f"invalid JSON: {e}"}
        sys.stdout.write(json.dumps(response))
        sys.exit(1)

    try:
        response = asyncio.run(handle_command(cmd))
    except Exception as e:
        response = {"error": str(e)}

    sys.stdout.write(json.dumps(response))
    sys.stdout.flush()


if __name__ == "__main__":
    main()
