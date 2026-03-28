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
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::budget::BudgetEnforcer;
use crate::dispatcher::Dispatcher;
use crate::models::*;
use crate::provider_bridge::ProviderBridge;
use crate::store::{JobInsertError, Store};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn err(status: StatusCode, msg: impl ToString) -> Response {
    (status, Json(serde_json::json!({"error": msg.to_string()}))).into_response()
}

fn not_found() -> Response {
    err(StatusCode::NOT_FOUND, "not found")
}

fn internal(e: impl ToString) -> Response {
    err(StatusCode::INTERNAL_SERVER_ERROR, e)
}

fn ok_json(value: serde_json::Value) -> Response {
    Json(value).into_response()
}

fn handle_for_job(job: &Job) -> Option<JobHandle> {
    job.provider_job_id.as_ref().map(|pjid| JobHandle {
        provider_name: job.provider_name.clone(),
        provider_job_id: pjid.clone(),
        launched_at: job.dispatched_at.unwrap_or(0.0),
    })
}

// ---------------------------------------------------------------------------
// State + Auth
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub budget: Arc<BudgetEnforcer>,
    pub bridge: Arc<ProviderBridge>,
    pub dispatcher: Arc<Dispatcher>,
    pub paused: Arc<AtomicBool>,
    pub agent_token: Option<String>,
    pub admin_token: Option<String>,
    pub concurrency_limits: std::collections::HashMap<String, u32>,
}

fn extract_token(req: &Request) -> Option<String> {
    req.headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
}

async fn require_agent_or_admin(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if state.agent_token.is_none() && state.admin_token.is_none() {
        return next.run(req).await;
    }
    match extract_token(&req).as_deref() {
        Some(t) if state.agent_token.as_deref() == Some(t) || state.admin_token.as_deref() == Some(t) => {
            next.run(req).await
        }
        _ => err(StatusCode::UNAUTHORIZED, "invalid or missing token"),
    }
}

async fn require_admin(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if state.admin_token.is_none() {
        return next.run(req).await;
    }
    match extract_token(&req).as_deref() {
        Some(t) if state.admin_token.as_deref() == Some(t) => next.run(req).await,
        _ => err(StatusCode::FORBIDDEN, "admin access required"),
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router(state: AppState) -> Router {
    let agent_routes = Router::new()
        .route("/jobs", post(create_job))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
        .route("/jobs/{id}/complete", post(complete_job))
        .route("/jobs/{id}/copy-in", post(copy_in))
        .route("/jobs/{id}/copy-out", post(copy_out))
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

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

async fn create_job(
    State(state): State<AppState>,
    Json(req): Json<CreateJobRequest>,
) -> Response {
    let now = now();
    let id = format!("job_{}", ulid::Ulid::new());
    let max_concurrent = state.concurrency_limits.get(&req.provider).copied().unwrap_or(4);

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
        &job, &req.lease_id, &req.provider, &req.resource_request.gpu, max_concurrent, now,
    ) {
        Ok(remaining) => remaining,
        Err(JobInsertError::Rejected(reason)) => return err(StatusCode::BAD_REQUEST, reason),
        Err(JobInsertError::ConcurrencyLimit(reason)) => return err(StatusCode::TOO_MANY_REQUESTS, reason),
        Err(JobInsertError::Internal(reason)) => return internal(reason),
    };

    match state.bridge.launch(&req.provider, &req.resource_request, &req.config).await {
        Ok(handle) => {
            if let Err(e) = state.store.set_job_running(&id, &handle.provider_job_id) {
                tracing::error!("failed to set provider_job_id for {}: {}", id, e);
            }
            (StatusCode::CREATED, Json(serde_json::json!({
                "job_id": id, "status": "running", "remaining_seconds": remaining_seconds,
            }))).into_response()
        }
        Err(e) => {
            let _ = state.store.fail_job(&id, &e.to_string());
            internal(format!("launch failed: {}", e))
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
) -> Response {
    match state.store.list_jobs(query.status.as_deref()) {
        Ok(jobs) => ok_json(serde_json::json!({"jobs": jobs})),
        Err(e) => internal(e),
    }
}

async fn get_job(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.store.get_job(&id) {
        Ok(Some(j)) => ok_json(serde_json::to_value(j).unwrap()),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

async fn complete_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<CompleteJobRequest>,
) -> Response {
    match state.store.get_job(&id) {
        Ok(Some(j)) if j.status == JobStatus::Running => {
            let started = j.started_at.unwrap_or(j.created_at);
            let now = now();
            let gpu_seconds = (now - started) * j.resource_request.gpu_count as f64;

            let cost_entry = CostEntry {
                id: format!("cost_{}", ulid::Ulid::new()),
                job_id: j.id.clone(),
                provider_name: j.provider_name.clone(),
                gpu_type: j.resource_request.gpu.clone(),
                gpu_count: j.resource_request.gpu_count,
                gpu_seconds,
                rate_per_gpu_second: 0.0,
                computed_cost_usd: 0.0,
                recorded_at: now,
            };
            let _ = state.store.record_cost(&cost_entry);

            match state.store.complete_job(&id, req.result_payload.as_ref()) {
                Ok(()) => ok_json(serde_json::json!({"status": "completed"})),
                Err(e) => internal(e),
            }
        }
        Ok(Some(j)) => err(StatusCode::CONFLICT, format!("job is {}, not running", j.status)),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

async fn cancel_job(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.store.get_job(&id) {
        Ok(Some(j)) if j.status == JobStatus::Running => {
            if let Some(handle) = handle_for_job(&j) {
                let _ = state.bridge.cancel(&j.provider_name, &handle).await;
            }
            let _ = state.store.kill_job(&id, "cancelled_by_user");
            ok_json(serde_json::json!({"status": "cancelled"}))
        }
        Ok(Some(j)) => err(StatusCode::CONFLICT, format!("job is {} and cannot be cancelled", j.status)),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// Copy in/out
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CopyRequest {
    local_path: String,
    remote_path: String,
}

async fn do_copy(state: &AppState, id: &str, req: &CopyRequest, direction: &str) -> Response {
    let job = match state.store.get_job(id) {
        Ok(Some(j)) => j,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };

    let handle = match handle_for_job(&job) {
        Some(h) => h,
        None => return err(StatusCode::BAD_REQUEST, "job has no provider resources"),
    };

    let result = match direction {
        "in" => state.bridge.copy_in(&job.provider_name, &handle, &req.local_path, &req.remote_path).await,
        _ => state.bridge.copy_out(&job.provider_name, &handle, &req.remote_path, &req.local_path).await,
    };

    match result {
        Ok(()) => {
            state.dispatcher.reset_cleanup_grace(id);
            ok_json(serde_json::json!({"status": "ok"}))
        }
        Err(e) => internal(e),
    }
}

async fn copy_in(State(state): State<AppState>, Path(id): Path<String>, Json(req): Json<CopyRequest>) -> Response {
    do_copy(&state, &id, &req, "in").await
}

async fn copy_out(State(state): State<AppState>, Path(id): Path<String>, Json(req): Json<CopyRequest>) -> Response {
    do_copy(&state, &id, &req, "out").await
}

// ---------------------------------------------------------------------------
// Leases
// ---------------------------------------------------------------------------

async fn create_lease(
    State(state): State<AppState>,
    Json(req): Json<CreateLeaseRequest>,
) -> Response {
    let id = format!("lease_{}", ulid::Ulid::new());
    let lease = Lease {
        id: id.clone(),
        status: LeaseStatus::Pending,
        provider_name: req.provider,
        gpu: req.gpu,
        duration_seconds: req.duration_seconds,
        created_at: now(),
        approved_at: None,
        rejected_at: None,
        expired_at: None,
    };

    match state.store.insert_lease(&lease) {
        Ok(()) => (StatusCode::CREATED, Json(serde_json::json!({"lease_id": id, "status": "pending"}))).into_response(),
        Err(e) => internal(e),
    }
}

async fn get_lease(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let now = now();
    match state.store.get_lease(&id) {
        Ok(Some(lease)) => {
            let time_used = state.store.cumulative_runtime_for_lease(&id, now).unwrap_or(0.0);
            let jobs = state.store.list_jobs_for_lease(&id).unwrap_or_default();
            ok_json(serde_json::to_value(LeaseResponse {
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
                time_remaining_seconds: (lease.duration_seconds as f64 - time_used).max(0.0),
                jobs,
            }).unwrap())
        }
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

async fn list_leases(State(state): State<AppState>, Query(query): Query<ListQuery>) -> Response {
    match state.store.list_leases(query.status.as_deref()) {
        Ok(leases) => ok_json(serde_json::json!({"leases": leases})),
        Err(e) => internal(e),
    }
}

async fn approve_lease(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.store.get_lease(&id) {
        Ok(Some(l)) if l.status == LeaseStatus::Pending => {}
        Ok(Some(l)) => return err(StatusCode::CONFLICT, format!("lease is {}, not pending", l.status)),
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    }
    match state.store.approve_lease(&id) {
        Ok(()) => ok_json(serde_json::json!({"status": "approved"})),
        Err(e) => internal(e),
    }
}

async fn reject_lease(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.store.get_lease(&id) {
        Ok(Some(l)) if l.status == LeaseStatus::Pending => {}
        Ok(Some(l)) => return err(StatusCode::CONFLICT, format!("lease is {}, not pending", l.status)),
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    }
    match state.store.reject_lease(&id) {
        Ok(()) => ok_json(serde_json::json!({"status": "rejected"})),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// Budget / Providers / System
// ---------------------------------------------------------------------------

async fn get_budget(State(state): State<AppState>) -> Response {
    match state.budget.daily_spent() {
        Ok(spent) => {
            let limit = state.budget.daily_limit();
            ok_json(serde_json::json!({
                "daily_limit_usd": limit,
                "daily_spent_usd": spent,
                "daily_remaining_usd": limit - spent,
                "alert_threshold_pct": state.budget.alert_threshold_pct(),
            }))
        }
        Err(e) => internal(e),
    }
}

async fn list_providers(State(state): State<AppState>) -> Response {
    match state.bridge.capabilities("local").await {
        Ok(caps) => ok_json(serde_json::json!({"providers": [caps]})),
        Err(_) => ok_json(serde_json::json!({"providers": [{
            "name": "local",
            "gpu_types": ["cpu-only"],
            "max_concurrent_jobs": 4,
            "supports_cancel": true,
            "supports_spot": false,
            "rate_card": {"cpu-only": 0.0}
        }]})),
    }
}

async fn system_status(State(state): State<AppState>) -> Response {
    let running = state.store.list_jobs(Some("running")).map(|v| v.len()).unwrap_or(0);
    ok_json(serde_json::json!({
        "dispatcher_paused": state.paused.load(Ordering::Relaxed),
        "running_jobs": running,
    }))
}

async fn system_pause(State(state): State<AppState>) -> Response {
    state.paused.store(true, Ordering::Relaxed);
    ok_json(serde_json::json!({"status": "paused"}))
}

async fn system_resume(State(state): State<AppState>) -> Response {
    state.paused.store(false, Ordering::Relaxed);
    ok_json(serde_json::json!({"status": "resumed"}))
}

async fn kill_all(State(state): State<AppState>) -> Response {
    let running = state.store.list_jobs(Some("running")).unwrap_or_default();
    let mut killed = 0;

    for j in &running {
        if let Some(handle) = handle_for_job(j) {
            let _ = state.bridge.cancel(&j.provider_name, &handle).await;
        }
        let _ = state.store.kill_job(&j.id, "kill_all");
        killed += 1;
    }

    state.paused.store(true, Ordering::Relaxed);
    ok_json(serde_json::json!({"killed": killed, "dispatcher_paused": true}))
}
