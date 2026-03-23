# Experiment Harness — Concept

> Sister project to [resource-gate](./concept.md). A CLI tool that orchestrates
> AI agent experiment loops, using resource-gate for gated compute access.

## Problem

You want an AI agent to iteratively develop, train, and evaluate ML experiments —
but you need human oversight at every turn. The agent should propose what to do,
execute against real compute, and present results for review before continuing.

Today this requires manually wiring together: an agent runtime, a compute
provider, a review workflow, and a resource approval flow. This project bundles
all of that into a single CLI.

## Core Concepts

### Experiment

A self-contained directory with code, config, and history. Created once via
`init`, then evolved over multiple sprints.

```
my-experiment/
├── experiment.toml          # experiment config (name, base image, resource profile, etc.)
├── src/                     # agent-managed code (model, training, eval scripts)
├── data/                    # symlinks or references to datasets
├── sprints/
│   ├── 001/
│   │   ├── plan.md          # what the agent proposed to do
│   │   ├── diff.patch       # code changes made this sprint
│   │   ├── logs/            # stdout/stderr from the run
│   │   ├── artifacts/       # checkpoints, metrics, plots
│   │   ├── story.md         # code-stories output for human review
│   │   └── review.md        # human comments for next sprint
│   └── 002/
│       └── ...
├── resource-gate.toml       # resource-gate client config (endpoint, token)
└── .git/
```

### Sprint

One iteration of the experiment loop. The unit of work and review.

```
┌─────────────────────────────────────────────────────────────┐
│                        SPRINT N                             │
│                                                             │
│  1. PLAN     — Agent reads review.md from sprint N-1,       │
│                proposes plan for this sprint                 │
│                                                             │
│  2. CODE     — Agent modifies src/ based on the plan        │
│                                                             │
│  3. REQUEST  — Agent submits resource proposal to           │
│                resource-gate (GPU type, budget, timeout)     │
│                Human approves/rejects via Telegram           │
│                                                             │
│  4. RUN      — Experiment executes in Docker container       │
│                against approved resources                    │
│                resource-gate enforces budget & timeout        │
│                                                             │
│  5. REPORT   — Agent collects artifacts, generates a         │
│                code-stories summary of what changed & why    │
│                                                             │
│  6. REVIEW   — Human reads the story, writes review.md       │
│                with comments, corrections, direction          │
│                                                             │
│  → Sprint N+1 begins at step 1                              │
└─────────────────────────────────────────────────────────────┘
```

### Resource Gate Integration

The harness starts a resource-gate instance (or connects to an existing one)
and configures the agent with a scoped token. The agent **cannot** access
provider credentials directly — it must go through resource-gate, where the
human approves each resource request.

### Code Stories Integration

At the end of each sprint, the harness invokes
[code-stories](https://github.com/charleslow/code-stories) to generate a
human-readable narrative of what changed and why. This is the primary artifact
the human reviews between sprints.

## CLI Interface

### `experiment init`

Interactive scaffolding for a new experiment.

```bash
$ experiment init my-kaggle-comp

Experiment name: my-kaggle-comp
Base docker image [python:3.11-slim]: nvcr.io/nvidia/pytorch:24.01-py3
Default GPU type [T4]: A100-40GB
Default budget per sprint [$2.00]: $5.00
Resource-gate endpoint [http://localhost:8420]: <enter>
Description: Kaggle competition - predict housing prices

✓ Created my-kaggle-comp/
✓ Scaffolded experiment.toml
✓ Initialized git repo
✓ Generated starter src/ from description using Claude CLI
✓ Linked to resource-gate at localhost:8420
```

Under the hood:
- Creates the directory structure
- Uses `claude` CLI to generate initial `src/` scaffolding based on the
  experiment description
- Configures resource-gate client (endpoint, token)
- Initializes git repo for change tracking

### `experiment run [--sprint N] [--auto]`

Starts or continues the experiment loop.

```bash
$ experiment run

Sprint 003 starting...
[PLAN]    Agent reading sprint 002 review...
[PLAN]    Agent proposing plan for sprint 003...
          → plan.md written

[CODE]    Agent modifying src/...
          → 3 files changed, diff.patch saved

[REQUEST] Submitting resource proposal to resource-gate:
          Provider: local | GPU: A100-40GB x1 | Budget: $3.50 | Timeout: 30m
          ⏳ Waiting for human approval...
          ✓ Approved

[RUN]     Launching in Docker container...
          ├── Training epoch 1/10... loss=2.341
          ├── Training epoch 2/10... loss=1.872
          └── ... (streaming logs)
          ✓ Completed in 18m32s | Cost: $1.12

[REPORT]  Collecting artifacts...
          Generating code-stories summary...
          → story.md written

[REVIEW]  Sprint 003 complete.
          Open sprints/003/story.md to review.
          Write your comments to sprints/003/review.md
          Then run: experiment run
```

### `experiment status`

Shows current experiment state, sprint history, and resource usage.

### `experiment resources`

Queries resource-gate for budget status and available providers.

## Architecture

```
┌──────────────┐     ┌──────────────────┐     ┌────────────────┐
│              │     │                  │     │                │
│  Human       │────▶│  Experiment CLI   │────▶│  Claude CLI    │
│  (terminal)  │◀────│                  │     │  (agent brain) │
│              │     │  Orchestrates the │     │                │
└──────┬───────┘     │  sprint loop     │     └────────────────┘
       │             │                  │
       │ review.md   └────────┬─────────┘
       │                      │
       │          ┌───────────┴───────────┐
       │          │                       │
       │          ▼                       ▼
       │   ┌──────────────┐       ┌──────────────┐
       │   │              │       │              │
       │   │ Resource-Gate │       │ Code-Stories │
       └──▶│ (approval +  │       │ (sprint      │
    approve│  budget)      │       │  summaries)  │
           │              │       │              │
           └──────┬───────┘       └──────────────┘
                  │
                  ▼
           ┌──────────────┐
           │ Docker /      │
           │ Modal /       │
           │ RunPod        │
           └──────────────┘
```

**Key design decisions:**

1. **Claude CLI as the agent brain.** The harness calls `claude` with
   appropriate context (previous review, current code, experiment config) rather
   than reimplementing agent logic. This keeps the harness thin and lets Claude
   handle reasoning.

2. **Docker for isolation.** Each sprint's RUN phase executes in a container.
   The agent writes code locally, but execution is sandboxed. The container
   mounts the experiment workspace read-write so artifacts are persisted.

3. **Resource-gate for the money boundary.** Every compute request goes through
   resource-gate. The human sees what the agent wants to spend before it spends
   it. The harness never holds provider credentials.

4. **Git as the changelog.** Each sprint commits its changes. The diff is
   captured for code-stories. The full history is always recoverable.

## experiment.toml

```toml
[experiment]
name = "my-kaggle-comp"
description = "Kaggle competition - predict housing prices"
created_at = "2026-03-20T10:00:00Z"

[runtime]
base_image = "nvcr.io/nvidia/pytorch:24.01-py3"
default_gpu = "A100-40GB"
default_gpu_count = 1
default_timeout_seconds = 1800
default_budget_usd = 5.00

[resource_gate]
endpoint = "http://localhost:8420"
token_env = "EXPERIMENT_AGENT_TOKEN"   # read from env, never stored in file

[agent]
model = "claude-sonnet-4-6"
system_prompt_file = "agent_prompt.md"  # optional custom instructions

[code_stories]
enabled = true
```

## Sprint Agent Context

When the agent is invoked for a sprint, it receives:

1. **Experiment config** — what this experiment is about, constraints
2. **Current src/ contents** — the code as it stands
3. **Previous sprint's story.md** — what happened last time
4. **Previous sprint's review.md** — human feedback and direction
5. **Resource budget** — what it can request this sprint
6. **Available resources** — what resource-gate offers (GPU types, providers)

The agent outputs:
1. **plan.md** — what it intends to do and why
2. **Code changes** — modifications to src/
3. **Resource request** — what compute it needs (submitted to resource-gate)
4. **Post-run analysis** — interpretation of results, fed into code-stories

## Open Questions

1. **How much autonomy per sprint?** Should the agent be able to run multiple
   resource-gate proposals within a single sprint (e.g., quick eval then full
   training), or strictly one proposal per sprint?

2. **Dataset management.** How do datasets get into the experiment? Symlinks?
   A data registry? Download scripts that run in the container?

3. **Multi-agent sprints.** Could multiple agents work on the same experiment
   in parallel (e.g., one for feature engineering, one for model architecture)?

4. **Sprint branching.** Should the agent be able to propose "fork this sprint
   into two variants" for A/B comparison runs?

5. **Auto-approve mode.** For trusted experiments under a budget ceiling, should
   there be a mode that auto-approves resource requests below a threshold?
   (Resource-gate would need to support this.)

6. **State between sprints.** Model checkpoints, preprocessed data, etc. — how
   are these carried forward? Shared volume? Artifact registry?
