"""LocalProvider — runs sprint jobs as Docker containers on the host machine.

launch() → docker run (detached) with resource limits and shared workspace
poll()   → docker inspect to check container status
cancel() → docker kill
wait_for_exit() → docker wait (blocks until container exits)

Note: Containers are NOT launched with --rm.  We need to inspect them after
exit to read exit code / OOM status, so auto-removal would race with the
polling interval.  Instead we clean up explicitly via docker rm after
recording the terminal state.
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
        """Start a Docker container for the sprint."""
        if not config.command:
            raise ValueError("config.command is required for local provider")

        image = request.docker_image
        if not image:
            raise ValueError("resource_request.docker_image is required for local provider")

        # Build docker run command (no --rm: we clean up after reading exit status)
        cmd = ["docker", "run", "-d"]

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

        # Mount shared workspace
        cmd += ["-v", f"{workspace_dir}:/workspace"]

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
            raise RuntimeError(f"docker run failed: {stderr}")

        container_id = stdout.strip()
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
                artifacts_path="/workspace",
            )

        state = json.loads(stdout)
        status = state.get("Status", "")

        # Handle non-terminal Docker states explicitly
        if status in ("running", "created", "paused", "restarting"):
            return JobStatus.RUNNING

        if status == "dead":
            await _run(["docker", "rm", "-f", container_id])
            return JobResult(
                status=JobStatus.FAILED,
                started_at=handle.launched_at,
                ended_at=time.time(),
                gpu_seconds=time.time() - handle.launched_at,
                result_payload=None,
                error="container entered 'dead' state — Docker daemon error",
                artifacts_path="/workspace",
            )

        # Container exited — use Docker's actual timestamps
        exit_code = state.get("ExitCode", -1)
        oom_killed = state.get("OOMKilled", False)
        started_at_str = state.get("StartedAt", "")
        finished_at_str = state.get("FinishedAt", "")

        started_at = _parse_docker_ts(started_at_str, handle.launched_at)
        ended_at = _parse_docker_ts(finished_at_str, time.time())

        if exit_code == 0 and not oom_killed:
            # Clean up the stopped container now that we've read its state
            await _run(["docker", "rm", "-f", container_id])
            return JobResult(
                status=JobStatus.COMPLETED,
                started_at=started_at,
                ended_at=ended_at,
                gpu_seconds=ended_at - started_at,
                result_payload={"exit_code": exit_code},
                error=None,
                artifacts_path="/workspace",
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

            # Clean up the stopped container
            await _run(["docker", "rm", "-f", container_id])
            return JobResult(
                status=JobStatus.FAILED,
                started_at=started_at,
                ended_at=ended_at,
                gpu_seconds=ended_at - started_at,
                result_payload={"exit_code": exit_code, "oom_killed": oom_killed},
                error=error_msg,
                artifacts_path="/workspace",
            )

    async def cancel(self, handle: JobHandle) -> None:
        """Kill and remove the Docker container."""
        container_id = handle.provider_job_id
        await _run(["docker", "kill", container_id])
        await _run(["docker", "rm", "-f", container_id])

    async def wait_for_exit(self, handle: JobHandle) -> int:
        """Block until the container exits, return exit code.

        This is used by the dispatcher to get near-instant notification of
        job completion instead of waiting for the next poll interval.
        ``docker wait`` blocks until the container stops and prints the
        exit code.  If the container is already stopped it returns
        immediately.
        """
        container_id = handle.provider_job_id
        rc, stdout, stderr = await _run(["docker", "wait", container_id])
        if rc != 0:
            return -1
        try:
            return int(stdout.strip())
        except ValueError:
            return -1

    async def get_artifacts(self, handle: JobHandle, local_dest: str) -> None:
        """No-op — workspace is already a shared volume on the host."""
        pass
