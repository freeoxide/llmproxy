//! Plan 09 step 1's done-when against a live Postgres: two instances
//! sharing the replica stores enforce one budget between them, share rate
//! windows and cooldowns, and append to one usage table.
//!
//! These tests need a Postgres server and are `#[ignore]`d. Run them with:
//!
//! ```text
//! LLM_PROXY_TEST_POSTGRES=postgres://localhost/llm_proxy_test \
//!     cargo test -p llm-proxy-store-postgres --test replica_state -- --ignored
//! ```

use std::collections::HashMap;
use std::time::Duration;

use llm_proxy_core::{KeyBudgetConfig, ModelPricing, UnestimatableCostPolicy};
use llm_proxy_engine::{Admission, AdmissionClaim, AdmissionOutcome, Cooldowns, EngineError};
use llm_proxy_protocol::core::ModelRef;
use llm_proxy_storage::{EventBus, ProxyEvent, RequestReceived};
use llm_proxy_store_postgres::{
    PostgresCooldowns, PostgresEventBus, PostgresStore, replica_admission,
};
use time::OffsetDateTime;

fn test_url() -> Option<String> {
    std::env::var("LLM_PROXY_TEST_POSTGRES").ok()
}

fn claim<'a>(caller: &'a str) -> AdmissionClaim<'a> {
    AdmissionClaim {
        caller,
        provider: "p",
        member: "p",
        model: "m",
        requested_model: "m",
        body_hash: "h",
        estimated_input_tokens: 500,
        max_output_tokens: Some(500),
    }
}

fn pricing() -> HashMap<(String, String), ModelPricing> {
    let price = ModelPricing {
        input: "0.001".parse().expect("decimal"),
        output: "0.001".parse().expect("decimal"),
        ..ModelPricing::default()
    };
    HashMap::from([(("p".to_owned(), "m".to_owned()), price)])
}

fn budgeted(usd: &str) -> HashMap<String, llm_proxy_engine::KeyPolicy> {
    HashMap::from([(
        "key".to_owned(),
        llm_proxy_engine::KeyPolicy {
            budget: Some(KeyBudgetConfig {
                usd_per_day: usd.parse().expect("decimal"),
                unestimatable: UnestimatableCostPolicy::Reject,
                assumed_max_output_tokens: 100,
            }),
            ..llm_proxy_engine::KeyPolicy::default()
        },
    )])
}

async fn store() -> PostgresStore {
    let url = test_url().expect("LLM_PROXY_TEST_POSTGRES must be set");
    PostgresStore::connect(&url)
        .await
        .expect("connect to test Postgres")
}

/// The done-when of plan 09 step 1: two replica admissions over one
/// shared database enforce one budget between them.
#[tokio::test]
#[ignore = "needs a live Postgres; set LLM_PROXY_TEST_POSTGRES"]
async fn two_instances_sharing_the_store_enforce_one_budget_between_them() {
    let store = store().await;
    let caller = format!(
        "budget-{}",
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    let keys = HashMap::from([(
        caller.clone(),
        llm_proxy_engine::KeyPolicy {
            budget: Some(KeyBudgetConfig {
                usd_per_day: "2.5".parse().expect("decimal"),
                unestimatable: UnestimatableCostPolicy::Reject,
                assumed_max_output_tokens: 100,
            }),
            ..llm_proxy_engine::KeyPolicy::default()
        },
    )]);
    let replica_a = std::sync::Arc::new(replica_admission(
        keys.clone(),
        pricing(),
        HashMap::new(),
        store.clone(),
    ));
    let replica_b = std::sync::Arc::new(replica_admission(keys, pricing(), HashMap::new(), store));

    let mut handles = Vec::new();
    for index in 0..5 {
        let admission = if index % 2 == 0 {
            std::sync::Arc::clone(&replica_a)
        } else {
            std::sync::Arc::clone(&replica_b)
        };
        let caller = caller.clone();
        handles.push(tokio::spawn(async move {
            admission.reserve(&claim(&caller)).await.ok()
        }));
    }
    let mut tickets = Vec::new();
    for handle in handles {
        if let Some(ticket) = handle.await.expect("task") {
            tickets.push(ticket);
        }
    }
    assert_eq!(tickets.len(), 2, "the shared budget bounds both replicas");

    replica_a.settle(
        &tickets.remove(0),
        &AdmissionOutcome {
            succeeded: false,
            usage: None,
            cost: None,
        },
    );
    let released = async {
        loop {
            if replica_b.reserve(&claim(&caller)).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), released)
        .await
        .expect("a refund on one replica releases budget on the other");
}

#[tokio::test]
#[ignore = "needs a live Postgres; set LLM_PROXY_TEST_POSTGRES"]
async fn rate_windows_are_shared_across_instances() {
    let store = store().await;
    let caller = format!("win-{}", OffsetDateTime::now_utc().unix_timestamp_nanos());
    let keys = HashMap::from([(
        caller.clone(),
        llm_proxy_engine::KeyPolicy {
            limits: llm_proxy_core::KeyWindowLimits {
                rpm: Some(2),
                tpm: None,
                rpd: None,
            },
            ..llm_proxy_engine::KeyPolicy::default()
        },
    )]);
    let replica_a = replica_admission(keys.clone(), pricing(), HashMap::new(), store.clone());
    let replica_b = replica_admission(keys, pricing(), HashMap::new(), store);

    replica_a
        .reserve(&claim(&caller))
        .await
        .expect("1/2 rpm on replica a");
    replica_b
        .reserve(&claim(&caller))
        .await
        .expect("2/2 rpm across both");
    assert!(
        matches!(
            replica_a.reserve(&claim(&caller)).await,
            Err(EngineError::RateLimited { .. })
        ),
        "the shared window refuses the third request"
    );
}

#[tokio::test]
#[ignore = "needs a live Postgres; set LLM_PROXY_TEST_POSTGRES"]
async fn cooldowns_written_by_one_replica_are_seen_by_the_other() {
    let store = store().await;
    let a = PostgresCooldowns::new(store.clone());
    let b = PostgresCooldowns::new(store);
    let now = OffsetDateTime::now_utc();
    let name = format!("member-{}", now.unix_timestamp_nanos());
    a.cooldown_provider(&name, now + Duration::from_secs(30))
        .await;
    assert!(!b.provider_available(&name, now).await);
    assert!(
        b.provider_available(&name, now + Duration::from_secs(31))
            .await
    );
}

#[tokio::test]
#[ignore = "needs a live Postgres; set LLM_PROXY_TEST_POSTGRES"]
async fn usage_events_append_to_the_shared_table() {
    let store = store().await;
    let bus = PostgresEventBus::new(store.clone());
    let request_id = format!("req-{}", OffsetDateTime::now_utc().unix_timestamp_nanos());
    bus.emit(&ProxyEvent::RequestReceived(RequestReceived {
        request_id: request_id.clone(),
        timestamp: OffsetDateTime::now_utc(),
        caller: "key".to_owned(),
        provider: "p".to_owned(),
        route_kind: "messages".to_owned(),
        client_protocol: "anthropic".to_owned(),
        model: ModelRef {
            requested: "m".to_owned(),
            upstream: None,
        },
        streaming: false,
        body_hash: "abc".to_owned(),
        session_id: None,
    }));
    bus.close();
    let lands = async {
        loop {
            if let Ok(1) = store
                .count_usage_events_where("request_id", &request_id)
                .await
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), lands)
        .await
        .expect("the event reaches the shared table");
}
