//! The one admission step (plan 05 steps 3-7, backlog 6/18/30, conflicts 2
//! and 8): per-key allowlists, rate windows, spend budgets and concurrency
//! caps decided in one place, reserve-then-settle.
//!
//! The rules:
//!
//! - **Allowlists** (step 3). A key with `allow_models` may only run a model
//!   matching one of its globs (the requested name or the upstream name);
//!   with `allow_providers`, only a matching route. A restricted key whose
//!   request names no model fails closed. Refusals are 403.
//! - **Windows** (step 5). `rpm` and `rpd` count admitted requests per
//!   calendar minute and UTC day; `tpm` counts tokens per calendar minute —
//!   the input estimate plus `max_tokens` is reserved at admission and
//!   replaced by the provider-reported total at settlement. Refusals are 429
//!   with `Retry-After` set to the window's reset.
//! - **Budgets** (step 4). A key with `usd_per_day` reserves its estimated
//!   cost (input estimate plus `max_tokens`, or the key's
//!   `assumed_max_output_tokens`, at the member's pricing) before egress.
//!   Admission requires `spent + reserved_in_flight + estimate <= budget`,
//!   so concurrent requests cannot overspend the budget by more than what
//!   is reserved. A model with no pricing cannot be estimated and goes to
//!   the key's explicit `unestimatable` policy: reject (402), or reserve a
//!   fixed amount. Refusals are 402.
//! - **What counts** (step 6). At settlement the reservation is released
//!   and the budget is charged exactly what the provider billed: the
//!   engine-computed cost of the usage the serving attempt reported. A
//!   request that fails before any usage charges nothing (the reservation
//!   is simply released: the refund). Cache hits never reach `reserve`, so
//!   they charge nothing. Unpriced usage cannot be charged and is logged.
//! - **Concurrency** (step 7). `[provider.concurrency]` caps in-flight
//!   requests per member and per upstream model; a saturated cap answers
//!   503 with a one-second `Retry-After`, never 429, because capacity
//!   shedding misread as rate limiting arms hour-long client backoff
//!   ladders. The slot is held from admission to settlement.
//!
//! Every check and every mutation happens under one lock, so a reservation
//! is atomic across all four rules. Keys without a policy entry and the
//! anonymous caller are unrestricted; a provider without
//! `[provider.concurrency]` is uncapped.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use llm_proxy_core::{
    AuthConfig, ClientKeySpec, KeyBudgetConfig, KeyWindowLimits, ModelPricing, ProviderRegistry,
    UnestimatableCostPolicy, glob_match,
};
use llm_proxy_protocol::core::Usage;
use rust_decimal::Decimal;
use time::{Date, OffsetDateTime};

use crate::error::EngineError;
use crate::traits::{Admission, AdmissionClaim, AdmissionOutcome, AdmissionTicket};

/// The wait advised when a concurrency cap is saturated: short, because a
/// slot frees as soon as any in-flight request settles.
const SATURATED_RETRY_AFTER: Duration = Duration::from_secs(1);

/// One key's admission policy, as configured.
#[derive(Debug, Clone, Default)]
pub struct KeyPolicy {
    /// Model globs this key may run; empty allows all.
    pub allow_models: Vec<String>,
    /// Route (provider or pool) globs this key may address; empty allows all.
    pub allow_providers: Vec<String>,
    /// Rate windows.
    pub limits: KeyWindowLimits,
    /// Spend budget.
    pub budget: Option<KeyBudgetConfig>,
}

impl KeyPolicy {
    fn restricts_anything(&self) -> bool {
        !self.allow_models.is_empty()
            || !self.allow_providers.is_empty()
            || self.limits != KeyWindowLimits::default()
            || self.budget.is_some()
    }
}

/// A member's in-flight caps.
#[derive(Debug, Clone, Default)]
struct MemberCaps {
    total: Option<u32>,
    models: HashMap<String, u32>,
}

type Clock = Box<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// Per-minute counters for one key.
#[derive(Debug, Default)]
struct MinuteWindow {
    /// Unix minute this window counts.
    minute: i64,
    requests: u64,
    /// Settled token totals.
    tokens: u64,
    /// In-flight token reservations.
    reserved_tokens: u64,
}

/// Per-day counters for one key.
#[derive(Debug)]
struct DayWindow {
    day: Date,
    requests: u64,
    /// Settled spend.
    spent: Decimal,
    /// In-flight spend reservations.
    reserved: Decimal,
}

#[derive(Debug)]
struct KeyState {
    minute: MinuteWindow,
    day: DayWindow,
}

impl KeyState {
    fn fresh(now: OffsetDateTime) -> Self {
        Self {
            minute: MinuteWindow {
                minute: unix_minute(now),
                ..MinuteWindow::default()
            },
            day: DayWindow {
                day: now.date(),
                requests: 0,
                spent: Decimal::ZERO,
                reserved: Decimal::ZERO,
            },
        }
    }

    /// Rolls the windows forward when the clock crossed a boundary.
    fn roll(&mut self, now: OffsetDateTime) {
        let minute = unix_minute(now);
        if self.minute.minute != minute {
            self.minute = MinuteWindow {
                minute,
                ..MinuteWindow::default()
            };
        }
        if self.day.day != now.date() {
            self.day = DayWindow {
                day: now.date(),
                requests: 0,
                spent: Decimal::ZERO,
                reserved: Decimal::ZERO,
            };
        }
    }
}

/// What one reservation holds, so settlement can release exactly it.
#[derive(Debug)]
struct Lease {
    caller: String,
    member: String,
    model: String,
    minute: i64,
    day: Date,
    reserved_tokens: u64,
    reserved_usd: Decimal,
    holds_member_slot: bool,
    holds_model_slot: bool,
}

#[derive(Debug, Default)]
struct State {
    keys: HashMap<String, KeyState>,
    leases: HashMap<u64, Lease>,
    next_lease: u64,
    inflight_members: HashMap<String, u32>,
    inflight_models: HashMap<(String, String), u32>,
}

/// The configured [`Admission`]: allowlists, windows, budgets and
/// concurrency caps from `[auth.clients]` and `[provider.concurrency]`.
pub struct PolicyAdmission {
    keys: HashMap<String, KeyPolicy>,
    pricing: HashMap<(String, String), ModelPricing>,
    caps: HashMap<String, MemberCaps>,
    clock: Clock,
    state: Mutex<State>,
}

impl std::fmt::Debug for PolicyAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyAdmission")
            .field("keys", &self.keys.keys().collect::<Vec<_>>())
            .field("priced_models", &self.pricing.len())
            .field("capped_members", &self.caps.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl PolicyAdmission {
    /// Builds the admission step from the auth section (per-key policies)
    /// and the provider registry (pricing for estimates, concurrency caps).
    #[must_use]
    pub fn from_config(auth: &AuthConfig, providers: &ProviderRegistry) -> Self {
        let keys = auth
            .clients
            .iter()
            .filter_map(|(id, spec)| match spec {
                ClientKeySpec::Verifier(_) => None,
                ClientKeySpec::Policy {
                    allow_models,
                    allow_providers,
                    limits,
                    budget,
                    ..
                } => Some((
                    id.clone(),
                    KeyPolicy {
                        allow_models: allow_models.clone(),
                        allow_providers: allow_providers.clone(),
                        limits: limits.clone().unwrap_or_default(),
                        budget: budget.clone(),
                    },
                )),
            })
            .collect();
        let mut pricing = HashMap::new();
        let mut caps: HashMap<String, (Option<u32>, HashMap<String, u32>)> = HashMap::new();
        for provider in providers.iter() {
            for (model, price) in &provider.pricing {
                pricing.insert(
                    (provider.name.clone(), model.as_str().to_owned()),
                    price.clone(),
                );
            }
            if let Some(concurrency) = &provider.concurrency {
                caps.insert(
                    provider.name.clone(),
                    (
                        concurrency.total,
                        concurrency
                            .models
                            .iter()
                            .map(|(model, cap)| (model.as_str().to_owned(), *cap))
                            .collect(),
                    ),
                );
            }
        }
        Self::new(keys, pricing, caps)
    }

    /// Builds from explicit parts: per-key policies, `(member, model)`
    /// pricing, and per-member `(total, per-model)` caps.
    #[must_use]
    pub fn new(
        keys: HashMap<String, KeyPolicy>,
        pricing: HashMap<(String, String), ModelPricing>,
        caps: HashMap<String, (Option<u32>, HashMap<String, u32>)>,
    ) -> Self {
        Self {
            keys,
            pricing,
            caps: caps
                .into_iter()
                .map(|(member, (total, models))| (member, MemberCaps { total, models }))
                .collect(),
            clock: Box::new(OffsetDateTime::now_utc),
            state: Mutex::new(State::default()),
        }
    }

    /// Replace the clock the windows read (tests drive it by hand).
    #[must_use]
    pub fn with_clock(
        mut self,
        clock: impl Fn() -> OffsetDateTime + Send + Sync + 'static,
    ) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Whether this step would ever refuse anything: `false` when no key
    /// carries a policy and no provider a cap, so a builder can keep the
    /// allow-all default.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.keys
            .values()
            .all(|policy| !policy.restricts_anything())
            && self.caps.is_empty()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("admission state poisoned; recovering");
            poisoned.into_inner()
        })
    }

    /// The reservation a budgeted key needs for this claim, or the refusal
    /// its unestimatable policy dictates.
    fn estimate_usd(
        &self,
        claim: &AdmissionClaim<'_>,
        budget: &KeyBudgetConfig,
    ) -> Result<Decimal, EngineError> {
        let Some(price) = self
            .pricing
            .get(&(claim.member.to_owned(), claim.model.to_owned()))
        else {
            return match &budget.unestimatable {
                UnestimatableCostPolicy::Reject => Err(EngineError::BudgetExhausted {
                    message: format!(
                        "cost of model \"{}\" on \"{}\" cannot be estimated (no pricing) and the key's policy rejects unestimatable requests",
                        claim.model, claim.member
                    ),
                    retry_after: None,
                }),
                UnestimatableCostPolicy::ReserveUsd { usd } => Ok(*usd),
            };
        };
        let output = claim
            .max_output_tokens
            .unwrap_or(budget.assumed_max_output_tokens);
        let input_cost = Decimal::from(claim.estimated_input_tokens)
            .checked_mul(price.input)
            .unwrap_or(Decimal::MAX);
        let output_cost = Decimal::from(output)
            .checked_mul(price.output)
            .unwrap_or(Decimal::MAX);
        Ok(input_cost.checked_add(output_cost).unwrap_or(Decimal::MAX))
    }
}

fn unix_minute(now: OffsetDateTime) -> i64 {
    now.unix_timestamp().div_euclid(60)
}

/// Seconds until the next calendar minute starts.
fn until_next_minute(now: OffsetDateTime) -> Duration {
    let elapsed = now.unix_timestamp().rem_euclid(60);
    Duration::from_secs((60 - elapsed).clamp(1, 60).unsigned_abs())
}

/// Seconds until the next UTC day starts.
fn until_next_day(now: OffsetDateTime) -> Duration {
    let (hours, minutes, seconds) = now.time().as_hms();
    let elapsed = i64::from(hours) * 3600 + i64::from(minutes) * 60 + i64::from(seconds);
    Duration::from_secs((86_400 - elapsed).clamp(1, 86_400).unsigned_abs())
}

fn total_tokens(usage: &Usage) -> u64 {
    let parts = [
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_creation_input_tokens.unwrap_or(0),
        usage.cache_read_input_tokens.unwrap_or(0),
        usage.reasoning_tokens.unwrap_or(0),
    ];
    parts
        .into_iter()
        .map(|count| u64::try_from(count).unwrap_or(0))
        .sum()
}

/// Whether `value` (or `alternate`) matches any glob; an empty list allows
/// everything, and a restricted list with nothing to match fails closed.
fn allowed_by(globs: &[String], value: &str, alternate: &str) -> bool {
    if globs.is_empty() {
        return true;
    }
    [value, alternate]
        .into_iter()
        .filter(|candidate| !candidate.is_empty())
        .any(|candidate| globs.iter().any(|glob| glob_match(glob, candidate)))
}

#[async_trait]
impl Admission for PolicyAdmission {
    #[expect(
        clippy::too_many_lines,
        reason = "the four rules are checked and then committed under one lock; splitting them would scatter the atomic reservation"
    )]
    async fn reserve(&self, claim: &AdmissionClaim<'_>) -> Result<AdmissionTicket, EngineError> {
        let now = (self.clock)();
        let policy = self.keys.get(claim.caller);

        // Step 3: allowlists, before any accounting.
        if let Some(policy) = policy {
            if !allowed_by(&policy.allow_models, claim.requested_model, claim.model) {
                return Err(EngineError::Forbidden(
                    if claim.requested_model.is_empty() && claim.model.is_empty() {
                        "this key is restricted to specific models and the request names none"
                            .to_owned()
                    } else {
                        format!(
                            "this key may not use model \"{}\"",
                            if claim.requested_model.is_empty() {
                                claim.model
                            } else {
                                claim.requested_model
                            }
                        )
                    },
                ));
            }
            if !allowed_by(&policy.allow_providers, claim.provider, "") {
                return Err(EngineError::Forbidden(format!(
                    "this key may not use provider \"{}\"",
                    claim.provider
                )));
            }
        }

        // Budget estimate is computed before the lock: it reads only config.
        let reserved_usd = match policy.and_then(|p| p.budget.as_ref()) {
            Some(budget) => Some((budget, self.estimate_usd(claim, budget)?)),
            None => None,
        };
        let reserved_tokens = claim
            .estimated_input_tokens
            .saturating_add(claim.max_output_tokens.unwrap_or(0));

        let mut state = self.lock();
        let State {
            keys,
            leases,
            next_lease,
            inflight_members,
            inflight_models,
        } = &mut *state;

        // Steps 4 and 5: windows and budget, checked against committed plus
        // in-flight totals.
        if let Some(policy) = policy {
            let key = keys
                .entry(claim.caller.to_owned())
                .or_insert_with(|| KeyState::fresh(now));
            key.roll(now);
            let limits = &policy.limits;
            if let Some(rpm) = limits.rpm
                && key.minute.requests >= u64::from(rpm)
            {
                return Err(EngineError::RateLimited {
                    message: format!("rpm limit {rpm} reached for this key"),
                    retry_after: Some(until_next_minute(now)),
                });
            }
            if let Some(rpd) = limits.rpd
                && key.day.requests >= rpd
            {
                return Err(EngineError::RateLimited {
                    message: format!("rpd limit {rpd} reached for this key"),
                    retry_after: Some(until_next_day(now)),
                });
            }
            if let Some(tpm) = limits.tpm
                && key
                    .minute
                    .tokens
                    .saturating_add(key.minute.reserved_tokens)
                    .saturating_add(reserved_tokens)
                    > tpm
            {
                return Err(EngineError::RateLimited {
                    message: format!("tpm limit {tpm} would be exceeded for this key"),
                    retry_after: Some(until_next_minute(now)),
                });
            }
            if let Some((budget, estimate)) = &reserved_usd {
                let committed = key
                    .day
                    .spent
                    .checked_add(key.day.reserved)
                    .unwrap_or(Decimal::MAX);
                let projected = committed.checked_add(*estimate).unwrap_or(Decimal::MAX);
                if projected > budget.usd_per_day {
                    return Err(EngineError::BudgetExhausted {
                        message: format!(
                            "daily budget of {} USD would be exceeded for this key (spent {}, reserved in flight {}, this request {})",
                            budget.usd_per_day.normalize(),
                            key.day.spent.normalize(),
                            key.day.reserved.normalize(),
                            estimate.normalize()
                        ),
                        retry_after: Some(until_next_day(now)),
                    });
                }
            }
        }

        // Step 7: concurrency caps on the member and its model.
        let caps = self.caps.get(claim.member);
        let member_cap = caps.and_then(|c| c.total);
        let model_cap = caps.and_then(|c| c.models.get(claim.model).copied());
        let model_key = (claim.member.to_owned(), claim.model.to_owned());
        if let Some(cap) = member_cap
            && inflight_members.get(claim.member).copied().unwrap_or(0) >= cap
        {
            return Err(EngineError::Overloaded {
                message: format!(
                    "provider \"{}\" is at its in-flight cap of {cap}",
                    claim.member
                ),
                retry_after: Some(SATURATED_RETRY_AFTER),
            });
        }
        if let Some(cap) = model_cap
            && inflight_models.get(&model_key).copied().unwrap_or(0) >= cap
        {
            return Err(EngineError::Overloaded {
                message: format!(
                    "model \"{}\" on \"{}\" is at its in-flight cap of {cap}",
                    claim.model, claim.member
                ),
                retry_after: Some(SATURATED_RETRY_AFTER),
            });
        }

        // Every check passed: commit the whole reservation at once.
        let mut lease = Lease {
            caller: claim.caller.to_owned(),
            member: claim.member.to_owned(),
            model: claim.model.to_owned(),
            minute: unix_minute(now),
            day: now.date(),
            reserved_tokens: 0,
            reserved_usd: Decimal::ZERO,
            holds_member_slot: member_cap.is_some(),
            holds_model_slot: model_cap.is_some(),
        };
        if policy.is_some()
            && let Some(key) = keys.get_mut(claim.caller)
        {
            key.minute.requests += 1;
            key.day.requests += 1;
            key.minute.reserved_tokens = key.minute.reserved_tokens.saturating_add(reserved_tokens);
            lease.reserved_tokens = reserved_tokens;
            if let Some((_, estimate)) = &reserved_usd {
                key.day.reserved = key
                    .day
                    .reserved
                    .checked_add(*estimate)
                    .unwrap_or(Decimal::MAX);
                lease.reserved_usd = *estimate;
            }
        }
        if lease.holds_member_slot {
            *inflight_members.entry(claim.member.to_owned()).or_insert(0) += 1;
        }
        if lease.holds_model_slot {
            *inflight_models.entry(model_key).or_insert(0) += 1;
        }
        let id = *next_lease;
        *next_lease += 1;
        leases.insert(id, lease);
        Ok(AdmissionTicket {
            reserved: Some(reserved_tokens),
            lease: Some(id),
        })
    }

    fn settle(&self, ticket: &AdmissionTicket, outcome: &AdmissionOutcome<'_>) {
        let Some(id) = ticket.lease else {
            return;
        };
        let now = (self.clock)();
        let mut state = self.lock();
        let Some(lease) = state.leases.remove(&id) else {
            tracing::warn!(
                lease = id,
                "admission settle for an unknown lease; ignoring"
            );
            return;
        };
        if lease.holds_member_slot
            && let Some(count) = state.inflight_members.get_mut(&lease.member)
        {
            *count = count.saturating_sub(1);
        }
        if lease.holds_model_slot
            && let Some(count) = state
                .inflight_models
                .get_mut(&(lease.member.clone(), lease.model.clone()))
        {
            *count = count.saturating_sub(1);
        }
        let Some(key) = state.keys.get_mut(&lease.caller) else {
            return;
        };
        key.roll(now);
        // Tokens: release the estimate and charge the provider-reported
        // total, but only into the minute the request was admitted in — a
        // window that already rolled over has no limit left to protect.
        if key.minute.minute == lease.minute {
            key.minute.reserved_tokens = key
                .minute
                .reserved_tokens
                .saturating_sub(lease.reserved_tokens);
            if let Some(usage) = outcome.usage {
                key.minute.tokens = key.minute.tokens.saturating_add(total_tokens(usage));
            }
        }
        // Money: release the reservation from the day it was made (if that
        // day is still current) and charge what the provider billed to
        // today — spend is never dropped at a midnight boundary.
        if key.day.day == lease.day {
            key.day.reserved = key
                .day
                .reserved
                .checked_sub(lease.reserved_usd)
                .unwrap_or(Decimal::ZERO)
                .max(Decimal::ZERO);
        }
        match (outcome.cost, outcome.usage) {
            (Some(cost), _) => {
                key.day.spent = key
                    .day
                    .spent
                    .checked_add(cost.total)
                    .unwrap_or(Decimal::MAX);
            }
            (None, Some(_))
                if lease.reserved_usd > Decimal::ZERO
                    || self
                        .keys
                        .get(&lease.caller)
                        .is_some_and(|p| p.budget.is_some()) =>
            {
                tracing::warn!(
                    caller = %lease.caller,
                    member = %lease.member,
                    model = %lease.model,
                    "usage settled against a budgeted key with no pricing for the model; nothing charged (unpriced)"
                );
            }
            // No usage at all: the request failed before any cost. The
            // reservation release above is the refund.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn claim<'a>(
        caller: &'a str,
        member: &'a str,
        model: &'a str,
        input: u64,
        max_output: Option<u64>,
    ) -> AdmissionClaim<'a> {
        AdmissionClaim {
            caller,
            provider: member,
            member,
            model,
            requested_model: model,
            body_hash: "h",
            estimated_input_tokens: input,
            max_output_tokens: max_output,
        }
    }

    fn pricing(input: &str, output: &str) -> ModelPricing {
        ModelPricing {
            input: input.parse().expect("decimal"),
            output: output.parse().expect("decimal"),
            ..ModelPricing::default()
        }
    }

    fn budgeted(usd_per_day: &str) -> KeyPolicy {
        KeyPolicy {
            budget: Some(KeyBudgetConfig {
                usd_per_day: usd_per_day.parse().expect("decimal"),
                unestimatable: UnestimatableCostPolicy::Reject,
                assumed_max_output_tokens: 100,
            }),
            ..KeyPolicy::default()
        }
    }

    /// An admission with one priced model: 1 USD per 1000 input tokens and
    /// per 1000 output tokens, so a (500 in, 500 out) claim reserves 1 USD.
    fn admission(policy: KeyPolicy) -> PolicyAdmission {
        PolicyAdmission::new(
            HashMap::from([("key".to_owned(), policy)]),
            HashMap::from([(("p".to_owned(), "m".to_owned()), pricing("0.001", "0.001"))]),
            HashMap::new(),
        )
    }

    fn usage(input: i32, output: i32) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        }
    }

    fn cost(total: &str) -> llm_proxy_protocol::core::Cost {
        llm_proxy_protocol::core::Cost {
            input: Decimal::ZERO,
            output: Decimal::ZERO,
            cache_creation: Decimal::ZERO,
            cache_read: Decimal::ZERO,
            reasoning: Decimal::ZERO,
            total: total.parse().expect("decimal"),
        }
    }

    #[tokio::test]
    async fn concurrent_reservations_cannot_overspend_the_budget() {
        // Budget 2.5 USD; each claim reserves 1 USD; five claims race.
        let admission = Arc::new(admission(budgeted("2.5")));
        let mut handles = Vec::new();
        for _ in 0..5 {
            let admission = Arc::clone(&admission);
            handles.push(tokio::spawn(async move {
                admission
                    .reserve(&claim("key", "p", "m", 500, Some(500)))
                    .await
                    .is_ok()
            }));
        }
        let mut admitted = 0;
        for handle in handles {
            if handle.await.expect("task") {
                admitted += 1;
            }
        }
        assert_eq!(
            admitted, 2,
            "spent + in-flight reservations + this request must stay within the budget"
        );
    }

    #[tokio::test]
    async fn a_refund_restores_the_budget_when_a_request_fails_before_any_cost() {
        let admission = admission(budgeted("1"));
        let ticket = admission
            .reserve(&claim("key", "p", "m", 500, Some(500)))
            .await
            .expect("first reservation fits");
        let refused = admission
            .reserve(&claim("key", "p", "m", 500, Some(500)))
            .await
            .expect_err("the budget is fully reserved");
        assert!(matches!(refused, EngineError::BudgetExhausted { .. }));
        assert_eq!(refused.status_code(), 402);

        // Failed before any usage: refund.
        admission.settle(
            &ticket,
            &AdmissionOutcome {
                succeeded: false,
                usage: None,
                cost: None,
            },
        );
        admission
            .reserve(&claim("key", "p", "m", 500, Some(500)))
            .await
            .expect("the refund restored the full budget");
    }

    #[tokio::test]
    async fn settlement_charges_what_the_provider_billed_not_the_estimate() {
        let admission = admission(budgeted("1"));
        let ticket = admission
            .reserve(&claim("key", "p", "m", 500, Some(500)))
            .await
            .expect("reserve 1 USD");
        // The provider billed 0.1 USD (far below the 1 USD estimate).
        let billed = usage(50, 50);
        admission.settle(
            &ticket,
            &AdmissionOutcome {
                succeeded: true,
                usage: Some(&billed),
                cost: Some(&cost("0.1")),
            },
        );
        // 0.9 USD remains: a 0.9 USD claim fits, a 0.95 USD claim does not.
        admission
            .reserve(&claim("key", "p", "m", 450, Some(450)))
            .await
            .expect("0.9 fits in the 0.9 left");
        let refused = admission
            .reserve(&claim("key", "p", "m", 25, Some(25)))
            .await
            .expect_err("nothing is left once 0.9 is reserved again");
        assert!(matches!(refused, EngineError::BudgetExhausted { .. }));
    }

    #[tokio::test]
    async fn an_unpriced_model_goes_to_the_keys_explicit_policy() {
        let reject = admission(budgeted("10"));
        let refused = reject
            .reserve(&claim("key", "p", "unpriced", 10, Some(10)))
            .await
            .expect_err("reject policy fails closed");
        assert!(matches!(refused, EngineError::BudgetExhausted { .. }));

        let mut policy = budgeted("1");
        if let Some(budget) = policy.budget.as_mut() {
            budget.unestimatable = UnestimatableCostPolicy::ReserveUsd {
                usd: "0.6".parse().expect("decimal"),
            };
        }
        let reserve = admission(policy);
        reserve
            .reserve(&claim("key", "p", "unpriced", 10, Some(10)))
            .await
            .expect("the fixed reservation fits");
        let refused = reserve
            .reserve(&claim("key", "p", "unpriced", 10, Some(10)))
            .await
            .expect_err("a second fixed reservation would overspend");
        assert!(matches!(refused, EngineError::BudgetExhausted { .. }));
    }

    #[tokio::test]
    async fn allowlists_fail_closed_without_an_identifiable_model() {
        let policy = KeyPolicy {
            allow_models: vec!["claude-*".to_owned()],
            allow_providers: vec!["anthropic*".to_owned()],
            ..KeyPolicy::default()
        };
        let admission = admission(policy);
        let ok = AdmissionClaim {
            provider: "anthropic-pool",
            member: "anthropic",
            requested_model: "claude-sonnet-4-6",
            model: "claude-sonnet-4-6",
            ..claim("key", "anthropic", "claude-sonnet-4-6", 1, None)
        };
        admission
            .reserve(&ok)
            .await
            .expect("matching model and provider");

        let wrong_model = AdmissionClaim {
            requested_model: "gpt-4o",
            model: "gpt-4o",
            ..ok
        };
        let refused = admission
            .reserve(&wrong_model)
            .await
            .expect_err("model denied");
        assert!(matches!(refused, EngineError::Forbidden(_)));
        assert_eq!(refused.status_code(), 403);

        let wrong_provider = AdmissionClaim {
            provider: "openai",
            ..ok
        };
        assert!(matches!(
            admission.reserve(&wrong_provider).await,
            Err(EngineError::Forbidden(_))
        ));

        let no_model = AdmissionClaim {
            requested_model: "",
            model: "",
            ..ok
        };
        let refused = admission
            .reserve(&no_model)
            .await
            .expect_err("a restricted key with no model fails closed");
        assert!(matches!(refused, EngineError::Forbidden(_)));

        // An unrestricted key (no policy entry) runs anything.
        admission
            .reserve(&AdmissionClaim {
                caller: "other",
                ..no_model
            })
            .await
            .expect("keys without a policy are unrestricted");
    }

    #[tokio::test]
    async fn rate_windows_refuse_with_the_window_reset_and_roll_over() {
        let clock = Arc::new(AtomicI64::new(1_700_000_000));
        let reader = Arc::clone(&clock);
        let policy = KeyPolicy {
            limits: KeyWindowLimits {
                rpm: Some(2),
                tpm: Some(1_000),
                rpd: Some(3),
            },
            ..KeyPolicy::default()
        };
        let admission = admission(policy).with_clock(move || {
            OffsetDateTime::from_unix_timestamp(reader.load(Ordering::SeqCst)).expect("timestamp")
        });

        let first = admission
            .reserve(&claim("key", "p", "m", 100, Some(100)))
            .await
            .expect("1/2 rpm");
        admission
            .reserve(&claim("key", "p", "m", 100, Some(100)))
            .await
            .expect("2/2 rpm");
        let refused = admission
            .reserve(&claim("key", "p", "m", 100, Some(100)))
            .await
            .expect_err("3rd in the minute");
        match refused {
            EngineError::RateLimited { retry_after, .. } => {
                let wait = retry_after.expect("window reset advised");
                assert!(wait.as_secs() >= 1 && wait.as_secs() <= 60, "got {wait:?}");
            }
            other => panic!("expected a 429, got {other:?}"),
        }

        // Next minute: rpm resets, but the day count (2) carries on.
        clock.fetch_add(60, Ordering::SeqCst);
        admission
            .reserve(&claim("key", "p", "m", 100, Some(100)))
            .await
            .expect("3/3 rpd in a fresh minute");
        let refused = admission
            .reserve(&claim("key", "p", "m", 100, Some(100)))
            .await
            .expect_err("4th in the day");
        assert!(
            matches!(&refused, EngineError::RateLimited { message, .. } if message.contains("rpd")),
            "expected the day window to refuse, got {refused:?}"
        );

        // Next day: everything resets.
        clock.fetch_add(86_400, Ordering::SeqCst);
        admission
            .reserve(&claim("key", "p", "m", 100, Some(100)))
            .await
            .expect("fresh day");

        // TPM: the fresh day's first claim reserved 200 tokens; a claim
        // estimated at 1000 would push the minute to 1200 > 1000.
        let refused = admission
            .reserve(&claim("key", "p", "m", 900, Some(100)))
            .await
            .expect_err("200 reserved + 1000 estimated exceeds tpm 1000");
        assert!(
            matches!(&refused, EngineError::RateLimited { message, .. } if message.contains("tpm")),
            "expected the token window to refuse, got {refused:?}"
        );

        // Settling the first (old-minute) lease never disturbs the current
        // minute's accounting.
        admission.settle(
            &first,
            &AdmissionOutcome {
                succeeded: true,
                usage: Some(&usage(100, 100)),
                cost: None,
            },
        );
    }

    #[tokio::test]
    async fn concurrency_caps_answer_503_with_a_short_wait_and_free_on_settle() {
        let admission = PolicyAdmission::new(
            HashMap::new(),
            HashMap::new(),
            HashMap::from([(
                "p".to_owned(),
                (Some(2), HashMap::from([("m".to_owned(), 1)])),
            )]),
        );
        let held = admission
            .reserve(&claim("anonymous", "p", "m", 1, None))
            .await
            .expect("first slot");
        let refused = admission
            .reserve(&claim("anonymous", "p", "m", 1, None))
            .await
            .expect_err("model cap of 1");
        match &refused {
            EngineError::Overloaded { retry_after, .. } => {
                assert!(
                    retry_after.is_some_and(|wait| wait <= Duration::from_secs(5)),
                    "Retry-After must be short, got {retry_after:?}"
                );
            }
            other => panic!("expected 503 Overloaded, got {other:?}"),
        }
        assert_eq!(refused.status_code(), 503);
        // Another model on the same member still fits under total=2.
        let other = admission
            .reserve(&claim("anonymous", "p", "n", 1, None))
            .await
            .expect("second member slot");
        assert!(matches!(
            admission
                .reserve(&claim("anonymous", "p", "o", 1, None))
                .await,
            Err(EngineError::Overloaded { .. })
        ));
        admission.settle(
            &held,
            &AdmissionOutcome {
                succeeded: false,
                usage: None,
                cost: None,
            },
        );
        admission.settle(
            &other,
            &AdmissionOutcome {
                succeeded: true,
                usage: None,
                cost: None,
            },
        );
        admission
            .reserve(&claim("anonymous", "p", "m", 1, None))
            .await
            .expect("slots freed at settlement");
    }

    #[test]
    fn noop_detection_ignores_policies_that_restrict_nothing() {
        let empty = PolicyAdmission::new(
            HashMap::from([("key".to_owned(), KeyPolicy::default())]),
            HashMap::new(),
            HashMap::new(),
        );
        assert!(empty.is_noop());
        assert!(!admission(budgeted("1")).is_noop());
    }

    #[test]
    fn window_resets_are_bounded() {
        // 1_700_000_040 is a minute boundary; 30 seconds in, 30 remain.
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_070).expect("ts");
        assert_eq!(until_next_minute(now), Duration::from_secs(30));
        let day = until_next_day(now);
        assert!(day >= Duration::from_secs(1) && day <= Duration::from_secs(86_400));
    }
}
