// HTTP API handlers (axum)
// Endpoints: /proposals, /budget, /providers, /system
//
// Auth: two token roles.
// - agent_token: can submit proposals, query status, complete sprints
// - admin_token: can approve/reject proposals, pause/resume, kill
// If no tokens configured, all endpoints are open (dev mode).

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

use crate::budget::{BudgetEnforcer, LeaseTimeCheck};
use crate::models::*;
use crate::provider_bridge::ProviderBridge;
use crate::store::Store;

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
    // Agent routes: submit, query, complete
    let agent_routes = Router::new()
        .route("/proposals", post(create_proposal))
        .route("/proposals", get(list_proposals))
        .route("/proposals/{id}", get(get_proposal))
        .route("/proposals/{id}/complete", post(complete_proposal))
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

    // Admin routes: approve, reject, cancel, pause, resume, kill
    let admin_routes = Router::new()
        .route("/proposals/{id}/approve", post(approve_proposal))
        .route("/proposals/{id}/reject", post(reject_proposal))
        .route("/proposals/{id}", delete(cancel_proposal))
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

// -- Proposals --

async fn create_proposal(
    State(state): State<AppState>,
    Json(req): Json<CreateProposalRequest>,
) -> impl IntoResponse {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();

    // Lease validation
    match state.budget.lease_has_time(&req.lease_id, now) {
        Ok(LeaseTimeCheck::Rejected { reason }) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": reason})),
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
        Ok(LeaseTimeCheck::Accepted { .. }) => {}
    }

    // Verify lease provider matches proposal provider
    if let Ok(Some(lease)) = state.store.get_lease(&req.lease_id) {
        if lease.provider_name != req.provider {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("lease provider '{}' does not match proposal provider '{}'", lease.provider_name, req.provider)
                })),
            )
                .into_response();
        }
    }

    // Concurrency check
    let max_concurrent = state.concurrency_limits.get(&req.provider).copied().unwrap_or(4);
    match state.store.count_running_jobs_for_provider(&req.provider) {
        Ok(running) if running >= max_concurrent => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({
                    "error": format!("provider '{}' at concurrency limit ({}/{})", req.provider, running, max_concurrent)
                })),
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
        _ => {}
    }

    let id = format!("prop_{}", ulid::Ulid::new());

    let proposal = Proposal {
        id: id.clone(),
        status: ProposalStatus::Pending,
        sprint_name: req.sprint_name,
        description: req.description,
        provider_name: req.provider,
        resource_request: req.resource_request,
        config: req.config,
        budget_cap_usd: req.budget_cap_usd,
        estimated_minutes: req.estimated_minutes,
        tags: req.tags,
        provider_job_id: None,
        created_at: now,
        approved_at: None,
        dispatched_at: None,
        started_at: None,
        ended_at: None,
        result_payload: None,
        error: None,
        kill_reason: None,
        lease_id: Some(req.lease_id),
        runtime_seconds: None,
        lease_remaining_at_start: None,
    };

    match state.store.insert_proposal(&proposal) {
        Ok(()) => (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "proposal_id": id,
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

#[derive(Deserialize)]
struct ListQuery {
    status: Option<String>,
}

async fn list_proposals(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    match state
        .store
        .list_proposals(query.status.as_deref())
    {
        Ok(proposals) => Json(serde_json::json!({"proposals": proposals})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn get_proposal(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.store.get_proposal(&id) {
        Ok(Some(p)) => Json(serde_json::to_value(p).unwrap()).into_response(),
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

async fn approve_proposal(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Verify it exists and is pending
    match state.store.get_proposal(&id) {
        Ok(Some(p)) if p.status == ProposalStatus::Pending => {}
        Ok(Some(p)) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": format!("proposal is {}, not pending", p.status)})),
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

    match state.store.approve_proposal(&id) {
        Ok(()) => Json(serde_json::json!({"status": "approved"})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn reject_proposal(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.store.reject_proposal(&id) {
        Ok(()) => Json(serde_json::json!({"status": "rejected"})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

async fn complete_proposal(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<CompleteProposalRequest>,
) -> impl IntoResponse {
    // Verify it's running
    match state.store.get_proposal(&id) {
        Ok(Some(p)) if p.status == ProposalStatus::Running => {
            // Record cost based on elapsed time
            let started = p.started_at.unwrap_or(p.created_at);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs_f64();
            let gpu_seconds = (now - started) * p.resource_request.gpu_count as f64;

            let cost_entry = CostEntry {
                id: format!("cost_{}", ulid::Ulid::new()),
                proposal_id: p.id.clone(),
                provider_name: p.provider_name.clone(),
                gpu_type: p.resource_request.gpu.clone(),
                gpu_count: p.resource_request.gpu_count,
                gpu_seconds,
                rate_per_gpu_second: 0.0, // local is free
                computed_cost_usd: 0.0,
                recorded_at: now,
            };
            let _ = state.store.record_cost(&cost_entry);

            match state
                .store
                .complete_proposal(&id, req.result_payload.as_ref())
            {
                Ok(()) => Json(serde_json::json!({"status": "completed"})).into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response(),
            }
        }
        Ok(Some(p)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": format!("proposal is {}, not running", p.status)})),
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

async fn cancel_proposal(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.store.get_proposal(&id) {
        Ok(Some(p)) => {
            match p.status {
                ProposalStatus::Pending => {
                    let _ = state.store.reject_proposal(&id);
                }
                ProposalStatus::Running | ProposalStatus::Approved | ProposalStatus::Dispatching => {
                    // Cancel via provider if running
                    if let Some(ref job_id) = p.provider_job_id {
                        let handle = crate::models::JobHandle {
                            provider_name: p.provider_name.clone(),
                            provider_job_id: job_id.clone(),
                            launched_at: p.dispatched_at.unwrap_or(0.0),
                        };
                        let _ = state.bridge.cancel(&p.provider_name, &handle).await;
                    }
                    let _ = state.store.kill_proposal(&id, "cancelled_by_user");
                }
                _ => {
                    return (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({"error": format!("proposal is {} and cannot be cancelled", p.status)})),
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
                .list_proposals_for_lease(&id)
                .unwrap_or_default();

            let response = LeaseResponse {
                id: lease.id,
                status: lease.status,
                provider_name: lease.provider_name,
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
        .list_proposals(Some("running"))
        .map(|v| v.len())
        .unwrap_or(0);
    let pending = state
        .store
        .list_proposals(Some("pending"))
        .map(|v| v.len())
        .unwrap_or(0);
    let paused = state.paused.load(Ordering::Relaxed);

    Json(serde_json::json!({
        "dispatcher_paused": paused,
        "running_sprints": running,
        "pending_proposals": pending,
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
    let running = state.store.list_proposals(Some("running")).unwrap_or_default();
    let mut killed = 0;

    for p in running {
        if let Some(ref job_id) = p.provider_job_id {
            let handle = crate::models::JobHandle {
                provider_name: p.provider_name.clone(),
                provider_job_id: job_id.clone(),
                launched_at: p.dispatched_at.unwrap_or(0.0),
            };
            let _ = state.bridge.cancel(&p.provider_name, &handle).await;
        }
        let _ = state.store.kill_proposal(&p.id, "kill_all");
        killed += 1;
    }

    // Also pause the dispatcher
    state.paused.store(true, Ordering::Relaxed);

    Json(serde_json::json!({
        "killed": killed,
        "dispatcher_paused": true,
    }))
}
