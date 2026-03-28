// SQLite operations for jobs, leases, and cost ledger (rusqlite)

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::*;

pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Clone for Store {
    fn clone(&self) -> Self {
        Store {
            conn: Arc::clone(&self.conn),
        }
    }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

impl Store {
    /// Create an in-memory store for testing.
    #[cfg(test)]
    pub fn new_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Store {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let store = Store {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS jobs (
                id TEXT PRIMARY KEY,
                status TEXT NOT NULL,
                sprint_name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                provider_name TEXT NOT NULL,
                resource_request TEXT NOT NULL,
                config TEXT NOT NULL DEFAULT '{}',
                budget_cap_usd REAL NOT NULL,
                estimated_minutes INTEGER,
                tags TEXT NOT NULL DEFAULT '{}',
                provider_job_id TEXT,
                created_at REAL NOT NULL,
                dispatched_at REAL,
                started_at REAL,
                ended_at REAL,
                result_payload TEXT,
                error TEXT,
                kill_reason TEXT,
                lease_id TEXT,
                runtime_seconds REAL,
                lease_remaining_at_start REAL
            );

            CREATE TABLE IF NOT EXISTS cost_ledger (
                id TEXT PRIMARY KEY,
                job_id TEXT NOT NULL,
                provider_name TEXT NOT NULL,
                gpu_type TEXT NOT NULL,
                gpu_count INTEGER NOT NULL,
                gpu_seconds REAL NOT NULL,
                rate_per_gpu_second REAL NOT NULL,
                computed_cost_usd REAL NOT NULL,
                recorded_at REAL NOT NULL,
                FOREIGN KEY (job_id) REFERENCES jobs(id)
            );

            CREATE TABLE IF NOT EXISTS daily_budget_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                daily_limit_usd REAL NOT NULL,
                alert_threshold_pct INTEGER NOT NULL DEFAULT 80,
                auto_pause_on_limit INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS leases (
                id TEXT PRIMARY KEY,
                status TEXT NOT NULL,
                provider_name TEXT NOT NULL,
                gpu TEXT NOT NULL DEFAULT 'cpu-only',
                duration_seconds INTEGER NOT NULL,
                created_at REAL NOT NULL,
                approved_at REAL,
                rejected_at REAL,
                expired_at REAL
            );
            ",
        )?;

        Ok(())
    }

    // -- Jobs --

    pub fn insert_job(&self, job: &Job) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO jobs (
                id, status, sprint_name, description, provider_name,
                resource_request, config, budget_cap_usd, estimated_minutes, tags,
                provider_job_id, created_at, dispatched_at,
                started_at, ended_at, result_payload, error, kill_reason,
                lease_id, runtime_seconds, lease_remaining_at_start
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
            params![
                job.id,
                job.status.to_string(),
                job.sprint_name,
                job.description,
                job.provider_name,
                serde_json::to_string(&job.resource_request)?,
                job.config.to_string(),
                job.budget_cap_usd,
                job.estimated_minutes,
                job.tags.to_string(),
                job.provider_job_id,
                job.created_at,
                job.dispatched_at,
                job.started_at,
                job.ended_at,
                job.result_payload.as_ref().map(|v| v.to_string()),
                job.error,
                job.kill_reason,
                job.lease_id,
                job.runtime_seconds,
                job.lease_remaining_at_start,
            ],
        )?;
        Ok(())
    }

    pub fn get_job(&self, id: &str) -> anyhow::Result<Option<Job>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, status, sprint_name, description, provider_name,
                    resource_request, config, budget_cap_usd, estimated_minutes, tags,
                    provider_job_id, created_at, dispatched_at,
                    started_at, ended_at, result_payload, error, kill_reason,
                    lease_id, runtime_seconds, lease_remaining_at_start
             FROM jobs WHERE id = ?1",
        )?;
        let result = stmt
            .query_row(params![id], |row| Ok(row_to_job(row)))
            .optional()?;
        match result {
            Some(Ok(j)) => Ok(Some(j)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    pub fn list_jobs(&self, status_filter: Option<&str>) -> anyhow::Result<Vec<Job>> {
        let conn = self.conn.lock().unwrap();
        let mut jobs = Vec::new();

        if let Some(status) = status_filter {
            let mut stmt = conn.prepare(
                "SELECT id, status, sprint_name, description, provider_name,
                        resource_request, config, budget_cap_usd, estimated_minutes, tags,
                        provider_job_id, created_at, dispatched_at,
                        started_at, ended_at, result_payload, error, kill_reason,
                        lease_id, runtime_seconds, lease_remaining_at_start
                 FROM jobs WHERE status = ?1 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map(params![status], |row| Ok(row_to_job(row)))?;
            for row in rows {
                jobs.push(row??);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, status, sprint_name, description, provider_name,
                        resource_request, config, budget_cap_usd, estimated_minutes, tags,
                        provider_job_id, created_at, dispatched_at,
                        started_at, ended_at, result_payload, error, kill_reason,
                        lease_id, runtime_seconds, lease_remaining_at_start
                 FROM jobs ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map([], |row| Ok(row_to_job(row)))?;
            for row in rows {
                jobs.push(row??);
            }
        }

        Ok(jobs)
    }

    pub fn set_job_running(&self, id: &str, provider_job_id: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET status = 'running', provider_job_id = ?1, dispatched_at = ?2, started_at = ?2 WHERE id = ?3",
            params![provider_job_id, now(), id],
        )?;
        Ok(())
    }

    pub fn complete_job(
        &self,
        id: &str,
        result_payload: Option<&serde_json::Value>,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET status = 'completed', ended_at = ?1, result_payload = ?2 WHERE id = ?3",
            params![now(), result_payload.map(|v| v.to_string()), id],
        )?;
        Ok(())
    }

    pub fn fail_job(&self, id: &str, error: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET status = 'failed', ended_at = ?1, error = ?2 WHERE id = ?3",
            params![now(), error, id],
        )?;
        Ok(())
    }

    pub fn kill_job(&self, id: &str, reason: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET status = 'killed', ended_at = ?1, kill_reason = ?2 WHERE id = ?3",
            params![now(), reason, id],
        )?;
        Ok(())
    }

    // -- Cost ledger --

    pub fn record_cost(&self, entry: &CostEntry) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO cost_ledger (
                id, job_id, provider_name, gpu_type, gpu_count,
                gpu_seconds, rate_per_gpu_second, computed_cost_usd, recorded_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                entry.id,
                entry.job_id,
                entry.provider_name,
                entry.gpu_type,
                entry.gpu_count,
                entry.gpu_seconds,
                entry.rate_per_gpu_second,
                entry.computed_cost_usd,
                entry.recorded_at,
            ],
        )?;
        Ok(())
    }

    pub fn daily_spend(&self) -> anyhow::Result<f64> {
        let conn = self.conn.lock().unwrap();
        let today_start = today_start_epoch();
        let spend: f64 = conn
            .query_row(
                "SELECT COALESCE(SUM(computed_cost_usd), 0.0) FROM cost_ledger WHERE recorded_at >= ?1",
                params![today_start],
                |row| row.get(0),
            )?;
        Ok(spend)
    }

    // -- Leases --

    pub fn insert_lease(&self, lease: &Lease) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO leases (id, status, provider_name, gpu, duration_seconds, created_at, approved_at, rejected_at, expired_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                lease.id,
                lease.status.to_string(),
                lease.provider_name,
                lease.gpu,
                lease.duration_seconds,
                lease.created_at,
                lease.approved_at,
                lease.rejected_at,
                lease.expired_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_lease(&self, id: &str) -> anyhow::Result<Option<Lease>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, status, provider_name, gpu, duration_seconds, created_at, approved_at, rejected_at, expired_at
             FROM leases WHERE id = ?1",
        )?;
        let result = stmt
            .query_row(params![id], |row| Ok(row_to_lease(row)))
            .optional()?;
        match result {
            Some(Ok(l)) => Ok(Some(l)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    pub fn list_leases(&self, status_filter: Option<&str>) -> anyhow::Result<Vec<Lease>> {
        let conn = self.conn.lock().unwrap();
        let mut leases = Vec::new();

        if let Some(status) = status_filter {
            let mut stmt = conn.prepare(
                "SELECT id, status, provider_name, gpu, duration_seconds, created_at, approved_at, rejected_at, expired_at
                 FROM leases WHERE status = ?1 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map(params![status], |row| Ok(row_to_lease(row)))?;
            for row in rows {
                leases.push(row??);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, status, provider_name, gpu, duration_seconds, created_at, approved_at, rejected_at, expired_at
                 FROM leases ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map([], |row| Ok(row_to_lease(row)))?;
            for row in rows {
                leases.push(row??);
            }
        }

        Ok(leases)
    }

    pub fn approve_lease(&self, id: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE leases SET status = 'approved', approved_at = ?1 WHERE id = ?2 AND status = 'pending'",
            params![now(), id],
        )?;
        Ok(())
    }

    pub fn reject_lease(&self, id: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE leases SET status = 'rejected', rejected_at = ?1 WHERE id = ?2 AND status = 'pending'",
            params![now(), id],
        )?;
        Ok(())
    }

    pub fn expire_lease(&self, id: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE leases SET status = 'expired', expired_at = ?1 WHERE id = ?2 AND status = 'approved'",
            params![now(), id],
        )?;
        Ok(())
    }

    /// Compute cumulative container runtime for all jobs in a lease.
    /// Uses COALESCE(ended_at, now_ts) so running jobs contribute their in-progress time.
    pub fn cumulative_runtime_for_lease(&self, lease_id: &str, now_ts: f64) -> anyhow::Result<f64> {
        let conn = self.conn.lock().unwrap();
        let runtime: f64 = conn.query_row(
            "SELECT COALESCE(SUM(COALESCE(ended_at, ?1) - started_at), 0.0)
             FROM jobs
             WHERE lease_id = ?2
               AND started_at IS NOT NULL
               AND status IN ('running', 'completed', 'failed', 'killed')",
            params![now_ts, lease_id],
            |row| row.get(0),
        )?;
        Ok(runtime)
    }

    /// Count running jobs for a given provider (for concurrency enforcement).
    pub fn count_running_jobs_for_provider(&self, provider_name: &str) -> anyhow::Result<u32> {
        let conn = self.conn.lock().unwrap();
        let count: u32 = conn.query_row(
            "SELECT COUNT(*) FROM jobs WHERE provider_name = ?1 AND status = 'running'",
            params![provider_name],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// List all jobs belonging to a lease.
    pub fn list_jobs_for_lease(&self, lease_id: &str) -> anyhow::Result<Vec<Job>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, status, sprint_name, description, provider_name,
                    resource_request, config, budget_cap_usd, estimated_minutes, tags,
                    provider_job_id, created_at, dispatched_at,
                    started_at, ended_at, result_payload, error, kill_reason,
                    lease_id, runtime_seconds, lease_remaining_at_start
             FROM jobs WHERE lease_id = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![lease_id], |row| Ok(row_to_job(row)))?;
        let mut jobs = Vec::new();
        for row in rows {
            jobs.push(row??);
        }
        Ok(jobs)
    }

    /// List terminal jobs (completed/failed/killed) that still have a
    /// provider_job_id — i.e. their provider resources haven't been cleaned up.
    pub fn list_jobs_needing_cleanup(&self) -> anyhow::Result<Vec<Job>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, status, sprint_name, description, provider_name,
                    resource_request, config, budget_cap_usd, estimated_minutes, tags,
                    provider_job_id, created_at, dispatched_at,
                    started_at, ended_at, result_payload, error, kill_reason,
                    lease_id, runtime_seconds, lease_remaining_at_start
             FROM jobs
             WHERE status IN ('completed', 'failed', 'killed')
               AND provider_job_id IS NOT NULL
             ORDER BY ended_at ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok(row_to_job(row)))?;
        let mut jobs = Vec::new();
        for row in rows {
            jobs.push(row??);
        }
        Ok(jobs)
    }

    /// Clear the provider_job_id for a job, marking its provider resources
    /// as cleaned up.  After this, the job won't appear in
    /// `list_jobs_needing_cleanup`.
    pub fn mark_job_cleaned_up(&self, id: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET provider_job_id = NULL WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Kill a job with lease expiry details.
    pub fn kill_job_lease_exceeded(
        &self,
        id: &str,
        runtime_seconds: f64,
        lease_remaining_at_start: f64,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET status = 'killed', ended_at = ?1, kill_reason = 'lease_time_exceeded',
             runtime_seconds = ?2, lease_remaining_at_start = ?3 WHERE id = ?4",
            params![now(), runtime_seconds, lease_remaining_at_start, id],
        )?;
        Ok(())
    }

    /// Atomically check lease validity + concurrency limit + insert job.
    /// Returns remaining_seconds on success, or an error describing why the job was rejected.
    /// This prevents race conditions where concurrent submissions both pass checks
    /// before either inserts.
    pub fn atomic_check_and_insert_job(
        &self,
        job: &Job,
        lease_id: &str,
        provider_name: &str,
        expected_gpu: &str,
        max_concurrent: u32,
        now_ts: f64,
    ) -> Result<f64, JobInsertError> {
        let conn = self.conn.lock().unwrap();

        // 1. Check lease exists, is approved, and matches provider/GPU
        let lease_row: Option<(String, String, String, u64)> = conn
            .query_row(
                "SELECT status, provider_name, gpu, duration_seconds FROM leases WHERE id = ?1",
                params![lease_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|e| JobInsertError::Internal(e.to_string()))?;

        let (status, lease_provider, lease_gpu, duration_seconds) = match lease_row {
            Some(r) => r,
            None => {
                return Err(JobInsertError::Rejected(format!(
                    "lease {} not found",
                    lease_id
                )));
            }
        };

        if status != "approved" {
            return Err(JobInsertError::Rejected(format!(
                "lease {} is {}, not approved",
                lease_id, status
            )));
        }

        if lease_provider != provider_name {
            return Err(JobInsertError::Rejected(format!(
                "lease provider '{}' does not match job provider '{}'",
                lease_provider, provider_name
            )));
        }

        if lease_gpu != expected_gpu {
            return Err(JobInsertError::Rejected(format!(
                "lease gpu '{}' does not match job gpu '{}'",
                lease_gpu, expected_gpu
            )));
        }

        // 2. Check lease time remaining
        let used: f64 = conn
            .query_row(
                "SELECT COALESCE(SUM(COALESCE(ended_at, ?1) - started_at), 0.0)
                 FROM jobs
                 WHERE lease_id = ?2
                   AND started_at IS NOT NULL
                   AND status IN ('running', 'completed', 'failed', 'killed')",
                params![now_ts, lease_id],
                |row| row.get(0),
            )
            .map_err(|e| JobInsertError::Internal(e.to_string()))?;

        let remaining = duration_seconds as f64 - used;
        if remaining <= 0.0 {
            return Err(JobInsertError::Rejected(format!(
                "lease {} has no time remaining ({:.0}s used of {}s)",
                lease_id, used, duration_seconds
            )));
        }

        // 3. Check concurrency limit
        let running: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE provider_name = ?1 AND status = 'running'",
                params![provider_name],
                |row| row.get(0),
            )
            .map_err(|e| JobInsertError::Internal(e.to_string()))?;

        if running >= max_concurrent {
            return Err(JobInsertError::ConcurrencyLimit(format!(
                "provider '{}' at concurrency limit ({}/{})",
                provider_name, running, max_concurrent
            )));
        }

        // 4. Insert the job (all checks passed while holding the lock)
        conn.execute(
            "INSERT INTO jobs (
                id, status, sprint_name, description, provider_name,
                resource_request, config, budget_cap_usd, estimated_minutes, tags,
                provider_job_id, created_at, dispatched_at,
                started_at, ended_at, result_payload, error, kill_reason,
                lease_id, runtime_seconds, lease_remaining_at_start
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
            params![
                job.id,
                job.status.to_string(),
                job.sprint_name,
                job.description,
                job.provider_name,
                serde_json::to_string(&job.resource_request).map_err(|e| JobInsertError::Internal(e.to_string()))?,
                job.config.to_string(),
                job.budget_cap_usd,
                job.estimated_minutes,
                job.tags.to_string(),
                job.provider_job_id,
                job.created_at,
                job.dispatched_at,
                job.started_at,
                job.ended_at,
                job.result_payload.as_ref().map(|v| v.to_string()),
                job.error,
                job.kill_reason,
                job.lease_id,
                job.runtime_seconds,
                job.lease_remaining_at_start,
            ],
        )
        .map_err(|e| JobInsertError::Internal(e.to_string()))?;

        Ok(remaining)
    }

    /// Find approved leases that have exceeded their wall-clock deadline
    /// (2x duration_seconds since approval). These should be expired even if
    /// no jobs have consumed the lease time, to prevent stale leases.
    pub fn list_wall_clock_expired_leases(&self, now_ts: f64, multiplier: f64) -> anyhow::Result<Vec<Lease>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, status, provider_name, gpu, duration_seconds, created_at, approved_at, rejected_at, expired_at
             FROM leases
             WHERE status = 'approved'
               AND approved_at IS NOT NULL
               AND (?1 - approved_at) > (duration_seconds * ?2)",
        )?;
        let rows = stmt.query_map(params![now_ts, multiplier], |row| Ok(row_to_lease(row)))?;
        let mut leases = Vec::new();
        for row in rows {
            leases.push(row??);
        }
        Ok(leases)
    }

    // -- Budget config --

    pub fn set_budget_config(
        &self,
        daily_limit: f64,
        alert_pct: u32,
        auto_pause: bool,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO daily_budget_config (id, daily_limit_usd, alert_threshold_pct, auto_pause_on_limit)
             VALUES (1, ?1, ?2, ?3)",
            params![daily_limit, alert_pct, auto_pause as i32],
        )?;
        Ok(())
    }
}

/// Error returned by `atomic_check_and_insert_job` when the job cannot be inserted.
#[derive(Debug)]
pub enum JobInsertError {
    /// Lease validation failed (not found, not approved, GPU mismatch, no time remaining).
    Rejected(String),
    /// Provider concurrency limit reached.
    ConcurrencyLimit(String),
    /// Internal database or serialization error.
    Internal(String),
}

impl std::fmt::Display for JobInsertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(msg) => write!(f, "{}", msg),
            Self::ConcurrencyLimit(msg) => write!(f, "{}", msg),
            Self::Internal(msg) => write!(f, "internal error: {}", msg),
        }
    }
}

fn today_start_epoch() -> f64 {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Round down to start of UTC day
    let day_secs = now_secs - (now_secs % 86400);
    day_secs as f64
}

fn row_to_lease(row: &rusqlite::Row) -> anyhow::Result<Lease> {
    let status_str: String = row.get(1)?;
    Ok(Lease {
        id: row.get(0)?,
        status: serde_json::from_value(serde_json::Value::String(status_str))?,
        provider_name: row.get(2)?,
        gpu: row.get(3)?,
        duration_seconds: row.get(4)?,
        created_at: row.get(5)?,
        approved_at: row.get(6)?,
        rejected_at: row.get(7)?,
        expired_at: row.get(8)?,
    })
}

fn row_to_job(row: &rusqlite::Row) -> anyhow::Result<Job> {
    let status_str: String = row.get(1)?;
    let rr_str: String = row.get(5)?;
    let config_str: String = row.get(6)?;
    let tags_str: String = row.get(9)?;
    let result_str: Option<String> = row.get(15)?;

    Ok(Job {
        id: row.get(0)?,
        status: serde_json::from_value(serde_json::Value::String(status_str))?,
        sprint_name: row.get(2)?,
        description: row.get(3)?,
        provider_name: row.get(4)?,
        resource_request: serde_json::from_str(&rr_str)?,
        config: serde_json::from_str(&config_str)?,
        budget_cap_usd: row.get(7)?,
        estimated_minutes: row.get(8)?,
        tags: serde_json::from_str(&tags_str)?,
        provider_job_id: row.get(10)?,
        created_at: row.get(11)?,
        dispatched_at: row.get(12)?,
        started_at: row.get(13)?,
        ended_at: row.get(14)?,
        result_payload: result_str.map(|s| serde_json::from_str(&s)).transpose()?,
        error: row.get(16)?,
        kill_reason: row.get(17)?,
        lease_id: row.get(18)?,
        runtime_seconds: row.get(19)?,
        lease_remaining_at_start: row.get(20)?,
    })
}
