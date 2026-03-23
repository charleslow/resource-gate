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


def _serialize_response(obj) -> dict:
    """Convert provider response objects to JSON-serializable dicts."""
    if isinstance(obj, JobHandle):
        return {"type": "handle", "data": asdict(obj)}
    elif isinstance(obj, JobStatus):
        return {"type": "status", "data": {"status": obj.value}}
    elif hasattr(obj, "status") and hasattr(obj, "gpu_seconds"):
        # JobResult
        d = asdict(obj)
        d["status"] = obj.status.value
        return {"type": "result", "data": d}
    else:
        return {"type": "ok", "data": None}


def _parse_sprint_config(raw: dict | None) -> SprintConfig:
    """Parse sprint config from JSON, handling missing fields."""
    if not raw:
        return SprintConfig()
    return SprintConfig(
        command=raw.get("command", []),
        env=raw.get("env", {}),
        working_dir=raw.get("working_dir"),
    )


async def handle_command(cmd: dict) -> dict:
    method = cmd["method"]
    provider_name = cmd["provider"]

    provider = PROVIDERS.get(provider_name)
    if provider is None:
        return {"error": f"unknown provider: {provider_name}"}

    if method == "preflight":
        await provider.preflight()
        return {"type": "ok", "data": None}

    elif method == "capabilities":
        caps = provider.capabilities()
        return {"type": "capabilities", "data": asdict(caps)}

    elif method == "launch":
        req_data = cmd["request"]
        request = ResourceRequest(**{
            k: v for k, v in req_data.items()
            if k in ResourceRequest.__dataclass_fields__
        })
        config = _parse_sprint_config(cmd.get("config"))
        workspace_dir = cmd.get("workspace_dir", "/workspace")
        handle = await provider.launch(request, config, workspace_dir)
        return _serialize_response(handle)

    elif method == "poll":
        handle = JobHandle(**cmd["handle"])
        result = await provider.poll(handle)
        return _serialize_response(result)

    elif method == "cancel":
        handle = JobHandle(**cmd["handle"])
        await provider.cancel(handle)
        return {"type": "ok", "data": None}

    elif method == "get_artifacts":
        handle = JobHandle(**cmd["handle"])
        local_dest = cmd["local_dest"]
        await provider.get_artifacts(handle, local_dest)
        return {"type": "ok", "data": None}

    else:
        return {"error": f"unknown method: {method}"}


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
