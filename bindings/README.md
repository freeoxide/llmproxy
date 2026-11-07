# Language bindings

Native packages wrapping the in-process engine through one shared surface,
`crates/llm-proxy-bridge` (JSON in, JSON out; no logic in the bindings):

- [`python/`](python/) — `llm-proxy` on PyPI (pyo3 + maturin, abi3 wheel)
- [`node/`](node/) — `@freeoxide/llm-proxy` on npm (napi-rs, prebuilt
  binaries; the unscoped `llm-proxy` name on npm belongs to an unrelated
  package)
- [`shared/`](shared/) — the binding contract: the plan-02 fixture cases,
  canned upstream responses and snapshots all three test suites share

Each package's suite replays the same plan-02 contract fixtures and compares
against the same snapshots, so the three implementations cannot drift apart.
Linux artifacts are built and install-tested here; Windows x64 artifacts are
produced by mingw-w64 cross-compilation (verified as PE modules with the
right imports, not executed — no Windows host). macOS artifacts need CI
runners with Apple toolchains.
