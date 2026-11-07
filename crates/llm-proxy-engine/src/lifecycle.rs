//! The request lifecycle (plan 03's pipeline slots, plan 04's ladder),
//! transport- and HTTP-free: admission, routing, credentials, guards, cache,
//! the attempt loop against one upstream with retries and failover, and
//! settlement. This module was extracted from the axum layer so the server,
//! an SDK embedder and the bindings all run the same pipeline.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use llm_proxy_adapters::client_wire::ClientWireRequest;
use llm_proxy_adapters::request::ProxyRequest;
use llm_proxy_adapters::sse::SseFramer;
use llm_proxy_adapters::{ProtocolName, ProviderStreamDecoder};
use llm_proxy_core::{AffinityConflictPolicy, ModelPricing, ProviderRouteKind};
use llm_proxy_protocol::core::{
    CoreEvent, CoreRequest, CoreStreamErrorKind, Cost, ModelRef, StopReason, Usage, UsageProvenance,
};
use llm_proxy_storage::{
    AttemptOutcome, ProxyEvent, RequestReceived, ResponseCompleted, ResponseFailed,
};
use rust_decimal::Decimal;
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::classification::{ErrorClass, classify, next_wait};
use crate::error::{AuthOwner, EngineError, map_provider_error, sanitize_upstream_error_body};
use crate::headers::select_forwarded_headers;
use crate::traits::{
    AdmissionOutcome, AdmissionTicket, CacheEntry, CacheKey, CredentialFeedback, CredentialPick,
    CredentialQuery, GuardDecision, ResolvedTarget, RoutePlan, RouteQuery, UsageSink,
};
use crate::types::{ClientDialect, EngineRequest, EngineResponse, EngineStream};
use crate::{EngineInner, ProviderAdapterTarget};

/// Client-facing message when the upstream ends a stream without a terminal
/// event; deliberately generic (no upstream body) since it is sent in-band.
const TRUNCATED_STREAM_MESSAGE: &str = "upstream stream ended before the response completed";

/// Client-facing message for a stream that fails to decode after the first
/// byte was committed; generic because it is sent in-band.
const PROVIDER_DECODE_CLIENT_MESSAGE: &str = "provider response decode error";

/// The inbound header the audit's session-attribution evidence uses
/// (backlog 17): the session selector Claude Code sends.
const SESSION_HEADER: &str = "x-claude-code-session-id";

/// Provider hints that survive a failover strip: dialect-neutral options any
/// adapter may consume, unlike account-bound state (signed thinking, file
/// ids) which the strip removes.
const HINTS_KEPT_ACROSS_FAILOVER: &[&str] = &["stream_options"];

fn map_encode_error(e: llm_proxy_adapters::ProviderError) -> EngineError {
    match e {
        llm_proxy_adapters::ProviderError::InvalidRequest(message) => {
            EngineError::InvalidRequest(message)
        }
        other => EngineError::Internal(format!("encode error: {other}")),
    }
}

pub(crate) fn route_kind_str(kind: ProviderRouteKind) -> &'static str {
    match kind {
        ProviderRouteKind::ChatCompletions => "chat_completions",
        ProviderRouteKind::Messages => "messages",
    }
}

pub(crate) fn client_dialect_str(dialect: ClientDialect) -> &'static str {
    match dialect {
        ClientDialect::OpenAiChat => "openai_chat",
        ClientDialect::Anthropic => "anthropic",
    }
}

/// The session id attributed to a request (plan 04 step 10): the inbound
/// session header, else the request metadata's `user_id`.
pub(crate) fn session_id_of(req: &EngineRequest) -> Option<String> {
    req.inbound
        .headers
        .get(SESSION_HEADER)
        .cloned()
        .or_else(|| req.core.metadata.user_id.clone())
}

fn compute_cost(usage: &Usage, pricing: &ModelPricing) -> Cost {
    fn priced(count: i32, price: Decimal) -> Decimal {
        Decimal::from(count)
            .checked_mul(price)
            .unwrap_or(Decimal::MAX)
            .round_dp(6)
    }
    fn sum(a: Decimal, b: Decimal) -> Decimal {
        a.checked_add(b).unwrap_or(Decimal::MAX)
    }

    let input = priced(usage.input_tokens, pricing.input);
    let output = priced(usage.output_tokens, pricing.output);
    let cache_creation = priced(
        usage.cache_creation_input_tokens.unwrap_or(0),
        pricing.cache_creation,
    );
    let cache_read = priced(
        usage.cache_read_input_tokens.unwrap_or(0),
        pricing.cache_read,
    );
    let reasoning = priced(usage.reasoning_tokens.unwrap_or(0), pricing.reasoning);
    let total = [input, output, cache_creation, cache_read, reasoning]
        .into_iter()
        .reduce(sum)
        .unwrap_or(Decimal::ZERO);
    Cost {
        input,
        output,
        cache_creation,
        cache_read,
        reasoning,
        total,
    }
}

pub(crate) fn emit_request_received(inner: &EngineInner, req: &EngineRequest, streaming: bool) {
    inner
        .usage
        .record(&ProxyEvent::RequestReceived(RequestReceived {
            request_id: req.request_id.clone(),
            timestamp: time::OffsetDateTime::now_utc(),
            caller: req.caller.clone(),
            provider: req.provider_name.clone(),
            route_kind: route_kind_str(req.route_kind).to_owned(),
            client_protocol: client_dialect_str(req.client_dialect).to_owned(),
            model: req.core.model.clone(),
            streaming,
            body_hash: req.body_hash.clone(),
            session_id: session_id_of(req),
        }));
}

/// Emits a [`ProxyEvent::ResponseFailed`] row for `error`.
pub(crate) fn emit_response_failed(
    inner: &EngineInner,
    request_id: &str,
    provider: Option<&str>,
    model: Option<&ModelRef>,
    error: &EngineError,
    start: Instant,
) {
    let provider = provider.or(match error {
        EngineError::UnknownProvider(name) => Some(name.as_str()),
        _ => None,
    });
    if let EngineError::Upstream {
        status,
        body,
        auth_owner,
        retry_after: _,
    } = error
    {
        tracing::warn!(
            upstream_status = status,
            auth_owner = ?auth_owner,
            upstream_body = %body,
            "upstream error mapped for the client; real body logged here only"
        );
    }
    inner
        .usage
        .record(&ProxyEvent::ResponseFailed(ResponseFailed {
            request_id: request_id.to_owned(),
            timestamp: time::OffsetDateTime::now_utc(),
            provider: provider.map(str::to_owned),
            model: model.cloned(),
            error_kind: error.kind_name().to_owned(),
            message: error.client_message(),
            http_status: error.status_code(),
            latency_ms: start.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        }));
}

/// Emits one [`ProxyEvent::AttemptCompleted`] row (plan 04 step 1): the
/// ledger's per-attempt record, linked to the request.
/// One attempt row's payload: who sent it, what it encoded for, and how it
/// ended.
struct AttemptRecord<'a> {
    request_id: &'a str,
    seq: u32,
    route: &'a str,
    provider: &'a str,
    model: &'a str,
    credential_id: Option<&'a str>,
    outcome: AttemptOutcome,
    error: Option<&'a EngineError>,
    latency: Duration,
}

fn emit_attempt(inner: &EngineInner, record: &AttemptRecord<'_>) {
    inner.usage.record(&ProxyEvent::AttemptCompleted(
        llm_proxy_storage::AttemptCompleted {
            request_id: record.request_id.to_owned(),
            timestamp: time::OffsetDateTime::now_utc(),
            attempt: record.seq,
            route: record.route.to_owned(),
            provider: record.provider.to_owned(),
            model: record.model.to_owned(),
            credential_id: record.credential_id.map(str::to_owned),
            outcome: record.outcome,
            error_kind: record.error.map(|e| e.kind_name().to_owned()),
            http_status: record.error.map(|e| match e {
                EngineError::Upstream { status, .. } => *status,
                other => other.status_code(),
            }),
            message: record.error.map(|e| e.client_message()),
            latency_ms: record.latency.as_millis().try_into().unwrap_or(u64::MAX),
        },
    ));
}

/// One resolved attempt: the target plus the codec and credential that
/// serve it.
struct Attempt {
    target: ProviderAdapterTarget,
    adapter: Arc<dyn llm_proxy_adapters::ProviderAdapter>,
    /// Which pool credential was picked (`key-<i>`), when the member has
    /// more than one key.
    credential_id: Option<String>,
    /// Whether the member forwards the client's own token.
    passthrough: bool,
}

/// The member's credential pool: the base key plus any `api_keys` extras
/// (plan 04 step 5).
pub(crate) fn probe_credential_pool(inner: &EngineInner, provider: &str) -> Vec<SecretString> {
    credential_pool(inner, provider)
}

fn credential_pool(inner: &EngineInner, provider: &str) -> Vec<SecretString> {
    let Some(config) = inner.providers.get(provider) else {
        return Vec::new();
    };
    let mut keys = vec![config.api_key.clone()];
    if let Some(extra) = &config.api_keys {
        keys.extend(extra.iter().cloned());
    }
    keys
}

/// Resolves one plan candidate into a sendable attempt: adapter, endpoint,
/// and a credential picked from the member's pool.
async fn resolve_attempt(
    inner: &Arc<EngineInner>,
    candidate: &ResolvedTarget,
    avoid_credential: Option<&str>,
    inbound: Option<&SecretString>,
) -> Result<Attempt, EngineError> {
    let configured = &candidate.target;
    let adapter = inner.adapters.get(&configured.protocol).ok_or_else(|| {
        EngineError::Internal(format!(
            "no adapter registered for protocol: {}",
            configured.protocol
        ))
    })?;
    let passthrough = inner
        .providers
        .get(&configured.provider_name)
        .is_some_and(|p| p.passthrough_auth);
    let keys = credential_pool(inner, &configured.provider_name);
    let cooling = inner
        .health
        .cooling_credentials(&configured.provider_name, Instant::now());
    let pick: CredentialPick = inner
        .credentials
        .pick(&CredentialQuery {
            provider: &configured.provider_name,
            keys: &keys,
            passthrough,
            inbound,
            avoid: avoid_credential,
            cooling: &cooling,
        })
        .await?;

    let target = ProviderAdapterTarget {
        provider_name: configured.provider_name.clone(),
        adapter_name: configured.adapter_name.clone(),
        protocol: ProtocolName::new(&configured.protocol),
        endpoint: configured.endpoint.clone(),
        auth_style: configured.auth_style,
        api_key: pick.secret,
        requested_model: configured.requested_model.clone(),
        upstream_model: configured.upstream_model.clone(),
        headers: Arc::clone(&configured.headers),
        lossy: configured.lossy,
        forward_inbound_headers: Arc::clone(&configured.forward_inbound_headers),
    };
    Ok(Attempt {
        target,
        adapter,
        credential_id: pick.credential_id,
        passthrough,
    })
}

fn ledger_model(core_model: &ModelRef, upstream_model: &str) -> ModelRef {
    let mut model = core_model.clone();
    if model.upstream.is_none() && upstream_model != model.requested {
        model.upstream = Some(upstream_model.to_owned());
    }
    model
}

fn settle(
    inner: &EngineInner,
    ticket: &AdmissionTicket,
    succeeded: bool,
    usage: Option<&Usage>,
    cost: Option<&Cost>,
) {
    inner.admission.settle(
        ticket,
        &AdmissionOutcome {
            succeeded,
            usage,
            cost,
        },
    );
}

/// The cost of provider-reported usage at the member's pricing, when both
/// exist: what the provider bills, which is what budgets charge (plan 05
/// step 6). Synthetic or unknown usage is never priced.
fn billed_cost(usage: &Usage, pricing: Option<&ModelPricing>) -> Option<Cost> {
    if usage.provenance != UsageProvenance::ProviderReported {
        return None;
    }
    pricing.map(|p| compute_cost(usage, p))
}

/// Slots 6 and 8 for one request: the token estimate, the context-window
/// preflight (plan 05 step 8) and the admission reservation against the
/// plan's first candidate (plan 05 step 4). Cache hits never reach here, so
/// they are neither preflighted nor charged.
async fn admit(
    inner: &Arc<EngineInner>,
    req: &EngineRequest,
    plan: &RoutePlan,
) -> Result<AdmissionTicket, EngineError> {
    let first = plan.candidates.first();
    let member = first.map_or("", |candidate| candidate.target.provider_name.as_str());
    let model = first.map_or("", |candidate| candidate.target.upstream_model.as_str());
    let estimated_input_tokens = crate::preflight::estimate_input_tokens(&inner.counter, &req.core);
    let max_output_tokens = req
        .core
        .sampling
        .max_tokens
        .and_then(|n| u64::try_from(n).ok());
    let preflight_config = inner
        .config
        .load()
        .map(|config| config.server.preflight)
        .unwrap_or_default();
    let context_length = match inner.providers.get(member) {
        Some(provider) => inner.catalogs.context_length(provider, model).await,
        None => None,
    };
    crate::preflight::preflight(
        preflight_config,
        crate::preflight::PreflightInput {
            model,
            estimated_input_tokens,
            max_output_tokens,
            context_length,
        },
    )?;
    let claim = crate::traits::AdmissionClaim {
        caller: &req.caller,
        provider: &req.provider_name,
        member,
        model,
        requested_model: &req.core.model.requested,
        body_hash: &req.body_hash,
        estimated_input_tokens,
        max_output_tokens,
    };
    inner.admission.reserve(&claim).await
}

/// Strips provider-bound request state for a failover target (plan 04 step
/// 10, conflict 6): everything in `provider_hints` except the dialect-neutral
/// keys the ladder keeps. The client's own words never change.
fn strip_provider_bound_state(core: &CoreRequest) -> CoreRequest {
    let mut stripped = core.clone();
    stripped.provider_hints.raw = stripped
        .provider_hints
        .raw
        .iter()
        .filter(|(key, _)| {
            HINTS_KEPT_ACROSS_FAILOVER
                .iter()
                .any(|kept| key.eq_ignore_ascii_case(kept))
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    stripped
}

/// The per-request ladder state shared by both execution modes.
struct Ladder {
    plan: RoutePlan,
    session_id: Option<String>,
    deadline: Option<Instant>,
    /// 0-based attempt sequence across the whole request (attempt rows).
    seq: u32,
    /// The member the first attempt targeted; failover away from it strips
    /// provider-bound state.
    first_member: Option<String>,
    /// Candidate indexes in the order the ladder walks them.
    order: Vec<usize>,
    request_id: String,
    /// Credential id the current attempt used.
    current_credential: Option<String>,
}

impl Ladder {
    fn remaining(&self) -> Option<Duration> {
        self.deadline
            .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
    }

    fn deadline_passed(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn attempts_per_member(&self) -> u32 {
        self.plan.retry.max_attempts.max(1)
    }

    /// The upstream model a member encodes for.
    fn member_model(&self, member: &str) -> String {
        self.plan
            .candidates
            .iter()
            .find(|candidate| candidate.target.provider_name == member)
            .map(|candidate| candidate.target.upstream_model.clone())
            .unwrap_or_default()
    }

    fn is_last_member(&self, member: &str) -> bool {
        self.order
            .last()
            .and_then(|index| self.plan.candidates.get(*index))
            .is_some_and(|candidate| candidate.target.provider_name == member)
    }

    /// The core request to encode for this member: stripped when the ladder
    /// switched providers (plan 04 step 10).
    fn core_for(&self, req: &EngineRequest, member: &str) -> CoreRequest {
        match &self.first_member {
            Some(first) if first != member => strip_provider_bound_state(&req.core),
            _ => req.core.clone(),
        }
    }
}

/// The candidate order the ladder walks (plan 04 step 9): members past their
/// cooldown first in configured order; when every member is cooling, the
/// configured order stands — cooldowns reorder, they never refuse service.
fn ordered_candidates(inner: &EngineInner, plan: &RoutePlan, now: Instant) -> Vec<usize> {
    let available: Vec<usize> = plan
        .candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| {
            inner
                .health
                .provider_available(&candidate.target.provider_name, now)
        })
        .map(|(index, _)| index)
        .collect();
    if available.is_empty() {
        (0..plan.candidates.len()).collect()
    } else {
        available
    }
}

/// The one deadline across all attempts (plan 04 step 3): the tighter of the
/// route's ladder deadline and the server-wide upstream total, when set.
fn ladder_deadline(inner: &EngineInner, plan: &RoutePlan, start: Instant) -> Option<Instant> {
    let route_deadline =
        (!plan.retry.total_timeout.is_zero()).then(|| start + plan.retry.total_timeout);
    let config_deadline = inner
        .config
        .load()
        .ok()
        .and_then(|config| config.server.upstream_timeouts.total)
        .map(|total| start + total);
    match (route_deadline, config_deadline) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (some, None) | (None, some) => some,
    }
}

/// Whether the failure feedback marks the credential as rejected (its key
/// should cool down separately from the member).
fn credential_feedback(class: &ErrorClass, error: &EngineError) -> CredentialFeedback {
    match class {
        ErrorClass::Terminal => match error {
            EngineError::Upstream {
                status: 401 | 403, ..
            } => CredentialFeedback::Rejected,
            _ => CredentialFeedback::TransientFailure,
        },
        ErrorClass::Transient {
            retry_after: Some(_),
        } => CredentialFeedback::Rejected,
        ErrorClass::Transient { retry_after: None } => CredentialFeedback::TransientFailure,
    }
}

/// The ladder's view of one failure: classification, except that an auth
/// rejection (401/403) stays retryable while the member still has untried
/// keys in its credential pool (plan 04 step 5) — the next key may simply be
/// the working one.
fn class_for_ladder(inner: &EngineInner, member: &str, error: &EngineError) -> ErrorClass {
    match classify(error) {
        ErrorClass::Terminal => {
            let auth_rejected = matches!(
                error,
                EngineError::Upstream {
                    status: 401 | 403,
                    ..
                }
            );
            if auth_rejected && credential_pool(inner, member).len() > 1 {
                ErrorClass::Transient { retry_after: None }
            } else {
                ErrorClass::Terminal
            }
        }
        other => other,
    }
}

/// Updates passive health (plan 04 step 9): a throttle with real reset
/// advice cools until the reported reset; any other transient failure cools
/// for the ladder's configured cooldown. A failure that belongs to one
/// operator key cools that key, and the member only while it has no other
/// key to serve with; a failure deterministic for this request (a
/// client-side 4xx, the client's own passthrough token being rejected)
/// cools nothing — see [`member_cooldown_applies`].
fn apply_cooldowns(
    inner: &EngineInner,
    ladder: &Ladder,
    provider: &str,
    error: &EngineError,
    class: &ErrorClass,
    feedback: CredentialFeedback,
) {
    let now = Instant::now();
    let advised = match error {
        EngineError::Upstream { retry_after, .. } => *retry_after,
        _ => None,
    };
    let until = now + advised.unwrap_or(ladder.plan.retry.cooldown);
    let credential_id = ladder.current_credential.as_deref();
    if feedback == CredentialFeedback::Rejected
        && let Some(id) = credential_id
    {
        inner.health.cooldown_credential(provider, id, until);
    }
    if member_cooldown_applies(
        class,
        error,
        feedback,
        another_key_available(inner, provider, credential_id, now),
    ) {
        inner.health.cooldown_provider(provider, until);
    }
}

/// Whether a failed attempt should cool the member itself. An upstream
/// fault (5xx, timeout, connect, throttle without key attribution) says the
/// member is unhealthy; a rejected operator credential says only that key
/// is; a client fault says nothing about the member — one client's
/// malformed request must not demote the member for everyone else.
fn member_cooldown_applies(
    class: &ErrorClass,
    error: &EngineError,
    feedback: CredentialFeedback,
    another_key_available: bool,
) -> bool {
    if operator_credential_rejected(error, feedback) {
        return !another_key_available;
    }
    matches!(class, ErrorClass::Transient { .. })
}

/// A failure that belongs to the operator's credential rather than to the
/// member or the client: an auth rejection or an advised throttle on a key
/// the operator configured. Never a passthrough client token — rejecting
/// that is between the upstream and the client.
fn operator_credential_rejected(error: &EngineError, feedback: CredentialFeedback) -> bool {
    feedback == CredentialFeedback::Rejected
        && matches!(
            error,
            EngineError::Upstream {
                status: 401 | 403 | 429,
                auth_owner: AuthOwner::Operator,
                ..
            }
        )
}

/// Whether the member has a key other than the one this failure rejected
/// that is not itself cooling (so the member can still serve). A pick that
/// recorded no credential id had no pool to rotate — nothing else to try.
/// Reads the store after the rejected key's cooldown was written, so that
/// key counts as unavailable.
fn another_key_available(
    inner: &EngineInner,
    provider: &str,
    rejected_credential: Option<&str>,
    now: Instant,
) -> bool {
    if rejected_credential.is_none() {
        return false;
    }
    credential_pool(inner, provider).len() > inner.health.cooling_credentials(provider, now).len()
}

/// Whether the RetryExhausted wrap applies: only when the route had a real
/// ladder (failover members or more than one attempt per member). A bare
/// single-provider route surfaces the raw error exactly as before plan 04.
fn ladder_had_capacity(plan: &RoutePlan) -> bool {
    plan.candidates.len() > 1 || plan.retry.max_attempts > 1
}

/// Wraps the ladder's last error for the client (plan 04 step 12): the
/// proxy already did the retries, so the client must not stack its own.
fn exhausted(
    inner: &EngineInner,
    plan: &RoutePlan,
    attempts: u32,
    last: EngineError,
) -> EngineError {
    let retry_after = last
        .retry_after()
        .or_else(|| {
            inner.health.soonest_reset(
                plan.candidates
                    .iter()
                    .map(|c| c.target.provider_name.as_str()),
                Instant::now(),
            )
        })
        .or_else(|| (!plan.retry.cooldown.is_zero()).then_some(plan.retry.cooldown));
    EngineError::RetryExhausted {
        attempts,
        last: Box::new(last),
        retry_after,
    }
}

/// Records one failed attempt: the row, passive health, credential
/// feedback. `Ok(())` when the ladder may run another attempt; the final
/// error when the failure is terminal.
async fn record_attempt_failure(
    inner: &Arc<EngineInner>,
    ladder: &Ladder,
    member: &str,
    error: &EngineError,
    attempt_start: Instant,
) {
    tracing::debug!(
        route = %ladder.plan.route,
        member = %member,
        error = %error,
        "upstream attempt failed"
    );
    let class = classify(error);
    let feedback = credential_feedback(&class, error);
    emit_attempt(
        inner,
        &AttemptRecord {
            request_id: &ladder.request_id,
            seq: ladder.seq - 1,
            route: &ladder.plan.route,
            provider: member,
            model: &ladder.member_model(member),
            credential_id: ladder.current_credential.as_deref(),
            outcome: AttemptOutcome::Failed,
            error: Some(error),
            latency: attempt_start.elapsed(),
        },
    );
    apply_cooldowns(inner, ladder, member, error, &class, feedback);
    inner
        .credentials
        .feedback(member, ladder.current_credential.as_deref(), feedback);
}

/// Refuses the failover when the route's affinity policy says so (conflict
/// 6): a session pinned to one member may not land on another with state it
/// cannot honour — not after a failed first attempt, and not because the
/// pinned member was cooling down when the ladder ordered its candidates.
fn refuse_switch(inner: &EngineInner, ladder: &Ladder, member: &str) -> Result<(), EngineError> {
    if ladder.plan.affinity.on_conflict != AffinityConflictPolicy::Refuse {
        return Ok(());
    }
    let Some(session) = &ladder.session_id else {
        return Ok(());
    };
    let Some(pinned) = inner.affinity.pinned_provider(session) else {
        return Ok(());
    };
    if pinned != member {
        return Err(EngineError::AffinityRefused {
            pinned,
            session: session.clone(),
        });
    }
    Ok(())
}

/// Non-streaming execution.
pub(crate) async fn execute(
    inner: Arc<EngineInner>,
    req: EngineRequest,
) -> Result<EngineResponse, EngineError> {
    let start = req.start;
    crate::validate_provider_name(&req.provider_name)?;
    inner.metrics.record_request(false);

    emit_request_received(&inner, &req, false);

    let outcome = run_once(&inner, &req).await;
    match outcome {
        Ok(response) => Ok(response),
        Err(error) => {
            inner.metrics.record_failure();
            emit_response_failed(
                &inner,
                &req.request_id,
                Some(&req.provider_name),
                Some(&req.core.model),
                &error,
                start,
            );
            Err(error)
        }
    }
}

async fn resolve_plan(
    inner: &Arc<EngineInner>,
    req: &EngineRequest,
) -> Result<RoutePlan, EngineError> {
    inner
        .router
        .resolve(&RouteQuery {
            provider: &req.provider_name,
            route_kind: req.route_kind,
            requested_model: &req.core.model.requested,
            session_id: session_id_of(req).as_deref(),
        })
        .await
}

async fn run_once(
    inner: &Arc<EngineInner>,
    req: &EngineRequest,
) -> Result<EngineResponse, EngineError> {
    // Slot 3: resolve the route to a plan (one candidate, or a pool's).
    let plan = resolve_plan(inner, req).await?;
    let first_model = plan
        .candidates
        .first()
        .map(|candidate| candidate.target.upstream_model.clone())
        .unwrap_or_default();

    // Slot 5: cache lookup. Only complete non-streaming responses are served
    // from the cache in this plan.
    let cache_key = CacheKey {
        caller: &req.caller,
        provider: &req.provider_name,
        route_kind: req.route_kind,
        model: &first_model,
        body_hash: &req.body_hash,
        streaming: false,
    };
    if let Some(entry) = inner
        .cache
        .lookup(&cache_key)
        .await
        .map_err(|error| EngineError::Internal(format!("cache lookup: {error}")))?
        && entry.events.is_empty()
    {
        tracing::debug!(
            request_id = %req.request_id,
            route = %req.provider_name,
            "cache hit"
        );
        let mut cached = entry.response;
        let usage = cached.usage.clone();
        let stop_reason = cached.stop_reason.clone();
        let upstream_message_id = cached.id.clone();
        let cost = cached.cost.clone();
        let model = ledger_model(&req.core.model, &first_model);
        // A hit is recorded as a hit (plan 05 step 6): the row keeps the
        // cached usage for the record, but the request never reserved and
        // is never charged — it did not reach admission.
        inner
            .usage
            .record(&ProxyEvent::ResponseCompleted(ResponseCompleted {
                request_id: req.request_id.clone(),
                timestamp: time::OffsetDateTime::now_utc(),
                provider: req.provider_name.clone(),
                upstream_message_id,
                model: model.clone(),
                usage,
                cost,
                stop_reason,
                latency_ms: 0,
                credential_id: None,
                cache_hit: true,
            }));
        cached.model = model;
        return Ok(EngineResponse {
            core: cached,
            raw_body: None,
        });
    }

    // Slots 6 and 8: preflight, then one reservation for the whole ladder;
    // settlement happens exactly once at the end.
    let ticket = admit(inner, req, &plan).await?;

    match run_ladder_once(inner, req, plan, &ticket, &cache_key).await {
        Ok(response) => Ok(response),
        Err(error) => {
            settle(inner, &ticket, false, None, None);
            Err(error)
        }
    }
}

/// Slots 7-13 of the non-streaming lifecycle against a reserved ticket: the
/// attempt loop (plan 04 steps 1-5). The success path settles the ticket;
/// every failure propagates to [`run_once`], which settles it as failed.
#[expect(
    clippy::too_many_lines,
    reason = "one attempt loop: candidate iteration, credential rotation, \
              classification, cooldowns and the deadline read as a unit; \
              splitting the arms would scatter the ladder state they mutate"
)]
async fn run_ladder_once(
    inner: &Arc<EngineInner>,
    req: &EngineRequest,
    plan: RoutePlan,
    ticket: &AdmissionTicket,
    cache_key: &CacheKey<'_>,
) -> Result<EngineResponse, EngineError> {
    // Slot 7: pre-call guards, once per request — the request does not
    // change between attempts (only provider-bound hints do, which are not
    // the client's words).
    for guard in &inner.guards {
        match guard.inspect_request(&req.core).await? {
            GuardDecision::Allow => {}
            GuardDecision::Reject(message) => {
                return Err(EngineError::InvalidRequest(message));
            }
        }
    }

    let mut ladder = Ladder {
        session_id: session_id_of(req),
        deadline: ladder_deadline(inner, &plan, req.start),
        seq: 0,
        first_member: None,
        order: ordered_candidates(inner, &plan, Instant::now()),
        request_id: req.request_id.clone(),
        current_credential: None,
        plan,
    };
    let mut last_error: Option<EngineError> = None;
    let order = ladder.order.clone();

    'ladder: for &member_index in &order {
        let Some(candidate) = ladder.plan.candidates.get(member_index).cloned() else {
            return Err(EngineError::Internal(
                "candidate index out of range".to_owned(),
            ));
        };
        let member = candidate.target.provider_name.clone();
        refuse_switch(inner, &ladder, &member)?;
        if ladder.first_member.is_none() {
            ladder.first_member = Some(member.clone());
        }
        // Pool credential ids are positional (`key-<i>`): an id that failed
        // against the previous member names a different secret here, so the
        // avoid hint lives and dies with one member's attempts.
        let mut avoid_credential: Option<String> = None;

        for attempt_in_member in 0..ladder.attempts_per_member() {
            if ladder.deadline_passed() {
                last_error = Some(EngineError::UpstreamTimeout(
                    "retry ladder deadline exceeded before the next attempt".to_owned(),
                ));
                break 'ladder;
            }
            ladder.seq += 1;
            let attempt_start = Instant::now();

            let attempt = resolve_attempt(
                inner,
                &candidate,
                avoid_credential.as_deref(),
                req.inbound.auth.as_ref(),
            )
            .await?;
            ladder.current_credential = attempt.credential_id.clone();
            avoid_credential = attempt.credential_id.clone();

            let core_for_member = ladder.core_for(req, &member);
            let mut wire: ClientWireRequest = req.inbound.wire.clone();
            wire.forwarded_headers = select_forwarded_headers(
                &req.inbound.headers,
                &attempt.target.forward_inbound_headers,
            );
            let proxy_req: ProxyRequest = attempt
                .adapter
                .encode_request(&core_for_member, &attempt.target, &wire)
                .map_err(map_encode_error)?;

            let sent = inner.transport.client_for(&member).send(proxy_req).await;

            let response_bytes = match sent {
                Ok(bytes) => bytes,
                Err(error) => {
                    let engine_error = map_provider_error(&error, attempt.passthrough);
                    last_error = Some(engine_error.clone());
                    record_attempt_failure(inner, &ladder, &member, &engine_error, attempt_start)
                        .await;
                    if matches!(
                        class_for_ladder(inner, &member, &engine_error),
                        ErrorClass::Terminal
                    ) {
                        return Err(engine_error);
                    }
                    if attempt_in_member + 1 < ladder.attempts_per_member() {
                        let wait = next_wait(
                            engine_error.retry_after(),
                            ladder.plan.retry.backoff,
                            ladder.remaining(),
                        );
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    continue 'ladder;
                }
            };

            let decoded = attempt
                .adapter
                .decode_response(&response_bytes, &attempt.target);
            let mut core_resp = match decoded {
                Ok(response) => response,
                Err(error) => {
                    let engine_error =
                        EngineError::ProviderDecode(format!("decode response: {error}"));
                    last_error = Some(engine_error.clone());
                    record_attempt_failure(inner, &ladder, &member, &engine_error, attempt_start)
                        .await;
                    if attempt_in_member + 1 < ladder.attempts_per_member() {
                        tokio::time::sleep(ladder.plan.retry.backoff).await;
                        continue;
                    }
                    continue 'ladder;
                }
            };

            // Slot 11: post-call guards. A guard rejection is proxy policy,
            // not an upstream fault: no retry, no failover.
            for guard in &inner.guards {
                match guard.inspect_response(&core_resp).await? {
                    GuardDecision::Allow => {}
                    GuardDecision::Reject(message) => {
                        return Err(EngineError::InvalidRequest(message));
                    }
                }
            }

            let latency = req.start.elapsed();
            let pricing = inner
                .providers
                .pricing_for(
                    &attempt.target.provider_name,
                    &attempt.target.upstream_model,
                )
                .cloned();
            let cost = billed_cost(&core_resp.usage, pricing.as_ref());
            core_resp.cost = cost.clone();

            emit_attempt(
                inner,
                &AttemptRecord {
                    request_id: &req.request_id,
                    seq: ladder.seq - 1,
                    route: &ladder.plan.route,
                    provider: &member,
                    model: &attempt.target.upstream_model,
                    credential_id: attempt.credential_id.as_deref(),
                    outcome: AttemptOutcome::Succeeded,
                    error: None,
                    latency: attempt_start.elapsed(),
                },
            );

            // Slot 13: settle (charging what the provider billed), store,
            // record.
            settle(inner, ticket, true, Some(&core_resp.usage), cost.as_ref());
            let entry = CacheEntry {
                response: core_resp.clone(),
                events: Vec::new(),
            };
            inner.cache.store(cache_key, &entry).await;

            // Pinning is the pool's opt-in (`affinity.sticky`); a direct
            // route or a non-sticky pool leaves the session's pin alone.
            if ladder.plan.affinity.sticky
                && let Some(session) = &ladder.session_id
            {
                inner.affinity.pin(session, &member);
            }
            inner.credentials.feedback(
                &member,
                attempt.credential_id.as_deref(),
                CredentialFeedback::Succeeded,
            );

            let resp_usage = core_resp.usage.clone();
            let resp_model = core_resp.model.clone();
            let resp_upstream_id = core_resp.id.clone();
            let resp_stop_reason = core_resp.stop_reason.clone();
            let credential_id = attempt.credential_id.clone();
            inner.metrics.record_success(
                &attempt.target.provider_name,
                &attempt.target.upstream_model,
                latency,
            );
            inner
                .usage
                .record(&ProxyEvent::ResponseCompleted(ResponseCompleted {
                    request_id: req.request_id.clone(),
                    timestamp: time::OffsetDateTime::now_utc(),
                    provider: attempt.target.provider_name.clone(),
                    upstream_message_id: resp_upstream_id,
                    model: ledger_model(&resp_model, &attempt.target.upstream_model),
                    usage: resp_usage,
                    cost,
                    stop_reason: resp_stop_reason,
                    latency_ms: latency.as_millis().try_into().unwrap_or(u64::MAX),
                    credential_id,
                    cache_hit: false,
                }));

            // Same-format passthrough: the upstream response body reaches the
            // client verbatim; the decode above only read usage and the stop
            // reason for the ledger, it did not rewrite the body.
            let raw_body = if wire.same_format_as(&attempt.target.protocol) {
                Some(response_bytes)
            } else {
                None
            };

            return Ok(EngineResponse {
                core: core_resp,
                raw_body,
            });
        }
    }

    // Every candidate exhausted: wrap for the client when the route had a
    // ladder (plan 04 step 12).
    let last = last_error.unwrap_or_else(|| {
        EngineError::Internal("retry ladder ended without attempting".to_owned())
    });
    if ladder_had_capacity(&ladder.plan) {
        Err(exhausted(inner, &ladder.plan, ladder.seq, last))
    } else {
        Err(last)
    }
}

/// What the pre-commit window reports: the buffered prefix (possibly empty,
/// when the hold budget expired in silence) that crosses the commit
/// boundary, or the error that aborts before any byte is committed.
enum FirstByteResult {
    Head(Vec<CoreEvent>),
    PreStreamError(EngineError),
}

type ByteStream = std::pin::Pin<
    Box<dyn futures::Stream<Item = Result<Bytes, llm_proxy_adapters::ProviderError>> + Send>,
>;

/// Streaming execution. Returns once the stream is committed to the client —
/// the first core events decoded, or the hold budget expiring in silence
/// (plan 04 step 6) — and hands back the buffered prefix plus the
/// continuation stream. Settlement happens inside the stream task as it
/// completes or is cancelled.
pub(crate) async fn execute_stream(
    inner: Arc<EngineInner>,
    req: EngineRequest,
) -> Result<EngineStream, EngineError> {
    let start = req.start;
    crate::validate_provider_name(&req.provider_name)?;
    inner.metrics.record_request(true);

    emit_request_received(&inner, &req, true);

    // Slot 3: the route plan.
    let plan = resolve_plan(&inner, &req).await?;

    // Slot 7: pre-call guards, once per request.
    for guard in &inner.guards {
        match guard.inspect_request(&req.core).await? {
            GuardDecision::Allow => {}
            GuardDecision::Reject(message) => {
                let error = EngineError::InvalidRequest(message);
                emit_response_failed(
                    &inner,
                    &req.request_id,
                    Some(&req.provider_name),
                    Some(&req.core.model),
                    &error,
                    start,
                );
                return Err(error);
            }
        }
    }

    // Slots 6 and 8: preflight, then one reservation for the whole ladder.
    let ticket = match admit(&inner, &req, &plan).await {
        Ok(ticket) => ticket,
        Err(error) => {
            emit_response_failed(
                &inner,
                &req.request_id,
                Some(&req.provider_name),
                Some(&req.core.model),
                &error,
                start,
            );
            return Err(error);
        }
    };

    let request_id = req.request_id.clone();
    let route_name = req.provider_name.clone();
    let requested_model = req.core.model.clone();

    let (first_byte_tx, first_byte_rx) = tokio::sync::oneshot::channel::<FirstByteResult>();
    let failure_recorded = Arc::new(AtomicBool::new(false));
    let core_stream = build_core_event_stream(
        Arc::clone(&inner),
        req,
        plan,
        ticket,
        first_byte_tx,
        Arc::clone(&failure_recorded),
    );

    let first_head = first_byte_rx.await.map_err(|error| {
        let err = EngineError::Internal(format!(
            "stream task exited before first event ({error}: possible panic, cancellation, or empty stream)"
        ));
        if !failure_recorded.swap(true, Ordering::SeqCst) {
            inner.metrics.record_failure();
            emit_response_failed(
                &inner,
                &request_id,
                Some(route_name.as_str()),
                Some(&requested_model),
                &err,
                start,
            );
        }
        err
    })?;

    match first_head {
        FirstByteResult::PreStreamError(error) => {
            emit_response_failed(
                &inner,
                &request_id,
                Some(route_name.as_str()),
                Some(&requested_model),
                &error,
                start,
            );
            Err(error)
        }
        FirstByteResult::Head(head) => Ok(EngineStream {
            head,
            tail: core_stream,
        }),
    }
}

struct StreamContext {
    decoder: Box<dyn ProviderStreamDecoder + Send>,
    sse_framer: SseFramer,
    core: CoreRequest,
    request_id: String,
    provider_name: String,
    upstream_model: String,
    start: Instant,
    pricing: Option<ModelPricing>,
    passthrough_auth: bool,
    pending_usage: Option<Usage>,
    stop_reason: Option<StopReason>,
    upstream_message_id: Option<String>,
    ticket: AdmissionTicket,
    credential_id: Option<String>,
}

impl StreamContext {
    #[expect(
        clippy::too_many_arguments,
        reason = "the context is the one place the committed stream's state lives; a builder would just re-list these"
    )]
    fn new(
        decoder: Box<dyn ProviderStreamDecoder + Send>,
        core: CoreRequest,
        request_id: String,
        provider_name: String,
        upstream_model: String,
        start: Instant,
        pricing: Option<ModelPricing>,
        passthrough_auth: bool,
        ticket: AdmissionTicket,
        credential_id: Option<String>,
    ) -> Self {
        Self {
            decoder,
            sse_framer: SseFramer::new(),
            core,
            request_id,
            provider_name,
            upstream_model,
            start,
            pricing,
            passthrough_auth,
            pending_usage: None,
            stop_reason: None,
            upstream_message_id: None,
            ticket,
            credential_id,
        }
    }

    /// The model ref recorded on ledger rows: the requested model plus the
    /// upstream model actually serving it, so alias rows stay attributable.
    fn ledger_model(&self) -> ModelRef {
        ledger_model(&self.core.model, &self.upstream_model)
    }

    /// Settles this stream's reservation with whatever usage the provider
    /// reported so far and its billed cost (plan 05 step 6): a stream that
    /// ends early still charges the tokens it consumed; one that reported
    /// nothing charges nothing.
    fn settle(&self, inner: &EngineInner, succeeded: bool) {
        let cost = self
            .pending_usage
            .as_ref()
            .and_then(|usage| billed_cost(usage, self.pricing.as_ref()));
        settle(
            inner,
            &self.ticket,
            succeeded,
            self.pending_usage.as_ref(),
            cost.as_ref(),
        );
    }
}

/// Records one failed attempt inline (encode failures never reach
/// `record_attempt_failure`, which needs the full ladder).
#[expect(
    clippy::too_many_arguments,
    reason = "one flat record call; grouping these into a struct would duplicate AttemptRecord"
)]
fn record_failed_attempt(
    inner: &EngineInner,
    request_id: &str,
    seq: u32,
    route: &str,
    provider: &str,
    model: &str,
    credential_id: Option<&str>,
    error: &EngineError,
    latency: Duration,
) {
    emit_attempt(
        inner,
        &AttemptRecord {
            request_id,
            seq,
            route,
            provider,
            model,
            credential_id,
            outcome: AttemptOutcome::Failed,
            error: Some(error),
            latency,
        },
    );
}

fn capture_lifecycle(ctx: &mut StreamContext, event: &CoreEvent) {
    match event {
        CoreEvent::MessageStart { id: Some(id), .. } => {
            ctx.upstream_message_id = Some(id.clone());
        }
        CoreEvent::UsageDelta { usage } => {
            ctx.pending_usage = Some(usage.clone());
        }
        CoreEvent::MessageStop { stop_reason, .. } => {
            ctx.stop_reason = Some(stop_reason.clone());
        }
        _ => {}
    }
}

/// Yields the in-band `CoreEvent::Error` for a post-commit failure; the
/// client codec renders it (an OpenAI client gets an error chunk, an
/// Anthropic client an `error` event) and closes cleanly.
async fn emit_in_band_error(tx: &tokio::sync::mpsc::Sender<CoreEvent>, message: &str) -> bool {
    let error = llm_proxy_protocol::core::CoreStreamError::new(
        CoreStreamErrorKind::Upstream,
        message.to_owned(),
    );
    tx.send(CoreEvent::Error { error }).await.is_ok()
}

fn record_stream_success(inner: &EngineInner, ctx: &StreamContext) {
    inner
        .metrics
        .record_success(&ctx.provider_name, &ctx.upstream_model, ctx.start.elapsed());
    let usage = ctx
        .pending_usage
        .clone()
        .unwrap_or_else(Usage::synthetic_zero);
    let cost = billed_cost(&usage, ctx.pricing.as_ref());
    inner
        .usage
        .record(&ProxyEvent::ResponseCompleted(ResponseCompleted {
            request_id: ctx.request_id.clone(),
            timestamp: time::OffsetDateTime::now_utc(),
            provider: ctx.provider_name.clone(),
            upstream_message_id: ctx.upstream_message_id.clone(),
            model: ctx.ledger_model(),
            usage,
            cost,
            stop_reason: ctx.stop_reason.clone().unwrap_or(StopReason::EndTurn),
            latency_ms: ctx
                .start
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            credential_id: ctx.credential_id.clone(),
            cache_hit: false,
        }));
}

fn record_client_cancelled(inner: &EngineInner, ctx: &StreamContext) {
    inner
        .usage
        .record(&ProxyEvent::ResponseFailed(ResponseFailed {
            request_id: ctx.request_id.clone(),
            timestamp: time::OffsetDateTime::now_utc(),
            provider: Some(ctx.provider_name.clone()),
            model: Some(ctx.ledger_model()),
            error_kind: "ClientCancelled".to_owned(),
            message: "client disconnected before the stream completed".to_owned(),
            http_status: 200,
            latency_ms: ctx
                .start
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        }));
}

fn record_client_cancelled_no_ctx(inner: &EngineInner, request_id: &str) {
    inner.metrics.record_client_cancel();
    inner
        .usage
        .record(&ProxyEvent::ResponseFailed(ResponseFailed {
            request_id: request_id.to_owned(),
            timestamp: time::OffsetDateTime::now_utc(),
            provider: None,
            model: None,
            error_kind: "ClientCancelled".to_owned(),
            message: "client disconnected before the stream completed".to_owned(),
            http_status: 200,
            latency_ms: 0,
        }));
}

/// The sanitized message of a `CoreEvent::Error`, for ledger rows.
fn core_event_message(event: &CoreEvent) -> &str {
    match event {
        CoreEvent::Error { error } => error.message(),
        _ => "",
    }
}

/// Captures lifecycle state for and forwards the final events; `false` when
/// the client went away.
///
/// Holds `&mut StreamContext` across the awaits (the send backs up on a slow
/// client); `&mut` of a `Send` struct is `Send`, unlike `&`.
async fn emit_final_events(
    inner: &Arc<EngineInner>,
    ctx: &mut StreamContext,
    tx: &tokio::sync::mpsc::Sender<CoreEvent>,
    events: Vec<CoreEvent>,
) -> bool {
    for core_event in events {
        capture_lifecycle(ctx, &core_event);
        if tx.send(core_event).await.is_err() {
            record_client_cancelled(inner, ctx);
            ctx.settle(inner, false);
            return false;
        }
    }
    true
}

/// Spawns the supervised streaming task and returns its event stream: the
/// task runs the ladder (attempts, the hold-budget buffer, failover) and the
/// committed stream to its end.
fn build_core_event_stream(
    inner: Arc<EngineInner>,
    req: EngineRequest,
    plan: RoutePlan,
    ticket: AdmissionTicket,
    first_byte_tx: tokio::sync::oneshot::Sender<FirstByteResult>,
    failure_recorded: Arc<AtomicBool>,
) -> BoxStream<'static, CoreEvent> {
    let (tx, rx) = tokio::sync::mpsc::channel::<CoreEvent>(256);
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let supervise_request_id = req.request_id.clone();
    let supervise_route = req.provider_name.clone();
    let supervise_model = req.core.model.clone();
    let supervise_start = req.start;
    // The task settles its reservation on every path it completes; the
    // supervisor keeps its own handle for the paths it does not.
    let supervise_ticket = ticket.clone();
    let stream_span = tracing::info_span!("stream_task", request_id = %supervise_request_id);

    let task_inner = Arc::clone(&inner);
    let stream_task = tokio::spawn(
        async move {
            let inner = task_inner;
            let mut stream = StreamLadder {
                ladder: Ladder {
                    session_id: session_id_of(&req),
                    deadline: ladder_deadline(&inner, &plan, req.start),
                    seq: 0,
                    first_member: None,
                    order: ordered_candidates(&inner, &plan, Instant::now()),
                    request_id: req.request_id.clone(),
                    current_credential: None,
                    plan,
                },
                ticket,
                first_byte_tx: Some(first_byte_tx),
                last_error: None,
            };
            let order = stream.ladder.order.clone();
            let io = StreamIo {
                tx: &tx,
                cancel: &cancel_clone,
            };

            for &member_index in &order {
                if !stream_attempt(&inner, &mut stream, &req, member_index, &io).await {
                    return;
                }
            }

            // Every candidate exhausted before commit.
            let last = stream.last_error.clone().unwrap_or_else(|| {
                EngineError::Internal("retry ladder ended without attempting".to_owned())
            });
            stop_with(&inner, &mut stream, last, true);
        }
        .instrument(stream_span),
    );

    tokio::spawn(async move {
        match stream_task.await {
            Ok(()) => {}
            Err(join_error) => {
                let panicked = join_error.is_panic();
                let error = EngineError::Internal(format!(
                    "stream task {}",
                    if panicked {
                        "panicked"
                    } else {
                        "was cancelled"
                    }
                ));
                tracing::warn!(
                    request_id = %supervise_request_id,
                    error = %join_error,
                    panicked,
                    "detached stream task did not complete cleanly"
                );
                if !failure_recorded.swap(true, Ordering::SeqCst) {
                    inner.metrics.record_failure();
                    emit_response_failed(
                        &inner,
                        &supervise_request_id,
                        Some(supervise_route.as_str()),
                        Some(&supervise_model),
                        &error,
                        supervise_start,
                    );
                }
                // The row above is deduplicated with the caller's first-byte
                // error path; the reservation is not — only the task settles
                // it, and it never reached that point.
                settle(&inner, &supervise_ticket, false, None, None);
            }
        }
    });
    let rx_stream = futures::stream::unfold(rx, |mut rx| async move {
        let received = rx.recv().await;
        received.map(|event| (event, rx))
    });
    let cancel_guard = cancel.drop_guard();
    rx_stream
        .map(move |item| {
            let _guard = &cancel_guard;
            item
        })
        .boxed()
}

/// Commits the buffered prefix to the caller and streams the response to
/// its end: streaming guards first (a rejection still maps to an HTTP error
/// and never fails over — proxy policy, not an upstream fault), then the
/// first-byte handoff, then the post-commit stream where errors are in-band
/// and a terminal row and settlement are guaranteed (plan 04 steps 6-7).
#[expect(
    clippy::too_many_arguments,
    reason = "the commit boundary carries the ladder, the committed stream and the IO handles together"
)]
async fn commit_and_stream(
    inner: &Arc<EngineInner>,
    stream: &mut StreamLadder,
    req: &EngineRequest,
    mut committed: CommittedStream,
    attempt_start: Instant,
    tx: &tokio::sync::mpsc::Sender<CoreEvent>,
    cancel: &CancellationToken,
) {
    let ladder = &mut stream.ladder;
    let member = committed.ctx.provider_name.clone();
    let head = std::mem::take(&mut committed.head);
    if let Some(error) = vet_prefix(inner, &head).await {
        inner.metrics.record_failure();
        signal_pre_stream_error(inner, stream, error);
        committed.ctx.settle(inner, false);
        return;
    }
    for core_event in head.iter() {
        capture_lifecycle(&mut committed.ctx, core_event);
    }
    // Same opt-in as the non-streaming path: only sticky pools pin.
    if ladder.plan.affinity.sticky
        && let Some(session) = &ladder.session_id
    {
        inner.affinity.pin(session, &member);
    }
    emit_attempt(
        inner,
        &AttemptRecord {
            request_id: &req.request_id,
            seq: ladder.seq - 1,
            route: &ladder.plan.route,
            provider: &member,
            model: &committed.ctx.upstream_model,
            credential_id: ladder.current_credential.as_deref(),
            outcome: AttemptOutcome::Streaming,
            error: None,
            latency: attempt_start.elapsed(),
        },
    );
    let signalled = stream
        .first_byte_tx
        .take()
        .map(|tx| tx.send(FirstByteResult::Head(head)))
        .is_some_and(|result| result.is_ok());
    if !signalled {
        // The caller is gone, so nobody else will settle the reservation;
        // write the row here.
        inner.metrics.record_client_cancel();
        record_client_cancelled(inner, &committed.ctx);
        committed.ctx.settle(inner, false);
        return;
    }
    stream_committed(inner, &mut committed.ctx, tx, committed.stream, cancel).await;
}

/// The task's IO handles: the client event channel and the cancellation
/// token.
struct StreamIo<'a> {
    tx: &'a tokio::sync::mpsc::Sender<CoreEvent>,
    cancel: &'a CancellationToken,
}

/// Runs the attempts against one plan candidate: encode, send, buffer to
/// the pre-commit window, and either hand the committed stream to
/// [`commit_and_stream`] or move the ladder on. Returns `false` when the
/// task must return (the failure was signalled and the ticket settled).
#[expect(
    clippy::too_many_lines,
    reason = "one candidate's attempt ladder: deadline, credential, encode, send and the buffer handoff read as a unit"
)]
async fn stream_attempt(
    inner: &Arc<EngineInner>,
    stream: &mut StreamLadder,
    req: &EngineRequest,
    member_index: usize,
    io: &StreamIo<'_>,
) -> bool {
    let hold = stream.ladder.plan.retry.hold;
    let Some(candidate) = stream.ladder.plan.candidates.get(member_index).cloned() else {
        stop_with(
            inner,
            stream,
            EngineError::Internal("candidate index out of range".to_owned()),
            false,
        );
        return false;
    };
    let member = candidate.target.provider_name.clone();
    if let Err(error) = refuse_switch(inner, &stream.ladder, &member) {
        stop_with(inner, stream, error, false);
        return false;
    }
    if stream.ladder.first_member.is_none() {
        stream.ladder.first_member = Some(member.clone());
    }
    // Same boundary rule as the non-streaming ladder: `current_credential`
    // doubles as the avoid hint, and a positional id from another member
    // must not skip this member's key at the same index. It is re-set right
    // after each pick, so record rows for this member's attempts are
    // unaffected.
    stream.ladder.current_credential = None;

    for attempt_in_member in 0..stream.ladder.attempts_per_member() {
        if stream.ladder.deadline_passed() {
            let timeout = EngineError::UpstreamTimeout(
                "retry ladder deadline exceeded before the next attempt".to_owned(),
            );
            stream.last_error = Some(timeout.clone());
            stop_with(inner, stream, timeout, true);
            return false;
        }
        stream.ladder.seq += 1;
        let attempt_start = Instant::now();

        let attempt = match resolve_attempt(
            inner,
            &candidate,
            stream.ladder.current_credential.as_deref(),
            req.inbound.auth.as_ref(),
        )
        .await
        {
            Ok(attempt) => attempt,
            Err(error) => {
                stop_with(inner, stream, error, false);
                return false;
            }
        };
        stream.ladder.current_credential = attempt.credential_id.clone();

        let core_for_member = stream.ladder.core_for(req, &member);
        let mut wire: ClientWireRequest = req.inbound.wire.clone();
        wire.forwarded_headers = select_forwarded_headers(
            &req.inbound.headers,
            &attempt.target.forward_inbound_headers,
        );
        let proxy_req =
            match attempt
                .adapter
                .encode_request(&core_for_member, &attempt.target, &wire)
            {
                Ok(proxy_req) => proxy_req,
                Err(error) => {
                    let engine_error = map_encode_error(error);
                    record_failed_attempt(
                        inner,
                        &stream.ladder.request_id,
                        stream.ladder.seq - 1,
                        &stream.ladder.plan.route,
                        &member,
                        &stream.ladder.member_model(&member),
                        stream.ladder.current_credential.as_deref(),
                        &engine_error,
                        attempt_start.elapsed(),
                    );
                    stop_with(inner, stream, engine_error, false);
                    return false;
                }
            };

        let sent = inner
            .transport
            .client_for(&member)
            .send_stream(proxy_req)
            .await;
        let byte_stream = match sent {
            Ok(byte_stream) => byte_stream,
            Err(error) => {
                let engine_error = map_provider_error(&error, attempt.passthrough);
                if !pre_commit_failure(
                    inner,
                    stream,
                    &member,
                    &engine_error,
                    attempt_start,
                    attempt_in_member,
                )
                .await
                {
                    return false;
                }
                continue;
            }
        };

        match buffer_pre_commit(
            inner,
            stream,
            req,
            &member,
            &attempt,
            byte_stream,
            io.cancel,
            hold,
            attempt_start,
            attempt_in_member,
        )
        .await
        {
            PreCommit::Retry => continue,
            PreCommit::Stop => return false,
            PreCommit::Committed(committed) => {
                commit_and_stream(
                    inner,
                    stream,
                    req,
                    *committed,
                    attempt_start,
                    io.tx,
                    io.cancel,
                )
                .await;
                return false;
            }
        }
    }
    true
}

/// The streaming ladder's task-local state: the shared ladder plus the
/// channel handles only the stream task owns.
struct StreamLadder {
    ladder: Ladder,
    ticket: AdmissionTicket,
    first_byte_tx: Option<tokio::sync::oneshot::Sender<FirstByteResult>>,
    last_error: Option<EngineError>,
}

/// A committed stream handed from the buffer phase to the commit boundary.
struct CommittedStream {
    /// The per-attempt stream state.
    ctx: StreamContext,
    /// The upstream byte stream, positioned after the buffered prefix.
    stream: ByteStream,
    /// The buffered prefix (empty when the hold budget expired first).
    head: Vec<CoreEvent>,
}

/// The buffered pre-commit phase's outcome.
enum PreCommit {
    /// The stream is committed: the (possibly empty) head crosses the
    /// first-byte window and the caller owns the post-commit streaming.
    Committed(Box<CommittedStream>),
    /// The attempt failed pre-commit and the ladder continues.
    Retry,
    /// The task must return: cancelled, terminal or exhausted — the caller
    /// was signalled and the ticket settled inside.
    Stop,
}

/// Sleeps until the hold deadline, or forever when there is none.
async fn hold_expired(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

/// Frames and decodes one upstream chunk into `pending`; `Err` carries the
/// framing/decode failure for the ladder to classify.
fn decode_chunk(
    ctx: &mut StreamContext,
    bytes: &Bytes,
    pending: &mut Vec<CoreEvent>,
) -> Result<(), EngineError> {
    let frames = ctx
        .sse_framer
        .push_chunk(bytes)
        .map_err(|e| EngineError::ProviderDecode(format!("stream framing error: {e}")))?;
    for frame in &frames {
        let events = ctx
            .decoder
            .decode_frame(frame)
            .map_err(|e| EngineError::ProviderDecode(format!("provider decode error: {e}")))?;
        pending.extend(events);
    }
    Ok(())
}

/// A leading in-band error event (backlog 2): the upstream answered 200 and
/// rejected the stream in its first frame. It never crossed as output, so
/// the ladder may still recover.
fn leading_stream_error(pending: &[CoreEvent]) -> Option<EngineError> {
    pending
        .iter()
        .find(|event| matches!(event, CoreEvent::Error { .. }))
        .map(|event| {
            EngineError::ProviderDecode(sanitize_upstream_error_body(core_event_message(event)))
        })
}

/// Signals a terminal pre-commit failure to the caller (which writes the
/// request's failure row); when the caller is already gone, records the
/// cancellation here.
fn signal_pre_stream_error(inner: &EngineInner, stream: &mut StreamLadder, error: EngineError) {
    match stream.first_byte_tx.take() {
        Some(tx) => {
            if tx.send(FirstByteResult::PreStreamError(error)).is_err() {
                record_client_cancelled_no_ctx(inner, &stream.ladder.request_id);
            }
        }
        None => record_client_cancelled_no_ctx(inner, &stream.ladder.request_id),
    }
}

/// Stops the task with `error`: signal the caller, settle the reservation.
/// `wrap` decides the RetryExhausted wrap (plan 04 step 12).
fn stop_with(inner: &Arc<EngineInner>, stream: &mut StreamLadder, error: EngineError, wrap: bool) {
    let final_error = if wrap && ladder_had_capacity(&stream.ladder.plan) {
        exhausted(inner, &stream.ladder.plan, stream.ladder.seq, error)
    } else {
        error
    };
    inner.metrics.record_failure();
    signal_pre_stream_error(inner, stream, final_error);
    settle(inner, &stream.ticket, false, None, None);
}

/// Slot 11 for streams: the guards vet the buffered prefix; the first
/// rejection wins.
async fn vet_prefix(inner: &Arc<EngineInner>, pending: &[CoreEvent]) -> Option<EngineError> {
    for core_event in pending {
        for guard in &inner.guards {
            match guard.inspect_event(core_event).await {
                Ok(GuardDecision::Allow) => {}
                Ok(GuardDecision::Reject(message)) => {
                    return Some(EngineError::InvalidRequest(message));
                }
                Err(error) => return Some(error),
            }
        }
    }
    None
}

/// The pre-commit buffer phase (plan 04 step 6): frame and decode the
/// upstream stream until the first events, an upstream failure, or the hold
/// deadline in silence — then commit; or hand a pre-commit failure to the
/// ladder.
#[expect(
    clippy::too_many_arguments,
    reason = "the buffer phase is one unit: attempt, ladder, ticket, request and timing state all read together"
)]
async fn buffer_pre_commit(
    inner: &Arc<EngineInner>,
    stream_ladder: &mut StreamLadder,
    req: &EngineRequest,
    member: &str,
    attempt: &Attempt,
    mut stream: ByteStream,
    cancel: &CancellationToken,
    hold: Duration,
    attempt_start: Instant,
    attempt_in_member: u32,
) -> PreCommit {
    let ladder = &mut stream_ladder.ladder;
    let mut ctx = StreamContext::new(
        attempt.adapter.new_stream_decoder(&attempt.target),
        req.core.clone(),
        req.request_id.clone(),
        member.to_owned(),
        ladder.member_model(member),
        req.start,
        inner
            .providers
            .pricing_for(member, &ladder.member_model(member))
            .cloned(),
        attempt.passthrough,
        stream_ladder.ticket.clone(),
        ladder.current_credential.clone(),
    );
    let hold_deadline =
        (!hold.is_zero()).then(|| tokio::time::Instant::from_std(Instant::now() + hold));
    let mut pending: Vec<CoreEvent> = Vec::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                inner.metrics.record_client_cancel();
                let signalled = stream_ladder
                    .first_byte_tx
                    .take()
                    .map(|tx| {
                        tx.send(FirstByteResult::PreStreamError(EngineError::Internal(
                            "client disconnected before first byte".to_owned(),
                        )))
                    })
                    .is_some_and(|result| result.is_ok());
                if !signalled {
                    record_client_cancelled(inner, &ctx);
                }
                ctx.settle(inner, false);
                return PreCommit::Stop;
            }
            _ = hold_expired(hold_deadline), if pending.is_empty() => {
                // Hold budget expired in silence: commit with an empty
                // head. The client gets HTTP 200 plus heartbeats while the
                // upstream stays silent; errors from here on are in-band.
                return PreCommit::Committed(Box::new(CommittedStream { ctx, stream, head: pending }));
            }
            chunk = stream.next() => match chunk {
                Some(Ok(bytes)) => {
                    if decode_chunk(&mut ctx, &bytes, &mut pending).is_err() {
                        // Framing/decode garbage: the ladder classifies it.
                        let error = EngineError::ProviderDecode(
                            "provider decode error before the first event".to_owned(),
                        );
                        return failure_or_retry(inner, stream_ladder, member, &error, attempt_start, attempt_in_member).await;
                    }
                    if pending.is_empty() {
                        continue;
                    }
                    if let Some(error) = leading_stream_error(&pending) {
                        return failure_or_retry(inner, stream_ladder, member, &error, attempt_start, attempt_in_member).await;
                    }
                    return PreCommit::Committed(Box::new(CommittedStream { ctx, stream, head: pending }));
                }
                Some(Err(e)) => {
                    let error = map_provider_error(&e, ctx.passthrough_auth);
                    return failure_or_retry(inner, stream_ladder, member, &error, attempt_start, attempt_in_member).await;
                }
                None => {
                    // The upstream ended with no events at all: an empty
                    // 200 is not a response; treat it as a transient
                    // failure.
                    let error = EngineError::ProviderDecode(
                        "upstream returned an empty stream with no events".to_owned(),
                    );
                    return failure_or_retry(inner, stream_ladder, member, &error, attempt_start, attempt_in_member).await;
                }
            }
        }
    }
}

/// One pre-commit failure funnel: record it, decide the next move, and map
/// the decision onto [`PreCommit`].
async fn failure_or_retry(
    inner: &Arc<EngineInner>,
    stream_ladder: &mut StreamLadder,
    member: &str,
    error: &EngineError,
    attempt_start: Instant,
    attempt_in_member: u32,
) -> PreCommit {
    if pre_commit_failure(
        inner,
        stream_ladder,
        member,
        error,
        attempt_start,
        attempt_in_member,
    )
    .await
    {
        PreCommit::Retry
    } else {
        PreCommit::Stop
    }
}

/// Handles one pre-commit streaming failure inside the buffer phase:
/// record it (row, cooldowns, feedback) and decide the next move. Returns
/// `true` when the ladder continues with another attempt (the caller breaks
/// the buffer loop and retries), `false` when the task must return — the
/// terminal and exhausted cases signal the caller and settle here.
async fn pre_commit_failure(
    inner: &Arc<EngineInner>,
    stream: &mut StreamLadder,
    member: &str,
    error: &EngineError,
    attempt_start: Instant,
    attempt_in_member: u32,
) -> bool {
    stream.last_error = Some(error.clone());
    let ladder = &mut stream.ladder;
    record_attempt_failure(inner, ladder, member, error, attempt_start).await;
    let class = class_for_ladder(inner, member, error);
    let is_last_member_attempt = attempt_in_member + 1 >= ladder.attempts_per_member();
    match class {
        ErrorClass::Terminal => {
            let final_error = error.clone();
            inner.metrics.record_failure();
            signal_pre_stream_error(inner, stream, final_error);
            settle(inner, &stream.ticket, false, None, None);
            false
        }
        ErrorClass::Transient { retry_after } => {
            if is_last_member_attempt && ladder.is_last_member(member) {
                // Ladder exhausted (plan 04 step 12): wrap for the client.
                let final_error = exhausted(inner, &ladder.plan, ladder.seq, error.clone());
                inner.metrics.record_failure();
                signal_pre_stream_error(inner, stream, final_error);
                settle(inner, &stream.ticket, false, None, None);
                false
            } else {
                if !is_last_member_attempt {
                    // Another attempt against the same member: honour the
                    // advised wait, clamped to the deadline.
                    let wait =
                        next_wait(retry_after, ladder.plan.retry.backoff, ladder.remaining());
                    tokio::time::sleep(wait).await;
                }
                true
            }
        }
    }
}

/// Drives the committed stream to its end: decode and forward events, mark
/// in-band failures, finalize with the decoder's terminal, settle, and
/// record the terminal row.
#[expect(
    clippy::too_many_lines,
    reason = "one committed-stream state machine: decode, forward, in-band failure, terminal flush and settlement read as a unit"
)]
async fn stream_committed(
    inner: &Arc<EngineInner>,
    ctx: &mut StreamContext,
    tx: &tokio::sync::mpsc::Sender<CoreEvent>,
    mut stream: ByteStream,
    cancel_clone: &CancellationToken,
) {
    let mut stream_failed = false;
    'outer: loop {
        tokio::select! {
            _ = cancel_clone.cancelled() => {
                inner.metrics.record_client_cancel();
                record_client_cancelled(inner, ctx);
                ctx.settle(inner, false);
                return;
            }
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        let frames = match ctx.sse_framer.push_chunk(&bytes) {
                            Ok(frames) => frames,
                            Err(e) => {
                                let error = EngineError::ProviderDecode(format!(
                                    "stream framing error: {e}"
                                ));
                                inner.metrics.record_failure();
                                if !emit_in_band_error(tx, PROVIDER_DECODE_CLIENT_MESSAGE).await {
                                    record_client_cancelled(inner, ctx);
                                }
                                emit_response_failed(
                                    inner,
                                    &ctx.request_id,
                                    Some(ctx.provider_name.as_str()),
                                    Some(&ctx.ledger_model()),
                                    &error,
                                    ctx.start,
                                );
                                stream_failed = true;
                                break 'outer;
                            }
                        };
                        for frame in &frames {
                            let core_events = match ctx.decoder.decode_frame(frame) {
                                Ok(events) => events,
                                Err(e) => {
                                    let error = EngineError::ProviderDecode(format!(
                                        "provider decode error: {e}"
                                    ));
                                    inner.metrics.record_failure();
                                    if !emit_in_band_error(tx, PROVIDER_DECODE_CLIENT_MESSAGE).await {
                                        record_client_cancelled(inner, ctx);
                                    }
                                    emit_response_failed(
                                        inner,
                                        &ctx.request_id,
                                        Some(ctx.provider_name.as_str()),
                                        Some(&ctx.ledger_model()),
                                        &error,
                                        ctx.start,
                                    );
                                    stream_failed = true;
                                    break 'outer;
                                }
                            };
                            for core_event in core_events {
                                capture_lifecycle(ctx, &core_event);
                                if matches!(core_event, CoreEvent::Error { .. }) {
                                    // In-band upstream failure after visible
                                    // output: mark it for the client and the
                                    // ledger, then keep the framing valid.
                                    stream_failed = true;
                                    inner.metrics.record_failure();
                                    emit_response_failed(
                                        inner,
                                        &ctx.request_id,
                                        Some(ctx.provider_name.as_str()),
                                        Some(&ctx.ledger_model()),
                                        &EngineError::ProviderDecode(
                                            sanitize_upstream_error_body(core_event_message(
                                                &core_event,
                                            )),
                                        ),
                                        ctx.start,
                                    );
                                    if tx.send(core_event).await.is_err() {
                                        record_client_cancelled(inner, ctx);
                                        ctx.settle(inner, false);
                                        return;
                                    }
                                    continue;
                                }
                                if tx.send(core_event).await.is_err() {
                                    record_client_cancelled(inner, ctx);
                                    ctx.settle(inner, false);
                                    return;
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        let error = map_provider_error(&e, ctx.passthrough_auth);
                        inner.metrics.record_failure();
                        let sanitized =
                            crate::error::fully_sanitize_upstream_error(&e.to_string());
                        if !emit_in_band_error(tx, &sanitized).await {
                            record_client_cancelled(inner, ctx);
                        }
                        emit_response_failed(
                            inner,
                            &ctx.request_id,
                            Some(ctx.provider_name.as_str()),
                            Some(&ctx.ledger_model()),
                            &error,
                            ctx.start,
                        );
                        stream_failed = true;
                        break 'outer;
                    }
                    None => break 'outer,
                }
            }
        }
    }

    // Flush any trailing partial SSE frame.
    let trailing_frames = match ctx.sse_framer.finish() {
        Ok(frames) => frames,
        Err(e) => {
            tracing::warn!(
                request_id = %ctx.request_id,
                error = %e,
                "SSE framer finish error"
            );
            Vec::new()
        }
    };
    for frame in &trailing_frames {
        let events = match ctx.decoder.decode_frame(frame) {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!(
                    request_id = %ctx.request_id,
                    error = %e,
                    "provider decode error on trailing frame"
                );
                break;
            }
        };
        if !emit_final_events(inner, ctx, tx, events).await {
            return;
        }
    }
    if !ctx.decoder.saw_terminal() {
        // The upstream ended without a terminal event: keep valid client
        // framing (synthetic terminal below) but mark the failure in-band
        // and in the ledger.
        stream_failed = true;
        inner.metrics.record_failure();
        emit_response_failed(
            inner,
            &ctx.request_id,
            Some(ctx.provider_name.as_str()),
            Some(&ctx.ledger_model()),
            &EngineError::ProviderDecode(TRUNCATED_STREAM_MESSAGE.to_owned()),
            ctx.start,
        );
        let error = llm_proxy_protocol::core::CoreStreamError::new(
            CoreStreamErrorKind::Upstream,
            TRUNCATED_STREAM_MESSAGE.to_owned(),
        );
        if tx.send(CoreEvent::Error { error }).await.is_err() {
            record_client_cancelled(inner, ctx);
            ctx.settle(inner, false);
            return;
        }
    }
    match ctx.decoder.finish() {
        Ok(final_events) => {
            if !emit_final_events(inner, ctx, tx, final_events).await {
                return;
            }
        }
        Err(e) => {
            tracing::warn!(
                request_id = %ctx.request_id,
                error = %e,
                "provider decoder finish error"
            );
        }
    }
    if stream_failed {
        ctx.settle(inner, false);
    } else {
        record_stream_success(inner, ctx);
        ctx.settle(inner, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(status: u16, retry_after: Option<Duration>, auth_owner: AuthOwner) -> EngineError {
        EngineError::Upstream {
            status,
            body: String::new(),
            auth_owner,
            retry_after,
        }
    }

    /// The decision as the attempt loop makes it: classification and
    /// credential feedback derived from the error itself.
    fn cools_member(error: &EngineError, another_key_available: bool) -> bool {
        let class = classify(error);
        let feedback = credential_feedback(&class, error);
        member_cooldown_applies(&class, error, feedback, another_key_available)
    }

    #[test]
    fn client_faults_never_cool_the_member() {
        for status in [400, 404, 413, 422] {
            assert!(
                !cools_member(&upstream(status, None, AuthOwner::Operator), false),
                "a {status} is deterministic for this request, not a verdict on the member"
            );
        }
        assert!(
            !cools_member(&upstream(401, None, AuthOwner::Client), false),
            "a rejected passthrough token is the client's to fix"
        );
    }

    #[test]
    fn upstream_faults_cool_the_member() {
        assert!(cools_member(
            &upstream(500, None, AuthOwner::Operator),
            true
        ));
        assert!(cools_member(
            &EngineError::UpstreamTimeout("first byte".to_owned()),
            true
        ));
        assert!(
            cools_member(&upstream(429, None, AuthOwner::Operator), true),
            "a throttle without reset advice is not attributed to one key"
        );
        assert!(
            cools_member(
                &upstream(503, Some(Duration::from_secs(5)), AuthOwner::Operator),
                true
            ),
            "a 5xx with Retry-After is still the member's fault"
        );
        assert!(
            cools_member(
                &upstream(429, Some(Duration::from_secs(5)), AuthOwner::Client),
                true
            ),
            "a throttled passthrough token cannot be pinned on one operator key"
        );
    }

    #[test]
    fn a_rejected_operator_key_cools_the_member_only_when_no_other_key_can_serve() {
        let rejected = upstream(401, None, AuthOwner::Operator);
        let throttled = upstream(429, Some(Duration::from_secs(30)), AuthOwner::Operator);
        for error in [&rejected, &throttled] {
            assert!(
                !cools_member(error, true),
                "with a spare key the member keeps serving: {error:?}"
            );
            assert!(
                cools_member(error, false),
                "with no spare key the member cannot serve: {error:?}"
            );
        }
    }
}
