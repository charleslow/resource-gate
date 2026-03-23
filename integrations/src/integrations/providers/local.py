"""LocalProvider — runs sprint jobs as Docker containers on the host machine.

launch() → docker run (detached) with resource limits and shared workspace
poll()   → docker inspect to check container status
cancel() → docker kill
"""

import asyncio
import json
import time

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

        # Build docker run command
        cmd = ["docker", "run", "-d", "--rm"]

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
            # Container gone (--rm cleaned it up, or was killed)
            # Treat as completed if we can't find it — the dispatcher
            # will have already recorded the kill if it initiated one.
            return JobResult(
                status=JobStatus.COMPLETED,
                started_at=handle.launched_at,
                ended_at=time.time(),
                gpu_seconds=time.time() - handle.launched_at,
                result_payload=None,
                error=None,
                artifacts_path="/workspace",
            )

        state = json.loads(stdout)
        status = state.get("Status", "")

        if status == "running":
            return JobStatus.RUNNING

        # Container exited
        exit_code = state.get("ExitCode", -1)
        finished_at_str = state.get("FinishedAt", "")
        started_at_str = state.get("StartedAt", "")

        ended_at = time.time()
        started_at = handle.launched_at

        if exit_code == 0:
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
            # Try to get last few lines of logs for error context
            _, logs, _ = await _run(
                ["docker", "logs", "--tail", "20", container_id]
            )
            return JobResult(
                status=JobStatus.FAILED,
                started_at=started_at,
                ended_at=ended_at,
                gpu_seconds=ended_at - started_at,
                result_payload={"exit_code": exit_code},
                error=f"exit code {exit_code}: {logs[-500:] if logs else 'no output'}",
                artifacts_path="/workspace",
            )

    async def cancel(self, handle: JobHandle) -> None:
        """Kill the Docker container."""
        container_id = handle.provider_job_id
        await _run(["docker", "kill", container_id])

    async def get_artifacts(self, handle: JobHandle, local_dest: str) -> None:
        """No-op — workspace is already a shared volume on the host."""
        pass
