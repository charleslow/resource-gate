// Dispatch loop: monitors running jobs, enforces timeouts, lease budgets,
// and detects completion/failure via provider polling.
//
// Jobs are launched immediately at submission time (in the API handler).
// The dispatcher only needs to:
// 1. Monitor running jobs for lease exhaustion (kill + expire lease)
// 2. Enforce per-job timeouts
// 3. Poll providers for completion/failure

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{interval, Duration};

use crate::budget::BudgetEnforcer;
use crate::models::*;
use crate::provider_bridge::{ProviderBridge, ProviderResponse};
use crate::store::Store;

/// Grace period after a job reaches a terminal state before its provider
/// resources (container, instance, etc.) are cleaned up.  This gives the
/// agent time to call copy_out.  Each copy_in/copy_out call resets the
/// timer (tracked via `cleanup_deadlines`).
const CLEANUP_GRACE_SECS: f64 = 300.0; // 5 minutes

pub struct Dispatcher {
    store: Store,
    budget: Arc<BudgetEnforcer>,
    bridge: Arc<ProviderBridge>,
    poll_interval: Duration,
    paused: Arc<AtomicBool>,
    concurrency_limits: std::collections::HashMap<String, u32>,
    /// job_id → deadline (epoch seconds) at which cleanup() will be called.
    /// Inserted when a job transitions to a terminal state; removed after
    /// cleanup fires.  copy_in/copy_out push the deadline forward.
    cleanup_deadlines: std::sync::Mutex<std::collections::HashMap<String, f64>>,
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
            cleanup_deadlines: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn paused_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.paused)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Recover cleanup state after a restart.  Terminal jobs whose provider
    /// resources are still present (provider_job_id IS NOT NULL) are scheduled
    /// for immediate cleanup — their grace period has long since elapsed.
    fn recover_stale_cleanups(&self) {
        match self.store.list_jobs_needing_cleanup() {
            Ok(jobs) if jobs.is_empty() => {}
            Ok(jobs) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs_f64();
                let count = jobs.len();
                if let Ok(mut deadlines) = self.cleanup_deadlines.lock() {
                    for job in jobs {
                        deadlines.entry(job.id).or_insert(now); // due immediately
                    }
                }
                tracing::info!(
                    "recovered {} stale job(s) needing provider cleanup",
                    count
                );
            }
            Err(e) => {
                tracing::error!("failed to query stale jobs for cleanup: {}", e);
            }
        }
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
        self.recover_stale_cleanups();

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

    /// Reset the cleanup grace timer for a job.  Call this whenever a
    /// copy_in or copy_out is performed so the container stays alive while
    /// the agent is still transferring files.
    pub fn reset_cleanup_grace(&self, job_id: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        if let Ok(mut deadlines) = self.cleanup_deadlines.lock() {
            if deadlines.contains_key(job_id) {
                deadlines.insert(job_id.to_string(), now + CLEANUP_GRACE_SECS);
                tracing::debug!("reset cleanup grace for job {}", job_id);
            }
        }
    }

    /// Schedule a job for cleanup after the grace period elapses.
    fn schedule_cleanup(&self, job_id: &str, now: f64) {
        if let Ok(mut deadlines) = self.cleanup_deadlines.lock() {
            deadlines.entry(job_id.to_string()).or_insert(now + CLEANUP_GRACE_SECS);
        }
    }

    async fn tick(&self) -> anyhow::Result<()> {
        self.monitor_running().await?;
        self.run_pending_cleanups().await;
        Ok(())
    }

    async fn run_pending_cleanups(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();

        // Collect job IDs whose grace period has elapsed.
        let due: Vec<String> = {
            let deadlines = match self.cleanup_deadlines.lock() {
                Ok(d) => d,
                Err(_) => return,
            };
            deadlines
                .iter()
                .filter(|(_, &deadline)| now >= deadline)
                .map(|(id, _)| id.clone())
                .collect()
        };

        for job_id in &due {
            // Look up the job to get provider info
            if let Ok(Some(job)) = self.store.get_job(job_id) {
                if let Some(ref provider_job_id) = job.provider_job_id {
                    let handle = JobHandle {
                        provider_name: job.provider_name.clone(),
                        provider_job_id: provider_job_id.clone(),
                        launched_at: job.dispatched_at.unwrap_or(0.0),
                    };
                    if let Err(e) = self.bridge.cleanup(&job.provider_name, &handle).await {
                        tracing::warn!("cleanup failed for job {}: {}", job_id, e);
                        // Don't remove from deadlines — retry next tick
                        continue;
                    }
                    // Mark in DB so this job won't appear in future startup sweeps
                    if let Err(e) = self.store.mark_job_cleaned_up(job_id) {
                        tracing::warn!("failed to mark job {} as cleaned up: {}", job_id, e);
                    }
                    tracing::info!("cleaned up job {} (provider resources removed)", job_id);
                }
            }

            // Remove from deadlines map
            if let Ok(mut deadlines) = self.cleanup_deadlines.lock() {
                deadlines.remove(job_id);
            }
        }
    }

    async fn monitor_running(&self) -> anyhow::Result<()> {
        let running = self.store.list_jobs(Some("running"))?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();

        // Track job IDs killed during lease expiry to avoid redundant DB reads
        let mut killed_job_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        // Group by lease_id to check lease budgets once per lease
        let mut lease_checked: std::collections::HashSet<String> = std::collections::HashSet::new();

        for job in &running {
            if let Some(ref lease_id) = job.lease_id {
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
                            for j in &running {
                                if j.lease_id.as_deref() == Some(lease_id) {
                                    let job_runtime = now - j.started_at.unwrap_or(now);
                                    let remaining_at_start =
                                        lease.duration_seconds as f64 - (cumulative - job_runtime);

                                    // Cancel via provider
                                    if let Some(ref job_id) = j.provider_job_id {
                                        let handle = JobHandle {
                                            provider_name: j.provider_name.clone(),
                                            provider_job_id: job_id.clone(),
                                            launched_at: j.dispatched_at.unwrap_or(0.0),
                                        };
                                        if let Err(e) =
                                            self.bridge.cancel(&j.provider_name, &handle).await
                                        {
                                            tracing::error!(
                                                "cancel error for job {}: {}",
                                                j.id,
                                                e
                                            );
                                        }
                                    }

                                    if let Err(e) = self.store.kill_job_lease_exceeded(
                                        &j.id,
                                        job_runtime,
                                        remaining_at_start,
                                    ) {
                                        tracing::error!(
                                            "failed to kill job {} for lease expiry: {}",
                                            j.id,
                                            e
                                        );
                                    }

                                    self.schedule_cleanup(&j.id, now);
                                    killed_job_ids.insert(j.id.clone());
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

        // Expire approved leases that have exceeded their wall-clock deadline
        // (2x duration_seconds since approval) even if no jobs consumed the time.
        const WALL_CLOCK_MULTIPLIER: f64 = 2.0;
        if let Ok(stale_leases) = self
            .store
            .list_wall_clock_expired_leases(now, WALL_CLOCK_MULTIPLIER)
        {
            for lease in &stale_leases {
                tracing::warn!(
                    "lease {} exceeded wall-clock deadline ({:.0}s since approval, limit {}s) — expiring",
                    lease.id,
                    now - lease.approved_at.unwrap_or(now),
                    lease.duration_seconds as f64 * WALL_CLOCK_MULTIPLIER,
                );

                // Kill any running jobs under this lease
                if let Ok(lease_jobs) = self.store.list_jobs_for_lease(&lease.id) {
                    for j in &lease_jobs {
                        if j.status == JobStatus::Running && !killed_job_ids.contains(&j.id) {
                            self.cancel_and_kill(j, "wall_clock_expired").await;
                            killed_job_ids.insert(j.id.clone());
                        }
                    }
                }

                if let Err(e) = self.store.expire_lease(&lease.id) {
                    tracing::error!("failed to expire stale lease {}: {}", lease.id, e);
                }
            }
        }

        // Now handle per-job checks (timeout, polling) for jobs not already killed
        for job in &running {
            // Skip if already killed by lease expiry above (in-memory check, no DB read)
            if killed_job_ids.contains(&job.id) {
                continue;
            }

            let started_at = match job.started_at {
                Some(t) => t,
                None => continue,
            };

            // Check per-job timeout
            let elapsed = now - started_at;
            if elapsed >= job.resource_request.timeout_seconds as f64 {
                tracing::warn!("job {} timed out after {:.0}s", job.id, elapsed);
                self.cancel_and_kill(job, "timeout_exceeded").await;
                continue;
            }

            // Poll the provider for status
            if let Some(ref job_id) = job.provider_job_id {
                let handle = JobHandle {
                    provider_name: job.provider_name.clone(),
                    provider_job_id: job_id.clone(),
                    launched_at: job.dispatched_at.unwrap_or(started_at),
                };

                match self.bridge.poll(&job.provider_name, &handle).await {
                    Ok(ProviderResponse::Result(result)) => {
                        if result.status == "completed" {
                            self.store.complete_job(&job.id, None)?;
                            self.record_cost(job, result.gpu_seconds)?;
                            self.schedule_cleanup(&job.id, now);
                        } else if result.status == "failed" {
                            self.store.fail_job(
                                &job.id,
                                result.error.as_deref().unwrap_or("unknown error"),
                            )?;
                            self.schedule_cleanup(&job.id, now);
                        }
                    }
                    Ok(ProviderResponse::Status(status)) => {
                        tracing::trace!("job {} status: {}", job.id, status);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("poll error for job {}: {}", job.id, e);
                    }
                }
            }
        }

        Ok(())
    }

    async fn cancel_and_kill(&self, job: &Job, reason: &str) {
        if let Some(ref job_id) = job.provider_job_id {
            let handle = JobHandle {
                provider_name: job.provider_name.clone(),
                provider_job_id: job_id.clone(),
                launched_at: job.dispatched_at.unwrap_or(0.0),
            };
            if let Err(e) = self.bridge.cancel(&job.provider_name, &handle).await {
                tracing::error!("cancel error for job {}: {}", job.id, e);
            }
        }
        if let Err(e) = self.store.kill_job(&job.id, reason) {
            tracing::error!("failed to mark job {} as killed: {}", job.id, e);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        self.schedule_cleanup(&job.id, now);
    }

    fn record_cost(&self, job: &Job, gpu_seconds: f64) -> anyhow::Result<()> {
        let rate = 0.0; // local provider is free; future: look up from provider capabilities
        let cost = gpu_seconds * job.resource_request.gpu_count as f64 * rate;

        let entry = CostEntry {
            id: format!("cost_{}", ulid::Ulid::new()),
            job_id: job.id.clone(),
            provider_name: job.provider_name.clone(),
            gpu_type: job.resource_request.gpu.clone(),
            gpu_count: job.resource_request.gpu_count,
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
