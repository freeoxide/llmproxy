# Project Goal

llm-proxy is one Rust codebase that covers what the proxies in `docs/research/`
offer, runs as a standalone server or inside another program, and lets features
be combined without breaking each other.

## Rules

- **Defaults plus opt-ins.** Each choice below has one safe default, and other
  modes are switched on explicitly. Adding a mode never removes another.
- **One engine.** The server, the embedded router, the SDK and the bindings run
  the same request pipeline.
- **Nothing lost silently.** A field, block or marker is carried, mapped or
  rejected with an error. Usage and finish reasons report what really happened.
- **Features take a slot.** Each feature has a fixed place in the pipeline and
  declares its effect on status codes, the usage ledger, cache keys and prompt
  bytes. Two features that disagree on any of these need a stated rule before
  either ships.

## Ways to use it

- Translation library: convert between provider formats, with no I/O.
- In-process Rust crate: run the full pipeline without HTTP.
- Embedded axum router: mount the proxy in an existing axum app.
- Standalone binary: the `llm-proxy` server and CLI.
- Python and Node bindings: wrap the in-process crate once its API is stable.

## Decisions

### Routing

- `/providers/{provider}/v1/...` reaches exactly that provider and never fails
  over. This is the default.
- `/providers/{pool}/v1/...` reaches a configured pool, with fallback inside it.
  Clients use the same URL shape as a single provider.
- Bare `/v1/...` is off by default. When enabled, it resolves a model name to a
  provider or pool.
- Provider selection stays out of request headers.

This amends locked decisions 1 and 2 in `docs/completed/remodel-proxy.md`. The
source-guard tests stay in place until pools are built.

### Deployment

- One node with SQLite is the default and the first target.
- Storage, limits and cache sit behind traits, so one instance and many use the
  same pipeline.
- Replicas are enabled by optional Postgres and Redis crates.
- Multi-tenant hosting comes after replicas work.

### Features

- MCP gateway, admin web UI, semantic cache and non-chat endpoints are optional
  crates behind cargo features.
- Reseller billing is out of scope. The usage ledger exposes what an external
  billing system needs.

### Request handling

- When client and provider use the same format, requests pass through with only
  the edits the proxy needs, and unknown fields survive.
- When formats differ, every field has a stated policy: carried, mapped or
  rejected.

### Privacy

- Prompts and responses are not stored by default.
- Response caching and payload logging are opt-in.

### Credentials

- Upstream access uses API keys and cloud credentials.
- Subscription OAuth is not built.
- Client fingerprinting and impersonation are not built.

## Plans

The work order and each plan's status are in the [documentation index](README.md).
