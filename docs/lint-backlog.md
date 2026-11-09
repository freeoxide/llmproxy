# Lint backlog

Remaining `cargo clippy --workspace --all-targets --all-features` warnings,
tracked since the lint-infrastructure adoption described in
`SETUP-RUST-LINTS.agent.md`.

Baseline when the Tier B config landed: 961 warnings. Now: **0** — the last
`too_many_lines`/`too_many_arguments` entries were cleared as the code moved
through the crate split; a clean run on 2026-11-08 confirmed the count.

The gates stay as they were: `too_many_lines` and `too_many_arguments`
remain enabled at their real thresholds (100 lines, 6 arguments), with no
`#[allow]`/`#[expect]` overrides and no raised thresholds — split the
functions instead. New debt belongs here only alongside the reason it
cannot be split now.
