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
Linux wheels/binaries build on this machine; macOS and Windows artifacts come
from CI runners with those toolchains.
