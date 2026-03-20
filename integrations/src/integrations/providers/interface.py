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

    def capabilities(self) -> ProviderCapabilities: ...

    async def launch(
        self, request: ResourceRequest, config: SprintConfig, workspace_dir: str
    ) -> JobHandle: ...

    async def poll(self, handle: JobHandle) -> JobStatus | JobResult: ...

    async def cancel(self, handle: JobHandle) -> None: ...

    async def get_artifacts(self, handle: JobHandle, local_dest: str) -> None: ...
