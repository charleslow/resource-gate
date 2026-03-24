use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// Lease types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LeaseStatus {
    Pending,
    Approved,
    Expired,
    Rejected,
}

impl fmt::Display for LeaseStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Approved => write!(f, "approved"),
            Self::Expired => write!(f, "expired"),
            Self::Rejected => write!(f, "rejected"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub id: String,
    pub status: LeaseStatus,
    pub provider_name: String,
    pub gpu: String,
    pub duration_seconds: u64,
    pub created_at: f64,
    pub approved_at: Option<f64>,
    pub rejected_at: Option<f64>,
    pub expired_at: Option<f64>,
}

/// Request body for POST /leases
#[derive(Debug, Deserialize)]
pub struct CreateLeaseRequest {
    pub provider: String,
    pub gpu: String,
    pub duration_seconds: u64,
}

/// Enriched response for GET /leases/{id}
#[derive(Debug, Serialize)]
pub struct LeaseResponse {
    pub id: String,
    pub status: LeaseStatus,
    pub provider_name: String,
    pub gpu: String,
    pub duration_seconds: u64,
    pub created_at: f64,
    pub approved_at: Option<f64>,
    pub rejected_at: Option<f64>,
    pub expired_at: Option<f64>,
    pub time_used_seconds: f64,
    pub time_remaining_seconds: f64,
    pub jobs: Vec<Proposal>,
}

// ---------------------------------------------------------------------------
// Proposal types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Approved,
    Rejected,
    Dispatching,
    Running,
    Completed,
    Failed,
    Killed,
}

impl fmt::Display for ProposalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Approved => write!(f, "approved"),
            Self::Rejected => write!(f, "rejected"),
            Self::Dispatching => write!(f, "dispatching"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Killed => write!(f, "killed"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceRequest {
    pub gpu: String,
    #[serde(default = "default_gpu_count")]
    pub gpu_count: u32,
    pub cpu_cores: Option<u32>,
    pub memory_gb: Option<u32>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    pub docker_image: Option<String>,
}

fn default_gpu_count() -> u32 {
    1
}

fn default_timeout() -> u64 {
    3600
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub status: ProposalStatus,
    pub sprint_name: String,
    pub description: String,
    pub provider_name: String,
    pub resource_request: ResourceRequest,
    pub config: serde_json::Value,
    pub budget_cap_usd: f64,
    pub estimated_minutes: Option<u32>,
    pub tags: serde_json::Value,
    pub provider_job_id: Option<String>,
    pub created_at: f64,
    pub approved_at: Option<f64>,
    pub dispatched_at: Option<f64>,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub result_payload: Option<serde_json::Value>,
    pub error: Option<String>,
    pub kill_reason: Option<String>,
    pub lease_id: Option<String>,
    pub runtime_seconds: Option<f64>,
    pub lease_remaining_at_start: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobHandle {
    pub provider_name: String,
    pub provider_job_id: String,
    pub launched_at: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub status: String,
    pub started_at: Option<f64>,
    pub ended_at: Option<f64>,
    pub gpu_seconds: f64,
    pub result_payload: Option<serde_json::Value>,
    pub error: Option<String>,
    pub artifacts_path: Option<String>,
}

/// Provider-specific sprint config (e.g. command, env vars, working_dir)
/// For local Docker provider: {"command": ["python", "train.py"], "env": {"FOO": "bar"}}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SprintConfig {
    /// Command to run inside the container
    #[serde(default)]
    pub command: Vec<String>,
    /// Environment variables
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Working directory inside container
    pub working_dir: Option<String>,
}

/// Request body for POST /proposals
#[derive(Debug, Deserialize)]
pub struct CreateProposalRequest {
    pub sprint_name: String,
    pub description: String,
    pub provider: String,
    pub resource_request: ResourceRequest,
    #[serde(default)]
    pub config: serde_json::Value,
    pub estimated_minutes: Option<u32>,
    #[serde(default)]
    pub budget_cap_usd: f64,
    #[serde(default)]
    pub tags: serde_json::Value,
    /// Lease this job belongs to (required for time-based budget enforcement)
    pub lease_id: String,
}

/// Request body for POST /proposals/{id}/complete
#[derive(Debug, Deserialize)]
pub struct CompleteProposalRequest {
    pub result_payload: Option<serde_json::Value>,
}

/// Cost ledger entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostEntry {
    pub id: String,
    pub proposal_id: String,
    pub provider_name: String,
    pub gpu_type: String,
    pub gpu_count: u32,
    pub gpu_seconds: f64,
    pub rate_per_gpu_second: f64,
    pub computed_cost_usd: f64,
    pub recorded_at: f64,
}

/// Budget summary returned by GET /budget
#[derive(Debug, Serialize)]
pub struct BudgetSummary {
    pub daily_limit_usd: f64,
    pub daily_spent_usd: f64,
    pub daily_remaining_usd: f64,
    pub alert_threshold_pct: u32,
}
