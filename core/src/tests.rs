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

fn test_job(id: &str) -> Job {
    Job {
        id: id.to_string(),
        status: JobStatus::Running,
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
        dispatched_at: None,
        started_at: Some(1000.0),
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
    concurrency_limits.insert("local".to_string(), 4);
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
// 1. Job state machine (store layer)
// ===========================================================================

#[tokio::test]
async fn test_job_lifecycle_happy_path() {
    let store = test_store();
    let j = test_job("j1");
    store.insert_job(&j).unwrap();

    // Job starts as Running
    let j = store.get_job("j1").unwrap().unwrap();
    assert_eq!(j.status, JobStatus::Running);

    // Running -> Completed
    store.complete_job("j1", None).unwrap();
    let j = store.get_job("j1").unwrap().unwrap();
    assert_eq!(j.status, JobStatus::Completed);
    assert!(j.ended_at.is_some());
}

#[tokio::test]
async fn test_kill_records_reason() {
    let store = test_store();
    let mut j = test_job("j1");
    j.provider_job_id = Some("docker-123".into());
    store.insert_job(&j).unwrap();

    store.kill_job("j1", "timeout_exceeded").unwrap();
    let j = store.get_job("j1").unwrap().unwrap();
    assert_eq!(j.status, JobStatus::Killed);
    assert_eq!(j.kill_reason.as_deref(), Some("timeout_exceeded"));
    assert!(j.ended_at.is_some());
}

#[tokio::test]
async fn test_fail_records_error() {
    let store = test_store();
    let j = test_job("j1");
    store.insert_job(&j).unwrap();

    store.fail_job("j1", "OOM killed").unwrap();
    let j = store.get_job("j1").unwrap().unwrap();
    assert_eq!(j.status, JobStatus::Failed);
    assert_eq!(j.error.as_deref(), Some("OOM killed"));
}

// ===========================================================================
// 2. Budget enforcement
// ===========================================================================

#[tokio::test]
async fn test_budget_rejects_over_limit() {
    let store = test_store();
    store.set_budget_config(100.0, 80, false).unwrap();

    // Need a job for foreign key
    let j = test_job("j1");
    store.insert_job(&j).unwrap();

    // Record $95 of spend
    let entry = CostEntry {
        id: "c1".into(),
        job_id: "j1".into(),
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
async fn test_unauthenticated_rejected() {
    let store = test_store();
    let state = test_app_state(store);
    let app = crate::api::router(state);

    // No Authorization header
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/jobs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
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
// 5. Kill-all: kills running jobs and pauses dispatcher
// ===========================================================================

#[tokio::test]
async fn test_kill_all_transitions_and_pauses() {
    let store = test_store();

    // Insert 3 running jobs
    for i in 0..3 {
        let j = test_job(&format!("run-{i}"));
        store.insert_job(&j).unwrap();
    }

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

    // Verify all running jobs are now killed
    for i in 0..3 {
        let j = store.get_job(&format!("run-{i}")).unwrap().unwrap();
        assert_eq!(j.status, JobStatus::Killed);
        assert_eq!(j.kill_reason.as_deref(), Some("kill_all"));
    }
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

    // Create two completed jobs under the lease
    let mut j1 = test_job("j1");
    j1.lease_id = Some("l1".into());
    j1.status = JobStatus::Completed;
    j1.started_at = Some(1000.0);
    j1.ended_at = Some(1060.0); // 60s
    store.insert_job(&j1).unwrap();

    let mut j2 = test_job("j2");
    j2.lease_id = Some("l1".into());
    j2.status = JobStatus::Completed;
    j2.started_at = Some(1100.0);
    j2.ended_at = Some(1130.0); // 30s
    store.insert_job(&j2).unwrap();

    let runtime = store.cumulative_runtime_for_lease("l1", 2000.0).unwrap();
    assert!((runtime - 90.0).abs() < 0.01, "expected 90s, got {}", runtime);
}

#[tokio::test]
async fn test_cumulative_runtime_includes_running_jobs() {
    let store = test_store();

    let lease = test_lease("l1", "local", "cpu-only", 600);
    store.insert_lease(&lease).unwrap();

    // A running job started at 1000, "now" is 1050 → 50s in progress
    let mut j1 = test_job("j1");
    j1.lease_id = Some("l1".into());
    j1.status = JobStatus::Running;
    j1.started_at = Some(1000.0);
    store.insert_job(&j1).unwrap();

    let runtime = store.cumulative_runtime_for_lease("l1", 1050.0).unwrap();
    assert!((runtime - 50.0).abs() < 0.01, "expected 50s, got {}", runtime);
}

#[tokio::test]
async fn test_count_running_jobs_for_provider() {
    let store = test_store();

    let j1 = test_job("j1");
    store.insert_job(&j1).unwrap();

    let j2 = test_job("j2");
    store.insert_job(&j2).unwrap();

    let mut j3 = test_job("j3");
    j3.status = JobStatus::Completed;
    store.insert_job(&j3).unwrap();

    let count = store.count_running_jobs_for_provider("local").unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn test_job_rejected_without_lease() {
    let store = test_store();
    let state = test_app_state(store);
    let app = crate::api::router(state);

    // Submit job with non-existent lease
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
                .uri("/jobs")
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
async fn test_job_rejected_on_expired_lease() {
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
                .uri("/jobs")
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
async fn test_job_rejected_on_gpu_mismatch() {
    let store = test_store();

    // Create lease for cpu-only
    let lease = test_lease("l1", "local", "cpu-only", 600);
    store.insert_lease(&lease).unwrap();
    store.approve_lease("l1").unwrap();

    let state = test_app_state(store);
    let app = crate::api::router(state);

    // Submit job requesting A100 under a cpu-only lease
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
                .uri("/jobs")
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
    let mut j1 = test_job("j1");
    j1.lease_id = Some("l1".into());
    j1.status = JobStatus::Completed;
    j1.started_at = Some(1000.0);
    j1.ended_at = Some(1060.0);
    store.insert_job(&j1).unwrap();

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
async fn test_kill_job_lease_exceeded() {
    let store = test_store();
    let mut j = test_job("j1");
    j.lease_id = Some("l1".into());
    store.insert_job(&j).unwrap();

    store.kill_job_lease_exceeded("j1", 123.0, 60.0).unwrap();
    let j = store.get_job("j1").unwrap().unwrap();
    assert_eq!(j.status, JobStatus::Killed);
    assert_eq!(j.kill_reason.as_deref(), Some("lease_time_exceeded"));
    assert!((j.runtime_seconds.unwrap() - 123.0).abs() < 0.01);
    assert!((j.lease_remaining_at_start.unwrap() - 60.0).abs() < 0.01);
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
