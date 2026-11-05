# AGENTS.md

`llm-proxy` is a Rust workspace (edition 2024, MSRV 1.85, toolchain pinned to
1.96.1 in `rust-toolchain.toml`): the binary `apps/llm-proxy` plus the
`crates/llm-proxy-{core,protocol,provider,storage,api,server}` libraries.
Common tasks have `just` recipes — `just fmt`, `just lint`, `just test`,
`just build`, `just run` — which wrap the cargo commands below.

The project goal, decisions and plans are indexed in `docs/README.md`.

## Rust quality gates

Run after every change:

    cargo fmt --all
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-features

Unused dependencies are checked by `cargo shear` in CI, not by the rustc
`unused_crate_dependencies` lint — that lint flags a dependency in every
compilation target of a crate that does not personally use it, which makes it
useless in a workspace where each `tests/*.rs` is its own target. Run
`cargo shear` before adding or removing a dependency.

### Rules

- Never silence a lint with `#[allow(...)]`. Use `#[expect(lint, reason = "…")]` at the
  narrowest scope, or fix the code. Crate-level `#![allow(...)]` is forbidden.
- No `unwrap()`, `expect()`, `panic!`, `todo!`, or `unimplemented!` in non-test code.
  Propagate errors with `?`. Use `thiserror` for libraries, `anyhow` for applications.
- No indexing (`v[i]`) or string slicing (`&s[..n]`) in non-test code — they panic.
  Use `.get()`, and respect UTF-8 char boundaries when truncating strings.
- Never write `let _ = …` on a `Result` or a `Future`. Handle or log it.
- Every `unsafe` block needs a `// SAFETY:` comment stating the invariant.
- Never hold a `MutexGuard` across an `.await`.
- Prefer `&str` over `String` and `&[T]` over `Vec<T>` in function parameters.
- Do not add a dependency without checking it exists on crates.io, is spelled correctly,
  and is actively maintained. Verify before importing.
- Tests must assert observable behaviour. A test that restates a constant
  (`assert_eq!(RETRIES, 3)`) or mirrors the implementation's branches is worthless —
  test a property instead.
- Public items should be reachable through exactly one path. Do not add re-exports to
  paper over a refactor.
- Do not put design narratives, "why we chose X over Y" essays, or self-report tables
  into user-facing documentation.
