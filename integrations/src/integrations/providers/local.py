"""LocalProvider — runs sprint jobs as Docker containers on the host machine.

launch()   → docker create + docker cp + docker start (copy-based, no bind mounts)
poll()     → docker inspect to check container status (read-only, never mutates)
cancel()   → docker kill (stops execution, container still exists for copy_out)
cleanup()  → docker rm -f (final removal, called by orchestrator after grace period)
copy_in()  → docker cp local → container
copy_out() → docker cp container → local
"""

import asyncio
import json
import time
from datetime import datetime

from .interface import (
    JobHandle,
    JobResult,
    JobStatus,
    ProviderCapabilities,
    ResourceRequest,
    SprintConfig,
)


async def _run(cmd: list[str]) -> tuple[int, str, str]:
    """Run a command and return (returncode, stdout, stderr)."""
    proc = await asyncio.create_subprocess_exec(
        *cmd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    stdout, stderr = await proc.communicate()
    return proc.returncode, stdout.decode().strip(), stderr.decode().strip()


def _parse_docker_ts(ts: str, fallback: float) -> float:
    """Parse Docker's RFC3339Nano timestamp to epoch seconds."""
    try:
        # Docker uses Go's time format: 2024-01-15T10:30:00.123456789Z
        return datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp()
    except (ValueError, AttributeError):
        return fallback


class LocalProvider:
    """Runs sprints as Docker containers on the local machine."""

    async def preflight(self) -> None:
        """Verify Docker is available and responsive. Raises on failure."""
        rc, stdout, stderr = await _run(["docker", "info", "--format", "{{.ServerVersion}}"])
        if rc != 0:
            raise RuntimeError(
                f"Docker preflight check failed — is Docker installed and running? {stderr}"
            )

    def capabilities(self) -> ProviderCapabilities:
        return ProviderCapabilities(
            name="local",
            gpu_types=["cpu-only"],
            max_concurrent_jobs=4,
            supports_cancel=True,
            supports_spot=False,
            rate_card={"cpu-only": 0.0},
        )

    async def launch(
        self, request: ResourceRequest, config: SprintConfig, workspace_dir: str
    ) -> JobHandle:
        """Create a Docker container, copy workspace in, then start it."""
        if not config.command:
            raise ValueError("config.command is required for local provider")

        image = request.docker_image
        if not image:
            raise ValueError("resource_request.docker_image is required for local provider")

        # Build docker create command (container is created but not started)
        cmd = ["docker", "create"]

        # Resource limits
        if request.cpu_cores:
            cmd += ["--cpus", str(request.cpu_cores)]
        if request.memory_gb:
            cmd += ["--memory", f"{request.memory_gb}g"]

        # GPU access
        if request.gpu and request.gpu != "cpu-only":
            if request.gpu_count > 0:
                cmd += ["--gpus", f'"device={",".join(str(i) for i in range(request.gpu_count))}"']
            else:
                cmd += ["--gpus", "all"]

        # Working directory
        work_dir = config.working_dir or "/workspace"
        cmd += ["-w", work_dir]

        # Environment variables
        for key, val in config.env.items():
            cmd += ["-e", f"{key}={val}"]

        # Label for identification
        cmd += ["--label", "resource-gate=true"]

        # Image and command
        cmd.append(image)
        cmd.extend(config.command)

        rc, stdout, stderr = await _run(cmd)
        if rc != 0:
            raise RuntimeError(f"docker create failed: {stderr}")

        container_id = stdout.strip()

        # Copy workspace contents into the container before starting it.
        # The trailing "/." copies the *contents* of workspace_dir into /workspace.
        rc, _, stderr = await _run(
            ["docker", "cp", f"{workspace_dir}/.", f"{container_id}:/workspace"]
        )
        if rc != 0:
            # Clean up the created-but-not-started container
            await _run(["docker", "rm", "-f", container_id])
            raise RuntimeError(f"docker cp (copy-in workspace) failed: {stderr}")

        # Now start the container
        rc, _, stderr = await _run(["docker", "start", container_id])
        if rc != 0:
            await _run(["docker", "rm", "-f", container_id])
            raise RuntimeError(f"docker start failed: {stderr}")

        return JobHandle(
            provider_name="local",
            provider_job_id=container_id,
            launched_at=time.time(),
        )

    async def poll(self, handle: JobHandle) -> JobStatus | JobResult:
        """Check container status via docker inspect."""
        container_id = handle.provider_job_id
        rc, stdout, stderr = await _run(
            ["docker", "inspect", "--format", "{{json .State}}", container_id]
        )

        if rc != 0:
            # Container truly gone (manually removed, or cleaned up by us on a
            # previous poll).  Report FAILED — if the dispatcher initiated a
            # kill it already recorded the outcome.
            return JobResult(
                status=JobStatus.FAILED,
                started_at=handle.launched_at,
                ended_at=time.time(),
                gpu_seconds=time.time() - handle.launched_at,
                result_payload=None,
                error="container not found — may have been removed externally",
            )

        state = json.loads(stdout)
        status = state.get("Status", "")

        # Handle non-terminal Docker states explicitly
        if status in ("running", "created", "paused", "restarting"):
            return JobStatus.RUNNING

        if status == "dead":
            return JobResult(
                status=JobStatus.FAILED,
                started_at=handle.launched_at,
                ended_at=time.time(),
                gpu_seconds=time.time() - handle.launched_at,
                result_payload=None,
                error="container entered 'dead' state — Docker daemon error",
            )

        # Container exited — use Docker's actual timestamps
        exit_code = state.get("ExitCode", -1)
        oom_killed = state.get("OOMKilled", False)
        started_at_str = state.get("StartedAt", "")
        finished_at_str = state.get("FinishedAt", "")

        started_at = _parse_docker_ts(started_at_str, handle.launched_at)
        ended_at = _parse_docker_ts(finished_at_str, time.time())

        if exit_code == 0 and not oom_killed:
            return JobResult(
                status=JobStatus.COMPLETED,
                started_at=started_at,
                ended_at=ended_at,
                gpu_seconds=ended_at - started_at,
                result_payload={"exit_code": exit_code},
                error=None,
            )
        else:
            # Get logs for error context before removing the container
            _, logs, _ = await _run(
                ["docker", "logs", "--tail", "20", container_id]
            )
            if oom_killed:
                error_msg = f"OOM killed (exit code {exit_code}): container exceeded memory limit"
            else:
                error_msg = f"exit code {exit_code}: {logs[-500:] if logs else 'no output'}"

            return JobResult(
                status=JobStatus.FAILED,
                started_at=started_at,
                ended_at=ended_at,
                gpu_seconds=ended_at - started_at,
                result_payload={"exit_code": exit_code, "oom_killed": oom_killed},
                error=error_msg,
            )

    async def cancel(self, handle: JobHandle) -> None:
        """Kill the Docker container (stop execution).

        Does NOT remove the container — the orchestrator can still copy_out
        artifacts after cancellation.  Call cleanup() to remove it.
        """
        container_id = handle.provider_job_id
        await _run(["docker", "kill", container_id])

    async def cleanup(self, handle: JobHandle) -> None:
        """Remove the Docker container.  Idempotent."""
        container_id = handle.provider_job_id
        await _run(["docker", "rm", "-f", container_id])

    async def copy_in(
        self, handle: JobHandle, local_path: str, remote_path: str
    ) -> None:
        """Copy a local file or directory into the Docker container."""
        container_id = handle.provider_job_id
        rc, _, stderr = await _run(
            ["docker", "cp", local_path, f"{container_id}:{remote_path}"]
        )
        if rc != 0:
            raise RuntimeError(f"docker cp (copy-in) failed: {stderr}")

    async def copy_out(
        self, handle: JobHandle, remote_path: str, local_path: str
    ) -> None:
        """Copy a file or directory from the Docker container to local."""
        container_id = handle.provider_job_id
        rc, _, stderr = await _run(
            ["docker", "cp", f"{container_id}:{remote_path}", local_path]
        )
        if rc != 0:
            raise RuntimeError(f"docker cp (copy-out) failed: {stderr}")
