# 07. Hardening and operations

- **Status:** Not started
- **Depends on:** Nothing; steps 10 and 11 need 03
- **Audit rows:** None
- **Code-audit bugs:** BUG-25
- **Backlog:** 11, 21, 23, 25, 26, 27, 32, 34, 37
- **Conflicts:** 11

Make the proxy safer to expose and easier to run. Items 1 to 6 are small and can ship
at any time.

## To do

1. **Host and Origin validation (backlog 11).** Defend against DNS rebinding in
   `mode = "none"`. Refusing non-loopback binds without auth already exists.
2. **Cleartext credentials (backlog 25).** Refuse an API key on an `http://` URL to an
   off-machine host, with a loopback exemption.
3. **Admin caching (backlog 37).** Send `Cache-Control: no-store` on all admin API
   responses.
4. **Self-loop prevention (backlog 32).** Refuse an upstream that resolves to the
   proxy's own listener, and add a hop header on egress.
5. **`log_format` (BUG-25).** The config value is ignored at startup in
   `apps/llm-proxy/src/commands/serve.rs`. Honour it, with `RUST_LOG_FORMAT` still
   winning as documented.
6. **Config provenance (backlog 34).** Print each resolved setting with its source.
7. **Metrics (backlog 21).** Export the existing counters through Prometheus and OTel.
8. **In-flight visibility (backlog 27).** Add state and elapsed time to the in-flight
   rows the admin API already returns.
9. **`doctor` command (backlog 26).** Run live reachability checks and one-token
   probes, each failure naming its fix.
10. **Config snapshots (conflict 11).** Load config through `ConfigSource` from plan 03
    into an immutable snapshot that can be swapped, with one stated precedence.
11. **Hot reload (backlog 23).** Swap one provider at a time without touching other
    connections, and refuse changes that need a restart rather than half-applying them.

## Done when

- A request with a foreign `Host` or `Origin` is refused in `mode = "none"`.
- `validate` refuses cleartext credentials to a remote host.
- A reload that changes one provider leaves in-flight requests on other providers
  running.
- `doctor` reports a pass or a named fix for each configured provider.
