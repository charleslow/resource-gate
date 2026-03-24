// Dispatch loop: watches for approved proposals, dispatches to providers,
// monitors running jobs, enforces timeouts and budgets.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{interval, Duration};

use crate::budget::BudgetEnforcer;
use crate::models::*;
use crate::provider_bridge::{ProviderBridge, ProviderResponse};
use crate::store::Store;

pub struct Dispatcher {
    store: Store,
    budget: Arc<BudgetEnforcer>,
    bridge: Arc<ProviderBridge>,
    poll_interval: Duration,
    paused: Arc<AtomicBool>,
    concurrency_limits: std::collections::HashMap<String, u32>,
}

impl Dispatcher {
    pub fn new(
        store: Store,
        budget: Arc<BudgetEnforcer>,
        bridge: Arc<ProviderBridge>,
        poll_interval_secs: u64,
        concurrency_limits: std::collections::HashMap<String, u32>,
    ) -> Self {
        Self {
            store,
            budget,
            bridge,
            poll_interval: Duration::from_secs(poll_interval_secs),
            paused: Arc::new(AtomicBool::new(false)),
            concurrency_limits,
        }
    }

    pub fn paused_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.paused)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Run the dispatch loop.
    ///
    /// Job completion is detected exclusively via polling (provider `poll()`),
    /// not via blocking waits like `docker wait`.  This is deliberate:
    /// polling is a clean, uniform interface that every provider can implement,
    /// and it avoids spawning idle background processes that block until a
    /// container exits.  If faster detection is needed, increase the polling
    /// frequency rather than introducing wait-based shortcuts.
    pub async fn run(self: Arc<Self>) {
        let mut ticker = interval(self.poll_interval);
        tracing::info!(
            "dispatcher started, polling every {}s",
            self.poll_interval.as_secs()
        );

        loop {
            ticker.tick().await;

            if self.paused.load(Ordering::Relaxed) {
                continue;
            }

            if let Err(e) = self.tick().await {
                tracing::error!("dispatcher tick error: {}", e);
            }
        }
    }

    async fn tick(&self) -> anyhow::Result<()> {
        // 1. Dispatch approved proposals
        self.dispatch_approved().await?;
        // 2. Monitor running sprints
        self.monitor_running().await?;
        Ok(())
    }

    async fn dispatch_approved(&self) -> anyhow::Result<()> {
        let approved = self.store.list_proposals(Some("approved"))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();

        for proposal in approved {
            // Concurrency check
            let max_concurrent = self
                .concurrency_limits
                .get(&proposal.provider_name)
                .copied()
                .unwrap_or(4);
            let running_count = self
                .store
                .count_running_jobs_for_provider(&proposal.provider_name)?;
            if running_count >= max_concurrent {
                tracing::debug!(
                    "skipping proposal {} — provider '{}' at concurrency limit ({}/{})",
                    proposal.id,
                    proposal.provider_name,
                    running_count,
                    max_concurrent
                );
                continue; // stay approved, retry next tick
            }

            // Lease time check
            if let Some(ref lease_id) = proposal.lease_id {
                let used = self.store.cumulative_runtime_for_lease(lease_id, now)?;
                if let Ok(Some(lease)) = self.store.get_lease(lease_id) {
                    if lease.status != crate::models::LeaseStatus::Approved {
                        tracing::warn!(
                            "failing proposal {} — lease {} is {}",
                            proposal.id,
                            lease_id,
                            lease.status
                        );
                        self.store.fail_proposal(
                            &proposal.id,
                            &format!("lease {} is {}", lease_id, lease.status),
                        )?;
                        continue;
                    }
                    if used >= lease.duration_seconds as f64 {
                        tracing::warn!(
                            "failing proposal {} — lease {} has no time remaining",
                            proposal.id,
                            lease_id
                        );
                        self.store.fail_proposal(
                            &proposal.id,
                            &format!("lease {} exhausted ({:.0}s used of {}s)", lease_id, used, lease.duration_seconds),
                        )?;
                        continue;
                    }
                }
            }

            tracing::info!("dispatching proposal {}: {}", proposal.id, proposal.sprint_name);

            match self.bridge.launch(&proposal.provider_name, &proposal.resource_request, &proposal.config).await {
                Ok(handle) => {
                    self.store
                        .set_dispatching(&proposal.id, &handle.provider_job_id)?;
                    tracing::info!(
                        "proposal {} launched, job_id={}",
                        proposal.id,
                        handle.provider_job_id
                    );
                }
                Err(e) => {
                    tracing::error!("failed to launch proposal {}: {}", proposal.id, e);
                    self.store.fail_proposal(&proposal.id, &e.to_string())?;
                }
            }
        }

        Ok(())
    }

    async fn monitor_running(&self) -> anyhow::Result<()> {
        let running = self.store.list_proposals(Some("running"))?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();

        // Group by lease_id to check lease budgets once per lease
        let mut lease_checked: std::collections::HashSet<String> = std::collections::HashSet::new();

        for proposal in &running {
            if let Some(ref lease_id) = proposal.lease_id {
                if !lease_checked.contains(lease_id) {
                    lease_checked.insert(lease_id.clone());

                    if let Ok(Some(lease)) = self.store.get_lease(lease_id) {
                        let cumulative = self
                            .store
                            .cumulative_runtime_for_lease(lease_id, now)
                            .unwrap_or(0.0);

                        if cumulative >= lease.duration_seconds as f64 {
                            tracing::warn!(
                                "lease {} exhausted ({:.0}s / {}s) — killing all jobs",
                                lease_id,
                                cumulative,
                                lease.duration_seconds
                            );

                            // Kill all running jobs in this lease
                            for p in &running {
                                if p.lease_id.as_deref() == Some(lease_id) {
                                    let job_runtime = now - p.started_at.unwrap_or(now);
                                    let remaining_at_start =
                                        lease.duration_seconds as f64 - (cumulative - job_runtime);

                                    // Cancel via provider
                                    if let Some(ref job_id) = p.provider_job_id {
                                        let handle = JobHandle {
                                            provider_name: p.provider_name.clone(),
                                            provider_job_id: job_id.clone(),
                                            launched_at: p.dispatched_at.unwrap_or(0.0),
                                        };
                                        if let Err(e) =
                                            self.bridge.cancel(&p.provider_name, &handle).await
                                        {
                                            tracing::error!(
                                                "cancel error for proposal {}: {}",
                                                p.id,
                                                e
                                            );
                                        }
                                    }

                                    if let Err(e) = self.store.kill_proposal_lease_exceeded(
                                        &p.id,
                                        job_runtime,
                                        remaining_at_start,
                                    ) {
                                        tracing::error!(
                                            "failed to kill proposal {} for lease expiry: {}",
                                            p.id,
                                            e
                                        );
                                    }
                                }
                            }

                            // Expire the lease
                            if let Err(e) = self.store.expire_lease(lease_id) {
                                tracing::error!("failed to expire lease {}: {}", lease_id, e);
                            }
                        }
                    }
                }
            }
        }

        // Now handle per-proposal checks (timeout, polling) for proposals not already killed
        for proposal in &running {
            // Skip if already killed by lease expiry above
            if let Some(ref lease_id) = proposal.lease_id {
                if lease_checked.contains(lease_id) {
                    // Re-check if this proposal was killed
                    if let Ok(Some(p)) = self.store.get_proposal(&proposal.id) {
                        if p.status == ProposalStatus::Killed {
                            continue;
                        }
                    }
                }
            }

            let started_at = match proposal.started_at {
                Some(t) => t,
                None => continue,
            };

            // Check per-proposal timeout
            let elapsed = now - started_at;
            if elapsed >= proposal.resource_request.timeout_seconds as f64 {
                tracing::warn!("proposal {} timed out after {:.0}s", proposal.id, elapsed);
                self.cancel_and_kill(proposal, "timeout_exceeded").await;
                continue;
            }

            // Poll the provider for status
            if let Some(ref job_id) = proposal.provider_job_id {
                let handle = JobHandle {
                    provider_name: proposal.provider_name.clone(),
                    provider_job_id: job_id.clone(),
                    launched_at: proposal.dispatched_at.unwrap_or(started_at),
                };

                match self.bridge.poll(&proposal.provider_name, &handle).await {
                    Ok(ProviderResponse::Result(result)) => {
                        if result.status == "completed" {
                            self.store.complete_proposal(&proposal.id, None)?;
                            self.record_cost(proposal, result.gpu_seconds)?;
                        } else if result.status == "failed" {
                            self.store.fail_proposal(
                                &proposal.id,
                                result.error.as_deref().unwrap_or("unknown error"),
                            )?;
                        }
                    }
                    Ok(ProviderResponse::Status(status)) => {
                        tracing::trace!("proposal {} status: {}", proposal.id, status);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("poll error for proposal {}: {}", proposal.id, e);
                    }
                }
            }
        }

        Ok(())
    }

    async fn cancel_and_kill(&self, proposal: &Proposal, reason: &str) {
        if let Some(ref job_id) = proposal.provider_job_id {
            let handle = JobHandle {
                provider_name: proposal.provider_name.clone(),
                provider_job_id: job_id.clone(),
                launched_at: proposal.dispatched_at.unwrap_or(0.0),
            };
            if let Err(e) = self.bridge.cancel(&proposal.provider_name, &handle).await {
                tracing::error!("cancel error for proposal {}: {}", proposal.id, e);
            }
        }
        if let Err(e) = self.store.kill_proposal(&proposal.id, reason) {
            tracing::error!("failed to mark proposal {} as killed: {}", proposal.id, e);
        }
    }

    fn record_cost(&self, proposal: &Proposal, gpu_seconds: f64) -> anyhow::Result<()> {
        let rate = 0.0; // local provider is free; future: look up from provider capabilities
        let cost = gpu_seconds * proposal.resource_request.gpu_count as f64 * rate;

        let entry = CostEntry {
            id: format!("cost_{}", ulid::Ulid::new()),
            proposal_id: proposal.id.clone(),
            provider_name: proposal.provider_name.clone(),
            gpu_type: proposal.resource_request.gpu.clone(),
            gpu_count: proposal.resource_request.gpu_count,
            gpu_seconds,
            rate_per_gpu_second: rate,
            computed_cost_usd: cost,
            recorded_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs_f64(),
        };

        self.store.record_cost(&entry)
    }
}
