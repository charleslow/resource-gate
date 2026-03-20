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
}

impl Dispatcher {
    pub fn new(
        store: Store,
        budget: Arc<BudgetEnforcer>,
        bridge: Arc<ProviderBridge>,
        poll_interval_secs: u64,
    ) -> Self {
        Self {
            store,
            budget,
            bridge,
            poll_interval: Duration::from_secs(poll_interval_secs),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn paused_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.paused)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

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

        for proposal in approved {
            tracing::info!("dispatching proposal {}: {}", proposal.id, proposal.sprint_name);

            match self.bridge.launch(&proposal.provider_name, &proposal.resource_request).await {
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

        for proposal in running {
            let started_at = match proposal.started_at {
                Some(t) => t,
                None => continue,
            };

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs_f64();

            // Check timeout
            let elapsed = now - started_at;
            if elapsed >= proposal.resource_request.timeout_seconds as f64 {
                tracing::warn!("proposal {} timed out after {:.0}s", proposal.id, elapsed);
                self.cancel_and_kill(&proposal, "timeout_exceeded").await;
                continue;
            }

            // Check per-sprint budget (for providers with non-zero rates)
            // Local provider has rate 0.0, so this is a no-op for local
            // For future providers, this would compute cost and kill if exceeded

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
                            self.record_cost(&proposal, result.gpu_seconds)?;
                        } else if result.status == "failed" {
                            self.store.fail_proposal(
                                &proposal.id,
                                result.error.as_deref().unwrap_or("unknown error"),
                            )?;
                        }
                    }
                    Ok(ProviderResponse::Status(status)) => {
                        // Still running, nothing to do
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
