"""ComputeProvider protocol and shared data classes."""

from dataclasses import dataclass, field, asdict
from enum import Enum
from typing import Protocol
import time


class JobStatus(Enum):
    QUEUED = "queued"
    RUNNING = "running"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELLED = "cancelled"


@dataclass
class ResourceRequest:
    gpu: str
    gpu_count: int = 1
    cpu_cores: int | None = None
    memory_gb: int | None = None
    timeout_seconds: int = 3600
    docker_image: str | None = None


@dataclass
class SprintConfig:
    """Provider-specific config passed through from the proposal."""
    command: list[str] = field(default_factory=list)
    env: dict[str, str] = field(default_factory=dict)
    working_dir: str | None = None


@dataclass
class JobHandle:
    provider_name: str
    provider_job_id: str
    """Opaque string token that uniquely identifies the job within the provider.

    The orchestrator never interprets this value — it stores it in SQLite and
    passes it back verbatim to :meth:`ComputeProvider.poll` and
    :meth:`ComputeProvider.cancel`.  Providers must ensure this single string
    is sufficient to resume all operations on the job (e.g. a Docker container
    ID, a cloud instance ARN, or a composite key like ``region:instance_id``).
    """
    launched_at: float


@dataclass
class JobResult:
    status: JobStatus
    started_at: float | None
    ended_at: float | None
    gpu_seconds: float
    result_payload: dict | None
    error: str | None
    artifacts_path: str | None


@dataclass
class ProviderCapabilities:
    name: str
    gpu_types: list[str]
    max_concurrent_jobs: int
    supports_cancel: bool
    supports_spot: bool
    rate_card: dict[str, float]  # gpu_type -> $/gpu-second


class ComputeProvider(Protocol):
    """The contract every provider must fulfill."""

    async def preflight(self) -> None: ...

    def capabilities(self) -> ProviderCapabilities: ...

    async def launch(
        self, request: ResourceRequest, config: SprintConfig, workspace_dir: str
    ) -> JobHandle:
        """Launch a job and return a handle the orchestrator can use to track it.

        The workspace_dir contents are copied (not mounted) into the job
        environment at /workspace before execution starts.  This ensures a
        consistent copy-based interface across all providers — local Docker,
        Modal, RunPod, etc.

        The returned :class:`JobHandle` must contain a ``provider_job_id`` that
        is an opaque string token.  The orchestrator stores it in SQLite and
        passes it back verbatim to :meth:`poll` and :meth:`cancel` — it never
        interprets the value.  Providers must pack whatever state they need into
        this single string (e.g. a Docker container ID, a cloud instance ARN,
        or a composite key like ``region:instance_id``).
        """
        ...

    async def poll(self, handle: JobHandle) -> JobStatus | JobResult: ...

    async def cancel(self, handle: JobHandle) -> None: ...

    async def copy_in(
        self, handle: JobHandle, local_path: str, remote_path: str
    ) -> None:
        """Copy a local file or directory into the running job environment."""
        ...

    async def copy_out(
        self, handle: JobHandle, remote_path: str, local_path: str
    ) -> None:
        """Copy a file or directory from the job environment to a local path."""
        ...
