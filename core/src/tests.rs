use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::api::AppState;
use crate::budget::{BudgetCheck, BudgetEnforcer};
use crate::config::BudgetConfig;
use crate::models::*;
use crate::provider_bridge::ProviderBridge;
use crate::store::Store;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_store() -> Store {
    Store::new_in_memory().expect("in-memory store")
}

fn test_proposal(id: &str) -> Proposal {
    Proposal {
        id: id.to_string(),
        status: ProposalStatus::Pending,
        sprint_name: "test-sprint".into(),
        description: "test".into(),
        provider_name: "local".into(),
        resource_request: ResourceRequest {
            gpu: "cpu-only".into(),
            gpu_count: 1,
            cpu_cores: None,
            memory_gb: None,
            timeout_seconds: 3600,
            docker_image: Some("alpine:latest".into()),
        },
        config: serde_json::json!({}),
        budget_cap_usd: 5.0,
        estimated_minutes: Some(10),
        tags: serde_json::json!({}),
        provider_job_id: None,
        created_at: 1000.0,
        approved_at: None,
        dispatched_at: None,
        started_at: None,
        ended_at: None,
        result_payload: None,
        error: None,
        kill_reason: None,
        lease_id: None,
        runtime_seconds: None,
        lease_remaining_at_start: None,
    }
}

fn test_lease(id: &str, provider: &str, gpu: &str, duration_seconds: u64) -> Lease {
    Lease {
        id: id.to_string(),
        status: LeaseStatus::Pending,
        provider_name: provider.to_string(),
        gpu: gpu.to_string(),
        duration_seconds,
        created_at: 1000.0,
        approved_at: None,
        rejected_at: None,
        expired_at: None,
    }
}

fn budget_config(daily_limit: f64) -> BudgetConfig {
    BudgetConfig {
        daily_limit_usd: daily_limit,
        alert_threshold_pct: 80,
        auto_pause_on_limit: false,
    }
}

/// Build an AppState suitable for API tests (with auth tokens set).
fn test_app_state(store: Store) -> AppState {
    let budget = Arc::new(BudgetEnforcer::new(budget_config(0.0), store.clone()));
    let bridge = Arc::new(ProviderBridge::new(
        "python3".into(),
        "/dev/null".into(),
        "/tmp".into(),
    ));
    let mut concurrency_limits = std::collections::HashMap::new();
    concurrency_limits.insert("local".to_string(), 1);
    AppState {
        store,
        budget,
        bridge,
        paused: Arc::new(AtomicBool::new(false)),
        agent_token: Some("agent-secret".into()),
        admin_token: Some("admin-secret".into()),
        concurrency_limits,
    }
}

async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

// ===========================================================================
// 1. State machine transitions (store layer)
// ===========================================================================

#[tokio::test]
async fn test_proposal_lifecycle_happy_path() {
    let store = test_store();
    let p = test_proposal("p1");
    store.insert_proposal(&p).unwrap();

    // Pending -> Approved
    store.approve_proposal("p1").unwrap();
    let p = store.get_proposal("p1").unwrap().unwrap();
    assert_eq!(p.status, ProposalStatus::Approved);
    assert!(p.approved_at.is_some());

    // Approved -> Running (via set_dispatching)
    store.set_dispatching("p1", "job-123").unwrap();
    let p = store.get_proposal("p1").unwrap().unwrap();
    assert_eq!(p.status, ProposalStatus::Running);
    assert_eq!(p.provider_job_id.as_deref(), Some("job-123"));

    // Running -> Completed
    store.complete_proposal("p1", None).unwrap();
    let p = store.get_proposal("p1").unwrap().unwrap();
    assert_eq!(p.status, ProposalStatus::Completed);
    assert!(p.ended_at.is_some());
}

#[tokio::test]
async fn test_approve_only_works_on_pending() {
    let store = test_store();
    let p = test_proposal("p1");
    store.insert_proposal(&p).unwrap();
    store.approve_proposal("p1").unwrap();

    // Try to approve again (now it's Approved, not Pending).
    // The SQL WHERE clause won't match, so status stays Approved.
    store.approve_proposal("p1").unwrap();
    let p = store.get_proposal("p1").unwrap().unwrap();
    assert_eq!(p.status, ProposalStatus::Approved, "double-approve should be a no-op");
}

#[tokio::test]
async fn test_reject_only_works_on_pending() {
    let store = test_store();
    let p = test_proposal("p1");
    store.insert_proposal(&p).unwrap();
    store.approve_proposal("p1").unwrap(); // now Approved

    // Reject should be a no-op because status is no longer pending
    store.reject_proposal("p1").unwrap();
    let p = store.get_proposal("p1").unwrap().unwrap();
    assert_eq!(p.status, ProposalStatus::Approved, "reject on non-pending should be no-op");
}

#[tokio::test]
async fn test_kill_records_reason() {
    let store = test_store();
    let mut p = test_proposal("p1");
    p.status = ProposalStatus::Running;
    p.started_at = Some(1000.0);
    p.provider_job_id = Some("job-1".into());
    store.insert_proposal(&p).unwrap();

    store.kill_proposal("p1", "timeout_exceeded").unwrap();
    let p = store.get_proposal("p1").unwrap().unwrap();
    assert_eq!(p.status, ProposalStatus::Killed);
    assert_eq!(p.kill_reason.as_deref(), Some("timeout_exceeded"));
    assert!(p.ended_at.is_some());
}

#[tokio::test]
async fn test_fail_records_error() {
    let store = test_store();
    let mut p = test_proposal("p1");
    p.status = ProposalStatus::Running;
    store.insert_proposal(&p).unwrap();

    store.fail_proposal("p1", "OOM killed").unwrap();
    let p = store.get_proposal("p1").unwrap().unwrap();
    assert_eq!(p.status, ProposalStatus::Failed);
    assert_eq!(p.error.as_deref(), Some("OOM killed"));
}

// ===========================================================================
// 2. Budget enforcement
// ===========================================================================

#[tokio::test]
async fn test_budget_rejects_over_limit() {
    let store = test_store();
    store.set_budget_config(100.0, 80, false).unwrap();

    // Need a proposal for foreign key
    let p = test_proposal("p1");
    store.insert_proposal(&p).unwrap();

    // Record $95 of spend
    let entry = CostEntry {
        id: "c1".into(),
        proposal_id: "p1".into(),
        provider_name: "local".into(),
        gpu_type: "A100".into(),
        gpu_count: 1,
        gpu_seconds: 100.0,
        rate_per_gpu_second: 0.95,
        computed_cost_usd: 95.0,
        recorded_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64(),
    };
    store.record_cost(&entry).unwrap();

    let enforcer = BudgetEnforcer::new(budget_config(100.0), store);
    // 95 + 10 > 100 → rejected
    match enforcer.can_accept(10.0).unwrap() {
        BudgetCheck::Rejected { reason } => {
            assert!(reason.contains("exceeds daily remaining"), "reason: {reason}");
        }
        BudgetCheck::Accepted => panic!("should have been rejected"),
    }
}

#[tokio::test]
async fn test_budget_accepts_within_limit() {
    let store = test_store();
    let enforcer = BudgetEnforcer::new(budget_config(100.0), store);
    // No spend recorded yet; $50 should be accepted
    assert!(matches!(enforcer.can_accept(50.0).unwrap(), BudgetCheck::Accepted));
}

#[tokio::test]
async fn test_budget_unlimited_when_zero() {
    let store = test_store();
    let enforcer = BudgetEnforcer::new(budget_config(0.0), store);
    // 0 means unlimited — any amount is accepted
    assert!(matches!(enforcer.can_accept(999999.0).unwrap(), BudgetCheck::Accepted));
}

#[tokio::test]
async fn test_exceeds_cap() {
    let store = test_store();
    let enforcer = BudgetEnforcer::new(budget_config(100.0), store);
    assert!(enforcer.exceeds_cap(50.0, 50.0));
    assert!(enforcer.exceeds_cap(51.0, 50.0));
    assert!(!enforcer.exceeds_cap(49.0, 50.0));
}

// ===========================================================================
// 3. Auth role separation
// ===========================================================================

#[tokio::test]
async fn test_agent_cannot_approve() {
    let store = test_store();
    let p = test_proposal("p1");
    store.insert_proposal(&p).unwrap();

    let state = test_app_state(store);
    let app = crate::api::router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/proposals/p1/approve")
                .header("authorization", "Bearer agent-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_admin_can_approve() {
    let store = test_store();
    let p = test_proposal("p1");
    store.insert_proposal(&p).unwrap();

    let state = test_app_state(store.clone());
    let app = crate::api::router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/proposals/p1/approve")
                .header("authorization", "Bearer admin-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let p = store.get_proposal("p1").unwrap().unwrap();
    assert_eq!(p.status, ProposalStatus::Approved);
}

#[tokio::test]
async fn test_unauthenticated_rejected() {
    let store = test_store();
    let state = test_app_state(store);
    let app = crate::api::router(state);

    // No Authorization header
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/proposals")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_agent_can_submit_proposal() {
    let store = test_store();

    // Create and approve a lease first
    let lease = test_lease("lease-1", "local", "cpu-only", 600);
    store.insert_lease(&lease).unwrap();
    store.approve_lease("lease-1").unwrap();

    let state = test_app_state(store);
    let app = crate::api::router(state);

    let body = serde_json::json!({
        "sprint_name": "test",
        "description": "test sprint",
        "provider": "local",
        "resource_request": {"gpu": "cpu-only", "gpu_count": 1},
        "lease_id": "lease-1"
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/proposals")
                .header("authorization", "Bearer agent-secret")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn test_agent_cannot_kill_all() {
    let store = test_store();
    let state = test_app_state(store);
    let app = crate::api::router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/system/kill-all")
                .header("authorization", "Bearer agent-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// ===========================================================================
// 4. Provider bridge protocol — JSON response parsing
// ===========================================================================

#[tokio::test]
async fn test_provider_response_parsing_status() {
    // Simulate what ProviderBridge.poll does when it gets a status response
    let resp: serde_json::Value = serde_json::json!({
        "type": "status",
        "data": {"status": "running"}
    });

    let resp_type = resp["type"].as_str().unwrap_or("");
    assert_eq!(resp_type, "status");
    let status = resp["data"]["status"].as_str().unwrap_or("running");
    assert_eq!(status, "running");
}

#[tokio::test]
async fn test_provider_response_parsing_result() {
    let resp: serde_json::Value = serde_json::json!({
        "type": "result",
        "data": {
            "status": "completed",
            "started_at": 1000.0,
            "ended_at": 1060.0,
            "gpu_seconds": 60.0,
            "result_payload": {"accuracy": 0.95},
            "error": null,
            "artifacts_path": "/workspace/out"
        }
    });

    let result: JobResult = serde_json::from_value(resp["data"].clone()).unwrap();
    assert_eq!(result.status, "completed");
    assert_eq!(result.gpu_seconds, 60.0);
    assert!(result.error.is_none());
    assert!(result.result_payload.is_some());
}

#[tokio::test]
async fn test_provider_response_parsing_error() {
    let resp: serde_json::Value = serde_json::json!({
        "error": "container not found"
    });

    // The bridge checks resp.get("error") and bails
    assert!(resp.get("error").is_some());
    let err = resp["error"].as_str().unwrap();
    assert_eq!(err, "container not found");
}

#[tokio::test]
async fn test_provider_response_parsing_failed_result() {
    let resp: serde_json::Value = serde_json::json!({
        "type": "result",
        "data": {
            "status": "failed",
            "started_at": 1000.0,
            "ended_at": 1010.0,
            "gpu_seconds": 10.0,
            "error": "OOMKilled",
            "result_payload": null,
            "artifacts_path": null
        }
    });

    let result: JobResult = serde_json::from_value(resp["data"].clone()).unwrap();
    assert_eq!(result.status, "failed");
    assert_eq!(result.error.as_deref(), Some("OOMKilled"));
}

#[tokio::test]
async fn test_job_handle_roundtrip() {
    let handle = JobHandle {
        provider_name: "local".into(),
        provider_job_id: "abc123".into(),
        launched_at: 1000.0,
    };
    let json = serde_json::to_value(&handle).unwrap();
    let parsed: JobHandle = serde_json::from_value(json).unwrap();
    assert_eq!(parsed.provider_job_id, "abc123");
}

// ===========================================================================
// 5. Kill-all: kills running proposals and pauses dispatcher
// ===========================================================================

#[tokio::test]
async fn test_kill_all_transitions_and_pauses() {
    let store = test_store();

    // Insert 3 running proposals
    for i in 0..3 {
        let mut p = test_proposal(&format!("run-{i}"));
        p.status = ProposalStatus::Running;
        p.started_at = Some(1000.0);
        store.insert_proposal(&p).unwrap();
    }
    // Insert 1 pending proposal (should NOT be killed)
    let pending = test_proposal("pending-1");
    store.insert_proposal(&pending).unwrap();

    let state = test_app_state(store.clone());
    let paused = Arc::clone(&state.paused);
    let app = crate::api::router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/system/kill-all")
                .header("authorization", "Bearer admin-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp.into_body()).await;
    assert_eq!(json["killed"], 3);
    assert_eq!(json["dispatcher_paused"], true);

    // Verify pause flag is set
    assert!(paused.load(Ordering::Relaxed));

    // Verify all running proposals are now killed
    for i in 0..3 {
        let p = store.get_proposal(&format!("run-{i}")).unwrap().unwrap();
        assert_eq!(p.status, ProposalStatus::Killed);
        assert_eq!(p.kill_reason.as_deref(), Some("kill_all"));
    }

    // Verify pending proposal is untouched
    let p = store.get_proposal("pending-1").unwrap().unwrap();
    assert_eq!(p.status, ProposalStatus::Pending);
}

#[tokio::test]
async fn test_kill_all_with_no_running_still_pauses() {
    let store = test_store();
    let state = test_app_state(store);
    let paused = Arc::clone(&state.paused);
    let app = crate::api::router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/system/kill-all")
                .header("authorization", "Bearer admin-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp.into_body()).await;
    assert_eq!(json["killed"], 0);
    assert!(paused.load(Ordering::Relaxed));
}

// ===========================================================================
// 6. Lease lifecycle (store layer)
// ===========================================================================

#[tokio::test]
async fn test_lease_lifecycle_happy_path() {
    let store = test_store();
    let lease = test_lease("l1", "local", "cpu-only", 600);
    store.insert_lease(&lease).unwrap();

    // Pending -> Approved
    store.approve_lease("l1").unwrap();
    let l = store.get_lease("l1").unwrap().unwrap();
    assert_eq!(l.status, LeaseStatus::Approved);
    assert!(l.approved_at.is_some());

    // Approved -> Expired
    store.expire_lease("l1").unwrap();
    let l = store.get_lease("l1").unwrap().unwrap();
    assert_eq!(l.status, LeaseStatus::Expired);
    assert!(l.expired_at.is_some());
}

#[tokio::test]
async fn test_lease_rejection() {
    let store = test_store();
    let lease = test_lease("l1", "local", "cpu-only", 600);
    store.insert_lease(&lease).unwrap();

    store.reject_lease("l1").unwrap();
    let l = store.get_lease("l1").unwrap().unwrap();
    assert_eq!(l.status, LeaseStatus::Rejected);
    assert!(l.rejected_at.is_some());
}

#[tokio::test]
async fn test_lease_approve_only_works_on_pending() {
    let store = test_store();
    let lease = test_lease("l1", "local", "cpu-only", 600);
    store.insert_lease(&lease).unwrap();
    store.approve_lease("l1").unwrap();

    // Try to approve again — should be no-op
    store.approve_lease("l1").unwrap();
    let l = store.get_lease("l1").unwrap().unwrap();
    assert_eq!(l.status, LeaseStatus::Approved);
}

#[tokio::test]
async fn test_cumulative_runtime_for_lease() {
    let store = test_store();

    // Create and approve lease
    let lease = test_lease("l1", "local", "cpu-only", 600);
    store.insert_lease(&lease).unwrap();
    store.approve_lease("l1").unwrap();

    // Create two completed proposals under the lease
    let mut p1 = test_proposal("p1");
    p1.lease_id = Some("l1".into());
    p1.status = ProposalStatus::Completed;
    p1.started_at = Some(1000.0);
    p1.ended_at = Some(1060.0); // 60s
    store.insert_proposal(&p1).unwrap();

    let mut p2 = test_proposal("p2");
    p2.lease_id = Some("l1".into());
    p2.status = ProposalStatus::Completed;
    p2.started_at = Some(1100.0);
    p2.ended_at = Some(1130.0); // 30s
    store.insert_proposal(&p2).unwrap();

    let runtime = store.cumulative_runtime_for_lease("l1", 2000.0).unwrap();
    assert!((runtime - 90.0).abs() < 0.01, "expected 90s, got {}", runtime);
}

#[tokio::test]
async fn test_cumulative_runtime_includes_running_jobs() {
    let store = test_store();

    let lease = test_lease("l1", "local", "cpu-only", 600);
    store.insert_lease(&lease).unwrap();

    // A running job started at 1000, "now" is 1050 → 50s in progress
    let mut p1 = test_proposal("p1");
    p1.lease_id = Some("l1".into());
    p1.status = ProposalStatus::Running;
    p1.started_at = Some(1000.0);
    store.insert_proposal(&p1).unwrap();

    let runtime = store.cumulative_runtime_for_lease("l1", 1050.0).unwrap();
    assert!((runtime - 50.0).abs() < 0.01, "expected 50s, got {}", runtime);
}

#[tokio::test]
async fn test_count_running_jobs_for_provider() {
    let store = test_store();

    let mut p1 = test_proposal("p1");
    p1.status = ProposalStatus::Running;
    store.insert_proposal(&p1).unwrap();

    let mut p2 = test_proposal("p2");
    p2.status = ProposalStatus::Running;
    store.insert_proposal(&p2).unwrap();

    let mut p3 = test_proposal("p3");
    p3.status = ProposalStatus::Completed;
    store.insert_proposal(&p3).unwrap();

    let count = store.count_running_jobs_for_provider("local").unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn test_proposal_rejected_without_lease() {
    let store = test_store();
    let state = test_app_state(store);
    let app = crate::api::router(state);

    // Submit proposal with non-existent lease
    let body = serde_json::json!({
        "sprint_name": "test",
        "description": "test sprint",
        "provider": "local",
        "resource_request": {"gpu": "cpu-only", "gpu_count": 1},
        "lease_id": "nonexistent"
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/proposals")
                .header("authorization", "Bearer agent-secret")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_proposal_rejected_on_expired_lease() {
    let store = test_store();

    let lease = test_lease("l1", "local", "cpu-only", 600);
    store.insert_lease(&lease).unwrap();
    store.approve_lease("l1").unwrap();
    store.expire_lease("l1").unwrap();

    let state = test_app_state(store);
    let app = crate::api::router(state);

    let body = serde_json::json!({
        "sprint_name": "test",
        "description": "test sprint",
        "provider": "local",
        "resource_request": {"gpu": "cpu-only", "gpu_count": 1},
        "lease_id": "l1"
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/proposals")
                .header("authorization", "Bearer agent-secret")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_proposal_rejected_on_gpu_mismatch() {
    let store = test_store();

    // Create lease for cpu-only
    let lease = test_lease("l1", "local", "cpu-only", 600);
    store.insert_lease(&lease).unwrap();
    store.approve_lease("l1").unwrap();

    let state = test_app_state(store);
    let app = crate::api::router(state);

    // Submit proposal requesting A100 under a cpu-only lease
    let body = serde_json::json!({
        "sprint_name": "test",
        "description": "test sprint",
        "provider": "local",
        "resource_request": {"gpu": "A100", "gpu_count": 1},
        "lease_id": "l1"
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/proposals")
                .header("authorization", "Bearer agent-secret")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp.into_body()).await;
    assert!(json["error"].as_str().unwrap().contains("gpu"));
}

#[tokio::test]
async fn test_lease_api_create_and_approve() {
    let store = test_store();
    let state = test_app_state(store.clone());
    let app = crate::api::router(state);

    // Create lease
    let body = serde_json::json!({
        "provider": "local",
        "gpu": "cpu-only",
        "duration_seconds": 600
    });

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/leases")
                .header("authorization", "Bearer agent-secret")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp.into_body()).await;
    let lease_id = json["lease_id"].as_str().unwrap().to_string();

    // Approve lease (need a new app instance since oneshot consumes it)
    let state2 = test_app_state(store.clone());
    let app2 = crate::api::router(state2);

    let resp = app2
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/leases/{}/approve", lease_id))
                .header("authorization", "Bearer admin-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let lease = store.get_lease(&lease_id).unwrap().unwrap();
    assert_eq!(lease.status, LeaseStatus::Approved);
}

#[tokio::test]
async fn test_get_lease_returns_enriched_response() {
    let store = test_store();

    // Create and approve lease
    let lease = test_lease("l1", "local", "cpu-only", 600);
    store.insert_lease(&lease).unwrap();
    store.approve_lease("l1").unwrap();

    // Add a completed job
    let mut p1 = test_proposal("p1");
    p1.lease_id = Some("l1".into());
    p1.status = ProposalStatus::Completed;
    p1.started_at = Some(1000.0);
    p1.ended_at = Some(1060.0);
    store.insert_proposal(&p1).unwrap();

    let state = test_app_state(store);
    let app = crate::api::router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/leases/l1")
                .header("authorization", "Bearer agent-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp.into_body()).await;
    assert_eq!(json["id"], "l1");
    assert_eq!(json["duration_seconds"], 600);
    assert!(json["time_used_seconds"].as_f64().unwrap() >= 60.0);
    assert!(json["time_remaining_seconds"].as_f64().unwrap() <= 540.0);
    assert_eq!(json["jobs"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_kill_proposal_lease_exceeded() {
    let store = test_store();
    let mut p = test_proposal("p1");
    p.status = ProposalStatus::Running;
    p.started_at = Some(1000.0);
    p.lease_id = Some("l1".into());
    store.insert_proposal(&p).unwrap();

    store.kill_proposal_lease_exceeded("p1", 123.0, 60.0).unwrap();
    let p = store.get_proposal("p1").unwrap().unwrap();
    assert_eq!(p.status, ProposalStatus::Killed);
    assert_eq!(p.kill_reason.as_deref(), Some("lease_time_exceeded"));
    assert!((p.runtime_seconds.unwrap() - 123.0).abs() < 0.01);
    assert!((p.lease_remaining_at_start.unwrap() - 60.0).abs() < 0.01);
}

#[tokio::test]
async fn test_agent_cannot_approve_lease() {
    let store = test_store();
    let lease = test_lease("l1", "local", "cpu-only", 600);
    store.insert_lease(&lease).unwrap();

    let state = test_app_state(store);
    let app = crate::api::router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/leases/l1/approve")
                .header("authorization", "Bearer agent-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
