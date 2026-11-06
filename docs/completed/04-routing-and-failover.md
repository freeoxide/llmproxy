# 04. Routing and failover

- **Status:** Completed (2026-11-05)
- **Depends on:** 03
- **Audit rows:** None
- **Code-audit bugs:** BUG-14
- **Backlog:** 1, 2, 8, 14, 17, 20, 31, 38
- **Conflicts:** 1, 3, 6, 8

Add pools, retries and failover on top of the routing decision in the
[project goal](../goal.md#routing).

## To do

1. **Attempt-level usage rows (conflict 8).** Add one row per upstream attempt,
   linked to its request. Failed attempts can still cost money.
2. **Error classification (backlog 1).** Sort transient from terminal errors in one
   place and honour `Retry-After`. Retries wrap the send itself, because 429s, 5xx
   responses and connect errors return before any stream exists.
3. **Retry ladder (backlog 1).** Add per-provider cooldowns and one deadline across
   all attempts.
4. **Pools (conflict 1).** Add config for named pools and serve them at
   `/providers/{pool}/v1/...`. Refuse a pool name that matches a provider name at
   config validation. Update the source-guard tests in
   `crates/llm-proxy-server/src/routes/messages.rs` to allow this path only.
5. **Credential pools (backlog 31).** Support several keys per provider, with
   selection and failure feedback in the attempt loop and a ledger column for the
   key used.
6. **Pre-output failover (backlog 2, conflict 3).** Each route gets a hold budget:
   buffer for up to N seconds, then commit and send heartbeats. Failover and status
   codes are possible only before the commit, and errors are in-band after it.
7. **Heartbeats and terminal usage (backlog 14).** Cover silence before the commit,
   and guarantee a terminal usage event on every stream. Heartbeats after the first
   event already exist.
8. **Timeouts (backlog 8, BUG-14).** Make connect, first-byte, idle and total
   timeouts configurable and validate them at config load. This replaces the fixed
   `DEFAULT_UPSTREAM_HEADERS_TIMEOUT`, and the total deadline is new.
9. **Health and quota (backlog 20).** Turn 429 and rate-limit headers into cooldowns
   based on real reset times, with an optional synthetic probe.
10. **Session affinity (backlog 17, conflict 6).** Record session ids on usage rows,
    and let routing take an affinity input. Failover strips state tied to one account
    or provider, such as signed thinking and file ids, or refuses the switch.
11. **Bare `/v1/*`.** Off by default. When enabled, it resolves a model name to a
    provider or pool and runs through the same engine.
12. **Retry signals (backlog 38).** After retries run out, send
    `x-should-retry: false` and a computed `Retry-After`.

## Done when

- `/providers/{provider}/v1/...` behaves exactly as before and never fails over.
- A pool request survives a 429, a 5xx and a connect error from its first member,
  and the ledger shows every attempt.
- A stream that fails before commit fails over, and one that fails after commit ends
  with an in-band error and a failed ledger row.
- With bare `/v1/*` left off, those routes return 404.
