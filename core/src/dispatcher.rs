// Dispatch loop: monitors running jobs, enforces timeouts, lease budgets,
// and detects completion/failure via provider polling.
//
// Jobs are launched immediately at submission time (in the API handler).
// The dispatcher only needs to:
// 1. Monitor running jobs for lease exhaustion (kill + expire lease)
// 2. Enforce per-job timeouts
// 3. Poll providers for completion/failure

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{interval, Duration};

use crate::budget::BudgetEnforcer;
use crate::models::*;
use crate::provider_bridge::{ProviderBridge, ProviderResponse};
use crate::store::Store;

/// Grace period (seconds) after terminal state before cleanup fires.
const CLEANUP_GRACE_SECS: f64 = 300.0;

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

fn handle_for_job(job: &Job) -> Option<JobHandle> {
    job.provider_job_id.as_ref().map(|pjid| JobHandle {
        provider_name: job.provider_name.clone(),
        provider_job_id: pjid.clone(),
        launched_at: job.dispatched_at.unwrap_or(0.0),
    })
}

pub struct Dispatcher {
    store: Store,
    budget: Arc<BudgetEnforcer>,
    bridge: Arc<ProviderBridge>,
    poll_interval: Duration,
    paused: Arc<AtomicBool>,
    concurrency_limits: HashMap<String, u32>,
    /// job_id → deadline (epoch secs) at which cleanup() fires.
    cleanup_deadlines: Mutex<HashMap<String, f64>>,
}

impl Dispatcher {
    pub fn new(
        store: Store,
        budget: Arc<BudgetEnforcer>,
        bridge: Arc<ProviderBridge>,
        poll_interval_secs: u64,
        concurrency_limits: HashMap<String, u32>,
    ) -> Self {
        Self {
            store, budget, bridge,
            poll_interval: Duration::from_secs(poll_interval_secs),
            paused: Arc::new(AtomicBool::new(false)),
            concurrency_limits,
            cleanup_deadlines: Mutex::new(HashMap::new()),
        }
    }

    pub fn paused_flag(&self) -> Arc<AtomicBool> { Arc::clone(&self.paused) }
    pub fn is_paused(&self) -> bool { self.paused.load(Ordering::Relaxed) }

    /// Reset the cleanup grace timer for a job (called on copy_in/copy_out).
    pub fn reset_cleanup_grace(&self, job_id: &str) {
        if let Ok(mut deadlines) = self.cleanup_deadlines.lock() {
            if deadlines.contains_key(job_id) {
                deadlines.insert(job_id.to_string(), now() + CLEANUP_GRACE_SECS);
                tracing::debug!("reset cleanup grace for job {}", job_id);
            }
        }
    }

    fn schedule_cleanup(&self, job_id: &str) {
        if let Ok(mut deadlines) = self.cleanup_deadlines.lock() {
            deadlines.entry(job_id.to_string()).or_insert(now() + CLEANUP_GRACE_SECS);
        }
    }

    /// Recover cleanup state after restart — stale terminal jobs get immediate cleanup.
    fn recover_stale_cleanups(&self) {
        let jobs = match self.store.list_jobs_needing_cleanup() {
            Ok(jobs) if jobs.is_empty() => return,
            Ok(jobs) => jobs,
            Err(e) => { tracing::error!("failed to query stale jobs for cleanup: {}", e); return; }
        };
        let count = jobs.len();
        let now = now();
        if let Ok(mut deadlines) = self.cleanup_deadlines.lock() {
            for job in jobs {
                deadlines.entry(job.id).or_insert(now);
            }
        }
        tracing::info!("recovered {} stale job(s) needing provider cleanup", count);
    }

    pub async fn run(self: Arc<Self>) {
        self.recover_stale_cleanups();
        let mut ticker = interval(self.poll_interval);
        tracing::info!("dispatcher started, polling every {}s", self.poll_interval.as_secs());

        loop {
            ticker.tick().await;
            if self.paused.load(Ordering::Relaxed) { continue; }
            if let Err(e) = self.tick().await {
                tracing::error!("dispatcher tick error: {}", e);
            }
        }
    }

    async fn tick(&self) -> anyhow::Result<()> {
        self.monitor_running().await?;
        self.run_pending_cleanups().await;
        Ok(())
    }

    async fn run_pending_cleanups(&self) {
        let now = now();
        let due: Vec<String> = {
            let Ok(deadlines) = self.cleanup_deadlines.lock() else { return };
            deadlines.iter()
                .filter(|(_, &d)| now >= d)
                .map(|(id, _)| id.clone())
                .collect()
        };

        for job_id in &due {
            if let Ok(Some(job)) = self.store.get_job(job_id) {
                if let Some(handle) = handle_for_job(&job) {
                    if let Err(e) = self.bridge.cleanup(&job.provider_name, &handle).await {
                        tracing::warn!("cleanup failed for job {}: {}", job_id, e);
                        continue; // retry next tick
                    }
                    if let Err(e) = self.store.mark_job_cleaned_up(job_id) {
                        tracing::warn!("failed to mark job {} as cleaned up: {}", job_id, e);
                    }
                    tracing::info!("cleaned up job {} (provider resources removed)", job_id);
                }
            }
            if let Ok(mut deadlines) = self.cleanup_deadlines.lock() {
                deadlines.remove(job_id);
            }
        }
    }

    async fn monitor_running(&self) -> anyhow::Result<()> {
        let running = self.store.list_jobs(Some("running"))?;
        let now = now();
        let mut killed_ids: HashSet<String> = HashSet::new();
        let mut lease_checked: HashSet<String> = HashSet::new();

        // Check lease budgets
        for job in &running {
            let Some(ref lease_id) = job.lease_id else { continue };
            if !lease_checked.insert(lease_id.clone()) { continue; }

            let Some(lease) = self.store.get_lease(lease_id).ok().flatten() else { continue };
            let cumulative = self.store.cumulative_runtime_for_lease(lease_id, now).unwrap_or(0.0);
            if cumulative < lease.duration_seconds as f64 { continue; }

            tracing::warn!("lease {} exhausted ({:.0}s / {}s) — killing all jobs",
                lease_id, cumulative, lease.duration_seconds);

            for j in &running {
                if j.lease_id.as_deref() != Some(lease_id) { continue; }
                let job_runtime = now - j.started_at.unwrap_or(now);
                let remaining_at_start = lease.duration_seconds as f64 - (cumulative - job_runtime);

                if let Some(handle) = handle_for_job(j) {
                    if let Err(e) = self.bridge.cancel(&j.provider_name, &handle).await {
                        tracing::error!("cancel error for job {}: {}", j.id, e);
                    }
                }
                if let Err(e) = self.store.kill_job_lease_exceeded(&j.id, job_runtime, remaining_at_start) {
                    tracing::error!("failed to kill job {} for lease expiry: {}", j.id, e);
                }
                self.schedule_cleanup(&j.id);
                killed_ids.insert(j.id.clone());
            }

            if let Err(e) = self.store.expire_lease(lease_id) {
                tracing::error!("failed to expire lease {}: {}", lease_id, e);
            }
        }

        // Wall-clock lease expiry
        const WALL_CLOCK_MULTIPLIER: f64 = 2.0;
        if let Ok(stale_leases) = self.store.list_wall_clock_expired_leases(now, WALL_CLOCK_MULTIPLIER) {
            for lease in &stale_leases {
                tracing::warn!("lease {} exceeded wall-clock deadline — expiring", lease.id);
                if let Ok(lease_jobs) = self.store.list_jobs_for_lease(&lease.id) {
                    for j in &lease_jobs {
                        if j.status == JobStatus::Running && killed_ids.insert(j.id.clone()) {
                            self.cancel_and_kill(j, "wall_clock_expired").await;
                        }
                    }
                }
                if let Err(e) = self.store.expire_lease(&lease.id) {
                    tracing::error!("failed to expire stale lease {}: {}", lease.id, e);
                }
            }
        }

        // Per-job: timeout + polling
        for job in &running {
            if killed_ids.contains(&job.id) { continue; }
            let Some(started_at) = job.started_at else { continue };

            if now - started_at >= job.resource_request.timeout_seconds as f64 {
                tracing::warn!("job {} timed out after {:.0}s", job.id, now - started_at);
                self.cancel_and_kill(job, "timeout_exceeded").await;
                continue;
            }

            let Some(handle) = handle_for_job(job) else { continue };
            match self.bridge.poll(&job.provider_name, &handle).await {
                Ok(ProviderResponse::Result(result)) => {
                    if result.status == "completed" {
                        self.store.complete_job(&job.id, None)?;
                        self.record_cost(job, result.gpu_seconds)?;
                    } else if result.status == "failed" {
                        self.store.fail_job(&job.id, result.error.as_deref().unwrap_or("unknown error"))?;
                    }
                    self.schedule_cleanup(&job.id);
                }
                Ok(ProviderResponse::Status(status)) => {
                    tracing::trace!("job {} status: {}", job.id, status);
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("poll error for job {}: {}", job.id, e),
            }
        }

        Ok(())
    }

    async fn cancel_and_kill(&self, job: &Job, reason: &str) {
        if let Some(handle) = handle_for_job(job) {
            if let Err(e) = self.bridge.cancel(&job.provider_name, &handle).await {
                tracing::error!("cancel error for job {}: {}", job.id, e);
            }
        }
        if let Err(e) = self.store.kill_job(&job.id, reason) {
            tracing::error!("failed to mark job {} as killed: {}", job.id, e);
        }
        self.schedule_cleanup(&job.id);
    }

    fn record_cost(&self, job: &Job, gpu_seconds: f64) -> anyhow::Result<()> {
        let rate = 0.0;
        let cost = gpu_seconds * job.resource_request.gpu_count as f64 * rate;
        self.store.record_cost(&CostEntry {
            id: format!("cost_{}", ulid::Ulid::new()),
            job_id: job.id.clone(),
            provider_name: job.provider_name.clone(),
            gpu_type: job.resource_request.gpu.clone(),
            gpu_count: job.resource_request.gpu_count,
            gpu_seconds,
            rate_per_gpu_second: rate,
            computed_cost_usd: cost,
            recorded_at: now(),
        })
    }
}
