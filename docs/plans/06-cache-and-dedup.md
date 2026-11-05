# 06. Cache and dedup

- **Status:** Not started
- **Depends on:** 05
- **Audit rows:** 14
- **Code-audit bugs:** BUG-13
- **Backlog:** 15, 24
- **Conflicts:** 4, 5, 7

Replace the dedup window with mechanisms that fit together, and protect provider
prompt caches from the proxy's own edits.

## To do

1. **Retire `dedup_window` (row 14, BUG-13, conflict 4).** It runs first on raw bytes, ignores the caller, covers
   only 500 ms from first sighting and answers 409, which stock SDKs retry. It's in
   `crates/llm-proxy-server/src/middleware.rs` and off by default.
2. **Per-caller single-flight.** Identical concurrent requests from one caller share
   one upstream call.
3. **`Idempotency-Key` (backlog 24).** Opt-in. Claim the key before budget checks,
   replay the stored response on a matching retry, and return 409 only while the
   first request is in flight.
4. **Response cache (conflict 7).** Opt-in and exact-match, with keys scoped to the
   caller. Lookup comes before single-flight and upstream calls. A hit records usage
   as a hit under the rule in plan 05.
5. **Data-class policy (conflict 7).** State what each opt-in stores and for how long.
   Prompts and responses stay unstored by default.
6. **Mutation phases (conflict 5).** Run request edits in a fixed order. Nothing
   changes bytes before the last cache breakpoint without an explicit opt-in.
7. **Prompt-cache hygiene (backlog 15).** Opt-in removal of volatile bytes and marker
   injection, at the provider edge. This depends on the marker fix in
   [plan 01](../completed/01-bug-fixes.md), which is done, and excludes
   fingerprinting.

## Done when

- With every opt-in off, the proxy stores no prompt or response, and a test checks
  this.
- A cache hit succeeds where dedup used to answer 409.
- One caller's cached response can't be returned to another caller.
- A prefix-stability test shows that no enabled feature changes bytes before the last
  breakpoint unless opted in.
