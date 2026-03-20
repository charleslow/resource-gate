"""LocalProvider — time-reservation provider for the host machine.

The local provider doesn't spawn processes. It acts as a time reservation:
the agent runs its own work and signals completion via the harness API.
The harness tracks duration and enforces timeouts/budgets.
"""

import time
import uuid

from .interface import (
    JobHandle,
    JobResult,
    JobStatus,
    ProviderCapabilities,
    ResourceRequest,
)


class LocalProvider:
    """Runs sprints on the local machine as time reservations."""

    def capabilities(self) -> ProviderCapabilities:
        return ProviderCapabilities(
            name="local",
            gpu_types=["cpu-only"],
            max_concurrent_jobs=4,
            supports_cancel=True,
            supports_spot=False,
            rate_card={"cpu-only": 0.0},
        )

    async def launch(self, request: ResourceRequest) -> JobHandle:
        """Start a time reservation. Returns immediately with a handle."""
        job_id = str(uuid.uuid4())
        return JobHandle(
            provider_name="local",
            provider_job_id=job_id,
            launched_at=time.time(),
        )

    async def poll(self, handle: JobHandle) -> JobStatus:
        """Local sprints are always RUNNING until the agent completes them via API."""
        return JobStatus.RUNNING

    async def cancel(self, handle: JobHandle) -> None:
        """No-op for local provider. The harness handles state transitions."""
        pass

    async def get_artifacts(self, handle: JobHandle, local_dest: str) -> None:
        """No-op — artifacts are already local."""
        pass
