// HTTP API handlers (axum)
// Endpoints: /jobs, /leases, /budget, /providers, /system
//
// Auth: two token roles.
// - agent_token: can submit jobs, query status, complete sprints
// - admin_token: can approve/reject leases, pause/resume, kill
// If no tokens configured, all endpoints are open (dev mode).
//
// Jobs run immediately on submission (no per-job approval).
// The lease is the approval gate — once a lease is approved,
// any job submitted under it starts running right away.

use axum::{
    extract::{Path, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::budget::BudgetEnforcer;
use crate::models::*;
use crate::provider_bridge::ProviderBridge;
use crate::store::{JobInsertError, Store};

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub budget: Arc<BudgetEnforcer>,
    pub bridge: Arc<ProviderBridge>,
    pub paused: Arc<AtomicBool>,
    pub agent_token: Option<String>,
    pub admin_token: Option<String>,
    pub concurrency_limits: std::collections::HashMap<String, u32>,
}

/// Extract bearer token from Authorization header.
fn extract_token(req: &Request) -> Option<String> {
    req.headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

/// Middleware: requires agent token OR admin token (either role can access).
async fn require_agent_or_admin(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> impl IntoResponse {
    // If no tokens configured, allow all (dev mode)
    if state.agent_token.is_none() && state.admin_token.is_none() {
        return next.run(req).await;
    }

    let token = extract_token(&req);
    let allowed = match token.as_deref() {
        Some(t) => {
            state.agent_token.as_deref() == Some(t) || state.admin_token.as_deref() == Some(t)
        }
        None => false,
    };

    if allowed {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "invalid or missing token"})),
        )
            .into_response()
    }
}

/// Middleware: requires admin token only. Agent token is rejected.
async fn require_admin(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> impl IntoResponse {
    // If no admin token configured, allow all (dev mode)
    if state.admin_token.is_none() {
        return next.run(req).await;
    }

    let token = extract_token(&req);
    let allowed = match token.as_deref() {
        Some(t) => state.admin_token.as_deref() == Some(t),
        None => false,
    };

    if allowed {
        next.run(req).await
    } else {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "admin access required"})),
        )
            .into_response()
    }
}

pub fn router(state: AppState) -> Router {
    // Agent routes: submit jobs, query, complete
    let agent_routes = Router::new()
        .route("/jobs", post(create_job))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
        .route("/jobs/{id}/complete", post(complete_job))
        .route("/leases", post(create_lease))
        .route("/leases", get(list_leases))
        .route("/leases/{id}", get(get_lease))
        .route("/budget", get(get_budget))
        .route("/providers", get(list_providers))
        .route("/system/status", get(system_status))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_agent_or_admin,
        ));

    // Admin routes: approve/reject leases, cancel jobs, pause, resume, kill
    let admin_routes = Router::new()
        .route("/jobs/{id}", delete(cancel_job))
        .route("/leases/{id}/approve", post(approve_lease))
        .route("/leases/{id}/reject", post(reject_lease))
        .route("/system/pause", post(system_pause))
        .route("/system/resume", post(system_resume))
        .route("/system/kill-all", post(kill_all))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_admin,
        ));

    agent_routes.merge(admin_routes).with_state(state)
}

// -- Jobs --

async fn create_job(
    State(state): State<AppState>,
    Json(req): Json<CreateJobRequest>,
) -> impl IntoResponse {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();

    let id = format!("job_{}", ulid::Ulid::new());
    let max_concurrent = state.concurrency_limits.get(&req.provider).copied().unwrap_or(4);

    // Atomically validate lease + concurrency + insert job as Running.
    // This prevents race conditions where concurrent submissions both pass
    // checks before either inserts.
    let job = Job {
        id: id.clone(),
        status: JobStatus::Running,
        sprint_name: req.sprint_name.clone(),
        description: req.description.clone(),
        provider_name: req.provider.clone(),
        resource_request: req.resource_request.clone(),
        config: req.config.clone(),
        budget_cap_usd: req.budget_cap_usd,
        estimated_minutes: req.estimated_minutes,
        tags: req.tags.clone(),
        provider_job_id: None,
        created_at: now,
        dispatched_at: Some(now),
        started_at: Some(now),
        ended_at: None,
        result_payload: None,
        error: None,
        kill_reason: None,
        lease_id: Some(req.lease_id.clone()),
        runtime_seconds: None,
        lease_remaining_at_start: None,
    };

    let remaining_seconds = match state.store.atomic_check_and_insert_job(
        &job,
        &req.lease_id,
        &req.provider,
        &req.resource_request.gpu,
        max_concurrent,
        now,
    ) {
        Ok(remaining) => remaining,
        Err(JobInsertError::Rejected(reason)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": reason})),
            )
                .into_response();
        }
        Err(JobInsertError::ConcurrencyLimit(reason)) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({"error": reason})),
            )
                .into_response();
        }
        Err(JobInsertError::Internal(reason)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": reason})),
            )
                .into_response();
        }
    };

    // Job is now inserted as Running. Launch via provider bridge.
    match state
        .bridge
        .launch(&req.provider, &req.resource_request, &req.config)
        .await
    {
        Ok(handle) => {
            // Update with provider job ID
            if let Err(e) = state.store.set_job_running(&id, &handle.provider_job_id) {
                tracing::error!("failed to set provider_job_id for {}: {}", id, e);
            }
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "job_id": id,
                    "status": "running",
                    "remaining_seconds": remaining_seconds,
                })),
            )
                .into_response()
        }
        Err(e) => {
            // Launch failed — mark the already-inserted job as failed
            let _ = state.store.fail_job(&id, &e.to_string());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("launch failed: {}", e),
                    "job_id": id,
                    "status": "failed"
                })),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
struct ListQuery {
    status: Option<String>,
}

async fn list_jobs(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    match state
        .store
        .list_jobs(query.status.as_deref())
    {
        Ok(jobs) => Json(serde_json::json!({"jobs": jobs})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn get_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.store.get_job(&id) {
        Ok(Some(j)) => Json(serde_json::to_value(j).unwrap()).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn complete_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<CompleteJobRequest>,
) -> impl IntoResponse {
    // Verify it's running
    match state.store.get_job(&id) {
        Ok(Some(j)) if j.status == JobStatus::Running => {
            // Record cost based on elapsed time
            let started = j.started_at.unwrap_or(j.created_at);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs_f64();
            let gpu_seconds = (now - started) * j.resource_request.gpu_count as f64;

            let cost_entry = CostEntry {
                id: format!("cost_{}", ulid::Ulid::new()),
                job_id: j.id.clone(),
                provider_name: j.provider_name.clone(),
                gpu_type: j.resource_request.gpu.clone(),
                gpu_count: j.resource_request.gpu_count,
                gpu_seconds,
                rate_per_gpu_second: 0.0, // local is free
                computed_cost_usd: 0.0,
                recorded_at: now,
            };
            let _ = state.store.record_cost(&cost_entry);

            match state
                .store
                .complete_job(&id, req.result_payload.as_ref())
            {
                Ok(()) => Json(serde_json::json!({"status": "completed"})).into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response(),
            }
        }
        Ok(Some(j)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": format!("job is {}, not running", j.status)})),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn cancel_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.store.get_job(&id) {
        Ok(Some(j)) => {
            match j.status {
                JobStatus::Running => {
                    // Cancel via provider
                    if let Some(ref job_id) = j.provider_job_id {
                        let handle = crate::models::JobHandle {
                            provider_name: j.provider_name.clone(),
                            provider_job_id: job_id.clone(),
                            launched_at: j.dispatched_at.unwrap_or(0.0),
                        };
                        let _ = state.bridge.cancel(&j.provider_name, &handle).await;
                    }
                    let _ = state.store.kill_job(&id, "cancelled_by_user");
                }
                _ => {
                    return (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({"error": format!("job is {} and cannot be cancelled", j.status)})),
                    )
                        .into_response();
                }
            }
            Json(serde_json::json!({"status": "cancelled"})).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// -- Leases --

async fn create_lease(
    State(state): State<AppState>,
    Json(req): Json<CreateLeaseRequest>,
) -> impl IntoResponse {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();

    let id = format!("lease_{}", ulid::Ulid::new());

    let lease = Lease {
        id: id.clone(),
        status: LeaseStatus::Pending,
        provider_name: req.provider,
        gpu: req.gpu,
        duration_seconds: req.duration_seconds,
        created_at: now,
        approved_at: None,
        rejected_at: None,
        expired_at: None,
    };

    match state.store.insert_lease(&lease) {
        Ok(()) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "lease_id": id,
                "status": "pending"
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn get_lease(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();

    match state.store.get_lease(&id) {
        Ok(Some(lease)) => {
            let time_used = state
                .store
                .cumulative_runtime_for_lease(&id, now)
                .unwrap_or(0.0);
            let time_remaining = (lease.duration_seconds as f64 - time_used).max(0.0);
            let jobs = state
                .store
                .list_jobs_for_lease(&id)
                .unwrap_or_default();

            let response = LeaseResponse {
                id: lease.id,
                status: lease.status,
                provider_name: lease.provider_name,
                gpu: lease.gpu,
                duration_seconds: lease.duration_seconds,
                created_at: lease.created_at,
                approved_at: lease.approved_at,
                rejected_at: lease.rejected_at,
                expired_at: lease.expired_at,
                time_used_seconds: time_used,
                time_remaining_seconds: time_remaining,
                jobs,
            };
            Json(serde_json::to_value(response).unwrap()).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn list_leases(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    match state.store.list_leases(query.status.as_deref()) {
        Ok(leases) => Json(serde_json::json!({"leases": leases})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn approve_lease(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.store.get_lease(&id) {
        Ok(Some(l)) if l.status == LeaseStatus::Pending => {}
        Ok(Some(l)) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": format!("lease is {}, not pending", l.status)})),
            )
                .into_response();
        }
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    }

    match state.store.approve_lease(&id) {
        Ok(()) => Json(serde_json::json!({"status": "approved"})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn reject_lease(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.store.get_lease(&id) {
        Ok(Some(l)) if l.status == LeaseStatus::Pending => {}
        Ok(Some(l)) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": format!("lease is {}, not pending", l.status)})),
            )
                .into_response();
        }
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    }

    match state.store.reject_lease(&id) {
        Ok(()) => Json(serde_json::json!({"status": "rejected"})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// -- Budget --

async fn get_budget(State(state): State<AppState>) -> impl IntoResponse {
    match state.budget.daily_spent() {
        Ok(spent) => {
            let limit = state.budget.daily_limit();
            Json(serde_json::json!({
                "daily_limit_usd": limit,
                "daily_spent_usd": spent,
                "daily_remaining_usd": limit - spent,
                "alert_threshold_pct": state.budget.alert_threshold_pct(),
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// -- Providers --

async fn list_providers(State(state): State<AppState>) -> impl IntoResponse {
    match state.bridge.capabilities("local").await {
        Ok(caps) => Json(serde_json::json!({"providers": [caps]})).into_response(),
        Err(_) => {
            // Fallback: return static info if Python bridge not available
            Json(serde_json::json!({"providers": [{
                "name": "local",
                "gpu_types": ["cpu-only"],
                "max_concurrent_jobs": 4,
                "supports_cancel": true,
                "supports_spot": false,
                "rate_card": {"cpu-only": 0.0}
            }]}))
            .into_response()
        }
    }
}

// -- System --

async fn system_status(State(state): State<AppState>) -> impl IntoResponse {
    let running = state
        .store
        .list_jobs(Some("running"))
        .map(|v| v.len())
        .unwrap_or(0);
    let paused = state.paused.load(Ordering::Relaxed);

    Json(serde_json::json!({
        "dispatcher_paused": paused,
        "running_jobs": running,
    }))
}

async fn system_pause(State(state): State<AppState>) -> impl IntoResponse {
    state.paused.store(true, Ordering::Relaxed);
    Json(serde_json::json!({"status": "paused"}))
}

async fn system_resume(State(state): State<AppState>) -> impl IntoResponse {
    state.paused.store(false, Ordering::Relaxed);
    Json(serde_json::json!({"status": "resumed"}))
}

async fn kill_all(State(state): State<AppState>) -> impl IntoResponse {
    let running = state.store.list_jobs(Some("running")).unwrap_or_default();
    let mut killed = 0;

    for j in running {
        if let Some(ref job_id) = j.provider_job_id {
            let handle = crate::models::JobHandle {
                provider_name: j.provider_name.clone(),
                provider_job_id: job_id.clone(),
                launched_at: j.dispatched_at.unwrap_or(0.0),
            };
            let _ = state.bridge.cancel(&j.provider_name, &handle).await;
        }
        let _ = state.store.kill_job(&j.id, "kill_all");
        killed += 1;
    }

    // Also pause the dispatcher
    state.paused.store(true, Ordering::Relaxed);

    Json(serde_json::json!({
        "killed": killed,
        "dispatcher_paused": true,
    }))
}
