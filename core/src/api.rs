// HTTP API handlers (axum)
// Endpoints: /proposals, /budget, /providers, /system

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::budget::{BudgetCheck, BudgetEnforcer};
use crate::models::*;
use crate::provider_bridge::ProviderBridge;
use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub budget: Arc<BudgetEnforcer>,
    pub bridge: Arc<ProviderBridge>,
    pub paused: Arc<AtomicBool>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/proposals", post(create_proposal))
        .route("/proposals", get(list_proposals))
        .route("/proposals/{id}", get(get_proposal))
        .route("/proposals/{id}", delete(cancel_proposal))
        .route("/proposals/{id}/approve", post(approve_proposal))
        .route("/proposals/{id}/reject", post(reject_proposal))
        .route("/proposals/{id}/complete", post(complete_proposal))
        .route("/budget", get(get_budget))
        .route("/providers", get(list_providers))
        .route("/system/status", get(system_status))
        .route("/system/pause", post(system_pause))
        .route("/system/resume", post(system_resume))
        .route("/system/kill-all", post(kill_all))
        .with_state(state)
}

// -- Proposals --

async fn create_proposal(
    State(state): State<AppState>,
    Json(req): Json<CreateProposalRequest>,
) -> impl IntoResponse {
    // Budget check
    match state.budget.can_accept(req.budget_cap_usd) {
        Ok(BudgetCheck::Rejected { reason }) => {
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
        Ok(BudgetCheck::Accepted) => {}
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();

    let id = format!("prop_{}", ulid::Ulid::new());

    let proposal = Proposal {
        id: id.clone(),
        status: ProposalStatus::Pending,
        sprint_name: req.sprint_name,
        description: req.description,
        provider_name: req.provider,
        resource_request: req.resource_request,
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
