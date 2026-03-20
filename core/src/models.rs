use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub experiment_name: String,
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
