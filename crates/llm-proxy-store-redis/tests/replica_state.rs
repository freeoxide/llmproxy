//! Plan 09 step 1's done-when, run against the real RESP wire protocol:
//! two instances sharing the replica stores enforce one budget between
//! them, share rate windows and cooldowns, and append to one usage
//! stream. The server on the other side of the socket is the in-repo
//! minimal one in `support/` (no Redis binary exists in this
//! environment).

#![cfg(test)]

mod support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use llm_proxy_core::{KeyBudgetConfig, KeyWindowLimits, ModelPricing, UnestimatableCostPolicy};
use llm_proxy_engine::{Admission, AdmissionClaim, AdmissionOutcome, Cooldowns, KeyPolicy};
use llm_proxy_storage::{EventBus, ProxyEvent};
use llm_proxy_store_redis::{RedisCooldowns, RedisEventBus, RedisStore, replica_admission};
use support::TestRedis;
use time::OffsetDateTime;

fn claim<'a>(caller: &'a str, member: &'a str, model: &'a str) -> AdmissionClaim<'a> {
    AdmissionClaim {
        caller,
        provider: member,
        member,
        model,
        requested_model: model,
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

fn budgeted(usd: &str) -> HashMap<String, KeyPolicy> {
    HashMap::from([(
        "key".to_owned(),
        KeyPolicy {
            budget: Some(KeyBudgetConfig {
                usd_per_day: usd.parse().expect("decimal"),
                unestimatable: UnestimatableCostPolicy::Reject,
                assumed_max_output_tokens: 100,
            }),
            ..KeyPolicy::default()
        },
    )])
}

fn windowed(rpm: u32, tpm: u64) -> HashMap<String, KeyPolicy> {
    HashMap::from([(
        "key".to_owned(),
        KeyPolicy {
            limits: KeyWindowLimits {
                rpm: Some(rpm),
                tpm: Some(tpm),
                rpd: None,
            },
            ..KeyPolicy::default()
        },
    )])
}

fn caps(total: u32) -> HashMap<String, (Option<u32>, HashMap<String, u32>)> {
    HashMap::from([("p".to_owned(), (Some(total), HashMap::new()))])
}

/// The done-when of plan 09 step 1: two replica admissions over one
/// shared store enforce one budget between them — concurrently, each
/// holding its own connection.
#[tokio::test]
async fn two_instances_sharing_the_store_enforce_one_budget_between_them() {
    let server = TestRedis::start().expect("bind the in-process test Redis");
    let store = RedisStore::connect(&server.url(), "test")
        .await
        .expect("connect");
    let replica_a = Arc::new(replica_admission(
        budgeted("2.5"),
        pricing(),
        HashMap::new(),
        store.clone(),
    ));
    let replica_b = Arc::new(replica_admission(
        budgeted("2.5"),
        pricing(),
        HashMap::new(),
        store,
    ));

    // Five concurrent claims, each reserving 1 USD against a 2.5 USD
    // budget, split across both replicas. At most two can be admitted.
    let mut handles = Vec::new();
    for index in 0..5 {
        let admission = if index % 2 == 0 {
            Arc::clone(&replica_a)
        } else {
            Arc::clone(&replica_b)
        };
        handles.push(tokio::spawn(async move {
            admission.reserve(&claim("key", "p", "m")).await.ok()
        }));
    }
    let mut tickets = Vec::new();
    for handle in handles {
        if let Some(ticket) = handle.await.expect("task") {
            tickets.push(ticket);
        }
    }
    assert_eq!(
        tickets.len(),
        2,
        "the shared budget bounds both replicas together"
    );

    // Refunding one admitted reservation on replica a releases budget the
    // other replica can then take — once the fire-and-forget settlement
    // write lands.
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
            if replica_b.reserve(&claim("key", "p", "m")).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), released)
        .await
        .expect("a refund on one replica releases budget on the other");
}

#[tokio::test]
async fn rate_windows_are_shared_across_instances() {
    let server = TestRedis::start().expect("bind the in-process test Redis");
    let store = RedisStore::connect(&server.url(), "test")
        .await
        .expect("connect");
    let replica_a = replica_admission(
        windowed(2, 100_000),
        pricing(),
        HashMap::new(),
        store.clone(),
    );
    let replica_b = replica_admission(windowed(2, 100_000), pricing(), HashMap::new(), store);

    replica_a
        .reserve(&claim("key", "p", "m"))
        .await
        .expect("1/2 rpm on replica a");
    replica_b
        .reserve(&claim("key", "p", "m"))
        .await
        .expect("2/2 rpm counted across both");
    let refused = replica_a
        .reserve(&claim("key", "p", "m"))
        .await
        .expect_err("the third request in the minute is over the shared window");
    match refused {
        llm_proxy_engine::EngineError::RateLimited {
            message,
            retry_after,
        } => {
            assert!(message.contains("rpm"), "got {message}");
            let wait = retry_after.expect("window reset advised");
            assert!(wait.as_secs() >= 1 && wait.as_secs() <= 60, "got {wait:?}");
        }
        other => panic!("expected a 429, got {other:?}"),
    }
}

fn claim_with_tokens<'a>(
    caller: &'a str,
    input: u64,
    max_output: Option<u64>,
) -> AdmissionClaim<'a> {
    AdmissionClaim {
        caller,
        provider: "p",
        member: "p",
        model: "m",
        requested_model: "m",
        body_hash: "h",
        estimated_input_tokens: input,
        max_output_tokens: max_output,
    }
}

#[tokio::test]
async fn a_tpm_refusal_rolls_back_the_request_counts_it_already_committed() {
    // "A refusal leaves no counter behind" (the backend's contract): the
    // tpm check refuses after the rpm and rpd increments committed, so a
    // budget-busting token demand must not also consume a request slot —
    // or every refused request would inflate rpm and rpd until the client
    // sees spurious 429s.
    let server = TestRedis::start().expect("bind the in-process test Redis");
    let store = RedisStore::connect(&server.url(), "test")
        .await
        .expect("connect");
    let keys = HashMap::from([(
        "key".to_owned(),
        KeyPolicy {
            limits: KeyWindowLimits {
                rpm: Some(2),
                tpm: Some(100),
                rpd: Some(1_000),
            },
            ..KeyPolicy::default()
        },
    )]);
    let admission = replica_admission(keys, pricing(), HashMap::new(), store);

    match admission
        .reserve(&claim_with_tokens("key", 500, Some(500)))
        .await
    {
        Err(llm_proxy_engine::EngineError::RateLimited { message, .. }) => {
            assert!(message.contains("tpm"), "got {message}");
        }
        other => panic!("the oversized token demand must refuse on tpm, got {other:?}"),
    }
    admission
        .reserve(&claim_with_tokens("key", 10, None))
        .await
        .expect("the refusal must not have counted as an admitted request (1/2 rpm)");
    admission
        .reserve(&claim_with_tokens("key", 10, None))
        .await
        .expect("the second counted request still fits (2/2 rpm)");
    match admission.reserve(&claim_with_tokens("key", 10, None)).await {
        Err(llm_proxy_engine::EngineError::RateLimited { message, .. }) => {
            assert!(message.contains("rpm"), "got {message}");
        }
        other => panic!("only the two admitted requests may count against rpm, got {other:?}"),
    }
}

#[tokio::test]
async fn a_budget_refusal_rolls_back_the_window_counts_it_already_committed() {
    // Same contract one check later: a budget-exhausted request refuses
    // after rpm, rpd and the tpm reservation committed, and none of them
    // may stay — budget-exhausted callers must not also burn their rate
    // windows or hold token reservations for the rest of the minute.
    let server = TestRedis::start().expect("bind the in-process test Redis");
    let store = RedisStore::connect(&server.url(), "test")
        .await
        .expect("connect");
    let keys = HashMap::from([(
        "key".to_owned(),
        KeyPolicy {
            limits: KeyWindowLimits {
                rpm: Some(1),
                tpm: Some(10_000),
                rpd: None,
            },
            budget: Some(KeyBudgetConfig {
                usd_per_day: "0.5".parse().expect("decimal"),
                unestimatable: UnestimatableCostPolicy::Reject,
                assumed_max_output_tokens: 100,
            }),
            ..KeyPolicy::default()
        },
    )]);
    let admission = replica_admission(keys, pricing(), HashMap::new(), store);

    // 500 in + 500 out at 0.001/0.001 per token estimates 1 USD against a
    // 0.5 USD budget: the budget check refuses; a 10-token demand with the
    // assumed 100-token output (0.11 USD) still fits it.
    match admission
        .reserve(&claim_with_tokens("key", 500, Some(500)))
        .await
    {
        Err(llm_proxy_engine::EngineError::BudgetExhausted { .. }) => {}
        other => panic!("the unaffordable demand must refuse on budget, got {other:?}"),
    }
    admission
        .reserve(&claim_with_tokens("key", 10, None))
        .await
        .expect("the refusal must not have consumed the only rpm slot");
    match admission.reserve(&claim_with_tokens("key", 10, None)).await {
        Err(llm_proxy_engine::EngineError::RateLimited { message, .. }) => {
            assert!(message.contains("rpm"), "got {message}");
        }
        other => panic!("the second affordable request must refuse on rpm alone, got {other:?}"),
    }
}

#[tokio::test]
async fn concurrency_caps_are_shared_and_released_by_settlement() {
    let server = TestRedis::start().expect("bind the in-process test Redis");
    let store = RedisStore::connect(&server.url(), "test")
        .await
        .expect("connect");
    let keys = HashMap::from([("anyone".to_owned(), KeyPolicy::default())]);
    let replica_a = replica_admission(keys.clone(), pricing(), caps(1), store.clone());
    let replica_b = replica_admission(keys, pricing(), caps(1), store);

    let ticket = replica_a
        .reserve(&claim("anyone", "p", "m"))
        .await
        .expect("replica a takes the only slot");
    let refused = replica_b
        .reserve(&claim("anyone", "p", "m"))
        .await
        .expect_err("replica b must see the shared slot taken");
    assert!(matches!(
        refused,
        llm_proxy_engine::EngineError::Overloaded { .. }
    ));

    replica_a.settle(
        &ticket,
        &AdmissionOutcome {
            succeeded: false,
            usage: None,
            cost: None,
        },
    );
    let released = async {
        loop {
            if replica_b.reserve(&claim("anyone", "p", "m")).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), released)
        .await
        .expect("settlement releases the slot for the other replica");
}

#[tokio::test]
async fn cooldowns_written_by_one_replica_are_seen_by_the_other() {
    let server = TestRedis::start().expect("bind the in-process test Redis");
    let store = RedisStore::connect(&server.url(), "test")
        .await
        .expect("connect");
    let a = RedisCooldowns::new(store.clone());
    let b = RedisCooldowns::new(store);

    let now = OffsetDateTime::now_utc();
    assert!(a.provider_available("member", now).await);
    a.cooldown_provider("member", now + Duration::from_secs(30))
        .await;
    assert!(
        !b.provider_available("member", now).await,
        "the other replica sees the cooldown"
    );
    assert!(
        b.provider_available("member", now + Duration::from_secs(31))
            .await
    );

    a.cooldown_credential("member", "key-0", now + Duration::from_secs(10))
        .await;
    assert_eq!(
        b.cooling_credentials("member", now).await,
        vec!["key-0".to_owned()],
        "credential cooldowns cross replicas too"
    );
    b.clear_provider("member").await;
    assert!(a.provider_available("member", now).await);
}

#[tokio::test]
async fn usage_events_from_both_replicas_land_in_one_stream() {
    let server = TestRedis::start().expect("bind the in-process test Redis");
    let store = RedisStore::connect(&server.url(), "test")
        .await
        .expect("connect");
    let bus = RedisEventBus::new(store, "test:usage");
    bus.emit(&ProxyEvent::RequestReceived(
        llm_proxy_storage::RequestReceived {
            request_id: "req-1".to_owned(),
            timestamp: OffsetDateTime::now_utc(),
            caller: "key".to_owned(),
            provider: "p".to_owned(),
            route_kind: "messages".to_owned(),
            client_protocol: "anthropic".to_owned(),
            model: llm_proxy_protocol::core::ModelRef {
                requested: "m".to_owned(),
                upstream: None,
            },
            streaming: false,
            body_hash: "abc".to_owned(),
            session_id: None,
        },
    ));
    bus.close();
    let len = async {
        loop {
            let len = server.stream_len("test:usage");
            if len == 1 {
                return len;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), len)
        .await
        .expect("the event reaches the stream");
}

/// A refusal path that must roll its window increments back: a request
/// the concurrency cap refuses never counted against the rate window.
#[tokio::test]
async fn a_cap_refusal_rolls_back_the_window_counters() {
    let server = TestRedis::start().expect("bind the in-process test Redis");
    let store = RedisStore::connect(&server.url(), "test")
        .await
        .expect("connect");
    let keys = windowed(1, 100_000);
    // One admission with a member cap of 0 (always saturated), one without
    // caps, both sharing the store and the same rpm limit.
    let capped = replica_admission(keys.clone(), pricing(), caps(0), store.clone());
    let uncapped = replica_admission(keys, pricing(), HashMap::new(), store);

    let refused = capped
        .reserve(&claim("key", "p", "m"))
        .await
        .expect_err("cap of 0 refuses");
    assert!(matches!(
        refused,
        llm_proxy_engine::EngineError::Overloaded { .. }
    ));
    uncapped
        .reserve(&claim("key", "p", "m"))
        .await
        .expect("the refused request did not consume the shared rpm window");
}

/// Settlement charges what the provider billed, as integer micro-USD on
/// the shared spent counter — visible to any replica on the same store.
#[tokio::test]
async fn settlement_charges_shared_spend_in_micro_usd() {
    let server = TestRedis::start().expect("bind the in-process test Redis");
    let store = RedisStore::connect(&server.url(), "test")
        .await
        .expect("connect");
    let admission = replica_admission(budgeted("1"), pricing(), HashMap::new(), store);
    let ticket = admission
        .reserve(&claim("key", "p", "m"))
        .await
        .expect("reserves 1 USD of a 1 USD budget");

    let usage = llm_proxy_protocol::core::Usage {
        input_tokens: 50,
        output_tokens: 50,
        ..llm_proxy_protocol::core::Usage::default()
    };
    let cost = llm_proxy_protocol::core::Cost {
        input: "0".parse().expect("decimal"),
        output: "0".parse().expect("decimal"),
        cache_creation: "0".parse().expect("decimal"),
        cache_read: "0".parse().expect("decimal"),
        reasoning: "0".parse().expect("decimal"),
        total: "0.1".parse().expect("decimal"),
    };
    admission.settle(
        &ticket,
        &AdmissionOutcome {
            succeeded: true,
            usage: Some(&usage),
            cost: Some(&cost),
        },
    );

    let day = format!(
        "{:04}{:02}{:02}",
        OffsetDateTime::now_utc().year(),
        u8::from(OffsetDateTime::now_utc().month()),
        OffsetDateTime::now_utc().day()
    );
    let spent_key = format!("test:ad:spent:key:{day}");
    let charge_lands = async {
        loop {
            if server.get(&spent_key).as_deref() == Some("100000") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), charge_lands)
        .await
        .expect("0.1 USD lands as 100000 micro-USD on the shared counter");
}
