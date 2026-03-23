// SQLite operations for proposals and cost ledger (rusqlite)

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
            CREATE TABLE IF NOT EXISTS proposals (
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
                approved_at REAL,
                dispatched_at REAL,
                started_at REAL,
                ended_at REAL,
                result_payload TEXT,
                error TEXT,
                kill_reason TEXT
            );

            CREATE TABLE IF NOT EXISTS cost_ledger (
                id TEXT PRIMARY KEY,
                proposal_id TEXT NOT NULL,
                provider_name TEXT NOT NULL,
                gpu_type TEXT NOT NULL,
                gpu_count INTEGER NOT NULL,
                gpu_seconds REAL NOT NULL,
                rate_per_gpu_second REAL NOT NULL,
                computed_cost_usd REAL NOT NULL,
                recorded_at REAL NOT NULL,
                FOREIGN KEY (proposal_id) REFERENCES proposals(id)
            );

            CREATE TABLE IF NOT EXISTS daily_budget_config (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                daily_limit_usd REAL NOT NULL,
                alert_threshold_pct INTEGER NOT NULL DEFAULT 80,
                auto_pause_on_limit INTEGER NOT NULL DEFAULT 0
            );
            ",
        )?;
        Ok(())
    }

    // -- Proposals --

    pub fn insert_proposal(&self, p: &Proposal) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO proposals (
                id, status, sprint_name, description, provider_name,
                resource_request, config, budget_cap_usd, estimated_minutes, tags,
                provider_job_id, created_at, approved_at, dispatched_at,
                started_at, ended_at, result_payload, error, kill_reason
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                p.id,
                p.status.to_string(),
                p.sprint_name,
                p.description,
                p.provider_name,
                serde_json::to_string(&p.resource_request)?,
                p.config.to_string(),
                p.budget_cap_usd,
                p.estimated_minutes,
                p.tags.to_string(),
                p.provider_job_id,
                p.created_at,
                p.approved_at,
                p.dispatched_at,
                p.started_at,
                p.ended_at,
                p.result_payload.as_ref().map(|v| v.to_string()),
                p.error,
                p.kill_reason,
            ],
        )?;
        Ok(())
    }

    pub fn get_proposal(&self, id: &str) -> anyhow::Result<Option<Proposal>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, status, sprint_name, description, provider_name,
                    resource_request, config, budget_cap_usd, estimated_minutes, tags,
                    provider_job_id, created_at, approved_at, dispatched_at,
                    started_at, ended_at, result_payload, error, kill_reason
             FROM proposals WHERE id = ?1",
        )?;
        let result = stmt
            .query_row(params![id], |row| Ok(row_to_proposal(row)))
            .optional()?;
        match result {
            Some(Ok(p)) => Ok(Some(p)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    pub fn list_proposals(&self, status_filter: Option<&str>) -> anyhow::Result<Vec<Proposal>> {
        let conn = self.conn.lock().unwrap();
        let mut proposals = Vec::new();

        if let Some(status) = status_filter {
            let mut stmt = conn.prepare(
                "SELECT id, status, sprint_name, description, provider_name,
                        resource_request, config, budget_cap_usd, estimated_minutes, tags,
                        provider_job_id, created_at, approved_at, dispatched_at,
                        started_at, ended_at, result_payload, error, kill_reason
                 FROM proposals WHERE status = ?1 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map(params![status], |row| Ok(row_to_proposal(row)))?;
            for row in rows {
                proposals.push(row??);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, status, sprint_name, description, provider_name,
                        resource_request, config, budget_cap_usd, estimated_minutes, tags,
                        provider_job_id, created_at, approved_at, dispatched_at,
                        started_at, ended_at, result_payload, error, kill_reason
                 FROM proposals ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map([], |row| Ok(row_to_proposal(row)))?;
            for row in rows {
                proposals.push(row??);
            }
        }

        Ok(proposals)
    }

    pub fn update_proposal_status(
        &self,
        id: &str,
        new_status: &ProposalStatus,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE proposals SET status = ?1 WHERE id = ?2",
            params![new_status.to_string(), id],
        )?;
        Ok(())
    }

    pub fn approve_proposal(&self, id: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE proposals SET status = 'approved', approved_at = ?1 WHERE id = ?2 AND status = 'pending'",
            params![now(), id],
        )?;
        Ok(())
    }

    pub fn reject_proposal(&self, id: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE proposals SET status = 'rejected' WHERE id = ?1 AND status = 'pending'",
            params![id],
        )?;
        Ok(())
    }

    pub fn set_dispatching(&self, id: &str, provider_job_id: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE proposals SET status = 'running', provider_job_id = ?1, dispatched_at = ?2, started_at = ?2 WHERE id = ?3",
            params![provider_job_id, now(), id],
        )?;
        Ok(())
    }

    pub fn complete_proposal(
        &self,
        id: &str,
        result_payload: Option<&serde_json::Value>,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE proposals SET status = 'completed', ended_at = ?1, result_payload = ?2 WHERE id = ?3",
            params![now(), result_payload.map(|v| v.to_string()), id],
        )?;
        Ok(())
    }

    pub fn fail_proposal(&self, id: &str, error: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE proposals SET status = 'failed', ended_at = ?1, error = ?2 WHERE id = ?3",
            params![now(), error, id],
        )?;
        Ok(())
    }

    pub fn kill_proposal(&self, id: &str, reason: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE proposals SET status = 'killed', ended_at = ?1, kill_reason = ?2 WHERE id = ?3",
            params![now(), reason, id],
        )?;
        Ok(())
    }

    // -- Cost ledger --

    pub fn record_cost(&self, entry: &CostEntry) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO cost_ledger (
                id, proposal_id, provider_name, gpu_type, gpu_count,
                gpu_seconds, rate_per_gpu_second, computed_cost_usd, recorded_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                entry.id,
                entry.proposal_id,
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

fn today_start_epoch() -> f64 {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Round down to start of UTC day
    let day_secs = now_secs - (now_secs % 86400);
    day_secs as f64
}

fn row_to_proposal(row: &rusqlite::Row) -> anyhow::Result<Proposal> {
    let status_str: String = row.get(1)?;
    let rr_str: String = row.get(5)?;
    let config_str: String = row.get(6)?;
    let tags_str: String = row.get(9)?;
    let result_str: Option<String> = row.get(16)?;

    Ok(Proposal {
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
        approved_at: row.get(12)?,
        dispatched_at: row.get(13)?,
        started_at: row.get(14)?,
        ended_at: row.get(15)?,
        result_payload: result_str.map(|s| serde_json::from_str(&s)).transpose()?,
        error: row.get(17)?,
        kill_reason: row.get(18)?,
    })
}
