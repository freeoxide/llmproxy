# Documentation

Start with the [project goal](goal.md). It holds the goal, the rules every
change follows and the decisions already made.

## Plans

Each plan is one stage of work, with its scope, steps and finish line.

| # | Plan | Status | Depends on |
|---|---|---|---|
| 03 | [Engine and crate split](plans/03-engine.md) | Not started | 02 |
| 04 | [Routing and failover](plans/04-routing-and-failover.md) | Not started | 03 |
| 05 | [Admission and budgets](plans/05-admission-and-budgets.md) | Not started | 04 |
| 06 | [Cache and dedup](plans/06-cache-and-dedup.md) | Not started | 05 |
| 07 | [Hardening and operations](plans/07-hardening-and-operations.md) | Not started | None; steps 10 and 11 need 03 |
| 08 | [Client dialects](plans/08-client-dialects.md) | Not started | 03 |
| 09 | [Optional crates](plans/09-optional-crates.md) | Not started | 05; the semantic cache also needs 06 |
| 10 | [Language bindings](plans/10-bindings.md) | Not started | 03, with a stable API |
| 11 | [Research corrections](plans/11-research-corrections.md) | Not started | None |

How to use them:

- Take the lowest-numbered plan whose dependencies are done. Plans 07 and 11 can
  run alongside the others.
- Update the status here and at the top of the plan in the same change as the
  work.
- When a plan is finished, move it to `completed/` and link it below.
- New work goes into an existing plan, or gets a new plan listed here.

Plan headers point to the [research audit](research/audit-2026-11-05/REPORT.md):

- **Audit rows** are the numbered bugs in its bug table.
- **Code-audit bugs** (`BUG-n`) are in
  [section F of the claim tables](research/audit-2026-11-05/evidence/llm-proxy-claims.md).
- **Conflicts** are the numbered rows in its feature conflicts table.
- **Backlog** numbers are positions in
  [`compensation-backlog.json`](research/compensation-backlog.json).

## Not planned

- Subscription OAuth and client fingerprinting (backlog 12, part of backlog 15,
  conflict 18). See the [credentials decision](goal.md#credentials).
- Reseller billing. See the [features decision](goal.md#features).
- Streaming tool-call fragment repair (backlog 29). The proxy doesn't reassemble
  fragments, so there's nothing to repair.
- Ledger retention work (backlog 33). Retention already works, and there's no
  sidecar data to orphan.

## Reference

- [Protocol architecture](protocol.md): the normalized core and streaming rules.
- [Lint guide](rust-anti-slop-lint-guide.md),
  [lint setup](SETUP-RUST-LINTS.agent.md) and
  [lint backlog](lint-backlog.md).

## Research

- [Research index](research/README.md): competitor surveys, provider notes and
  the feature backlog. Use it as a catalogue of options, not a ranking of demand.
- [Research audit, 2026-11-05](research/audit-2026-11-05/REPORT.md): what was
  checked and what turned out to be wrong. Where the two disagree, trust the
  audit until [plan 11](plans/11-research-corrections.md) is done.

## Completed

- [02. Request fidelity](completed/02-request-fidelity.md)
- [01. Bug fixes](completed/01-bug-fixes.md)
- [Provider-scoped remodel](completed/remodel-proxy.md)
