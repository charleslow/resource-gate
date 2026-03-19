# Claw Harness — Concept Document

A human-gated compute harness for AI-driven ML experiments.

The claw agent proposes experiments. A human approves them via Telegram. Approved work dispatches to pluggable compute providers (local GPU, Modal, RunPod, etc.) through a unified interface. Hard budget enforcement ensures no approved job can exceed its approved cost, and no cumulative spend can exceed configured limits.

---

## Design principles

**The agent never holds provider credentials.** It talks to the harness over HTTP and can only submit proposals. The harness owns all secrets and all dispatch authority.

**Nothing billable happens without explicit human approval.** Every proposal requires a human action (approve/reject) before any compute is provisioned. The only exception is if the human pre-approves a budget envelope for batch operations.

**Providers are pluggable.** The harness defines an abstract provider interface. Each backend (local, Modal, RunPod) implements it using its own SDK/API. Adding a new provider means implementing the interface — no changes to the core harness, the approval flow, or the agent.

**Hard enforcement, not advisory.** Budget limits are enforced by the harness at dispatch time and via active monitoring during execution. A job that approaches its approved budget is killed, not warned about.

**The harness is the single source of truth for cost.** Each provider reports usage, but the harness maintains its own ledger and computes costs from observed duration × known rates. Provider billing is used for reconciliation, not as the primary record.

---

## Architecture overview

```
┌─────────────────────────────────────────────────────┐
│  Beelink mini PC                                    │
│                                                     │
│  ┌───────────┐    HTTP     ┌──────────────────────┐ │
│  │ Claw      │────────────▶│ Harness API server   │ │
│  │ Agent     │◀────────────│                      │ │
│  └───────────┘             │  ┌────────────────┐  │ │
│                            │  │ Proposal store │  │ │
│                            │  │ (SQLite)       │  │ │
│                            │  └────────────────┘  │ │
│                            │  ┌────────────────┐  │ │
│                            │  │ Cost ledger    │  │ │
│                            │  │ (SQLite)       │  │ │
│                            │  └────────────────┘  │ │
│                            │  ┌────────────────┐  │ │
│                            │  │ Dispatcher     │  │ │
│                            │  └───────┬────────┘  │ │
│                            └──────────┼───────────┘ │
│                                       │             │
│  ┌───────────┐                        │             │
│  │ Telegram  │◀── notifies ───────────┤             │
│  │ Bot       │─── approve/reject ────▶│             │
│  └─────┬─────┘                        │             │
└────────┼──────────────────────────────┼─────────────┘
         │                              │
         ▼                              ▼
    You (phone)              ┌─────────────────────┐
                             │ Provider interface   │
                             │                      │
                             │  ├─ LocalProvider     │
                             │  ├─ ModalProvider     │
                             │  └─ RunPodProvider    │
                             └─────────────────────┘
```

There are five components. All run on the Beelink except for the remote providers themselves.

### Harness API server

An HTTP server (FastAPI or similar) that the agent talks to. It exposes endpoints for submitting proposals, querying job status, querying budget, and reading results. It owns the SQLite databases and the dispatcher loop.

### Proposal store

SQLite database tracking the lifecycle of every proposed experiment:
`pending → approved → dispatching → running → completed | failed | killed | rejected`.

### Cost ledger

SQLite database recording per-job and aggregate cost data. Queryable by the agent so it can self-regulate ("I've spent $4.20 of my $10 daily budget").

### Telegram bot

Sends proposal summaries to the human. Receives approve/reject taps via inline keyboard buttons. Also sends completion notifications, budget alerts, and supports a `/kill` command. Reuses the existing picoclaw Telegram bot infrastructure and HMAC-signed dispatch pattern.

### Dispatcher

A background loop (asyncio task or separate thread) that watches for approved proposals, dispatches them to the appropriate provider, monitors running jobs, enforces timeouts/budgets, and records results.

---

## Provider interface

Each provider implements this interface. The harness only interacts with providers through these methods.

```python
from dataclasses import dataclass
from enum import Enum
from typing import Protocol

class JobStatus(Enum):
    QUEUED = "queued"
    RUNNING = "running"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELLED = "cancelled"

@dataclass
class ResourceRequest:
    """What the job needs."""
    gpu: str                    # e.g. "A100-40GB", "T4", "cpu-only"
    gpu_count: int = 1
    cpu_cores: int | None = None
    memory_gb: int | None = None
    timeout_seconds: int = 3600
    docker_image: str | None = None  # for providers that need it

@dataclass
class JobHandle:
    """Returned by launch(). Opaque to the harness except for these fields."""
    provider_name: str
    provider_job_id: str        # provider's native ID (Modal call_id, RunPod job_id, PID, etc.)
    launched_at: float          # time.time()

@dataclass
class JobResult:
    """Returned by poll() when a job reaches a terminal state."""
    status: JobStatus
    started_at: float | None
    ended_at: float | None
    gpu_seconds: float          # actual GPU time consumed
    result_payload: dict | None # whatever the job returned
    error: str | None
    artifacts_path: str | None  # local or remote path to outputs

@dataclass
class ProviderCapabilities:
    """What this provider can do."""
    name: str
    gpu_types: list[str]
    max_concurrent_jobs: int
    supports_cancel: bool
    supports_spot: bool
    rate_card: dict[str, float]  # gpu_type → $/gpu-second


class ComputeProvider(Protocol):
    """The contract every provider must fulfill."""

    def capabilities(self) -> ProviderCapabilities:
        """Return what this provider offers and its rate card."""
        ...

    async def launch(self, request: ResourceRequest, config: dict) -> JobHandle:
        """
        Start a job. Must return quickly (non-blocking).
        `config` is the experiment configuration dict passed through from the proposal.
        Raises if the provider cannot fulfill the ResourceRequest.
        """
        ...

    async def poll(self, handle: JobHandle) -> JobStatus | JobResult:
        """
        Check on a running job.
        Returns JobStatus if still running, JobResult if terminal.
        """
        ...

    async def cancel(self, handle: JobHandle) -> None:
        """Kill a running job. Best-effort. Idempotent."""
        ...

    async def get_artifacts(self, handle: JobHandle, local_dest: str) -> None:
        """Download job outputs to a local directory."""
        ...
```

### Provider: Local

For experiments that run on the Beelink's own resources (CPU benchmarking, small model inference, data preprocessing).

- `launch()` spawns a subprocess, returns PID as `provider_job_id`
- `poll()` checks if the process is alive, reads exit code
- `cancel()` sends SIGTERM, then SIGKILL after grace period
- `rate_card` is `{"cpu-only": 0.0}` — local is free, but still tracked for time
- `gpu_types` reflects whatever is physically installed (or empty list)

### Provider: Modal

- `launch()` calls `modal_function.spawn(config)`, returns `call.object_id`
- `poll()` calls `FunctionCall.from_id(call_id).get(timeout=0)`, catches `TimeoutError` for still-running
- `cancel()` calls `FunctionCall.from_id(call_id).cancel()`
- `rate_card` populated from Modal's published per-second rates
- The actual Modal function is a thin wrapper deployed separately — it receives config, runs training/eval, writes to a Modal volume, returns a result dict

### Provider: RunPod

- `launch()` calls the RunPod serverless endpoint API or creates a pod via REST
- `poll()` checks job status via the RunPod API
- `cancel()` terminates the pod or cancels the serverless job
- `rate_card` populated from RunPod's published rates
- Supports webhooks as an optimization — the harness can expose a webhook endpoint for RunPod to call on completion, avoiding polling

### Adding a new provider

Implement `ComputeProvider`. Register it with the harness at startup. That's it. The proposal, approval, dispatch, monitoring, and cost tracking layers don't change.

---

## Proposal lifecycle

### 1. Agent submits a proposal

```
POST /proposals
{
    "experiment_name": "lr_sweep_embedding_v3",
    "provider": "modal",           // or "local", "runpod", "auto"
    "resource_request": {
        "gpu": "A100-40GB",
        "timeout_seconds": 1800
    },
    "config": {
        "model": "bert-small",
        "learning_rate": 3e-4,
        "epochs": 10,
        "dataset": "govsg_jobs_v2"
    },
    "estimated_minutes": 20,
    "budget_cap_usd": 0.80,
    "tags": {"sweep_id": "lr_003", "project": "govsg-recommender"},
    "batch_id": null
}
```

If `provider` is `"auto"`, the harness selects the cheapest provider that can fulfill the `resource_request` from its registered providers.

The harness validates the proposal (provider exists, GPU type available, budget cap is within daily limits) and writes it to the proposal store with status `pending`.

Returns: `{"proposal_id": "prop_a1b2c3", "status": "pending", "estimated_cost_usd": 0.70}`

### 2. Human receives Telegram notification

```
🔬 New proposal: lr_sweep_embedding_v3

Provider:  Modal
GPU:       A100-40GB
Timeout:   30 min
Est. cost: $0.70 (cap: $0.80)
Daily:     $4.20 / $10.00 used

Config:
  model: bert-small
  lr: 3e-4, epochs: 10

[✅ Approve]  [❌ Reject]  [📋 Details]
```

### 3. Human taps approve or reject

Approve → status becomes `approved`, dispatcher picks it up.
Reject → status becomes `rejected`, agent is notified via the status endpoint.

### 4. Dispatcher launches the job

The dispatcher picks up approved proposals, calls `provider.launch()`, and transitions the status to `dispatching` → `running`. It stores the `JobHandle` for subsequent polling.

### 5. Monitor loop

The dispatcher polls running jobs on a configurable interval (e.g. every 10 seconds). For each running job it:

1. Calls `provider.poll(handle)` to get current status
2. Computes elapsed GPU time and current cost
3. If cost exceeds the approved `budget_cap_usd` → calls `provider.cancel(handle)`, marks as `killed`
4. If elapsed time exceeds `timeout_seconds` → calls `provider.cancel(handle)`, marks as `killed`
5. If job completed or failed → records the `JobResult`, updates cost ledger

### 6. Completion notification

```
✅ Completed: lr_sweep_embedding_v3

Duration: 18m 42s
Cost:     $0.65
GPU:      A100-40GB (Modal)

Result:
  loss: 0.042
  checkpoint: /vol/checkpoints/run_17

Daily spend: $4.85 / $10.00
```

---

## Batch proposals

For hyperparameter sweeps, the agent submits a batch:

```
POST /proposals/batch
{
    "batch_name": "lr_sweep_embedding_v3",
    "provider": "modal",
    "resource_request": { "gpu": "A100-40GB", "timeout_seconds": 1800 },
    "configs": [
        {"learning_rate": 1e-4, "epochs": 10},
        {"learning_rate": 3e-4, "epochs": 10},
        {"learning_rate": 1e-3, "epochs": 10},
        {"learning_rate": 3e-3, "epochs": 5}
    ],
    "total_budget_cap_usd": 3.00,
    "max_concurrent": 2,
    "tags": {"sweep_id": "lr_003"}
}
```

The human approves or rejects the batch as a unit. The dispatcher manages concurrency within the batch and enforces the total budget across all jobs in the batch — if cumulative cost approaches the cap, remaining queued jobs are cancelled.

---

## Budget enforcement

Budgets are enforced at three levels. All three are hard limits — exceeding any one triggers a kill or rejection.

### Per-job budget

Set by the agent in `budget_cap_usd` on each proposal. The human sees it and approves it. The dispatcher kills any job whose computed cost (`elapsed_gpu_seconds × rate`) reaches this cap. There is a small overrun window (one poll interval × rate), which is acceptable.

### Per-batch budget

Set by the agent in `total_budget_cap_usd`. The dispatcher tracks cumulative spend across all jobs in the batch. If cumulative spend approaches the cap, remaining queued jobs in the batch are cancelled.

### Daily budget

Configured in the harness (not by the agent). The harness rejects any proposal whose `budget_cap_usd` would cause the projected daily total to exceed the daily limit. Even if the agent proposes a $0.50 job, if the daily limit is $10 and $9.80 has been spent, it's rejected at submission time — never sent to the human for approval.

### Implementation

```python
# Computed every poll interval for each running job
elapsed_seconds = time.time() - job.started_at
gpu_seconds = elapsed_seconds * job.resource_request.gpu_count
current_cost = gpu_seconds * provider.rate_card[job.resource_request.gpu]

if current_cost >= job.budget_cap_usd:
    await provider.cancel(job.handle)
    job.status = "killed"
    job.kill_reason = "per_job_budget_exceeded"
```

The monitor loop runs every N seconds (configurable, default 10). The worst-case budget overrun is therefore `N × rate_per_second × gpu_count`. For an A100 at ~$0.00058/s with a 10-second interval, the maximum overrun is ~$0.006. Acceptable for this use case.

---

## Harness API

All communication between the agent and the harness is via HTTP. The harness runs on `localhost` on the Beelink.

### Proposals

```
POST   /proposals              Submit a single proposal
POST   /proposals/batch        Submit a batch proposal
GET    /proposals/{id}         Get proposal status and result
GET    /proposals?status=...   List proposals, filterable by status
DELETE /proposals/{id}         Cancel a pending or running proposal
```

### Budget

```
GET    /budget                 Current daily spend, remaining budget, limits
GET    /budget/history         Historical daily spend
GET    /budget/ledger          Per-job cost records, filterable by tags
```

### Providers

```
GET    /providers              List registered providers and their capabilities
GET    /providers/{name}/rate-card   Current rate card for a provider
```

### System

```
POST   /system/pause           Pause the dispatcher (no new dispatches)
POST   /system/resume          Resume the dispatcher
POST   /system/kill-all        Cancel all running jobs immediately
GET    /system/status          Dispatcher state, running job count, etc.
```

### Authentication

The agent authenticates to the harness API via a shared secret in an `Authorization` header. Since both run on the same machine over localhost, this is sufficient. The secret is stored in a file readable only by the harness process.

---

## Telegram integration

The Telegram bot is a thin interface between the human and the harness. It does not hold state — all state lives in the harness's SQLite databases.

### Notifications sent to the human

- **New proposal**: summary with approve/reject inline keyboard
- **Job started**: brief confirmation with provider and GPU info
- **Job completed**: results summary, cost, daily spend update
- **Job failed/killed**: error summary, kill reason
- **Budget alert**: when daily spend exceeds 80% of the daily limit
- **Batch progress**: periodic summary for long-running batches ("3/8 complete, $1.20 spent")

### Commands from the human

- Inline keyboard: **Approve** / **Reject** on proposal notifications
- `/status` — summary of running jobs, daily spend
- `/kill {proposal_id}` — cancel a specific job
- `/kill all` — cancel all running jobs and pause dispatcher
- `/pause` — pause dispatcher
- `/resume` — resume dispatcher
- `/budget` — show daily budget status

### Callback flow

When the human taps approve:

1. Telegram sends callback to the bot
2. Bot makes `POST /proposals/{id}/approve` to the harness API
3. Harness validates (still pending? within budget?) and transitions status
4. Bot updates the Telegram message to show "Approved ✅" and removes the buttons

This keeps the bot stateless. If the bot restarts, no approvals are lost — the harness has the ground truth.

---

## Data model

### proposals table

| Column | Type | Description |
|--------|------|-------------|
| id | TEXT PK | `prop_{ulid}` |
| batch_id | TEXT | nullable, groups batch proposals |
| status | TEXT | pending, approved, rejected, dispatching, running, completed, failed, killed |
| experiment_name | TEXT | human-readable label |
| provider_name | TEXT | "modal", "runpod", "local" |
| resource_request | JSON | ResourceRequest as JSON |
| config | JSON | experiment config passed to provider |
| budget_cap_usd | REAL | per-job hard cap |
| estimated_minutes | INT | agent's estimate |
| tags | JSON | arbitrary key-value pairs |
| provider_job_id | TEXT | set after dispatch |
| created_at | REAL | time.time() |
| approved_at | REAL | nullable |
| dispatched_at | REAL | nullable |
| started_at | REAL | nullable (from provider) |
| ended_at | REAL | nullable |
| result_payload | JSON | nullable, from JobResult |
| error | TEXT | nullable |
| kill_reason | TEXT | nullable |

### cost_ledger table

| Column | Type | Description |
|--------|------|-------------|
| id | TEXT PK | `cost_{ulid}` |
| proposal_id | TEXT FK | links to proposals |
| provider_name | TEXT | |
| gpu_type | TEXT | |
| gpu_count | INT | |
| gpu_seconds | REAL | actual measured GPU time |
| rate_per_gpu_second | REAL | from provider rate card at dispatch time |
| computed_cost_usd | REAL | gpu_seconds × rate × count |
| recorded_at | REAL | |

### daily_budget_config

| Column | Type | Description |
|--------|------|-------------|
| daily_limit_usd | REAL | hard ceiling |
| alert_threshold_pct | INT | e.g. 80 |
| auto_pause_on_limit | BOOL | pause dispatcher when daily limit hit |

---

## Security model

### Credential isolation

- The agent process cannot read provider API keys or the Telegram bot token
- Provider credentials are stored in files readable only by the harness process user
- The harness runs as a separate Unix user from the agent (reusing existing picoclaw sandboxing)

### Agent authentication

- The agent authenticates to the harness API with a bearer token stored in a file readable only by the agent's Unix user
- The harness validates this token on every request

### Telegram callback verification

- The bot verifies Telegram webhook signatures to ensure callbacks come from Telegram
- Each proposal notification includes a unique callback ID; the bot rejects replayed or stale callbacks

### No remote access to the harness API

The harness API binds to `127.0.0.1` only. External access is only via the Telegram bot (which runs on the same machine). If remote access is ever needed, it goes through the Tailscale network.

---

## Directory structure

```
claw-harness/
├── concept.md                  # this file
├── pyproject.toml
├── src/
│   ├── harness/
│   │   ├── __init__.py
│   │   ├── api.py              # FastAPI app
│   │   ├── dispatcher.py       # dispatch loop + monitor
│   │   ├── models.py           # data classes (Proposal, JobHandle, etc.)
│   │   ├── store.py            # SQLite operations (proposals, ledger)
│   │   ├── budget.py           # budget enforcement logic
│   │   └── config.py           # load config from TOML/env
│   ├── providers/
│   │   ├── __init__.py
│   │   ├── interface.py        # ComputeProvider protocol + data classes
│   │   ├── local.py            # LocalProvider
│   │   ├── modal_provider.py   # ModalProvider
│   │   └── runpod_provider.py  # RunPodProvider
│   ├── telegram/
│   │   ├── __init__.py
│   │   ├── bot.py              # Telegram bot (polling or webhook)
│   │   └── formatting.py       # message templates
│   └── cli.py                  # optional CLI for admin ops
├── modal_functions/
│   ├── train.py                # generic training wrapper deployed to Modal
│   └── eval.py                 # generic eval wrapper deployed to Modal
├── tests/
│   ├── test_budget.py
│   ├── test_dispatcher.py
│   ├── test_providers.py
│   └── test_store.py
└── config.toml                 # runtime config (not checked in)
```

---

## Configuration

```toml
[harness]
host = "127.0.0.1"
port = 8420
db_path = "./data/harness.db"
poll_interval_seconds = 10
agent_token_file = "/etc/claw-harness/agent.token"

[budget]
daily_limit_usd = 10.00
alert_threshold_pct = 80
auto_pause_on_limit = true

[telegram]
bot_token_file = "/etc/claw-harness/telegram.token"
chat_id = 123456789

[providers.local]
enabled = true

[providers.modal]
enabled = true
token_file = "/etc/claw-harness/modal.token"
default_volume = "claw-experiments"

[providers.runpod]
enabled = false
api_key_file = "/etc/claw-harness/runpod.key"
```

---

## Open questions

1. **Should the agent be able to propose its own provider, or should the harness always select?** Current design allows both (`"auto"` or explicit). May want to restrict the agent to `"auto"` only and let provider selection be a harness-side policy.

2. **Webhook vs polling for provider status.** Modal doesn't natively support completion webhooks; RunPod does. The current design uses polling for simplicity. A webhook optimization for RunPod could reduce latency and harness load.

3. **Artifact retrieval.** How and when to pull results back from remote providers. Eagerly on completion? On-demand when queried? Modal volumes persist but cost storage. RunPod network volumes have their own pricing. Probably: pull critical outputs (metrics, small checkpoints) immediately, leave large artifacts (full model weights) in provider storage with a TTL.

4. **Multi-user.** Currently designed for a single human approver. If this grows to a team tool, need role-based approval (who can approve jobs over $X?).

5. **Agent self-regulation.** The agent can query `/budget` before proposing. Should the harness enforce a softer "agent budget" (lower than the daily limit) to leave headroom for manual experiments?

6. **Rate card staleness.** Provider pricing can change. The harness should periodically refresh rate cards, or at least warn if the cached rate card is older than N days.
