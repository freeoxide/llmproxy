# 10. Language bindings

- **Status:** Completed (2026-11-08)
- **Depends on:** 03, with a stable API
- **Audit rows:** None
- **Code-audit bugs:** None
- **Backlog:** None
- **Conflicts:** None

Give Python and Node users the in-process engine as native packages. These wrap
the engine and add no logic of their own, so they start once its API has stopped changing.

## To do

1. **Define the shared surface.** Cover translation, running a request, streaming events
   and configuration from code.
2. **Python package.** Check that `pyo3` and `maturin` are still maintained, then build
   wheels for Linux, macOS and Windows.
3. **Node package.** Check that `napi` is still maintained, then build prebuilt binaries
   for the same platforms.
4. **Share the tests.** Run the plan 02 contract fixtures through each binding.

## Done when

- `pip install` and `npm install` work on each platform with no Rust toolchain.
- The same fixtures give the same results through Rust, Python and Node.
- Streaming and cancellation work from each language.

## Done

- **Shared surface (step 1).** `crates/llm-proxy-bridge` is the JSON facade
  both bindings wrap: configuration from code (`BridgeConfig` — providers,
  aliases, routing, `request_timeout_secs`, `record_usage`), translation
  (`translate_request` / `translate_response`, pure codec work through the
  same client decoders, provider encoders and cross-format field policy the
  routes use), request execution (`execute` — the full engine pipeline with
  the caller's dialect decoded in and encoded back out, same-format
  passthrough included), and streaming events (`execute_stream` → an
  `EventStream` of tagged core-event JSON with `next`, `next_blocking` and
  `cancel`; dropping or cancelling settles the request as `ClientCancelled`).
  Failures surface as `BridgeError` carrying `kind`, `status`, `message`,
  `retry_after` and `should_retry`. One process-wide tokio runtime is shared
  by every bridge (a per-bridge runtime panics when dropped from an async
  context).
- **Maintenance check (steps 2–3).** crates.io, 2026-11-07: `pyo3` 0.30.1
  (updated 2026-10-14, 298M downloads), `maturin` 1.16.0 (2026-10-29),
  `napi` 3.16.1 (2026-10-31), `napi-derive` 3.7.0, `napi-build` 2.6.0. The
  Node crate resolves napi 3.4.0/napi-derive 3.3.0 instead: the workspace
  MSRV (1.85) caps dependency resolution, and newer napi releases need a
  newer rustc. `@napi-rs/cli` ^3 comes from npm.
- **Python package (step 2).** `bindings/python`: pyo3 0.30 (abi3-py39) +
  maturin. The extension converts dicts/lists to JSON and back; the
  pure-Python layer (`llm_proxy/__init__.py`) unpacks the error tuple into a
  `ProxyError` with attributes and documents the surface. Two wheels are
  built here: `llm_proxy-0.1.0-cp39-abi3-manylinux_2_34_x86_64.whl`
  (installed into a fresh venv whose `pip install` runs no Rust; 13 pytest
  tests pass against the installed wheel) and
  `llm_proxy-0.1.0-cp39-abi3-win_amd64.whl` (mingw-w64 cross-compilation;
  verified as a PE module importing `python3.dll`, but built-not-run — no
  Windows host here). macOS wheels need CI runners with Apple toolchains.
- **Node package (step 3).** `bindings/node`: napi-rs + `@napi-rs/cli`
  (`napi build --platform --release`). The native module returns structured
  results (`{response, error}` objects, JSON-encoded `ErrorInfo` on
  pre-commit stream failures); `index.js` turns them into thrown
  `ProxyError`s and an async-iterable `Stream` whose `cancel()` settles as
  `ClientCancelled`. 13 `node --test` tests pass; `npm install` of the
  packed tarball (`npm pack` → `freeoxide-llm-proxy-0.1.0.tgz`) loads the
  prebuilt `llm-proxy.linux-x64-gnu.node` with no Rust toolchain. A
  Windows x64 binary (`llm-proxy.win32-x64-gnu.node`) is produced by
  mingw-w64 cross-compilation — the napi import library generated from the
  official `node.exe` exports (objdump export table → dlltool) — and
  verified as a PE module importing the `napi_*` symbols from `node.exe`;
  built-not-run on this machine. The npm name is scoped
  (`@freeoxide/llm-proxy`) because the unscoped `llm-proxy` on npm belongs
  to an unrelated package. macOS binaries need CI runners with Apple
  toolchains.
- **Shared tests (step 4).** `bindings/shared/cases.json` holds the shared
  cases: the plan-02 contract fixtures (Claude Code, the Anthropic SDK, the
  OpenAI SDK, Codex — captured and synthesized; provenance in the fixture
  README), the canned upstream responses extracted from the plan-02 contract
  replay harness, the provider templates, the translation cases and the
  cancellation scenario. The Rust bridge test
  (`crates/llm-proxy-bridge/tests/contract.rs`) replays every case and pins
  upstream request, upstream headers, client response, stream events and
  errors to snapshots under `bindings/shared/snapshots/` (regenerate with
  `UPDATE_SNAPSHOTS=1`; a shared normalization replaces wall-clock `created`
  stamps with a marker). The Python and Node suites replay the same cases
  against their own fake upstreams and compare to the same snapshots: the
  same fixtures give the same results through Rust, Python and Node
  (13 tests in each of the three languages). Streaming and cancellation are
  exercised in all three: events flow incrementally, `cancel()` stops
  delivery, the `ClientCancelled` usage row lands, and the proxy stays
  usable.
