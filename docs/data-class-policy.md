# Data-class policy

What the proxy stores, where, and for how long. Prompts and responses are
**not stored by default**; every store below that holds them is an opt-in.

## What is stored by default

| Data | Stored | Where | Retention |
|---|---|---|---|
| Usage ledger rows | Yes, when `[storage]` is set | SQLite (`usage.db`) | `max_age` / `max_rows` from `[storage]`; forever if unset |
| Ledger rows (no `[storage]`) | In memory only, discarded | none | none |
| Prompt bytes | No | — | — |
| Response bytes | No | — | — |
| Body digests | SHA-256 of the raw request body, on ledger rows | with the row | with the row |
| Caller identity | Key id or `"anonymous"`, on ledger rows | with the row | with the row |

The ledger never carries prompt or response content: rows hold digests,
token counts, costs, model names and error kinds. A test
(`cache_and_dedup::with_every_opt_in_off_nothing_is_stored_and_every_request_runs`)
asserts that no recorded row contains prompt or response text.

## Opt-in stores

### Response cache — `[server.cache] enabled = true`

- **Stores:** complete non-streaming responses (the decoded response:
  content, usage, cost, stop reason), keyed by caller, provider, route,
  upstream model and body digest. Streaming responses are shared while in
  flight (single-flight) but never stored.
- **Where:** in process memory. Nothing is written to disk. A restart
  empties it.
- **Retention:** `ttl` (default 10m) from storage; at most `max_entries`
  (default 1000), oldest stored evicted first.
- **Served to:** the same caller only. Keys include the caller identity, so
  one caller's stored response is never returned to another.

### `Idempotency-Key` replay — `[server.idempotency] enabled = true`

- **Stores:** the completed outcome of a request that carried an
  `Idempotency-Key` header: the full response (non-streaming) or the full
  collected event list (streaming), keyed by caller + key. The body digest
  and provider route are stored alongside for match checking. Requests
  without the header store nothing.
- **Where:** in process memory. Nothing is written to disk.
- **Retention:** `ttl` (default 24h) from completion, swept as it passes; at
  most `max_entries` (default 1000) completed outcomes, oldest completed
  evicted first. Failed requests are not stored — their key is released so a
  retry executes.
- **While in flight:** the first request holds the key; a concurrent request
  with the same key gets 409 until it finishes. Nothing is stored for the
  conflict itself.

### Single-flight — always on, stores nothing

- Identical concurrent requests from one caller share one upstream call.
  The leader's events and outcome are held in memory **only while requests
  are waiting on them**, then dropped. Nothing persists after completion;
  there is no TTL because there is no storage.
- A joiner shares the leader's fate while attached: if the leader is refused
  (guard, budget) or fails before its first byte, the joiner gets the same
  error, and if the leader goes away entirely before its first byte the
  joiner re-runs the request once itself. Once a shared stream has committed,
  a disconnecting leader ends it for its joiners too — they receive the
  events collected so far followed by an in-band error, not the remainder of
  the response.

### Prompt-cache hygiene — `[provider.prompt_cache]`

- **Stores:** nothing. It edits outgoing request bytes (strips configured
  volatile pointers, optionally injects cache markers) at the provider
  edge. It reads `[provider.prompt_cache]` from config; no request or
  response data is retained.

## Not built

- Payload logging, shadow evaluation and semantic caching are not in the
  proxy. Optional crates (plan 09) that add stores must extend this page.
- Subscription OAuth and client fingerprinting are out of scope; see the
  [credentials decision](goal.md#credentials).
